//! One ordered injector, separate from both the hook message loop and control pump.
//! Queue admission is asynchronous only for ordinary remote input. Handoff/local
//! batches and release barriers still acknowledge actual Win32 completion.
use std::collections::VecDeque;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::{
    COMMAND_DEADLINE, COMMAND_WAIT, MAX_BATCH, NativeInputRecoveryReason, NativeInputShutdown,
    RELIABLE_EVENT_RESERVE, SLOW_INJECTION_COMMAND, SendInputEndpoint, Shared,
};
use crate::cross_screen::InputEvent;
use crate::cross_screen::{KeyTransition, WheelAxis};

const QUEUE_CAPACITY: usize = MAX_BATCH;
const LOSSY_LIMIT: usize = QUEUE_CAPACITY - RELIABLE_EVENT_RESERVE;
const WHEEL_MAX_AGE: Duration = Duration::from_millis(250);

pub(super) trait Endpoint {
    fn inject(&mut self, event: InputEvent) -> io::Result<()>;
    fn release_all(&mut self) -> (usize, Option<io::Error>);
    fn outstanding_count(&self) -> usize;
    fn injected_events(&self) -> u64;
}

impl Endpoint for SendInputEndpoint {
    fn inject(&mut self, event: InputEvent) -> io::Result<()> {
        self.inject(event)
    }
    fn release_all(&mut self) -> (usize, Option<io::Error>) {
        self.release_all()
    }
    fn outstanding_count(&self) -> usize {
        self.outstanding_count()
    }
    fn injected_events(&self) -> u64 {
        self.injected_events()
    }
}

struct Job {
    events: Vec<InputEvent>,
    generation: u64,
    queued: Instant,
    deadline: Option<Instant>,
    cancelled: Arc<AtomicBool>,
    reply: Option<mpsc::SyncSender<io::Result<()>>>,
}

impl Job {
    fn valid(&self, shared: &Shared) -> bool {
        !shared.stop.load(Ordering::Acquire)
            && !self.cancelled.load(Ordering::Acquire)
            && self.generation == shared.generation.load(Ordering::Acquire)
            && self
                .deadline
                .is_none_or(|deadline| Instant::now() < deadline)
    }

    fn finish(self, result: io::Result<()>) {
        if let Some(reply) = self.reply {
            let _ = reply.send(result);
        }
    }
}

#[derive(Default)]
struct Pending {
    jobs: VecDeque<Job>,
    events: usize,
    closed: bool,
}

pub(super) struct InjectionQueue {
    pending: Mutex<Pending>,
    available: Condvar,
}

impl InjectionQueue {
    pub(super) fn new() -> Self {
        Self {
            pending: Mutex::new(Pending::default()),
            available: Condvar::new(),
        }
    }

    pub(super) fn wake(&self) {
        self.available.notify_one();
    }

    pub(super) fn enqueue(&self, shared: &Shared, events: &[InputEvent]) -> io::Result<()> {
        validate_batch(shared, events)?;
        let mut pending = self.lock()?;
        if pending.closed {
            return Err(stopped());
        }
        let generation = shared.generation.load(Ordering::Acquire);
        for &event in events {
            // Only adjacent absolute moves can coalesce. Never cross a wheel or
            // key/button boundary (Ctrl+wheel and drag positions must keep order).
            if event.is_mouse_move()
                && let Some(previous) = pending.jobs.back_mut()
                && previous.reply.is_none()
                && previous.generation == generation
                && previous.events[0].is_mouse_move()
            {
                previous.events[0] = event;
                previous.queued = Instant::now();
                shared
                    .coalesced_injection_moves
                    .fetch_add(1, Ordering::Relaxed);
                continue;
            }
            if matches!(
                event,
                InputEvent::Wheel { .. } | InputEvent::MouseMove { .. }
            ) && pending.events >= LOSSY_LIMIT
            {
                if matches!(event, InputEvent::Wheel { .. }) {
                    shared
                        .dropped_injection_wheels
                        .fetch_add(1, Ordering::Relaxed);
                } else {
                    shared
                        .dropped_injection_moves
                        .fetch_add(1, Ordering::Relaxed);
                }
                continue;
            }
            if pending.events >= QUEUE_CAPACITY {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "native reliable injection queue full",
                ));
            }
            pending.jobs.push_back(Job {
                events: vec![event],
                generation,
                queued: Instant::now(),
                deadline: None,
                cancelled: Arc::new(AtomicBool::new(false)),
                reply: None,
            });
            pending.events += 1;
        }
        shared.injection_queue_peak.fetch_max(
            u64::try_from(pending.events).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
        drop(pending);
        self.wake();
        Ok(())
    }

    pub(super) fn inject(&self, shared: &Shared, events: &[InputEvent]) -> io::Result<()> {
        validate_batch(shared, events)?;
        self.request(shared, Some(events))
    }

    pub(super) fn barrier(&self, shared: &Shared) -> io::Result<()> {
        self.request(shared, None)
    }

    fn request(&self, shared: &Shared, events: Option<&[InputEvent]>) -> io::Result<()> {
        let (reply, response) = mpsc::sync_channel(1);
        let cancelled = Arc::new(AtomicBool::new(false));
        let mut pending = self.lock()?;
        if pending.closed {
            return Err(stopped());
        }
        let barrier = events.is_none();
        let generation = if barrier {
            // The generation changes under the queue lock. Any in-flight call
            // finishes on the sole injector before its actual ledger is released;
            // queued presses/wheels from the old generation can never replay.
            let generation = shared
                .generation
                .fetch_add(1, Ordering::AcqRel)
                .wrapping_add(1);
            for job in pending.jobs.drain(..) {
                job.finish(Err(stale()));
            }
            pending.events = 0;
            generation
        } else {
            shared.generation.load(Ordering::Acquire)
        };
        let events = events.unwrap_or_default();
        if pending.events.saturating_add(events.len()) > QUEUE_CAPACITY {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "native injection queue full",
            ));
        }
        pending.events += events.len();
        let queued = Instant::now();
        pending.jobs.push_back(Job {
            events: events.to_vec(),
            generation,
            queued,
            deadline: (!barrier).then_some(queued + COMMAND_DEADLINE),
            cancelled: Arc::clone(&cancelled),
            reply: Some(reply),
        });
        drop(pending);
        self.wake();
        if let Ok(result) = response.recv_timeout(COMMAND_WAIT) {
            result
        } else {
            cancelled.store(true, Ordering::Release);
            shared.invalidate_injection();
            shared.wake();
            self.wake();
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "native injection barrier or synchronous batch did not complete",
            ))
        }
    }

    fn lock(&self) -> io::Result<std::sync::MutexGuard<'_, Pending>> {
        self.pending
            .lock()
            .map_err(|_| io::Error::other("native injection queue poisoned"))
    }

    fn next(&self) -> io::Result<Option<Job>> {
        let mut pending = self.lock()?;
        if pending.jobs.is_empty() && !pending.closed {
            pending = self
                .available
                .wait_timeout(pending, Duration::from_millis(20))
                .map_err(|_| io::Error::other("native injection wait poisoned"))?
                .0;
        }
        let job = pending.jobs.pop_front();
        if let Some(job) = &job {
            pending.events -= job.events.len();
        }
        Ok(job)
    }

    fn close(&self) {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        pending.closed = true;
        for job in pending.jobs.drain(..) {
            job.finish(Err(stopped()));
        }
        pending.events = 0;
        drop(pending);
        self.wake();
    }
}

fn validate_batch(shared: &Shared, events: &[InputEvent]) -> io::Result<()> {
    if shared.stop.load(Ordering::Acquire) {
        return Err(stopped());
    }
    if events.len() > MAX_BATCH {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "native input batch exceeds limit",
        ));
    }
    Ok(())
}

fn stopped() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "native injector stopped")
}

fn stale() -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        "stale or cancelled native injection",
    )
}

struct WorkerGuard {
    shared: Arc<Shared>,
    queue: Arc<InjectionQueue>,
}

impl Drop for WorkerGuard {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.shared
                .fail("native injector panicked; release is unconfirmed");
            self.shared.wake();
            self.shared.notify_event();
        }
        self.queue.close();
    }
}

pub(super) fn start<E: Endpoint + 'static>(
    shared: Arc<Shared>,
    queue: Arc<InjectionQueue>,
    endpoint: impl FnOnce() -> E + Send + 'static,
) -> io::Result<JoinHandle<NativeInputShutdown>> {
    std::thread::Builder::new()
        .name("pierdesk-native-inject".into())
        .spawn(move || {
            let _guard = WorkerGuard {
                shared: Arc::clone(&shared),
                queue: Arc::clone(&queue),
            };
            #[cfg(not(test))]
            crate::traffic_control::promote_interactive_thread();
            run(&shared, &queue, &mut endpoint())
        })
}

fn run(
    shared: &Shared,
    queue: &InjectionQueue,
    endpoint: &mut impl Endpoint,
) -> NativeInputShutdown {
    let mut generation = shared.generation.load(Ordering::Acquire);
    while !shared.stop.load(Ordering::Acquire) {
        let next = match queue.next() {
            Ok(job) => job,
            Err(error) => {
                shared.fail(error.to_string());
                break;
            }
        };
        let current = shared.generation.load(Ordering::Acquire);
        if generation != current {
            let (_, error) = release_native(shared, endpoint);
            if let Some(error) = error {
                shared.fail(format!("native release barrier failed: {error}"));
                if let Some(job) = next {
                    job.finish(Err(error));
                }
                break;
            }
            generation = current;
        }
        if let Some(job) = next {
            let result = execute(shared, endpoint, &job);
            if result.is_err()
                && job.reply.is_some()
                && job.generation == shared.generation.load(Ordering::Acquire)
            {
                shared.invalidate_injection();
                shared.wake();
            }
            job.finish(result);
        }
    }
    let mut last_error = None;
    for _ in 0..3 {
        let (_, error) = release_native(shared, endpoint);
        last_error = error.map(|error| error.to_string());
        if last_error.is_none() {
            break;
        }
    }
    shared.wake();
    shared.notify_event();
    NativeInputShutdown {
        clean: endpoint.outstanding_count() == 0 && last_error.is_none(),
        remaining_inputs: endpoint.outstanding_count(),
        injected_events: endpoint.injected_events(),
        last_error,
    }
}

fn execute(shared: &Shared, endpoint: &mut impl Endpoint, job: &Job) -> io::Result<()> {
    if !job.valid(shared) {
        return Err(stale());
    }
    for &event in &job.events {
        if !job.valid(shared) {
            return Err(stale());
        }
        let queued = job.queued.elapsed();
        let wheel = matches!(event, InputEvent::Wheel { .. });
        if job.reply.is_none() && wheel && queued > WHEEL_MAX_AGE {
            shared
                .dropped_injection_wheels
                .fetch_add(1, Ordering::Relaxed);
            note(shared, job, event, queued, Duration::ZERO, "expired");
            continue;
        }
        if event.is_mouse_move() && shared.target_capture.load(Ordering::Acquire) {
            shared
                .injected_pointer_in_flight
                .store(true, Ordering::Release);
            shared.mark_injected_pointer();
        }
        let started = Instant::now();
        let result = endpoint.inject(event);
        let elapsed = started.elapsed();
        if event.is_mouse_move() {
            shared.mark_injected_pointer();
            shared
                .injected_pointer_in_flight
                .store(false, Ordering::Release);
        }
        shared.observe_injection_command(elapsed);
        if elapsed > SLOW_INJECTION_COMMAND || queued > SLOW_INJECTION_COMMAND || result.is_err() {
            let outcome = result.as_ref().map_or_else(
                |error| {
                    format!(
                        "failed error_kind={:?} error_code={:?}",
                        error.kind(),
                        error.raw_os_error()
                    )
                },
                |()| "completed".to_owned(),
            );
            note(shared, job, event, queued, elapsed, &outcome);
        }
        if let Err(error) = result {
            if wheel && job.reply.is_none() {
                shared
                    .failed_injection_wheels
                    .fetch_add(1, Ordering::Relaxed);
                continue;
            }
            if event.is_mouse_move()
                && job.reply.is_none()
                && error.kind() == io::ErrorKind::PermissionDenied
            {
                // The interactive desktop can temporarily reject SendInput (for
                // example while Windows shows a secure UAC prompt). Return control
                // locally and cancel the old generation. The worker must still
                // release its actual key/button ledger before another handoff;
                // a failed release remains a fatal native fault.
                shared.recover_local(NativeInputRecoveryReason::InjectionBlocked);
                return Err(error);
            }
            if job.reply.is_none() {
                shared.fail(format!("native {} injection failed: {error}", kind(event)));
                shared.notify_event();
            } else {
                shared.invalidate_injection();
            }
            shared.wake();
            return Err(error);
        }
    }
    // A late Down is now in the actual endpoint ledger. Its generation barrier
    // must release it; never acknowledge completion to the old handoff caller.
    if !job.valid(shared) {
        return Err(stale());
    }
    Ok(())
}

fn release_native(shared: &Shared, endpoint: &mut impl Endpoint) -> (usize, Option<io::Error>) {
    let started = Instant::now();
    let result = endpoint.release_all();
    let elapsed = started.elapsed();
    shared.observe_injection_command(elapsed);
    if elapsed > SLOW_INJECTION_COMMAND || result.1.is_some() {
        record_probe(
            shared,
            format!(
                "event=native_injection kind=release_all outcome={} call_us={} error_code={:?}",
                if result.1.is_none() {
                    "completed"
                } else {
                    "failed"
                },
                elapsed.as_micros(),
                result.1.as_ref().and_then(io::Error::raw_os_error),
            ),
        );
    }
    result
}

fn kind(event: InputEvent) -> &'static str {
    match event {
        InputEvent::Key {
            transition: KeyTransition::Down,
            ..
        } => "key_down",
        InputEvent::Key {
            transition: KeyTransition::Up,
            ..
        } => "key_up",
        InputEvent::MouseButton {
            transition: KeyTransition::Down,
            ..
        } => "button_down",
        InputEvent::MouseButton {
            transition: KeyTransition::Up,
            ..
        } => "button_up",
        InputEvent::MouseMove { .. } => "move",
        InputEvent::Wheel {
            axis: WheelAxis::Vertical,
            ..
        } => "wheel_vertical",
        InputEvent::Wheel {
            axis: WheelAxis::Horizontal,
            ..
        } => "wheel_horizontal",
    }
}

fn note(
    shared: &Shared,
    job: &Job,
    event: InputEvent,
    queued: Duration,
    elapsed: Duration,
    outcome: &str,
) {
    let time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |time| time.as_millis());
    record_probe(
        shared,
        format!(
            "event=native_injection at_unix_ms={time} generation={} kind={} outcome={outcome} queue_us={} call_us={} total_us={}",
            job.generation,
            kind(event),
            queued.as_micros(),
            elapsed.as_micros(),
            job.queued.elapsed().as_micros(),
        ),
    );
}

fn record_probe(shared: &Shared, detail: String) {
    if let Ok(mut probes) = shared.cursor_probes.try_lock() {
        if probes.len() == 32 {
            probes.pop_front();
        }
        probes.push_back(detail);
    }
    shared.notify_event();
}

#[cfg(test)]
mod tests {
    use super::super::NativeInputEvent;
    use super::*;
    use crate::cross_screen::{InputLedger, MouseButton, PhysicalKey};
    use std::sync::atomic::AtomicU64;

    #[derive(Default)]
    struct Observed {
        events: Vec<InputEvent>,
        ledger: InputLedger,
    }

    struct FakeEndpoint {
        observed: Arc<Mutex<Observed>>,
        block: Option<(InputEvent, mpsc::SyncSender<()>, mpsc::Receiver<()>)>,
        fail: Option<InputEvent>,
        release_failures: Arc<AtomicU64>,
    }

    impl Endpoint for FakeEndpoint {
        fn inject(&mut self, event: InputEvent) -> io::Result<()> {
            if self
                .block
                .as_ref()
                .is_some_and(|(blocked, _, _)| event == *blocked)
            {
                let (_, entered, proceed) = self.block.take().unwrap();
                entered.send(()).unwrap();
                proceed.recv_timeout(Duration::from_secs(5)).unwrap();
            }
            if self.fail == Some(event) {
                return Err(if event.is_mouse_move() {
                    io::Error::from_raw_os_error(5)
                } else {
                    io::Error::other("simulated native failure")
                });
            }
            let mut observed = self.observed.lock().unwrap();
            let _ = observed.ledger.apply(event);
            observed.events.push(event);
            Ok(())
        }
        fn release_all(&mut self) -> (usize, Option<io::Error>) {
            if self
                .release_failures
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1))
                .is_ok()
            {
                return (0, Some(io::Error::other("simulated release failure")));
            }
            let mut observed = self.observed.lock().unwrap();
            let releases = observed.ledger.release_events();
            for &event in &releases {
                let _ = observed.ledger.apply(event);
                observed.events.push(event);
            }
            (releases.len(), None)
        }
        fn outstanding_count(&self) -> usize {
            self.observed.lock().unwrap().ledger.outstanding_count()
        }
        fn injected_events(&self) -> u64 {
            u64::try_from(self.observed.lock().unwrap().events.len()).unwrap()
        }
    }

    struct Fixture {
        shared: Arc<Shared>,
        queue: Arc<InjectionQueue>,
        observed: Arc<Mutex<Observed>>,
        release_failures: Arc<AtomicU64>,
        worker: Option<JoinHandle<NativeInputShutdown>>,
        // Dropping a failed assertion must still unblock and join the fake worker.
        proceed: Option<mpsc::SyncSender<()>>,
    }

    impl Fixture {
        fn new(block: Option<InputEvent>, fail: Option<InputEvent>) -> (Self, mpsc::Receiver<()>) {
            let shared = Arc::new(Shared::new());
            shared.target_capture.store(true, Ordering::Release);
            let queue = Arc::new(InjectionQueue::new());
            let observed = Arc::new(Mutex::new(Observed::default()));
            let release_failures = Arc::new(AtomicU64::new(0));
            let (entered, started) = mpsc::sync_channel(1);
            let (proceed, wait) = mpsc::sync_channel(1);
            let endpoint = FakeEndpoint {
                observed: Arc::clone(&observed),
                release_failures: Arc::clone(&release_failures),
                block: block.map(|event| (event, entered, wait)),
                fail,
            };
            let worker = start(Arc::clone(&shared), Arc::clone(&queue), move || endpoint).unwrap();
            (
                Self {
                    shared,
                    queue,
                    observed,
                    release_failures,
                    worker: Some(worker),
                    proceed: Some(proceed),
                },
                started,
            )
        }

        fn unblock(&mut self) {
            let _ = self.proceed.take().unwrap().send(());
        }

        fn flush(&self) {
            self.queue.inject(&self.shared, &[]).unwrap();
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            if let Some(proceed) = self.proceed.take() {
                let _ = proceed.send(());
            }
            self.shared.stop.store(true, Ordering::Release);
            self.queue.wake();
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
        }
    }

    fn button(transition: KeyTransition) -> InputEvent {
        InputEvent::MouseButton {
            button: MouseButton::Left,
            transition,
        }
    }
    fn ctrl(transition: KeyTransition) -> InputEvent {
        InputEvent::Key {
            physical: PhysicalKey {
                scan_code: 29,
                extended: false,
            },
            virtual_key: 0xa2,
            transition,
        }
    }
    fn wheel(axis: WheelAxis) -> InputEvent {
        InputEvent::Wheel { axis, delta: 120 }
    }

    #[test]
    fn slow_wheel_does_not_cancel_control_or_reliable_transitions_and_records_real_duration() {
        let scroll = wheel(WheelAxis::Vertical);
        let (mut f, started) = Fixture::new(Some(scroll), None);
        f.queue
            .enqueue(
                &f.shared,
                &[
                    ctrl(KeyTransition::Down),
                    button(KeyTransition::Down),
                    scroll,
                ],
            )
            .unwrap();
        started.recv_timeout(COMMAND_WAIT).unwrap();
        let generation = f.shared.generation.load(Ordering::Acquire);
        f.queue
            .enqueue(
                &f.shared,
                &[
                    InputEvent::MouseMove { x: 10, y: 10 },
                    InputEvent::MouseMove { x: 20, y: 30 },
                    button(KeyTransition::Up),
                    ctrl(KeyTransition::Up),
                ],
            )
            .unwrap();
        // Longer than the old caller timeout. No Windows input is injected.
        std::thread::sleep(COMMAND_WAIT + Duration::from_millis(30));
        assert!(f.shared.target_capture.load(Ordering::Acquire));
        assert!(!f.shared.release_requested.load(Ordering::Acquire));
        assert!(f.shared.error().is_none());
        assert_eq!(f.shared.generation.load(Ordering::Acquire), generation);
        // Capture uses a different queue and remains usable while SendInput waits.
        f.shared
            .push(NativeInputEvent::Physical(InputEvent::MouseMove {
                x: 1,
                y: 2,
            }));
        assert_eq!(f.shared.take_events().unwrap().len(), 1);
        f.unblock();
        f.flush();
        let observed = f.observed.lock().unwrap();
        assert_eq!(
            observed.events,
            [
                ctrl(KeyTransition::Down),
                button(KeyTransition::Down),
                scroll,
                InputEvent::MouseMove { x: 20, y: 30 },
                button(KeyTransition::Up),
                ctrl(KeyTransition::Up)
            ]
        );
        assert_eq!(observed.ledger.outstanding_count(), 0);
        let diagnostics = f.shared.diagnostics();
        assert!(diagnostics.max_injection_command_micros > 1_000_000);
        assert_eq!(diagnostics.slow_injection_commands, 1);
        assert_eq!(diagnostics.coalesced_injection_moves, 1);
        let probes = f.shared.cursor_probes.lock().unwrap();
        assert!(
            probes
                .iter()
                .any(|line| line.contains("kind=wheel_vertical outcome=completed"))
        );
    }

    #[test]
    fn expired_queued_wheel_is_dropped_without_dropping_its_modifier_release() {
        let scroll = wheel(WheelAxis::Vertical);
        let (mut f, started) = Fixture::new(Some(scroll), None);
        f.queue
            .enqueue(&f.shared, &[ctrl(KeyTransition::Down), scroll])
            .unwrap();
        started.recv_timeout(COMMAND_WAIT).unwrap();
        f.queue
            .enqueue(
                &f.shared,
                &[wheel(WheelAxis::Horizontal), ctrl(KeyTransition::Up)],
            )
            .unwrap();
        // Age only the pending wheel; reliable releases have no lossy deadline.
        f.queue
            .pending
            .lock()
            .unwrap()
            .jobs
            .front_mut()
            .unwrap()
            .queued = Instant::now()
            .checked_sub(WHEEL_MAX_AGE + Duration::from_millis(1))
            .unwrap();
        f.unblock();
        f.flush();
        assert_eq!(
            f.observed.lock().unwrap().events,
            [ctrl(KeyTransition::Down), scroll, ctrl(KeyTransition::Up)]
        );
        assert_eq!(f.shared.diagnostics().dropped_injection_wheels, 1);
        assert!(f.shared.error().is_none());
    }

    #[test]
    fn release_barrier_cancels_queued_presses_and_releases_a_late_in_flight_down() {
        let down = button(KeyTransition::Down);
        let (mut f, started) = Fixture::new(Some(down), None);
        f.queue.enqueue(&f.shared, &[down]).unwrap();
        started.recv_timeout(COMMAND_WAIT).unwrap();
        f.queue
            .enqueue(
                &f.shared,
                &[ctrl(KeyTransition::Down), wheel(WheelAxis::Vertical)],
            )
            .unwrap();
        let shared = Arc::clone(&f.shared);
        let queue = Arc::clone(&f.queue);
        let generation = shared.generation.load(Ordering::Acquire);
        let (done, response) = mpsc::sync_channel(1);
        let waiter = std::thread::spawn(move || done.send(queue.barrier(&shared)).unwrap());
        let deadline = Instant::now() + COMMAND_WAIT;
        while f.shared.generation.load(Ordering::Acquire) == generation {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(matches!(
            response.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        f.unblock();
        response.recv_timeout(COMMAND_WAIT).unwrap().unwrap();
        waiter.join().unwrap();
        let observed = f.observed.lock().unwrap();
        assert_eq!(observed.events, [down, button(KeyTransition::Up)]);
        assert_eq!(observed.ledger.outstanding_count(), 0);
        assert!(f.shared.target_capture.load(Ordering::Acquire));
    }

    #[test]
    fn a_failed_wheel_is_nonfatal_but_a_failed_button_is_reported() {
        let scroll = wheel(WheelAxis::Horizontal);
        let (f, _) = Fixture::new(None, Some(scroll));
        f.queue
            .enqueue(
                &f.shared,
                &[ctrl(KeyTransition::Down), scroll, ctrl(KeyTransition::Up)],
            )
            .unwrap();
        f.flush();
        assert_eq!(f.observed.lock().unwrap().ledger.outstanding_count(), 0);
        assert_eq!(f.shared.diagnostics().failed_injection_wheels, 1);
        assert!(f.shared.error().is_none());
        let down = button(KeyTransition::Down);
        let (mut broken, _) = Fixture::new(None, Some(down));
        broken
            .queue
            .enqueue(&broken.shared, &[ctrl(KeyTransition::Down), down])
            .unwrap();
        broken.worker.take().unwrap().join().unwrap();
        assert_eq!(
            broken.observed.lock().unwrap().events,
            [ctrl(KeyTransition::Down), ctrl(KeyTransition::Up)]
        );
        assert_eq!(
            broken.observed.lock().unwrap().ledger.outstanding_count(),
            0
        );
        assert!(
            broken
                .shared
                .error()
                .unwrap()
                .contains("button_down injection failed")
        );
        assert!(broken.queue.enqueue(&broken.shared, &[down]).is_err());
    }

    #[test]
    fn blocked_remote_move_returns_control_without_replaying_stale_keys() {
        let move_event = InputEvent::MouseMove { x: 10, y: 20 };
        let (f, _) = Fixture::new(None, Some(move_event));
        f.queue
            .enqueue(
                &f.shared,
                &[
                    button(KeyTransition::Down),
                    move_event,
                    ctrl(KeyTransition::Down),
                ],
            )
            .unwrap();
        let deadline = Instant::now() + COMMAND_WAIT;
        while !f.shared.emergency.load(Ordering::Acquire) {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        // The barrier waits for the worker to release the actually injected
        // button before a later control session can use the same injector.
        f.queue.barrier(&f.shared).unwrap();
        assert_eq!(
            f.shared.take_events().unwrap(),
            [NativeInputEvent::EmergencyReturn]
        );
        assert!(!f.shared.stop.load(Ordering::Acquire));
        assert!(!f.shared.target_capture.load(Ordering::Acquire));
        assert_eq!(
            f.shared.diagnostics().last_recovery,
            Some(NativeInputRecoveryReason::InjectionBlocked)
        );
        assert_eq!(f.shared.diagnostics().recovery_returns, 1);
        assert_eq!(
            f.observed.lock().unwrap().events,
            [button(KeyTransition::Down), button(KeyTransition::Up)]
        );
        f.queue
            .enqueue(&f.shared, &[InputEvent::MouseMove { x: 30, y: 40 }])
            .unwrap();
        f.flush();
        assert_eq!(
            f.observed.lock().unwrap().events,
            [
                button(KeyTransition::Down),
                button(KeyTransition::Up),
                InputEvent::MouseMove { x: 30, y: 40 }
            ]
        );
    }

    #[test]
    fn blocked_move_with_unreleased_button_still_fails_closed() {
        let move_event = InputEvent::MouseMove { x: 10, y: 20 };
        let (mut f, _) = Fixture::new(None, Some(move_event));
        f.release_failures.store(4, Ordering::Release);
        f.queue
            .enqueue(&f.shared, &[button(KeyTransition::Down), move_event])
            .unwrap();
        let shutdown = f.worker.take().unwrap().join().unwrap();
        assert!(!shutdown.clean);
        assert_eq!(shutdown.remaining_inputs, 1);
        assert!(f.shared.stop.load(Ordering::Acquire));
        assert!(
            f.shared
                .error()
                .unwrap()
                .contains("native release barrier failed")
        );
    }

    #[test]
    fn injection_queue_reserves_space_for_reliable_events_and_never_coalesces_across_them() {
        let shared = Shared::new();
        let queue = InjectionQueue::new();
        for _ in 0..(LOSSY_LIMIT / 2) {
            queue
                .enqueue(
                    &shared,
                    &[
                        InputEvent::MouseMove { x: 1, y: 2 },
                        wheel(WheelAxis::Vertical),
                    ],
                )
                .unwrap();
        }
        assert_eq!(queue.pending.lock().unwrap().events, LOSSY_LIMIT);
        queue
            .enqueue(&shared, &[wheel(WheelAxis::Horizontal)])
            .unwrap();
        for _ in 0..RELIABLE_EVENT_RESERVE {
            queue
                .enqueue(&shared, &[button(KeyTransition::Up)])
                .unwrap();
        }
        assert_eq!(queue.pending.lock().unwrap().events, QUEUE_CAPACITY);
        assert_eq!(
            queue
                .enqueue(&shared, &[button(KeyTransition::Up)])
                .unwrap_err()
                .kind(),
            io::ErrorKind::WouldBlock
        );
        assert_eq!(shared.diagnostics().dropped_injection_wheels, 1);
        assert_eq!(shared.diagnostics().coalesced_injection_moves, 0);
    }

    #[test]
    fn failed_native_release_never_acknowledges_the_barrier() {
        let (mut f, _) = Fixture::new(None, None);
        f.queue
            .enqueue(&f.shared, &[button(KeyTransition::Down)])
            .unwrap();
        f.flush();
        f.release_failures.store(4, Ordering::Release);
        assert!(f.queue.barrier(&f.shared).is_err());
        let report = f.worker.take().unwrap().join().unwrap();
        assert_eq!(report.remaining_inputs, 1);
        assert!(!report.clean);
        assert!(
            f.shared
                .error()
                .unwrap()
                .contains("native release barrier failed")
        );
    }

    #[test]
    fn a_cancelled_synchronous_batch_releases_its_late_down_and_never_replays_the_rest() {
        let down = button(KeyTransition::Down);
        let (mut f, started) = Fixture::new(Some(down), None);
        let queue = Arc::clone(&f.queue);
        let shared = Arc::clone(&f.shared);
        let caller =
            std::thread::spawn(move || queue.inject(&shared, &[down, ctrl(KeyTransition::Down)]));
        started.recv_timeout(COMMAND_WAIT).unwrap();
        assert_eq!(
            caller.join().unwrap().unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        assert!(!f.shared.target_capture.load(Ordering::Acquire));
        f.unblock();
        f.queue.barrier(&f.shared).unwrap();
        let observed = f.observed.lock().unwrap();
        assert_eq!(observed.events, [down, button(KeyTransition::Up)]);
        assert_eq!(observed.ledger.outstanding_count(), 0);
    }

    #[test]
    fn raw_pointer_echo_is_filtered_for_the_whole_in_flight_call_and_after_completion() {
        let motion = InputEvent::MouseMove { x: 300, y: 400 };
        let (mut f, started) = Fixture::new(Some(motion), None);
        f.queue.enqueue(&f.shared, &[motion]).unwrap();
        started.recv_timeout(COMMAND_WAIT).unwrap();
        *f.shared.injected_raw_echo_deadline.lock().unwrap() =
            Instant::now().checked_sub(Duration::from_secs(1));
        assert!(f.shared.injected_pointer_echo_is_recent());
        f.unblock();
        f.flush();
        assert!(!f.shared.injected_pointer_in_flight.load(Ordering::Acquire));
        assert!(f.shared.injected_pointer_echo_is_recent());
    }
}
