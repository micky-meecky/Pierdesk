//! Presentation sampling runs after drag tasks are merged, independent of which window is open.
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use super::product::{
    ProductSnapshot, ProductTransferId, ProductTransferSnapshot, ProductTransferState,
};

pub(super) struct RateSample {
    sampled_at: Instant,
    bytes: u64,
    initial_bytes: u64,
    smoothed_speed: u64,
    active_elapsed: Duration,
    state: ProductTransferState,
    average_speed: u64,
}

impl RateSample {
    pub(super) fn new(now: Instant, bytes: u64, state: ProductTransferState) -> Self {
        Self {
            sampled_at: now,
            bytes,
            initial_bytes: bytes,
            smoothed_speed: 0,
            active_elapsed: Duration::ZERO,
            state,
            average_speed: 0,
        }
    }

    fn measure(&mut self, state: ProductTransferState, bytes: u64, now: Instant) -> (u64, u64) {
        let elapsed = now.saturating_duration_since(self.sampled_at);
        // Several windows may request the same snapshot during one timer tick. They must
        // reuse the last rate, not replace it with a zero-byte millisecond sample.
        if state == self.state && elapsed < Duration::from_millis(500) {
            return (self.smoothed_speed, self.average_speed);
        }
        if self.state == ProductTransferState::Running {
            self.active_elapsed += elapsed;
            let delta = bytes.saturating_sub(self.bytes);
            let raw = u64::try_from(u128::from(delta) * 1000 / elapsed.as_millis().max(1))
                .unwrap_or(u64::MAX);
            self.smoothed_speed = if delta == 0 {
                0
            } else if self.smoothed_speed == 0 {
                raw
            } else {
                self.smoothed_speed.saturating_mul(3).saturating_add(raw) / 4
            };
            self.average_speed = u64::try_from(
                u128::from(bytes.saturating_sub(self.initial_bytes)) * 1000
                    / self.active_elapsed.as_millis().max(1),
            )
            .unwrap_or(u64::MAX);
        }
        if state != ProductTransferState::Running || self.state != ProductTransferState::Running {
            self.smoothed_speed = 0;
        }
        self.state = state;
        self.sampled_at = now;
        self.bytes = bytes;
        (self.smoothed_speed, self.average_speed)
    }

    pub(super) fn update(&mut self, transfer: &mut ProductTransferSnapshot, now: Instant) {
        (transfer.bytes_per_second, transfer.average_bytes_per_second) =
            self.measure(transfer.state, transfer.transferred, now);
    }
}

#[derive(Default)]
pub(super) struct SnapshotRates {
    tasks: HashMap<ProductTransferId, RateSample>,
    files: HashMap<(ProductTransferId, [u8; 16]), RateSample>,
}

impl SnapshotRates {
    pub(super) fn apply(&mut self, snapshot: &mut ProductSnapshot, now: Instant) {
        let mut task_ids = HashSet::new();
        let mut file_ids = HashSet::new();
        for task in &mut snapshot.transfer_tasks {
            task_ids.insert(task.id);
            self.tasks
                .entry(task.id)
                .or_insert_with(|| {
                    RateSample::new(now, task.transfer.transferred, task.transfer.state)
                })
                .update(&mut task.transfer, now);
            for file in &mut task.files {
                let key = (task.id, file.id);
                file_ids.insert(key);
                let state = if task.transfer.state != ProductTransferState::Running
                    && file.state == ProductTransferState::Running
                {
                    task.transfer.state
                } else {
                    file.state
                };
                file.bytes_per_second = if file.is_directory {
                    0
                } else {
                    self.files
                        .entry(key)
                        .or_insert_with(|| RateSample::new(now, file.transferred, state))
                        .measure(state, file.transferred, now)
                        .0
                };
            }
        }
        self.tasks.retain(|id, _| task_ids.contains(id));
        self.files.retain(|id, _| file_ids.contains(id));
        let selected = snapshot
            .transfer_tasks
            .iter()
            .rev()
            .find(|task| {
                matches!(
                    task.transfer.state,
                    ProductTransferState::Running | ProductTransferState::Paused
                )
            })
            .or_else(|| snapshot.transfer_tasks.last());
        if let Some(task) = selected {
            snapshot.transfer_generation = task.generation;
            snapshot.last_manifest = Some(task.manifest.clone());
            snapshot.transfer = Some(task.transfer.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merged_tasks_and_files_keep_separate_rates_in_both_directions() {
        use super::super::product::{
            ProductFileSnapshot, ProductManifestSummary, ProductTransferDirection,
            ProductTransferTaskSnapshot,
        };
        let tasks = [
            ProductTransferDirection::Sending,
            ProductTransferDirection::Receiving,
        ]
        .into_iter()
        .map(|direction| ProductTransferTaskSnapshot {
            id: ProductTransferId::new(direction, [1; 16]),
            generation: 1,
            manifest: ProductManifestSummary {
                direction: direction.label(),
                primary_name: "video".into(),
                items: 1,
                files: 1,
                directories: 0,
                total_size: 10000,
            },
            transfer: ProductTransferSnapshot {
                state: ProductTransferState::Running,
                transferred: 0,
                total_size: 10000,
                bytes_per_second: 0,
                average_bytes_per_second: 0,
                started_files: 1,
                total_files: 1,
                current_file_name: None,
                current_file_transferred: 0,
                current_file_size: 10000,
                reconnect_attempts: 0,
                recovered_commands: 0,
                recovery_active: false,
            },
            files: vec![ProductFileSnapshot {
                id: [2; 16],
                name: "video".into(),
                size: 10000,
                transferred: 0,
                bytes_per_second: 0,
                state: ProductTransferState::Running,
                is_directory: false,
            }],
            source_location: String::new(),
            target_location: String::new(),
            started_unix_ms: 1,
            completed_unix_ms: 0,
        })
        .collect();
        let mut snapshot = ProductSnapshot {
            listener_running: true,
            local_endpoint: "127.0.0.1:45233".parse().unwrap(),
            active_peer_label: "peer".into(),
            active_peer: crate::security::CertificateFingerprint::from_bytes([7; 32]),
            auto_receive: true,
            pending_offer: false,
            last_manifest: None,
            transfer: None,
            transfer_generation: 1,
            transfer_tasks: tasks,
            shared_text_enabled: false,
            shared_text_generation: 0,
            last_shared_text: None,
            remote_boundary_workspace: None,
            last_error: None,
        };
        let now = Instant::now();
        let mut rates = SnapshotRates::default();
        rates.apply(&mut snapshot, now);
        for (task, bytes) in snapshot.transfer_tasks.iter_mut().zip([1000, 2000]) {
            task.transfer.transferred = bytes;
            task.files[0].transferred = bytes;
        }
        rates.apply(&mut snapshot, now + Duration::from_secs(1));
        for (task, expected) in snapshot.transfer_tasks.iter().zip([1000, 2000]) {
            assert_eq!(task.transfer.bytes_per_second, expected);
            assert_eq!(task.files[0].bytes_per_second, expected);
        }
        assert_eq!(snapshot.transfer.as_ref().unwrap().bytes_per_second, 2000);
        snapshot.transfer_tasks[0].transfer.state = ProductTransferState::Paused;
        rates.apply(&mut snapshot, now + Duration::from_millis(1001));
        assert_eq!(snapshot.transfer_tasks[0].files[0].bytes_per_second, 0);
        assert_eq!(snapshot.transfer_tasks[1].files[0].bytes_per_second, 2000);
        snapshot.transfer_tasks.clear();
        rates.apply(&mut snapshot, now + Duration::from_secs(2));
        assert!(rates.tasks.is_empty() && rates.files.is_empty());
    }

    #[test]
    fn repeated_ui_reads_keep_rate_and_stalls_drop_to_zero() {
        let now = Instant::now();
        let mut sample = RateSample::new(now, 1000, ProductTransferState::Running);
        assert_eq!(
            sample.measure(
                ProductTransferState::Running,
                2000,
                now + Duration::from_secs(1)
            ),
            (1000, 1000)
        );
        assert_eq!(
            sample
                .measure(
                    ProductTransferState::Running,
                    2000,
                    now + Duration::from_millis(1001)
                )
                .0,
            1000
        );
        assert_eq!(
            sample
                .measure(
                    ProductTransferState::Running,
                    2000,
                    now + Duration::from_secs(2)
                )
                .0,
            0
        );
        assert_eq!(
            sample
                .measure(
                    ProductTransferState::Paused,
                    2000,
                    now + Duration::from_secs(3)
                )
                .0,
            0
        );
        assert_eq!(
            sample
                .measure(
                    ProductTransferState::Running,
                    2000,
                    now + Duration::from_mins(1)
                )
                .0,
            0
        );
        assert_eq!(
            sample
                .measure(
                    ProductTransferState::Running,
                    3000,
                    now + Duration::from_secs(61)
                )
                .0,
            1000
        );
        assert_eq!(
            sample
                .measure(
                    ProductTransferState::Completed,
                    3000,
                    now + Duration::from_secs(62)
                )
                .0,
            0
        );
    }
}
