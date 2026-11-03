//! Bounded, non-blocking metadata diagnostics. Never enqueue clipboard payloads or paths.
use std::io;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use super::shared_protocol::SharedTextBegin;

static WRITER: OnceLock<Option<SyncSender<String>>> = OnceLock::new();
static DROPPED: AtomicU64 = AtomicU64::new(0);

pub(super) fn initialize(path: PathBuf) {
    WRITER.get_or_init(|| {
        let (sender, receiver) = mpsc::sync_channel::<String>(256);
        std::thread::Builder::new()
            .name("pierdesk-clipboard-diagnostics".into())
            .spawn(move || {
                let _ = crate::logging::append(&path, "shared_diag phase=enabled schema=1 queue_capacity=256 getdata_limit=32 query_limit=32");
                for message in receiver {
                    let _ = crate::logging::append(&path, &message);
                }
            })
            .ok()
            .map(|_| sender)
    });
}

#[derive(Clone)]
pub(super) struct SharedTrace {
    context: Option<String>,
    sender: Option<SyncSender<String>>,
    started: Instant,
}

impl SharedTrace {
    pub(super) fn new(begin: &SharedTextBegin, role: &'static str) -> Self {
        use std::fmt::Write;
        let context = WRITER.get().and_then(Option::as_ref).map(|_| {
            let mut event = String::with_capacity(32);
            for byte in begin.event_id.0 {
                let _ = write!(event, "{byte:02x}");
            }
            format!("event={event} revision={} role={role}", begin.revision)
        });
        Self {
            context,
            sender: WRITER.get().and_then(Clone::clone),
            started: Instant::now(),
        }
    }

    pub(super) fn disabled() -> Self {
        Self {
            context: None,
            sender: None,
            started: Instant::now(),
        }
    }

    pub(super) fn mark(&self, phase: &'static str, detail: &str) {
        let (Some(context), Some(sender)) = (&self.context, &self.sender) else {
            return;
        };
        let at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let dropped = DROPPED.load(Ordering::Relaxed);
        enqueue(
            sender,
            format!(
                "shared_diag {context} phase={phase} at_unix_ms={at} age_ms={} dropped_total={dropped} {detail}",
                self.started.elapsed().as_millis()
            ),
        );
    }

    pub(super) fn stage(&self, phase: &'static str) -> TraceStage<'_> {
        self.mark(phase, "state=begin");
        TraceStage {
            trace: self,
            phase,
            started: Instant::now(),
            finished: false,
        }
    }

    pub(super) fn io<T>(
        &self,
        phase: &'static str,
        operation: impl FnOnce() -> io::Result<T>,
    ) -> io::Result<T> {
        let stage = self.stage(phase);
        let result = operation();
        let status = result
            .as_ref()
            .map_or_else(|error| format!("error_{:?}", error.kind()), |_| "ok".into());
        stage.finish(&status);
        result
    }

    #[cfg(test)]
    pub(super) fn for_test(sender: SyncSender<String>) -> Self {
        Self {
            context: Some("event=synthetic revision=1 role=test".into()),
            sender: Some(sender),
            started: Instant::now(),
        }
    }
}

fn enqueue(sender: &SyncSender<String>, record: String) {
    if sender.try_send(record).is_err() {
        DROPPED.fetch_add(1, Ordering::Relaxed);
    }
}

pub(super) struct TraceStage<'a> {
    trace: &'a SharedTrace,
    phase: &'static str,
    started: Instant,
    finished: bool,
}

impl TraceStage<'_> {
    pub(super) fn finish(mut self, outcome: &str) {
        self.trace.mark(
            self.phase,
            &format!(
                "state=end elapsed_ms={} outcome={outcome}",
                self.started.elapsed().as_millis()
            ),
        );
        self.finished = true;
    }
}

impl Drop for TraceStage<'_> {
    fn drop(&mut self) {
        if !self.finished {
            self.trace.mark(
                self.phase,
                &format!(
                    "state=end elapsed_ms={} outcome=aborted",
                    self.started.elapsed().as_millis()
                ),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_diagnostic_queue_never_blocks_the_clipboard_caller() {
        let (sender, receiver) = mpsc::sync_channel(1);
        enqueue(&sender, "first".into());
        enqueue(&sender, "dropped".into());
        assert_eq!(receiver.try_recv().unwrap(), "first");
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn tracing_preserves_io_errors_and_values() {
        let trace = SharedTrace::disabled();
        assert_eq!(trace.io("test", || Ok(42)).unwrap(), 42);
        let error = trace
            .io::<()>("test", || Err(io::ErrorKind::TimedOut.into()))
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }

    #[test]
    fn stage_records_timing_and_error_kind_not_error_contents() {
        let (sender, receiver) = mpsc::sync_channel(8);
        let trace = SharedTrace::for_test(sender);
        let error = trace
            .io::<()>("body_read", || {
                Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "PRIVATE CLIPBOARD CONTENT",
                ))
            })
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        let records = receiver.try_iter().collect::<Vec<_>>();
        assert_eq!(records.len(), 2);
        assert!(records[0].contains("phase=body_read"));
        assert!(records[0].contains("state=begin"));
        assert!(records[1].contains("elapsed_ms="));
        assert!(records[1].contains("outcome=error_TimedOut"));
        assert!(!records.join(" ").contains("PRIVATE"));
    }
}
