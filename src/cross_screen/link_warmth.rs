//! Keep-warm policy for Wi-Fi peer links (CF-CS-007).
//!
//! A link that carried only 500 ms heartbeats for a while loses packets during the first seconds
//! of dense pointer traffic, and TCP retransmission stalls the cursor. While a user is active, the
//! machine they use keeps each peer link at a denser heartbeat. Each end advertises whether it
//! allows that (switch on and running on AC power), so a laptop on battery opts its link out.

use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};

use crate::device_store::DeviceStore;

const SETTINGS_FILE: &str = "link-warmth.settings";
const SETTINGS_HEADER: &str = "clipferry-link-warmth-v1";
const SETTINGS_LIMIT: u64 = 256;
const POWER_CACHE: Duration = Duration::from_secs(5);
const AC_LINE_ONLINE: u8 = 1;
const NO_SYSTEM_BATTERY: u8 = 128;

static ENABLED: AtomicBool = AtomicBool::new(true);
static QOS_MARKING: AtomicBool = AtomicBool::new(true);
/// Presence of this file in the `Pierdesk` data folder disables the Windows `Control` `QoS` flow on
/// new cross-screen sockets (`CF-CS-007` A/B: that class maps to the Wi-Fi voice queue).
pub const QOS_DISABLED_FLAG_FILE: &str = "cross-screen-qos.disabled";

/// Reads the `QoS` A/B flag once at startup; new channels honour it.
pub fn load_qos_marking(store: &DeviceStore) -> bool {
    let marking = !store.root().join(QOS_DISABLED_FLAG_FILE).exists();
    QOS_MARKING.store(marking, Ordering::Release);
    marking
}

#[must_use]
pub fn qos_marking() -> bool {
    QOS_MARKING.load(Ordering::Acquire)
}
static POWER: Mutex<Option<(Instant, bool)>> = Mutex::new(None);

/// Applies the persisted switch to the running process.
pub fn set_enabled(enabled: bool) {
    ENABLED.store(enabled, Ordering::Release);
}

#[must_use]
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Acquire)
}

/// This end allows keep-warm on its links: the switch is on and the machine is on AC power.
#[must_use]
pub fn local_allows() -> bool {
    enabled() && on_ac_power()
}

fn on_ac_power() -> bool {
    let now = Instant::now();
    let mut cached = POWER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some((checked, value)) = *cached
        && now.saturating_duration_since(checked) < POWER_CACHE
    {
        return value;
    }
    let mut status = SYSTEM_POWER_STATUS::default();
    // A failed query is treated as battery power: never drain a laptop on an unknown state.
    let value = unsafe { GetSystemPowerStatus(&raw mut status) }.is_ok()
        && power_status_is_ac(status.ACLineStatus, status.BatteryFlag);
    *cached = Some((now, value));
    value
}

const fn power_status_is_ac(ac_line_status: u8, battery_flag: u8) -> bool {
    ac_line_status == AC_LINE_ONLINE || battery_flag == NO_SYSTEM_BATTERY
}

/// The persisted "keep links warm" switch. Stored in its own file so that an older build, which
/// rejects unknown fields in the cross-screen settings file, still loads after a rollback.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LinkWarmthSettings {
    pub keep_link_warm: bool,
}

impl Default for LinkWarmthSettings {
    fn default() -> Self {
        Self {
            keep_link_warm: true,
        }
    }
}

impl LinkWarmthSettings {
    /// Loads the switch, falling back to the default when it was never saved.
    ///
    /// # Errors
    /// Reports an unreadable or corrupt existing file.
    pub fn load_or_default(store: &DeviceStore) -> io::Result<Self> {
        let file = match OpenOptions::new().read(true).open(settings_path(store)) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(error) => return Err(error),
        };
        let mut text = String::new();
        file.take(SETTINGS_LIMIT + 1).read_to_string(&mut text)?;
        decode(&text)
    }

    /// # Errors
    /// Returns an error when the file cannot be written durably.
    pub fn save(self, store: &DeviceStore) -> io::Result<()> {
        fs::create_dir_all(store.root())?;
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(settings_path(store))?;
        file.write_all(encode(self).as_bytes())?;
        file.sync_all()
    }
}

fn settings_path(store: &DeviceStore) -> PathBuf {
    store.root().join(SETTINGS_FILE)
}

fn encode(settings: LinkWarmthSettings) -> String {
    format!(
        "{SETTINGS_HEADER}\nkeep_link_warm={}\n",
        u8::from(settings.keep_link_warm)
    )
}

fn decode(text: &str) -> io::Result<LinkWarmthSettings> {
    let invalid = |message: &str| io::Error::new(io::ErrorKind::InvalidData, message.to_owned());
    if text.len() as u64 > SETTINGS_LIMIT {
        return Err(invalid("link warmth settings file is too large"));
    }
    let mut lines = text.lines();
    if lines.next() != Some(SETTINGS_HEADER) {
        return Err(invalid("unsupported link warmth settings header"));
    }
    let mut keep_link_warm = None;
    for line in lines.filter(|line| !line.is_empty()) {
        match line.split_once('=') {
            Some(("keep_link_warm", "1")) if keep_link_warm.is_none() => {
                keep_link_warm = Some(true);
            }
            Some(("keep_link_warm", "0")) if keep_link_warm.is_none() => {
                keep_link_warm = Some(false);
            }
            _ => return Err(invalid("invalid link warmth settings line")),
        }
    }
    keep_link_warm
        .map(|keep_link_warm| LinkWarmthSettings { keep_link_warm })
        .ok_or_else(|| invalid("link warmth settings are incomplete"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_round_trip_and_reject_corruption() {
        for keep_link_warm in [true, false] {
            let settings = LinkWarmthSettings { keep_link_warm };
            assert_eq!(decode(&encode(settings)).unwrap(), settings);
        }
        assert!(decode("clipferry-link-warmth-v1\n").is_err());
        assert!(decode("clipferry-link-warmth-v1\nkeep_link_warm=yes\n").is_err());
        assert!(decode("clipferry-link-warmth-v1\nkeep_link_warm=1\nkeep_link_warm=0\n").is_err());
        assert!(decode("other\nkeep_link_warm=1\n").is_err());
        assert!(LinkWarmthSettings::default().keep_link_warm);
    }

    #[test]
    fn desktops_without_battery_count_as_ac_power() {
        assert!(power_status_is_ac(1, 8));
        assert!(power_status_is_ac(255, NO_SYSTEM_BATTERY));
        assert!(!power_status_is_ac(0, 1));
        assert!(!power_status_is_ac(255, 2));
    }
}
