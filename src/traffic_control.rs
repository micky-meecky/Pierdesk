use std::collections::BTreeSet;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

#[cfg(not(test))]
use windows::Win32::System::Threading::{
    GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL,
};

const DEFAULT_INTERACTIVE_RATE: u64 = 2 * 1024 * 1024;
const MIN_INTERACTIVE_RATE: u64 = 128 * 1024;
const MAX_INTERACTIVE_RATE: u64 = 8 * 1024 * 1024;
const ELEVATED_RTT_DELTA_US: u64 = 4_000;
const HIGH_RTT_DELTA_US: u64 = 12_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LatencyPressure {
    None,
    Elevated,
    High,
}

struct AdaptiveFilePacer {
    interactive: bool,
    active_sessions: BTreeSet<String>,
    baseline_rtt_us: Option<u64>,
    pressure: LatencyPressure,
    bytes_per_second: u64,
    next_payload_at: Instant,
    payload_since_probe: bool,
    low_latency_probes: u8,
    last_rtt_us: u64,
    last_log_at: Instant,
}

impl AdaptiveFilePacer {
    fn new(now: Instant) -> Self {
        Self {
            interactive: false,
            active_sessions: BTreeSet::new(),
            baseline_rtt_us: None,
            pressure: LatencyPressure::None,
            bytes_per_second: DEFAULT_INTERACTIVE_RATE,
            next_payload_at: now,
            payload_since_probe: false,
            low_latency_probes: 0,
            last_rtt_us: 0,
            last_log_at: now,
        }
    }

    fn set_interactive(&mut self, interactive: bool, now: Instant) {
        if self.interactive == interactive {
            return;
        }
        self.interactive = interactive;
        self.pressure = LatencyPressure::None;
        self.next_payload_at = now;
        self.bytes_per_second = self.bytes_per_second.min(DEFAULT_INTERACTIVE_RATE);
        self.low_latency_probes = 0;
    }

    fn set_session(&mut self, session: &str, active: bool, now: Instant) {
        if active && !self.active_sessions.contains(session) {
            self.active_sessions.insert(session.to_owned());
        } else if !active {
            self.active_sessions.remove(session);
        }
        self.set_interactive(!self.active_sessions.is_empty(), now);
    }

    fn observe_rtt(&mut self, rtt_us: u64) {
        self.last_rtt_us = rtt_us;
        let had_payload = std::mem::take(&mut self.payload_since_probe);
        let baseline = match self.baseline_rtt_us {
            None => {
                self.baseline_rtt_us = Some(rtt_us);
                rtt_us
            }
            Some(previous) if rtt_us < previous => {
                self.baseline_rtt_us = Some(rtt_us);
                rtt_us
            }
            Some(previous) if !self.interactive && !had_payload => {
                let adjusted = previous.saturating_mul(31).saturating_add(rtt_us) / 32;
                self.baseline_rtt_us = Some(adjusted);
                adjusted
            }
            Some(previous) => previous,
        };
        if !self.interactive {
            self.pressure = LatencyPressure::None;
            return;
        }
        let delta = rtt_us.saturating_sub(baseline);
        if delta >= HIGH_RTT_DELTA_US {
            self.pressure = LatencyPressure::High;
            self.low_latency_probes = 0;
            self.bytes_per_second = self
                .bytes_per_second
                .saturating_div(2)
                .max(MIN_INTERACTIVE_RATE);
        } else if delta >= ELEVATED_RTT_DELTA_US {
            self.pressure = LatencyPressure::Elevated;
            self.low_latency_probes = 0;
            self.bytes_per_second = self
                .bytes_per_second
                .saturating_mul(4)
                .saturating_div(5)
                .max(MIN_INTERACTIVE_RATE);
        } else {
            self.pressure = LatencyPressure::None;
            self.low_latency_probes = if had_payload {
                self.low_latency_probes.saturating_add(1)
            } else {
                0
            };
            if self.low_latency_probes >= 8 {
                self.bytes_per_second = self
                    .bytes_per_second
                    .saturating_add(128 * 1024)
                    .min(MAX_INTERACTIVE_RATE);
                self.low_latency_probes = 0;
            }
        }
    }

    const fn range_limit(&self, requested: usize) -> usize {
        if !self.interactive {
            return requested;
        }
        let limit = match self.pressure {
            LatencyPressure::None => 16 * 1024,
            LatencyPressure::Elevated => 8 * 1024,
            LatencyPressure::High => 4 * 1024,
        };
        if requested < limit { requested } else { limit }
    }

    fn reserve_payload(&mut self, bytes: usize, now: Instant) -> Option<Duration> {
        self.payload_since_probe |= bytes != 0;
        if !self.interactive || bytes == 0 {
            return None;
        }
        if now.saturating_duration_since(self.next_payload_at) > Duration::from_secs(1) {
            self.next_payload_at = now;
        }
        let scheduled = self.next_payload_at.max(now);
        let micros = u64::try_from(
            (bytes as u128)
                .saturating_mul(1_000_000)
                .div_ceil(u128::from(self.bytes_per_second.max(1))),
        )
        .unwrap_or(u64::MAX);
        self.next_payload_at = scheduled
            .checked_add(Duration::from_micros(micros))
            .unwrap_or(scheduled);
        (scheduled > now).then(|| scheduled.duration_since(now))
    }
}

pub(crate) fn diagnostic() -> Option<String> {
    let mut pacer = controller().lock().ok()?;
    if !pacer.interactive || pacer.last_log_at.elapsed() < Duration::from_secs(5) {
        return None;
    }
    pacer.last_log_at = Instant::now();
    Some(format!(
        "interactive_traffic rtt_us={} baseline_us={} limit_bytes_s={} range_bytes={} pressure={:?} inbound_wakeup=event",
        pacer.last_rtt_us,
        pacer.baseline_rtt_us.unwrap_or(0),
        pacer.bytes_per_second,
        pacer.range_limit(256 * 1024),
        pacer.pressure
    ))
}

fn controller() -> &'static Mutex<AdaptiveFilePacer> {
    static CONTROLLER: OnceLock<Mutex<AdaptiveFilePacer>> = OnceLock::new();
    CONTROLLER.get_or_init(|| Mutex::new(AdaptiveFilePacer::new(Instant::now())))
}

/// One transport/control lifetime cannot clear another session's traffic budget.
#[cfg_attr(test, allow(dead_code))]
pub(crate) struct InteractiveSession(String);

#[cfg_attr(test, allow(dead_code))]
impl InteractiveSession {
    pub(crate) fn new(session: String) -> Self {
        Self(session)
    }

    pub(crate) fn set_active(&self, active: bool) {
        if let Ok(mut controller) = controller().lock() {
            controller.set_session(&self.0, active, Instant::now());
        }
    }
}

impl Drop for InteractiveSession {
    fn drop(&mut self) {
        self.set_active(false);
    }
}

#[cfg_attr(test, allow(dead_code))]
pub(crate) fn observe_cross_screen_rtt(rtt_us: u64) {
    if let Ok(mut controller) = controller().lock() {
        controller.observe_rtt(rtt_us);
    }
}

pub(crate) fn file_range_limit(requested: usize) -> usize {
    controller()
        .lock()
        .map_or(requested, |controller| controller.range_limit(requested))
}

pub(crate) fn pace_file_payload(bytes: usize) {
    let delay = controller()
        .lock()
        .ok()
        .and_then(|mut controller| controller.reserve_payload(bytes, Instant::now()));
    if let Some(delay) = delay {
        std::thread::sleep(delay);
    }
}

#[cfg(not(test))]
pub(crate) fn promote_interactive_thread() {
    // SAFETY: GetCurrentThread returns a valid pseudo-handle for this caller. Raising only the
    // latency-sensitive cross-screen workers to ABOVE_NORMAL avoids starving the desktop while
    // protecting pointer delivery from bulk-transfer CPU contention.
    let _ = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ending_one_session_preserves_other_sessions_and_repeated_updates_are_idempotent() {
        let now = Instant::now();
        let mut pacer = AdaptiveFilePacer::new(now);
        pacer.set_session("a", true, now);
        pacer.set_session("b", true, now);
        pacer.set_session("a", true, now);
        pacer.set_session("a", false, now);
        assert!(pacer.interactive);
        assert_eq!(pacer.active_sessions.len(), 1);
        assert_eq!(pacer.range_limit(256 * 1024), 16 * 1024);
        pacer.set_session("b", false, now);
        assert!(!pacer.interactive);
    }

    #[test]
    fn idle_heartbeats_cannot_raise_the_bulk_budget() {
        let now = Instant::now();
        let mut pacer = AdaptiveFilePacer::new(now);
        pacer.set_interactive(true, now);
        for _ in 0..1000 {
            pacer.observe_rtt(1000);
        }
        assert_eq!(pacer.bytes_per_second, DEFAULT_INTERACTIVE_RATE);
        pacer.observe_rtt(40_000);
        let reduced = pacer.bytes_per_second;
        for _ in 0..1000 {
            pacer.observe_rtt(1000);
        }
        assert_eq!(pacer.bytes_per_second, reduced);
        pacer.set_interactive(false, now);
        assert_eq!(pacer.reserve_payload(256 * 1024, now), None);
        assert_eq!(pacer.range_limit(256 * 1024), 256 * 1024);
    }

    #[test]
    fn interaction_caps_ranges_and_high_latency_tightens_the_cap() {
        let now = Instant::now();
        let mut pacer = AdaptiveFilePacer::new(now);
        pacer.observe_rtt(10_000);
        assert_eq!(pacer.range_limit(256 * 1024), 256 * 1024);

        pacer.set_interactive(true, now);
        assert_eq!(pacer.range_limit(256 * 1024), 16 * 1024);
        pacer.observe_rtt(50_000);
        assert_eq!(pacer.pressure, LatencyPressure::High);
        assert_eq!(pacer.range_limit(256 * 1024), 4 * 1024);
        assert!(pacer.bytes_per_second < DEFAULT_INTERACTIVE_RATE);
    }

    #[test]
    fn low_latency_recovers_rate_without_exceeding_the_ceiling() {
        let now = Instant::now();
        let mut pacer = AdaptiveFilePacer::new(now);
        pacer.observe_rtt(10_000);
        pacer.set_interactive(true, now);
        pacer.observe_rtt(60_000);
        let reduced = pacer.bytes_per_second;
        for _ in 0..8 {
            pacer.reserve_payload(4096, now);
            pacer.observe_rtt(10_000);
        }
        assert!(pacer.bytes_per_second > reduced);
        assert!(pacer.bytes_per_second <= MAX_INTERACTIVE_RATE);
    }

    #[test]
    fn aggregate_reservations_are_serialized_across_callers() {
        let now = Instant::now();
        let mut pacer = AdaptiveFilePacer::new(now);
        pacer.set_interactive(true, now);
        assert_eq!(pacer.reserve_payload(64 * 1024, now), None);
        let delay = pacer
            .reserve_payload(64 * 1024, now)
            .expect("the second payload must wait for the first reservation");
        assert!(delay >= Duration::from_micros(900));
    }
}
