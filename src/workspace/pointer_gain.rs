//! Learns the Windows processed-pointer gain and applies it to raw cross-screen motion.
//!
//! Raw input reports device units while the visible Windows cursor has already passed through
//! the user's speed and acceleration settings. Cross-screen transport must therefore learn the
//! relationship locally and forward processed pixels, otherwise enabling the workspace runtime
//! changes pointer speed.

use std::collections::VecDeque;

use crate::cross_screen::PhysicalScreenSize;

const SCALE: i64 = 1_024;
const MIN_GAIN: u64 = 64;
const MAX_GAIN: u64 = 8_192;
const BIN_COUNT: usize = 4;
const MIN_RAW_DISTANCE: u64 = 8;
const HISTORY_LIMIT: usize = 128;

#[derive(Clone, Copy, Debug, Default)]
struct AxisSample {
    raw_distance: u64,
    processed_distance: u64,
}

impl AxisSample {
    fn observe(&mut self, raw: i32, processed: i32, clamped: bool) {
        if clamped || raw == 0 || (processed != 0 && raw.signum() != processed.signum()) {
            return;
        }
        self.raw_distance = self
            .raw_distance
            .saturating_add(u64::from(raw.unsigned_abs()));
        self.processed_distance = self
            .processed_distance
            .saturating_add(u64::from(processed.unsigned_abs()));
        if self.raw_distance > 16_384 {
            self.raw_distance = self.raw_distance.div_ceil(2);
            self.processed_distance = self.processed_distance.div_ceil(2);
        }
    }

    fn gain(self) -> Option<u64> {
        (self.raw_distance >= MIN_RAW_DISTANCE).then(|| {
            self.processed_distance
                .saturating_mul(SCALE.cast_unsigned())
                .div_ceil(self.raw_distance)
                .clamp(MIN_GAIN, MAX_GAIN)
        })
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct PointerGainModel {
    gains: [u64; BIN_COUNT],
}

impl Default for PointerGainModel {
    fn default() -> Self {
        Self {
            gains: [SCALE.cast_unsigned(); BIN_COUNT],
        }
    }
}

impl PointerGainModel {
    pub(super) fn scale(
        self,
        delta_x: i32,
        delta_y: i32,
        residual_x: &mut i64,
        residual_y: &mut i64,
    ) -> (i32, i32) {
        // One vector gets one scalar: independent axis gains or bins rotate strokes.
        let gain = self.gains[gain_bin(dominant_axis(delta_x, delta_y))];
        (
            scale_axis(delta_x, gain, residual_x),
            scale_axis(delta_y, gain, residual_y),
        )
    }
}

#[derive(Clone, Copy, Debug)]
struct ProcessedSample {
    message_time_ms: u32,
    delta_x: i32,
    delta_y: i32,
    x_clamped: bool,
    y_clamped: bool,
    previous_clamped: bool,
}

#[derive(Debug, Default)]
pub(super) struct PointerGainCalibration {
    baseline: Option<(i32, i32)>,
    pending: VecDeque<ProcessedSample>,
    samples: [AxisSample; BIN_COUNT],
    accepted: u64,
    rejected: u64,
    unmatched: u64,
}

impl PointerGainCalibration {
    pub(super) fn record_processed(
        &mut self,
        x: i32,
        y: i32,
        message_time_ms: u32,
        screen: PhysicalScreenSize,
    ) {
        let Some((previous_x, previous_y)) = self.baseline.replace((x, y)) else {
            return;
        };
        if self.pending.len() == HISTORY_LIMIT {
            self.pending.pop_front();
        }
        self.pending.push_back(ProcessedSample {
            message_time_ms,
            delta_x: x.saturating_sub(previous_x),
            delta_y: y.saturating_sub(previous_y),
            x_clamped: x == 0 || x == i32::try_from(screen.width - 1).unwrap_or(i32::MAX),
            y_clamped: y == 0 || y == i32::try_from(screen.height - 1).unwrap_or(i32::MAX),
            previous_clamped: previous_x == 0
                || previous_y == 0
                || previous_x == i32::try_from(screen.width - 1).unwrap_or(i32::MAX)
                || previous_y == i32::try_from(screen.height - 1).unwrap_or(i32::MAX),
        });
    }

    pub(super) fn record_raw(
        &mut self,
        delta_x: i32,
        delta_y: i32,
        message_time_ms: u32,
    ) -> Option<(i32, i32)> {
        let Some(index) = self
            .pending
            .iter()
            .position(|sample| message_time_ms.wrapping_sub(sample.message_time_ms) <= 2)
        else {
            self.unmatched = self.unmatched.saturating_add(1);
            return None;
        };
        for _ in 0..index {
            self.pending.pop_front();
        }
        let processed = self.pending.pop_front()?;
        let model = self.model();
        let raw = dominant_axis(delta_x, delta_y);
        let bin = gain_bin(raw);
        let overflow_x = edge_overflow(
            delta_x,
            processed.delta_x,
            processed.x_clamped,
            model.gains[bin],
        );
        let overflow_y = edge_overflow(
            delta_y,
            processed.delta_y,
            processed.y_clamped,
            model.gains[bin],
        );
        // Tiny components can round to zero and timestamp pairing can be imperfect.
        // Learn from the dominant component only, rejecting clipped/inconsistent vectors.
        let processed_major = if delta_x.unsigned_abs() >= delta_y.unsigned_abs() {
            processed.delta_x
        } else {
            processed.delta_y
        };
        let cross = i128::from(delta_x) * i128::from(processed.delta_y)
            - i128::from(delta_y) * i128::from(processed.delta_x);
        let tolerance =
            2 * (i128::from(delta_x.unsigned_abs()) + i128::from(delta_y.unsigned_abs()));
        if raw != 0
            && processed_major != 0
            && raw.signum() == processed_major.signum()
            && !processed.x_clamped
            && !processed.y_clamped
            && !processed.previous_clamped
            && cross.abs() <= tolerance
            && u64::from(processed_major.unsigned_abs()) <= u64::from(raw.unsigned_abs()) * 8
        {
            self.samples[bin].observe(raw, processed_major, false);
            self.accepted = self.accepted.saturating_add(1);
        } else {
            self.rejected = self.rejected.saturating_add(1);
        }
        Some((overflow_x, overflow_y))
    }

    pub(super) fn model(&self) -> PointerGainModel {
        PointerGainModel {
            gains: resolved_gains(&self.samples),
        }
    }

    pub(super) fn diagnostic(&self) -> String {
        format!(
            "gain_q1024={:?} accepted={} rejected={} unmatched={}",
            self.model().gains,
            self.accepted,
            self.rejected,
            self.unmatched
        )
    }

    pub(super) fn rebase(&mut self) {
        self.baseline = None;
        self.pending.clear();
    }
}

fn dominant_axis(x: i32, y: i32) -> i32 {
    if x.unsigned_abs() >= y.unsigned_abs() {
        x
    } else {
        y
    }
}

fn edge_overflow(raw: i32, processed: i32, clamped: bool, gain: u64) -> i32 {
    if !clamped || raw == 0 {
        return 0;
    }
    let mut residual = 0;
    let overflow = scale_axis(raw, gain, &mut residual).saturating_sub(processed);
    if overflow != 0 && overflow.signum() == raw.signum() {
        overflow
    } else {
        0
    }
}

const fn gain_bin(delta: i32) -> usize {
    match delta.unsigned_abs() {
        0..=2 => 0,
        3..=6 => 1,
        7..=16 => 2,
        _ => 3,
    }
}

fn resolved_gains(samples: &[AxisSample; BIN_COUNT]) -> [u64; BIN_COUNT] {
    let mut gains = [SCALE.cast_unsigned(); BIN_COUNT];
    for (index, output) in gains.iter_mut().enumerate() {
        *output = samples[index]
            .gain()
            .or_else(|| {
                (1..BIN_COUNT).find_map(|distance| {
                    index
                        .checked_sub(distance)
                        .and_then(|candidate| samples[candidate].gain())
                        .or_else(|| {
                            samples
                                .get(index + distance)
                                .and_then(|sample| sample.gain())
                        })
                })
            })
            .unwrap_or(SCALE.cast_unsigned());
    }
    gains
}

fn scale_axis(delta: i32, gain: u64, residual: &mut i64) -> i32 {
    let scaled = i64::from(delta)
        .saturating_mul(i64::try_from(gain).expect("bounded pointer gain fits i64"))
        .saturating_add(*residual);
    let pixels = scaled / SCALE;
    *residual = scaled % SCALE;
    i32::try_from(pixels.clamp(i64::from(i32::MIN), i64::from(i32::MAX)))
        .expect("clamped pointer delta fits i32")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn horizontal_training_must_not_rotate_a_diagonal() {
        let screen = PhysicalScreenSize::new(1920, 1080).unwrap();
        let mut calibration = PointerGainCalibration::default();
        calibration.record_processed(100, 100, 1, screen);
        for step in 1_u32..=8 {
            calibration.record_processed(100 + step.cast_signed() * 20, 100, step + 1, screen);
            calibration.record_raw(10, 0, step + 1);
        }
        assert_eq!(calibration.model().scale(10, 10, &mut 0, &mut 0), (20, 20));
    }

    #[test]
    fn zero_minor_axis_samples_must_not_flatten_vertical_motion() {
        let screen = PhysicalScreenSize::new(1920, 1080).unwrap();
        let mut calibration = PointerGainCalibration::default();
        calibration.record_processed(100, 100, 1, screen);
        for step in 1_u32..=8 {
            calibration.record_processed(100 + step.cast_signed() * 20, 100, step + 1, screen);
            calibration.record_raw(10, 1, step + 1);
        }
        // Tiny horizontal noise must not dominate a mostly vertical stroke.
        assert_eq!(calibration.model().scale(1, 16, &mut 0, &mut 0), (2, 32));
    }

    #[test]
    fn learned_windows_gain_scales_remote_motion() {
        let screen = PhysicalScreenSize::new(1920, 1080).unwrap();
        let mut calibration = PointerGainCalibration::default();
        calibration.record_processed(100, 100, 1, screen);
        for step in 0_u32..4 {
            let time = 2 + step;
            calibration.record_processed(120 + step.cast_signed() * 20, 100, time, screen);
            calibration.record_raw(10, 0, time);
        }
        let mut residual_x = 0;
        let mut residual_y = 0;
        assert_eq!(
            calibration
                .model()
                .scale(10, 0, &mut residual_x, &mut residual_y),
            (20, 0)
        );
    }

    #[test]
    fn clamped_samples_do_not_reduce_learned_gain() {
        let screen = PhysicalScreenSize::new(100, 100).unwrap();
        let mut calibration = PointerGainCalibration::default();
        calibration.record_processed(10, 50, 1, screen);
        calibration.record_processed(30, 50, 2, screen);
        calibration.record_raw(10, 0, 2);
        calibration.record_processed(99, 50, 3, screen);
        calibration.record_raw(100, 0, 3);
        let mut residual_x = 0;
        let mut residual_y = 0;
        assert_eq!(
            calibration
                .model()
                .scale(10, 0, &mut residual_x, &mut residual_y),
            (20, 0)
        );
    }

    #[test]
    fn both_axes_use_same_speed_bin_in_every_quadrant() {
        let model = PointerGainModel {
            gains: [512, 1024, 2048, 4096],
        };
        for (x, y) in [(1, 16), (-1, 16), (1, -16), (-1, -16), (16, 1)] {
            assert_eq!(model.scale(x, y, &mut 0, &mut 0), (x * 2, y * 2));
        }
        assert_eq!(model.scale(0, 16, &mut 0, &mut 0), (0, 32));
        assert_eq!(model.scale(16, 0, &mut 0, &mut 0), (32, 0));
    }

    #[test]
    fn fractional_motion_preserves_distance_and_reversal() {
        let model = PointerGainModel {
            gains: [512; BIN_COUNT],
        };
        let (mut rx, mut ry) = (0, 0);
        assert_eq!(model.scale(1, 1, &mut rx, &mut ry), (0, 0));
        assert_eq!(model.scale(1, 1, &mut rx, &mut ry), (1, 1));
        assert_eq!(model.scale(-1, -1, &mut rx, &mut ry), (0, 0));
        assert_eq!(model.scale(-1, -1, &mut rx, &mut ry), (-1, -1));
        assert_eq!((rx, ry), (0, 0));
    }

    #[test]
    fn mismatched_zero_and_excessive_vectors_do_not_train() {
        let screen = PhysicalScreenSize::new(1920, 1080).unwrap();
        for (x, y) in [(100, 100), (80, 100), (100, 120), (200, 100)] {
            let mut calibration = PointerGainCalibration::default();
            calibration.record_processed(100, 100, 1, screen);
            calibration.record_processed(x, y, 2, screen);
            calibration.record_raw(10, 0, 2);
            assert_eq!(calibration.accepted, 0);
            assert_eq!(calibration.rejected, 1);
            assert_eq!(calibration.model().gains, [1024; BIN_COUNT]);
        }
    }

    #[test]
    fn rebase_preserves_shared_gain_but_discards_stale_positions() {
        let screen = PhysicalScreenSize::new(100, 100).unwrap();
        let mut calibration = PointerGainCalibration::default();
        calibration.record_processed(10, 50, 1, screen);
        calibration.record_processed(30, 50, 2, screen);
        calibration.record_raw(10, 0, 2);
        calibration.rebase();
        calibration.record_processed(99, 50, 3, screen);
        assert_eq!(calibration.record_raw(10, 0, 3), None);
        calibration.record_processed(79, 50, 4, screen);
        calibration.record_raw(-10, 0, 4);
        assert_eq!(calibration.accepted, 1);
        assert_eq!(calibration.rejected, 1);
        calibration.record_processed(99, 50, 5, screen);
        assert_eq!(calibration.record_raw(15, 0, 5), Some((10, 0)));
        assert_eq!(calibration.model().scale(10, 10, &mut 0, &mut 0), (20, 20));
    }
}
