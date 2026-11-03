use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FenceStatus {
    Pending,
    Installed,
    Ready,
    Failed,
    NotShared,
}

#[derive(Clone, Copy, Debug)]
struct FenceState {
    enabled: bool,
    sequence: u32,
    generation: u64,
    status: FenceStatus,
}

impl Default for FenceState {
    fn default() -> Self {
        Self {
            enabled: false,
            sequence: 0,
            generation: 0,
            status: FenceStatus::NotShared,
        }
    }
}

struct PasteFence {
    state: Mutex<FenceState>,
    changed: Condvar,
}

impl PasteFence {
    fn global() -> &'static Self {
        static GLOBAL: OnceLock<PasteFence> = OnceLock::new();
        GLOBAL.get_or_init(|| PasteFence {
            state: Mutex::new(FenceState::default()),
            changed: Condvar::new(),
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PasteFenceDecision {
    Send,
    Block,
}

pub(crate) fn configure(enabled: bool, current_sequence: u32) {
    let fence = PasteFence::global();
    let mut state = fence
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *state = FenceState {
        enabled,
        sequence: current_sequence,
        generation: 0,
        status: FenceStatus::NotShared,
    };
    fence.changed.notify_all();
}

pub(crate) fn begin(sequence: u32, generation: u64) {
    let fence = PasteFence::global();
    let mut state = fence
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !state.enabled {
        return;
    }
    state.sequence = sequence;
    state.generation = generation;
    state.status = FenceStatus::Pending;
    fence.changed.notify_all();
}

pub(crate) fn mark_ready(generation: u64) {
    finish(generation, FenceStatus::Ready);
}

pub(crate) fn mark_installed(generation: u64) {
    finish(generation, FenceStatus::Installed);
}

pub(crate) fn mark_failed(generation: u64) {
    finish(generation, FenceStatus::Failed);
}

pub(crate) fn mark_not_shared(generation: u64) {
    finish(generation, FenceStatus::NotShared);
}

pub(crate) fn observe_remote(sequence: u32) {
    let fence = PasteFence::global();
    let mut state = fence
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !state.enabled {
        return;
    }
    state.sequence = sequence;
    state.generation = state.generation.wrapping_add(1).max(1);
    state.status = FenceStatus::Ready;
    fence.changed.notify_all();
}

fn finish(generation: u64, status: FenceStatus) {
    let fence = PasteFence::global();
    let mut state = fence
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if state.enabled && state.generation == generation {
        state.status = status;
        fence.changed.notify_all();
    }
}

pub(crate) fn wait_for_sequence(current_sequence: u32, timeout: Duration) -> PasteFenceDecision {
    let fence = PasteFence::global();
    let deadline = Instant::now()
        .checked_add(timeout)
        .unwrap_or_else(Instant::now);
    let mut state = fence
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    loop {
        if !state.enabled {
            return PasteFenceDecision::Send;
        }
        // A different sequence is newer local clipboard state, not the remote snapshot guarded by
        // this fence. It must never keep ordinary pointer handoff or a local paste blocked.
        if state.sequence != current_sequence {
            return PasteFenceDecision::Send;
        }
        if state.sequence == current_sequence {
            match state.status {
                FenceStatus::Ready | FenceStatus::NotShared => {
                    return PasteFenceDecision::Send;
                }
                FenceStatus::Failed => return PasteFenceDecision::Block,
                FenceStatus::Pending | FenceStatus::Installed => {}
            }
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return PasteFenceDecision::Block;
        }
        let waited = fence
            .changed
            .wait_timeout(state, remaining)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state = waited.0;
        if waited.1.timed_out() {
            return PasteFenceDecision::Block;
        }
    }
}

pub(crate) fn wait_for_install(current_sequence: u32, timeout: Duration) -> PasteFenceDecision {
    let _ = wait_until(current_sequence, timeout, |status| match status {
        FenceStatus::Installed
        | FenceStatus::Ready
        | FenceStatus::Failed
        | FenceStatus::NotShared => {
            // A failed shared snapshot must continue blocking the corresponding Ctrl+V, but it
            // must never disable the independent mouse-control feature indefinitely. Let the
            // pointer cross after the failure is known; wait_for_sequence still blocks paste.
            Some(PasteFenceDecision::Send)
        }
        FenceStatus::Pending => None,
    });
    // Pending clipboard installation is allowed to delay one boundary transition, but it must
    // never disable the independent mouse-control path indefinitely. `wait_for_sequence` remains
    // fail-closed, so Ctrl+V still cannot deliver the peer's stale clipboard after this bounded
    // pointer-only wait expires.
    PasteFenceDecision::Send
}

pub(crate) fn diagnostic_state(current_sequence: u32) -> String {
    let state = PasteFence::global()
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    format!(
        "current_sequence={current_sequence},fence_enabled={},fence_sequence={},fence_generation={},fence_status={:?}",
        state.enabled, state.sequence, state.generation, state.status,
    )
}

fn wait_until(
    current_sequence: u32,
    timeout: Duration,
    decide: impl Fn(FenceStatus) -> Option<PasteFenceDecision>,
) -> PasteFenceDecision {
    let fence = PasteFence::global();
    let deadline = Instant::now()
        .checked_add(timeout)
        .unwrap_or_else(Instant::now);
    let mut state = fence
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    loop {
        if !state.enabled {
            return PasteFenceDecision::Send;
        }
        if state.sequence != current_sequence {
            return PasteFenceDecision::Send;
        }
        if state.sequence == current_sequence
            && let Some(decision) = decide(state.status)
        {
            return decision;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return PasteFenceDecision::Block;
        }
        let waited = fence
            .changed
            .wait_timeout(state, remaining)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state = waited.0;
        if waited.1.timed_out() {
            return PasteFenceDecision::Block;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn disabled_and_non_shared_clipboards_never_delay_paste() {
        let _guard = TEST_LOCK.lock().unwrap();
        configure(false, 10);
        assert_eq!(
            wait_for_sequence(11, Duration::ZERO),
            PasteFenceDecision::Send
        );
        configure(true, 10);
        begin(11, 2);
        mark_not_shared(2);
        assert_eq!(
            wait_for_sequence(11, Duration::ZERO),
            PasteFenceDecision::Send
        );
    }

    #[test]
    fn pending_paste_waits_for_matching_ack_and_failure_blocks_stale_content() {
        let _guard = TEST_LOCK.lock().unwrap();
        configure(true, 20);
        begin(21, 3);
        let ready = std::thread::spawn(|| {
            std::thread::sleep(Duration::from_millis(20));
            mark_ready(3);
        });
        assert_eq!(
            wait_for_sequence(21, Duration::from_secs(1)),
            PasteFenceDecision::Send
        );
        ready.join().unwrap();

        begin(22, 4);
        mark_failed(3);
        assert_eq!(
            wait_for_sequence(22, Duration::from_millis(1)),
            PasteFenceDecision::Block
        );
        mark_failed(4);
        assert_eq!(
            wait_for_sequence(22, Duration::ZERO),
            PasteFenceDecision::Block
        );
        configure(false, 22);
    }

    #[test]
    fn installed_pending_object_allows_handoff_but_not_keyboard_paste() {
        let _guard = TEST_LOCK.lock().unwrap();
        configure(true, 30);
        begin(31, 5);
        mark_installed(5);
        assert_eq!(
            wait_for_install(31, Duration::ZERO),
            PasteFenceDecision::Send
        );
        assert_eq!(
            wait_for_sequence(31, Duration::ZERO),
            PasteFenceDecision::Block
        );
        mark_ready(5);
        assert_eq!(
            wait_for_sequence(31, Duration::ZERO),
            PasteFenceDecision::Send
        );

        begin(32, 6);
        mark_failed(6);
        assert_eq!(
            wait_for_install(32, Duration::ZERO),
            PasteFenceDecision::Send
        );
        assert_eq!(
            wait_for_sequence(32, Duration::ZERO),
            PasteFenceDecision::Block
        );

        begin(33, 7);
        assert_eq!(
            wait_for_install(33, Duration::ZERO),
            PasteFenceDecision::Send,
            "a stuck pending install may delay but never permanently disable pointer handoff"
        );
        assert_eq!(
            wait_for_sequence(33, Duration::ZERO),
            PasteFenceDecision::Block,
            "keyboard paste must remain fail-closed until the same snapshot is ready"
        );
        configure(false, 31);
    }

    #[test]
    fn newer_local_clipboard_sequence_cannot_leave_cross_screen_blocked() {
        let _guard = TEST_LOCK.lock().unwrap();
        configure(true, 40);
        begin(41, 7);
        mark_ready(7);
        assert_eq!(
            wait_for_install(42, Duration::ZERO),
            PasteFenceDecision::Send
        );
        assert_eq!(
            wait_for_sequence(42, Duration::ZERO),
            PasteFenceDecision::Send
        );
        configure(false, 42);
    }
}
