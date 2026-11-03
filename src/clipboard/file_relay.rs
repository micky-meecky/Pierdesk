//! Bounded nonblocking forwarding for the nested TLS file channel.
use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

fn transient(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
    )
}

struct Pending {
    bytes: Vec<u8>,
    position: usize,
    eof: bool,
    flushed: bool,
}

impl Pending {
    fn new() -> Self {
        Self {
            bytes: Vec::new(),
            position: 0,
            eof: false,
            flushed: true,
        }
    }

    fn step(
        &mut self,
        source: &mut (impl Read + Write),
        target: &mut (impl Read + Write),
    ) -> io::Result<bool> {
        let mut progress = false;
        if self.position == self.bytes.len() && !self.eof {
            self.bytes.resize(64 * 1024, 0);
            match source.read(&mut self.bytes) {
                Ok(n) => {
                    self.bytes.truncate(n);
                    self.position = 0;
                    self.eof = n == 0;
                    progress = n > 0;
                }
                Err(error) if transient(&error) => {
                    self.bytes.clear();
                    self.position = 0;
                }
                Err(error) => return Err(error),
            }
        }
        if self.position < self.bytes.len() {
            match target.write(&self.bytes[self.position..]) {
                // rustls can accept zero plaintext while its bounded TLS buffer is full.
                Ok(n) => {
                    self.position += n;
                    self.flushed = false;
                    progress |= n > 0;
                }
                Err(error) if transient(&error) => {}
                Err(error) => return Err(error),
            }
        }
        match target.flush() {
            Ok(()) => self.flushed = true,
            Err(error) if transient(&error) => self.flushed = false,
            Err(error) => return Err(error),
        }
        Ok(progress)
    }

    fn drained(&self) -> bool {
        self.position == self.bytes.len() && self.flushed
    }
}

/// Both underlying sockets must be nonblocking. Keep partial writes across polls; never
/// block on the empty direction while the other direction has file data available.
pub(super) fn relay(
    left: &mut (impl Read + Write),
    right: &mut (impl Read + Write),
    stop: &AtomicBool,
) -> io::Result<()> {
    let (mut a, mut b) = (Pending::new(), Pending::new());
    let mut drain_started = None;
    while !stop.load(Ordering::Acquire) {
        let progress = a.step(left, right)? | b.step(right, left)?;
        if a.eof || b.eof {
            if a.drained() && b.drained() {
                break;
            }
            if drain_started.get_or_insert_with(Instant::now).elapsed() > Duration::from_secs(5) {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "file tunnel output did not drain",
                ));
            }
        }
        if !progress {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    #[test]
    fn partial_and_saturated_writes_retain_every_byte() {
        struct Pipe {
            input: VecDeque<u8>,
            output: Vec<u8>,
            blocked: bool,
        }
        impl Read for Pipe {
            fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
                if self.input.is_empty() {
                    return Err(io::ErrorKind::WouldBlock.into());
                }
                let n = b.len().min(self.input.len());
                for v in &mut b[..n] {
                    *v = self.input.pop_front().unwrap();
                }
                Ok(n)
            }
        }
        impl Write for Pipe {
            fn write(&mut self, b: &[u8]) -> io::Result<usize> {
                if self.blocked {
                    return Ok(0);
                }
                let n = b.len().min(7);
                self.output.extend_from_slice(&b[..n]);
                Ok(n)
            }
            fn flush(&mut self) -> io::Result<()> {
                self.blocked = false;
                Ok(())
            }
        }
        let expected = (0..251_u8).collect::<Vec<_>>();
        let mut source = Pipe {
            input: expected.clone().into(),
            output: Vec::new(),
            blocked: false,
        };
        let mut target = Pipe {
            input: VecDeque::new(),
            output: Vec::new(),
            blocked: true,
        };
        let mut pending = Pending::new();
        for _ in 0..50 {
            pending.step(&mut source, &mut target).unwrap();
        }
        assert_eq!(target.output, expected);
        assert!(pending.drained());
    }
}
