use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Read as _, Write as _};
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::str::FromStr as _;

use crate::cross_screen::PeerPlacement;
use crate::device_store::DeviceStore;
use crate::security::CertificateFingerprint;

const SETTINGS_FILE: &str = "settings.v1";
const SETTINGS_HEADER: &str = "ClipFerrySettingsV1";
const SETTINGS_LIMIT: u64 = 4096;
const CROSS_SCREEN_SETTINGS_FILE: &str = "cross_screen.v1";
const CROSS_SCREEN_SETTINGS_HEADER: &str = "ClipFerryCrossScreenSettingsV3";
const LEGACY_CROSS_SCREEN_SETTINGS_V2_HEADER: &str = "ClipFerryCrossScreenSettingsV2";
const LEGACY_CROSS_SCREEN_SETTINGS_V1_HEADER: &str = "ClipFerryCrossScreenSettingsV1";
const CROSS_SCREEN_SETTINGS_LIMIT: u64 = 4096;
const SHARED_CLIPBOARD_SETTINGS_FILE: &str = "shared_clipboard.v1";
const SHARED_CLIPBOARD_SETTINGS_HEADER: &str = "ClipFerrySharedClipboardSettingsV3";
const LEGACY_SHARED_CLIPBOARD_SETTINGS_V2_HEADER: &str = "ClipFerrySharedClipboardSettingsV2";
const LEGACY_SHARED_CLIPBOARD_SETTINGS_V1_HEADER: &str = "ClipFerrySharedClipboardSettingsV1";
const SHARED_CLIPBOARD_SETTINGS_LIMIT: u64 = 4096;
const TRANSFER_WINDOW_SETTINGS_FILE: &str = "transfer_window.v1";
const TRANSFER_WINDOW_SETTINGS_HEADER: &str = "ClipFerryTransferWindowSettingsV1";
const TRANSFER_WINDOW_SETTINGS_LIMIT: u64 = 4096;
const UI_PREFERENCES_FILE: &str = "ui_preferences.v1";
const UI_PREFERENCES_HEADER: &str = "ClipFerryUiPreferencesV2";
const LEGACY_UI_PREFERENCES_HEADER: &str = "ClipFerryUiPreferencesV1";
const UI_PREFERENCES_LIMIT: u64 = 1024;
pub const MIN_UI_FONT_SCALE_PERCENT: u16 = 50;
pub const MAX_UI_FONT_SCALE_PERCENT: u16 = 125;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum UiLanguage {
    #[default]
    English,
    ChineseSimplified,
}

impl UiLanguage {
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::English => "en",
            Self::ChineseSimplified => "zh-CN",
        }
    }

    fn parse(value: &str) -> io::Result<Self> {
        match value {
            "en" => Ok(Self::English),
            "zh-CN" => Ok(Self::ChineseSimplified),
            _ => Err(invalid_data("language must be en or zh-CN")),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum UiTheme {
    #[default]
    Light,
    Dark,
}

impl UiTheme {
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Light => "light",
            Self::Dark => "dark",
        }
    }

    fn parse(value: &str) -> io::Result<Self> {
        match value {
            "light" => Ok(Self::Light),
            "dark" => Ok(Self::Dark),
            _ => Err(invalid_data("theme must be light or dark")),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UiPreferences {
    pub language: UiLanguage,
    pub theme: UiTheme,
    pub font_scale_percent: u16,
}

impl Default for UiPreferences {
    fn default() -> Self {
        Self {
            language: UiLanguage::English,
            theme: UiTheme::Light,
            font_scale_percent: 100,
        }
    }
}

impl UiPreferences {
    /// Loads the persisted user-interface preferences.
    ///
    /// # Errors
    ///
    /// Returns an error when the preference file cannot be read or is malformed.
    pub fn load(store: &DeviceStore) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .open(ui_preferences_path(store))?;
        if file.metadata()?.len() == 0 || file.metadata()?.len() > UI_PREFERENCES_LIMIT {
            return Err(invalid_data(
                "UI preferences file length is outside the allowed bounds",
            ));
        }
        let mut text = String::new();
        file.take(UI_PREFERENCES_LIMIT + 1)
            .read_to_string(&mut text)?;
        decode_ui_preferences(&text)
    }

    /// Loads persisted preferences, using the first-install defaults when no file exists.
    ///
    /// # Errors
    ///
    /// Returns an error when an existing preference file cannot be read or is malformed.
    pub fn load_or_default(store: &DeviceStore) -> io::Result<Self> {
        match Self::load(store) {
            Ok(preferences) => Ok(preferences),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error),
        }
    }

    /// Persists the user-interface preferences atomically enough for the single-process host.
    ///
    /// # Errors
    ///
    /// Returns an error when the settings directory or preference file cannot be written.
    pub fn save(self, store: &DeviceStore) -> io::Result<()> {
        fs::create_dir_all(store.root())?;
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(ui_preferences_path(store))?;
        file.write_all(self.encode().as_bytes())?;
        file.sync_all()
    }

    fn encode(self) -> String {
        format!(
            "{UI_PREFERENCES_HEADER}\nlanguage={}\ntheme={}\nfont_scale={}\n",
            self.language.key(),
            self.theme.key(),
            self.font_scale_percent,
        )
    }
}

fn decode_ui_preferences(text: &str) -> io::Result<UiPreferences> {
    let mut lines = text.lines();
    let header = lines
        .next()
        .ok_or_else(|| invalid_data("UI preferences are incomplete"))?;
    let Some(language) = lines.next().and_then(|line| line.strip_prefix("language=")) else {
        return Err(invalid_data("UI preferences are incomplete"));
    };
    if header == LEGACY_UI_PREFERENCES_HEADER {
        if lines.any(|line| !line.is_empty()) {
            return Err(invalid_data("UI preferences contain an unknown field"));
        }
        return Ok(UiPreferences {
            language: UiLanguage::parse(language)?,
            ..UiPreferences::default()
        });
    }
    if header != UI_PREFERENCES_HEADER {
        return Err(invalid_data("unsupported UI preferences header"));
    }
    let Some(theme) = lines.next().and_then(|line| line.strip_prefix("theme=")) else {
        return Err(invalid_data("UI preferences are incomplete"));
    };
    let Some(font_scale) = lines
        .next()
        .and_then(|line| line.strip_prefix("font_scale="))
    else {
        return Err(invalid_data("UI preferences are incomplete"));
    };
    if lines.any(|line| !line.is_empty()) {
        return Err(invalid_data("UI preferences contain an unknown field"));
    }
    let font_scale_percent = font_scale
        .parse::<u16>()
        .map_err(|_| invalid_data("font scale must be an integer"))?;
    if !(MIN_UI_FONT_SCALE_PERCENT..=MAX_UI_FONT_SCALE_PERCENT).contains(&font_scale_percent)
        || !font_scale_percent.is_multiple_of(5)
    {
        return Err(invalid_data(
            "font scale must be between 50 and 125 in steps of 5",
        ));
    }
    Ok(UiPreferences {
        language: UiLanguage::parse(language)?,
        theme: UiTheme::parse(theme)?,
        font_scale_percent,
    })
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TransferWindowSettings {
    /// Whether a newly-started incoming transfer may bring the transfer window to the screen.
    /// The default is deliberately quiet; the tray command remains available at all times.
    pub auto_popup: bool,
}

impl TransferWindowSettings {
    /// Loads the independent transfer-window preference.
    ///
    /// # Errors
    ///
    /// Returns an error for a missing, corrupt, oversized, or unsupported settings file.
    pub fn load(store: &DeviceStore) -> io::Result<Self> {
        let path = transfer_window_settings_path(store);
        let file = OpenOptions::new().read(true).open(path)?;
        let metadata = file.metadata()?;
        if metadata.len() == 0 || metadata.len() > TRANSFER_WINDOW_SETTINGS_LIMIT {
            return Err(invalid_data(
                "transfer window settings file length is outside the allowed bounds",
            ));
        }
        let mut text = String::new();
        file.take(TRANSFER_WINDOW_SETTINGS_LIMIT + 1)
            .read_to_string(&mut text)?;
        decode_transfer_window(&text)
    }

    /// Uses the quiet first-run default only when no preference has been saved yet.
    ///
    /// # Errors
    ///
    /// Existing unreadable or malformed files are reported instead of silently overwritten.
    pub fn load_or_default(store: &DeviceStore) -> io::Result<Self> {
        match Self::load(store) {
            Ok(settings) => Ok(settings),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error),
        }
    }

    /// Saves the transfer-window preference without touching pairing or transfer settings.
    ///
    /// # Errors
    ///
    /// Returns an error when the settings directory or file cannot be written durably.
    pub fn save(self, store: &DeviceStore) -> io::Result<()> {
        fs::create_dir_all(store.root())?;
        let path = transfer_window_settings_path(store);
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        file.write_all(self.encode().as_bytes())?;
        file.sync_all()
    }

    fn encode(self) -> String {
        format!(
            "{TRANSFER_WINDOW_SETTINGS_HEADER}\nauto_popup={}\n",
            u8::from(self.auto_popup),
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SharedClipboardSettings {
    pub enabled: bool,
}

impl Default for SharedClipboardSettings {
    fn default() -> Self {
        Self { enabled: true }
    }
}

impl SharedClipboardSettings {
    /// Loads shared-clipboard preferences without changing pairing or file-transfer settings.
    ///
    /// # Errors
    ///
    /// Returns an error for a missing, corrupt, oversized, or unsupported settings file.
    pub fn load(store: &DeviceStore) -> io::Result<Self> {
        let path = shared_clipboard_settings_path(store);
        let file = OpenOptions::new().read(true).open(path)?;
        let metadata = file.metadata()?;
        if metadata.len() == 0 || metadata.len() > SHARED_CLIPBOARD_SETTINGS_LIMIT {
            return Err(invalid_data(
                "shared clipboard settings file length is outside the allowed bounds",
            ));
        }
        let mut text = String::new();
        file.take(SHARED_CLIPBOARD_SETTINGS_LIMIT + 1)
            .read_to_string(&mut text)?;
        decode_shared_clipboard(&text)
    }

    /// Loads first-run defaults when shared clipboard has never been configured.
    ///
    /// # Errors
    ///
    /// Corrupt or inaccessible existing files are reported instead of silently reset.
    pub fn load_or_default(store: &DeviceStore) -> io::Result<Self> {
        match Self::load(store) {
            Ok(settings) => Ok(settings),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error),
        }
    }

    /// Saves shared-clipboard preferences independently from every other feature.
    ///
    /// # Errors
    ///
    /// Returns an error when durable replacement fails.
    pub fn save(self, store: &DeviceStore) -> io::Result<()> {
        fs::create_dir_all(store.root())?;
        let path = shared_clipboard_settings_path(store);
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        file.write_all(self.encode().as_bytes())?;
        file.sync_all()
    }

    fn encode(self) -> String {
        format!(
            "{SHARED_CLIPBOARD_SETTINGS_HEADER}\nenabled={}\n",
            u8::from(self.enabled),
        )
    }

    #[must_use]
    pub const fn any_enabled(self) -> bool {
        self.enabled
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[allow(clippy::struct_excessive_bools)]
pub struct CrossScreenSettings {
    pub enabled: bool,
    /// Uses the V2 per-segment boundary map. Disable only as a temporary legacy rollback.
    pub boundary_map_v2_enabled: bool,
    /// Canonical layout: where the connector screen is relative to the listener screen.
    /// Both devices persist the same value and derive their local reciprocal boundary from it.
    pub listener_peer_placement: PeerPlacement,
    pub layout_revision: u64,
    pub corner_guard_enabled: bool,
    pub corner_guard_pixels: u32,
    pub require_second_push: bool,
    pub pause_on_fullscreen: bool,
}

impl Default for CrossScreenSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            boundary_map_v2_enabled: true,
            listener_peer_placement: PeerPlacement::Above,
            layout_revision: 0,
            corner_guard_enabled: true,
            corner_guard_pixels: 32,
            require_second_push: false,
            pause_on_fullscreen: false,
        }
    }
}

impl CrossScreenSettings {
    /// Loads cross-screen preferences without changing the existing file-transfer settings.
    ///
    /// # Errors
    ///
    /// Returns an error for a corrupt, oversized, or unsupported settings file.
    pub fn load(store: &DeviceStore) -> io::Result<Self> {
        let path = cross_screen_settings_path(store);
        let file = OpenOptions::new().read(true).open(path)?;
        let metadata = file.metadata()?;
        if metadata.len() == 0 || metadata.len() > CROSS_SCREEN_SETTINGS_LIMIT {
            return Err(invalid_data(
                "cross-screen settings file length is outside the allowed bounds",
            ));
        }
        let mut text = String::new();
        file.take(CROSS_SCREEN_SETTINGS_LIMIT + 1)
            .read_to_string(&mut text)?;
        decode_cross_screen(&text)
    }

    /// Loads cross-screen preferences or returns safe disabled defaults when never configured.
    ///
    /// # Errors
    ///
    /// Corrupt or inaccessible existing files are reported instead of silently reset.
    pub fn load_or_default(store: &DeviceStore) -> io::Result<Self> {
        match Self::load(store) {
            Ok(settings) => Ok(settings),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error),
        }
    }

    /// Saves cross-screen preferences independently from file-transfer pairing settings.
    ///
    /// # Errors
    ///
    /// Returns an error when values are out of bounds or durable replacement fails.
    pub fn save(&self, store: &DeviceStore) -> io::Result<()> {
        self.validate()?;
        fs::create_dir_all(store.root())?;
        let path = cross_screen_settings_path(store);
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        file.write_all(self.encode().as_bytes())?;
        file.sync_all()
    }

    #[must_use]
    pub const fn local_peer_placement(&self, local_is_listener: bool) -> PeerPlacement {
        if local_is_listener {
            self.listener_peer_placement
        } else {
            self.listener_peer_placement.opposite()
        }
    }

    pub fn set_local_peer_placement(
        &mut self,
        local_is_listener: bool,
        placement: PeerPlacement,
        revision: u64,
    ) {
        self.listener_peer_placement = if local_is_listener {
            placement
        } else {
            placement.opposite()
        };
        self.layout_revision = revision;
    }

    fn validate(&self) -> io::Result<()> {
        if self.layout_revision == u64::MAX {
            return Err(invalid_data(
                "cross-screen layout revision has reached its reserved maximum",
            ));
        }
        if self.corner_guard_pixels > 512 {
            return Err(invalid_data(
                "cross-screen corner guard must be between 0 and 512 physical pixels",
            ));
        }
        Ok(())
    }

    fn encode(&self) -> String {
        format!(
            "{CROSS_SCREEN_SETTINGS_HEADER}\nenabled={}\nboundary_map_v2_enabled={}\nlistener_peer_placement={}\nlayout_revision={}\ncorner_guard_enabled={}\ncorner_guard_pixels={}\nrequire_second_push={}\npause_on_fullscreen={}\n",
            u8::from(self.enabled),
            u8::from(self.boundary_map_v2_enabled),
            placement_label(self.listener_peer_placement),
            self.layout_revision,
            u8::from(self.corner_guard_enabled),
            self.corner_guard_pixels,
            u8::from(self.require_second_push),
            u8::from(self.pause_on_fullscreen),
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppSettings {
    pub local_endpoint: SocketAddr,
    pub active_peer: CertificateFingerprint,
    pub peer_endpoint: SocketAddr,
    pub auto_receive: bool,
}

impl AppSettings {
    /// Loads the bounded per-user product settings and revalidates the selected peer.
    ///
    /// # Errors
    ///
    /// Returns an error for a missing/corrupt file, non-private endpoints, or a peer that is no
    /// longer trusted.
    pub fn load(store: &DeviceStore) -> io::Result<Self> {
        let path = settings_path(store);
        let file = OpenOptions::new().read(true).open(&path)?;
        let metadata = file.metadata()?;
        if metadata.len() == 0 || metadata.len() > SETTINGS_LIMIT {
            return Err(invalid_data(
                "settings file length is outside the allowed bounds",
            ));
        }
        let mut text = String::new();
        file.take(SETTINGS_LIMIT + 1).read_to_string(&mut text)?;
        let settings = decode(&text)?;
        settings.validate(store)?;
        Ok(settings)
    }

    /// Saves non-secret product settings for the current user.
    ///
    /// # Errors
    ///
    /// Returns an error when validation or durable file replacement fails.
    pub fn save(&self, store: &DeviceStore) -> io::Result<()> {
        self.validate(store)?;
        fs::create_dir_all(store.root())?;
        let path = settings_path(store);
        let encoded = self.encode();
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        file.write_all(encoded.as_bytes())?;
        file.sync_all()
    }

    fn validate(&self, store: &DeviceStore) -> io::Result<()> {
        validate_private_endpoint(self.local_endpoint)?;
        validate_private_endpoint(self.peer_endpoint)?;
        if self.local_endpoint.ip().is_loopback() != self.peer_endpoint.ip().is_loopback() {
            return Err(invalid_data(
                "local and peer endpoints must both be loopback or both be private LAN addresses",
            ));
        }
        let peer = store.load_peer(self.active_peer)?;
        if peer.fingerprint != self.active_peer {
            return Err(invalid_data(
                "active peer record does not match its fingerprint",
            ));
        }
        Ok(())
    }

    fn encode(&self) -> String {
        format!(
            "{SETTINGS_HEADER}\nlocal_endpoint={}\nactive_peer={}\npeer_endpoint={}\nauto_receive={}\n",
            self.local_endpoint,
            self.active_peer,
            self.peer_endpoint,
            u8::from(self.auto_receive)
        )
    }
}

fn decode(text: &str) -> io::Result<AppSettings> {
    let mut lines = text.lines();
    if lines.next() != Some(SETTINGS_HEADER) {
        return Err(invalid_data("unsupported settings header"));
    }
    let mut fields = HashMap::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| invalid_data("settings line is missing '='"))?;
        if !matches!(
            key,
            "local_endpoint" | "active_peer" | "peer_endpoint" | "auto_receive"
        ) {
            return Err(invalid_data("settings contain an unknown field"));
        }
        if fields.insert(key, value).is_some() {
            return Err(invalid_data("settings contain a duplicate field"));
        }
    }
    if fields.len() != 4 {
        return Err(invalid_data("settings are incomplete"));
    }
    let local_endpoint = parse_endpoint(required(&fields, "local_endpoint")?)?;
    let active_peer = CertificateFingerprint::from_str(required(&fields, "active_peer")?)?;
    let peer_endpoint = parse_endpoint(required(&fields, "peer_endpoint")?)?;
    let auto_receive = match required(&fields, "auto_receive")? {
        "0" => false,
        "1" => true,
        _ => return Err(invalid_data("auto_receive must be 0 or 1")),
    };
    Ok(AppSettings {
        local_endpoint,
        active_peer,
        peer_endpoint,
        auto_receive,
    })
}

fn required<'a>(fields: &'a HashMap<&str, &str>, key: &str) -> io::Result<&'a str> {
    fields
        .get(key)
        .copied()
        .ok_or_else(|| invalid_data("settings are incomplete"))
}

fn parse_endpoint(value: &str) -> io::Result<SocketAddr> {
    let endpoint = value
        .parse::<SocketAddr>()
        .map_err(|_| invalid_data("settings contain an invalid socket endpoint"))?;
    validate_private_endpoint(endpoint)?;
    Ok(endpoint)
}

fn decode_cross_screen(text: &str) -> io::Result<CrossScreenSettings> {
    let mut lines = text.lines();
    let version = match lines.next() {
        Some(CROSS_SCREEN_SETTINGS_HEADER) => 3,
        Some(LEGACY_CROSS_SCREEN_SETTINGS_V2_HEADER) => 2,
        Some(LEGACY_CROSS_SCREEN_SETTINGS_V1_HEADER) => 1,
        _ => return Err(invalid_data("unsupported cross-screen settings header")),
    };
    let mut fields = HashMap::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| invalid_data("cross-screen settings line is missing '='"))?;
        if !matches!(
            key,
            "enabled"
                | "boundary_map_v2_enabled"
                | "listener_peer_placement"
                | "layout_revision"
                | "corner_guard_enabled"
                | "corner_guard_pixels"
                | "require_second_push"
                | "pause_on_fullscreen"
        ) {
            return Err(invalid_data(
                "cross-screen settings contain an unknown field",
            ));
        }
        if fields.insert(key, value).is_some() {
            return Err(invalid_data(
                "cross-screen settings contain a duplicate field",
            ));
        }
    }
    let expected_fields = if version == 3 { 8 } else { 7 };
    if fields.len() != expected_fields {
        return Err(invalid_data("cross-screen settings are incomplete"));
    }
    let stored_pause_on_fullscreen = parse_flag(
        required(&fields, "pause_on_fullscreen")?,
        "pause_on_fullscreen",
    )?;
    let settings = CrossScreenSettings {
        enabled: parse_flag(required(&fields, "enabled")?, "enabled")?,
        boundary_map_v2_enabled: if version == 3 {
            parse_flag(
                required(&fields, "boundary_map_v2_enabled")?,
                "boundary_map_v2_enabled",
            )?
        } else {
            true
        },
        listener_peer_placement: parse_placement(required(&fields, "listener_peer_placement")?)?,
        layout_revision: required(&fields, "layout_revision")?
            .parse()
            .map_err(|_| invalid_data("layout_revision must be an unsigned integer"))?,
        corner_guard_enabled: parse_flag(
            required(&fields, "corner_guard_enabled")?,
            "corner_guard_enabled",
        )?,
        corner_guard_pixels: required(&fields, "corner_guard_pixels")?
            .parse()
            .map_err(|_| invalid_data("corner_guard_pixels must be an unsigned integer"))?,
        require_second_push: parse_flag(
            required(&fields, "require_second_push")?,
            "require_second_push",
        )?,
        // V1 enabled fullscreen pausing by default. Migrate it to the new product default so an
        // existing installation does not keep tearing down an otherwise healthy session merely
        // because a browser, video player, or other ordinary application covers the display.
        pause_on_fullscreen: if version == 1 {
            false
        } else {
            stored_pause_on_fullscreen
        },
    };
    settings.validate()?;
    Ok(settings)
}

fn decode_shared_clipboard(text: &str) -> io::Result<SharedClipboardSettings> {
    let mut lines = text.lines();
    let header = lines.next();
    if !matches!(
        header,
        Some(
            SHARED_CLIPBOARD_SETTINGS_HEADER
                | LEGACY_SHARED_CLIPBOARD_SETTINGS_V2_HEADER
                | LEGACY_SHARED_CLIPBOARD_SETTINGS_V1_HEADER
        )
    ) {
        return Err(invalid_data("unsupported shared clipboard settings header"));
    }
    let mut fields = HashMap::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| invalid_data("shared clipboard settings line is missing '='"))?;
        if !matches!(
            key,
            "enabled" | "text_enabled" | "rich_text_enabled" | "image_enabled"
        ) {
            return Err(invalid_data(
                "shared clipboard settings contain an unknown field",
            ));
        }
        if fields.insert(key, value).is_some() {
            return Err(invalid_data(
                "shared clipboard settings contain a duplicate field",
            ));
        }
    }
    if header == Some(SHARED_CLIPBOARD_SETTINGS_HEADER) {
        if fields.len() != 1 {
            return Err(invalid_data("shared clipboard settings are incomplete"));
        }
        return Ok(SharedClipboardSettings {
            enabled: parse_flag(required(&fields, "enabled")?, "enabled")?,
        });
    }
    let text_enabled = parse_flag(required(&fields, "text_enabled")?, "text_enabled")?;
    if header == Some(LEGACY_SHARED_CLIPBOARD_SETTINGS_V1_HEADER) {
        if fields.len() != 1 {
            return Err(invalid_data(
                "legacy shared clipboard settings contain unexpected fields",
            ));
        }
        return Ok(SharedClipboardSettings {
            enabled: text_enabled,
        });
    }
    if fields.len() != 3 {
        return Err(invalid_data(
            "legacy shared clipboard settings are incomplete",
        ));
    }
    let rich_text_enabled =
        parse_flag(required(&fields, "rich_text_enabled")?, "rich_text_enabled")?;
    let image_enabled = parse_flag(required(&fields, "image_enabled")?, "image_enabled")?;
    Ok(SharedClipboardSettings {
        // The former three-category UI is intentionally collapsed into one switch. If any
        // category was enabled, upgrading keeps shared clipboard enabled and exposes every
        // supported content type so the menu can never claim "on" while silently omitting one.
        enabled: text_enabled || rich_text_enabled || image_enabled,
    })
}

fn decode_transfer_window(text: &str) -> io::Result<TransferWindowSettings> {
    let mut lines = text.lines();
    if lines.next() != Some(TRANSFER_WINDOW_SETTINGS_HEADER) {
        return Err(invalid_data("unsupported transfer window settings header"));
    }
    let mut fields = HashMap::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| invalid_data("transfer window settings line is missing '='"))?;
        if key != "auto_popup" {
            return Err(invalid_data(
                "transfer window settings contain an unknown field",
            ));
        }
        if fields.insert(key, value).is_some() {
            return Err(invalid_data(
                "transfer window settings contain a duplicate field",
            ));
        }
    }
    if fields.len() != 1 {
        return Err(invalid_data("transfer window settings are incomplete"));
    }
    Ok(TransferWindowSettings {
        auto_popup: parse_flag(required(&fields, "auto_popup")?, "auto_popup")?,
    })
}

fn parse_flag(value: &str, field: &str) -> io::Result<bool> {
    match value {
        "0" => Ok(false),
        "1" => Ok(true),
        _ => Err(invalid_data(format!("{field} must be 0 or 1"))),
    }
}

const fn placement_label(placement: PeerPlacement) -> &'static str {
    match placement {
        PeerPlacement::Left => "left",
        PeerPlacement::Right => "right",
        PeerPlacement::Above => "above",
        PeerPlacement::Below => "below",
    }
}

fn parse_placement(value: &str) -> io::Result<PeerPlacement> {
    match value {
        "left" => Ok(PeerPlacement::Left),
        "right" => Ok(PeerPlacement::Right),
        "above" => Ok(PeerPlacement::Above),
        "below" => Ok(PeerPlacement::Below),
        _ => Err(invalid_data(
            "listener_peer_placement must be left, right, above, or below",
        )),
    }
}

/// Verifies that an endpoint is non-zero and limited to loopback or private unicast space.
///
/// # Errors
///
/// Returns an error for port zero, public addresses, multicast, or unspecified addresses.
pub fn validate_private_endpoint(endpoint: SocketAddr) -> io::Result<()> {
    if endpoint.port() == 0 {
        return Err(invalid_data("endpoint port must not be zero"));
    }
    let allowed = match endpoint.ip() {
        IpAddr::V4(ip) => ip.is_loopback() || ip.is_private(),
        IpAddr::V6(ip) => ip.is_loopback() || ip.is_unique_local(),
    };
    if !allowed {
        return Err(invalid_data(
            "endpoint must use a loopback or private unicast address",
        ));
    }
    Ok(())
}

fn settings_path(store: &DeviceStore) -> PathBuf {
    store.root().join(SETTINGS_FILE)
}

fn cross_screen_settings_path(store: &DeviceStore) -> PathBuf {
    store.root().join(CROSS_SCREEN_SETTINGS_FILE)
}

fn shared_clipboard_settings_path(store: &DeviceStore) -> PathBuf {
    store.root().join(SHARED_CLIPBOARD_SETTINGS_FILE)
}

fn transfer_window_settings_path(store: &DeviceStore) -> PathBuf {
    store.root().join(TRANSFER_WINDOW_SETTINGS_FILE)
}

fn ui_preferences_path(store: &DeviceStore) -> PathBuf {
    store.root().join(UI_PREFERENCES_FILE)
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_text_round_trip_is_strict() {
        let fingerprint = CertificateFingerprint::from_certificate(b"peer");
        let settings = AppSettings {
            local_endpoint: "192.168.1.10:45233".parse().unwrap(),
            active_peer: fingerprint,
            peer_endpoint: "192.168.1.11:45233".parse().unwrap(),
            auto_receive: true,
        };
        assert_eq!(decode(&settings.encode()).unwrap(), settings);
    }

    #[test]
    fn settings_reject_unknown_duplicate_and_public_endpoints() {
        let fingerprint = CertificateFingerprint::from_certificate(b"peer");
        let base = format!(
            "{SETTINGS_HEADER}\nlocal_endpoint=192.168.1.10:45233\nactive_peer={fingerprint}\npeer_endpoint=192.168.1.11:45233\nauto_receive=0\n"
        );
        assert!(decode(&format!("{base}unknown=1\n")).is_err());
        assert!(decode(&format!("{base}auto_receive=1\n")).is_err());
        assert!(decode(&base.replace("192.168.1.10", "8.8.8.8")).is_err());
        assert!(decode(&base.replace(":45233", ":0")).is_err());
    }

    #[test]
    fn settings_require_matching_loopback_scope() {
        let fingerprint = CertificateFingerprint::from_certificate(b"peer");
        let settings = AppSettings {
            local_endpoint: "127.0.0.1:45233".parse().unwrap(),
            active_peer: fingerprint,
            peer_endpoint: "192.168.1.11:45233".parse().unwrap(),
            auto_receive: false,
        };
        assert!(settings.validate(&DeviceStore::new("unused")).is_err());
    }

    #[test]
    fn cross_screen_settings_round_trip_and_reciprocal_layout_are_strict() {
        let settings = CrossScreenSettings {
            enabled: true,
            boundary_map_v2_enabled: false,
            listener_peer_placement: PeerPlacement::Below,
            layout_revision: 42,
            corner_guard_enabled: true,
            corner_guard_pixels: 48,
            require_second_push: true,
            pause_on_fullscreen: true,
        };
        assert_eq!(decode_cross_screen(&settings.encode()).unwrap(), settings);
        assert_eq!(settings.local_peer_placement(true), PeerPlacement::Below);
        assert_eq!(settings.local_peer_placement(false), PeerPlacement::Above);
    }

    #[test]
    fn cross_screen_settings_reject_unknown_duplicate_and_oversized_guard() {
        let base = CrossScreenSettings::default().encode();
        assert!(decode_cross_screen(&format!("{base}unknown=1\n")).is_err());
        assert!(decode_cross_screen(&format!("{base}enabled=1\n")).is_err());
        assert!(
            decode_cross_screen(&base.replace("corner_guard_pixels=32", "corner_guard_pixels=513"))
                .is_err()
        );
        assert!(
            decode_cross_screen(&base.replace("pause_on_fullscreen=0", "pause_on_fullscreen=yes"))
                .is_err()
        );
    }

    #[test]
    fn legacy_cross_screen_settings_migrate_to_v2_boundaries() {
        let legacy = CrossScreenSettings {
            pause_on_fullscreen: true,
            ..CrossScreenSettings::default()
        }
        .encode()
        .replacen(
            CROSS_SCREEN_SETTINGS_HEADER,
            LEGACY_CROSS_SCREEN_SETTINGS_V1_HEADER,
            1,
        )
        .replace("boundary_map_v2_enabled=1\n", "");

        let migrated = decode_cross_screen(&legacy).unwrap();

        assert!(!migrated.pause_on_fullscreen);
        assert!(migrated.boundary_map_v2_enabled);
        assert!(migrated.encode().starts_with(CROSS_SCREEN_SETTINGS_HEADER));
        assert!(
            decode_cross_screen(
                &legacy.replace("pause_on_fullscreen=1", "pause_on_fullscreen=yes")
            )
            .is_err()
        );
        assert!(!CrossScreenSettings::default().pause_on_fullscreen);
    }

    #[test]
    fn v2_cross_screen_settings_preserve_fullscreen_choice_and_enable_boundary_map() {
        let legacy = CrossScreenSettings {
            pause_on_fullscreen: true,
            ..CrossScreenSettings::default()
        }
        .encode()
        .replacen(
            CROSS_SCREEN_SETTINGS_HEADER,
            LEGACY_CROSS_SCREEN_SETTINGS_V2_HEADER,
            1,
        )
        .replace("boundary_map_v2_enabled=1\n", "");
        let migrated = decode_cross_screen(&legacy).unwrap();
        assert!(migrated.pause_on_fullscreen);
        assert!(migrated.boundary_map_v2_enabled);
    }

    #[test]
    fn first_run_defaults_enable_requested_features() {
        let cross_screen = CrossScreenSettings::default();
        assert!(cross_screen.enabled);
        assert_eq!(cross_screen.listener_peer_placement, PeerPlacement::Above);
        assert!(cross_screen.corner_guard_enabled);
        assert!(SharedClipboardSettings::default().enabled);
        assert!(!TransferWindowSettings::default().auto_popup);
        assert_eq!(UiPreferences::default().language, UiLanguage::English);
        assert_eq!(UiPreferences::default().theme, UiTheme::Light);
        assert_eq!(UiPreferences::default().font_scale_percent, 100);

        let enabled = SharedClipboardSettings { enabled: true };
        assert_eq!(decode_shared_clipboard(&enabled.encode()).unwrap(), enabled);

        let base = enabled.encode();
        assert!(decode_shared_clipboard(&format!("{base}unknown=1\n")).is_err());
        assert!(decode_shared_clipboard(&format!("{base}enabled=0\n")).is_err());
        assert!(decode_shared_clipboard(&base.replace("enabled=1", "enabled=yes")).is_err());

        let legacy = "ClipFerrySharedClipboardSettingsV1\ntext_enabled=0\n";
        let migrated = decode_shared_clipboard(legacy).unwrap();
        assert!(!migrated.any_enabled());
        let legacy_enabled = "ClipFerrySharedClipboardSettingsV1\ntext_enabled=1\n";
        let migrated = decode_shared_clipboard(legacy_enabled).unwrap();
        assert!(migrated.enabled);

        let previous_v2 = "ClipFerrySharedClipboardSettingsV2\ntext_enabled=1\nrich_text_enabled=0\nimage_enabled=0\n";
        assert!(decode_shared_clipboard(previous_v2).unwrap().enabled);
        let previous_v2_disabled = "ClipFerrySharedClipboardSettingsV2\ntext_enabled=0\nrich_text_enabled=0\nimage_enabled=0\n";
        assert!(
            !decode_shared_clipboard(previous_v2_disabled)
                .unwrap()
                .enabled
        );
    }

    #[test]
    fn ui_preferences_round_trip_and_reject_unknown_language() {
        let preferences = UiPreferences {
            language: UiLanguage::ChineseSimplified,
            theme: UiTheme::Dark,
            font_scale_percent: MIN_UI_FONT_SCALE_PERCENT,
        };
        assert_eq!(
            decode_ui_preferences(&preferences.encode()).unwrap(),
            preferences
        );
        assert_eq!(
            decode_ui_preferences("ClipFerryUiPreferencesV1\nlanguage=zh-CN\n").unwrap(),
            UiPreferences {
                language: UiLanguage::ChineseSimplified,
                ..UiPreferences::default()
            }
        );
        assert!(
            decode_ui_preferences(
                "ClipFerryUiPreferencesV2\nlanguage=fr\ntheme=light\nfont_scale=100\n"
            )
            .is_err()
        );
        assert!(
            decode_ui_preferences(
                "ClipFerryUiPreferencesV2\nlanguage=en\ntheme=system\nfont_scale=100\n"
            )
            .is_err()
        );
        assert!(
            decode_ui_preferences(
                "ClipFerryUiPreferencesV2\nlanguage=en\ntheme=light\nfont_scale=101\n"
            )
            .is_err()
        );
        assert!(
            decode_ui_preferences(
                "ClipFerryUiPreferencesV2\nlanguage=en\ntheme=light\nfont_scale=45\n"
            )
            .is_err()
        );
        assert!(
            decode_ui_preferences(
                "ClipFerryUiPreferencesV2\nlanguage=en\ntheme=light\nfont_scale=100\nextra=1\n"
            )
            .is_err()
        );
    }

    #[test]
    fn transfer_window_popup_setting_is_quiet_and_strict() {
        let enabled = TransferWindowSettings { auto_popup: true };
        assert_eq!(decode_transfer_window(&enabled.encode()).unwrap(), enabled);
        assert!(decode_transfer_window("ClipFerryTransferWindowSettingsV1\n").is_err());
        assert!(
            decode_transfer_window(
                "ClipFerryTransferWindowSettingsV1\nauto_popup=0\nauto_popup=1\n"
            )
            .is_err()
        );
        assert!(
            decode_transfer_window("ClipFerryTransferWindowSettingsV1\nauto_popup=yes\n").is_err()
        );
        assert!(decode_transfer_window("ClipFerryTransferWindowSettingsV1\nunknown=1\n").is_err());
    }
}
