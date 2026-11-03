pub mod about;
pub mod app_settings;
mod autostart;
pub mod clipboard;
mod corner_guard_window;
pub mod cross_screen;
mod cursor_locator;
pub mod device_store;
pub mod discovery;
pub mod license_runtime;
#[cfg(feature = "licensing")]
pub mod licensing;
mod logging;
pub mod pairing;
mod screen_info_sync;
pub mod screenshot;
pub mod security;
mod settings_window;
pub mod ssh;
mod traffic_control;
mod transfer_window;
pub mod tray;
pub mod update_support;
pub mod workspace;

pub const APP_NAME: &str = "Pierdesk";

#[must_use]
pub const fn validation_banner() -> &'static str {
    "Pierdesk technical validation"
}

#[cfg(test)]
mod tests {
    use super::{APP_NAME, validation_banner};

    #[test]
    fn validation_banner_identifies_the_application() {
        assert!(validation_banner().starts_with(APP_NAME));
    }
}
