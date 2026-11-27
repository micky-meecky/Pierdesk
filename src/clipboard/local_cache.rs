use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write as _};
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use windows::Win32::Foundation::{HANDLE, HWND};
use windows::Win32::System::DataExchange::GetOpenClipboardWindow;
use windows::Win32::System::IO::DeviceIoControl;
use windows::Win32::UI::WindowsAndMessaging::{
    GA_ROOT, GetAncestor, GetForegroundWindow, GetWindowThreadProcessId,
};

use super::data_object::VirtualFileEntry;
use super::drag_helper::process_image;

pub(crate) const LOCAL_CACHE_RETENTION: Duration = Duration::from_hours(168);
const LOCAL_CACHE_READ_BUFFER: usize = 1024 * 1024;
/// Fallback hover size cap when the local cache volume does not support NTFS sparse files.
pub(crate) const DRAG_HOVER_FALLBACK_MAX_BYTES: u64 = 64 * 1024 * 1024;
pub(crate) const DRAG_HOVER_FALLBACK_MAX_FILES: usize = 256;
/// Standard Win32 `FSCTL_SET_SPARSE` control code (`CTL_CODE(FILE_DEVICE_FILE_SYSTEM, 49, METHOD_BUFFERED, FILE_SPECIAL_ACCESS)`).
const FSCTL_SET_SPARSE: u32 = 0x0009_00C4;

pub(crate) type MaterializeLogger = Arc<dyn Fn(&str) + Send + Sync>;

enum MaterializedState {
    Empty,
    Placeholder {
        root: PathBuf,
        paths: Arc<[PathBuf]>,
    },
    Complete {
        paths: Arc<[PathBuf]>,
    },
}

pub(crate) struct LocalFileMaterializer {
    cache_root: PathBuf,
    cleanup_prefix: &'static str,
    directory_name: String,
    entries: Arc<[VirtualFileEntry]>,
    state: Mutex<MaterializedState>,
    placeholder_active: AtomicBool,
    served: AtomicBool,
    logger: Option<MaterializeLogger>,
    log_tag: &'static str,
}

impl LocalFileMaterializer {
    pub(crate) fn new(
        cache_root: PathBuf,
        cleanup_prefix: &'static str,
        directory_name: String,
        entries: Vec<VirtualFileEntry>,
        log_tag: &'static str,
        logger: Option<MaterializeLogger>,
    ) -> Self {
        Self {
            cache_root,
            cleanup_prefix,
            directory_name,
            entries: Arc::from(entries),
            state: Mutex::new(MaterializedState::Empty),
            placeholder_active: AtomicBool::new(false),
            served: AtomicBool::new(false),
            logger,
            log_tag,
        }
    }

    #[must_use]
    pub(crate) fn total_file_bytes(&self) -> u64 {
        self.entries
            .iter()
            .filter(|entry| !entry.descriptor.is_directory())
            .map(|entry| entry.descriptor.size)
            .sum()
    }

    #[must_use]
    pub(crate) fn ordinary_file_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|entry| !entry.descriptor.is_directory())
            .count()
    }

    #[must_use]
    pub(crate) fn has_unhydrated_placeholder(&self) -> bool {
        self.placeholder_active.load(Ordering::Acquire)
    }

    /// Returns `CF_HDROP` paths for drag targets.
    ///
    /// During hover (`dropping == false`), applications such as `WeChat` (Qt) call
    /// `IDataObject::GetData(CF_HDROP)` immediately inside `DragEnter` to validate that the
    /// dragged paths exist and have non-zero size. Materializing large offers synchronously there
    /// stalls the OLE modal loop for seconds; refusing returns a forbidden cursor. Instead,
    /// create instant NTFS sparse placeholders with the authenticated names and sizes (0 network
    /// bytes transferred) and hydrate them with real contents when the user releases the drop.
    pub(crate) fn hdrop_paths_for_drag(&self, dropping: bool) -> io::Result<Arc<[PathBuf]>> {
        if dropping {
            return self.hydrate_for_drop();
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("local file materializer was poisoned"))?;
        match &*state {
            MaterializedState::Complete { paths } if paths.iter().all(|path| path.exists()) => {
                self.served.store(true, Ordering::Release);
                return Ok(Arc::clone(paths));
            }
            MaterializedState::Placeholder { paths, .. }
                if paths.iter().all(|path| path.exists()) =>
            {
                return Ok(Arc::clone(paths));
            }
            _ => {}
        }

        match self.create_sparse_placeholder() {
            Ok((root, paths)) => {
                let paths: Arc<[PathBuf]> = Arc::from(paths);
                *state = MaterializedState::Placeholder {
                    root,
                    paths: Arc::clone(&paths),
                };
                self.placeholder_active.store(true, Ordering::Release);
                Ok(paths)
            }
            Err(placeholder_error) => {
                self.log(&format!(
                    "event={}_placeholder_unavailable error={placeholder_error}",
                    self.log_tag
                ));
                if !hover_fallback_allowed(self.total_file_bytes(), self.ordinary_file_count()) {
                    return Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "large local drop files are fetched only after the drop",
                    ));
                }
                let paths = self.materialize_fresh_locked(&mut state, "hover")?;
                self.served.store(true, Ordering::Release);
                Ok(paths)
            }
        }
    }

    /// Hydrates any previously returned sparse placeholders with authenticated remote bytes (or
    /// performs a fresh full download if no placeholder exists yet) before the target consumes
    /// the drop.
    pub(crate) fn hydrate_for_drop(&self) -> io::Result<Arc<[PathBuf]>> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("local file materializer was poisoned"))?;
        if let MaterializedState::Complete { paths } = &*state
            && paths.iter().all(|path| path.exists())
        {
            self.served.store(true, Ordering::Release);
            return Ok(Arc::clone(paths));
        }

        let placeholder = match std::mem::replace(&mut *state, MaterializedState::Empty) {
            MaterializedState::Placeholder { root, paths } => {
                self.placeholder_active.store(false, Ordering::Release);
                Some((root, paths))
            }
            MaterializedState::Complete { .. } | MaterializedState::Empty => None,
        };

        if let Some((root, paths)) = placeholder {
            if root.is_dir() && paths.iter().all(|path| path.exists()) {
                let started = Instant::now();
                let file_count = self.ordinary_file_count();
                let total_bytes = self.total_file_bytes();
                self.log(&format!(
                    "event={}_hydrate_start files={file_count} bytes={total_bytes} root={}",
                    self.log_tag,
                    root.display()
                ));
                match self.write_entries_into(&root, true) {
                    Ok(()) => {
                        self.log(&format!(
                            "event={}_hydrate_complete files={file_count} bytes={total_bytes} elapsed_ms={} paths={}",
                            self.log_tag,
                            started.elapsed().as_millis(),
                            paths.len()
                        ));
                        *state = MaterializedState::Complete {
                            paths: Arc::clone(&paths),
                        };
                        self.served.store(true, Ordering::Release);
                        return Ok(paths);
                    }
                    Err(error) => {
                        let _ = fs::remove_dir_all(&root);
                        self.log(&format!(
                            "event={}_hydrate_failed elapsed_ms={} error={error}",
                            self.log_tag,
                            started.elapsed().as_millis()
                        ));
                        return Err(error);
                    }
                }
            }
            let _ = fs::remove_dir_all(&root);
        }

        let paths = self.materialize_fresh_locked(&mut state, "drop")?;
        self.served.store(true, Ordering::Release);
        Ok(paths)
    }

    pub(crate) fn cleanup_unhydrated(&self) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        if matches!(&*state, MaterializedState::Placeholder { .. })
            && let MaterializedState::Placeholder { root, .. } =
                std::mem::replace(&mut *state, MaterializedState::Empty)
        {
            self.placeholder_active.store(false, Ordering::Release);
            let _ = fs::remove_dir_all(&root);
            self.log(&format!(
                "event={}_placeholder_cleaned root={}",
                self.log_tag,
                root.display()
            ));
        }
    }

    pub(crate) fn hdrop_paths_for_clipboard(&self) -> io::Result<Arc<[PathBuf]>> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("local file materializer was poisoned"))?;
        if let MaterializedState::Complete { paths } = &*state
            && paths.iter().all(|path| path.exists())
        {
            self.served.store(true, Ordering::Release);
            return Ok(Arc::clone(paths));
        }
        let paths = self.materialize_fresh_locked(&mut state, "paste")?;
        self.served.store(true, Ordering::Release);
        Ok(paths)
    }

    #[cfg(test)]
    pub(crate) fn paths(&self) -> io::Result<Arc<[PathBuf]>> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("local file materializer was poisoned"))?;
        if let MaterializedState::Complete { paths } = &*state
            && paths.iter().all(|path| path.exists())
        {
            return Ok(Arc::clone(paths));
        }
        self.materialize_fresh_locked(&mut state, "direct")
    }

    fn materialize_fresh_locked(
        &self,
        state: &mut MaterializedState,
        phase: &str,
    ) -> io::Result<Arc<[PathBuf]>> {
        fs::create_dir_all(&self.cache_root)?;
        cleanup_local_cache(&self.cache_root, self.cleanup_prefix);
        let root = self.allocate_root()?;
        let started = Instant::now();
        let file_count = self.ordinary_file_count();
        let total_bytes = self.total_file_bytes();
        self.log(&format!(
            "event={}_materialize_start phase={phase} files={file_count} bytes={total_bytes} root={}",
            self.log_tag,
            root.display()
        ));

        let result = self
            .write_entries_into(&root, false)
            .and_then(|()| self.collect_top_level_paths(&root));
        let paths = match result {
            Ok(paths) => paths,
            Err(error) => {
                let _ = fs::remove_dir_all(&root);
                self.log(&format!(
                    "event={}_materialize_failed phase={phase} elapsed_ms={} error={error}",
                    self.log_tag,
                    started.elapsed().as_millis()
                ));
                return Err(error);
            }
        };
        self.log(&format!(
            "event={}_materialize_complete phase={phase} files={file_count} bytes={total_bytes} elapsed_ms={} paths={}",
            self.log_tag,
            started.elapsed().as_millis(),
            paths.len()
        ));
        let paths: Arc<[PathBuf]> = Arc::from(paths);
        *state = MaterializedState::Complete {
            paths: Arc::clone(&paths),
        };
        self.placeholder_active.store(false, Ordering::Release);
        Ok(paths)
    }

    fn create_sparse_placeholder(&self) -> io::Result<(PathBuf, Vec<PathBuf>)> {
        fs::create_dir_all(&self.cache_root)?;
        cleanup_local_cache(&self.cache_root, self.cleanup_prefix);
        let root = self.allocate_root()?;
        let started = Instant::now();
        let result = (|| -> io::Result<Vec<PathBuf>> {
            for entry in self
                .entries
                .iter()
                .filter(|entry| entry.descriptor.is_directory())
            {
                fs::create_dir_all(root.join(entry.descriptor.file_name.as_ref()))?;
            }
            for entry in self
                .entries
                .iter()
                .filter(|entry| !entry.descriptor.is_directory())
            {
                let path = root.join(entry.descriptor.file_name.as_ref());
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent)?;
                }
                let file = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create_new(true)
                    .open(&path)?;
                if entry.descriptor.size > 0 {
                    mark_file_sparse(&file)?;
                    file.set_len(entry.descriptor.size)?;
                }
            }
            self.collect_top_level_paths(&root)
        })();
        match result {
            Ok(paths) => {
                self.log(&format!(
                    "event={}_placeholder_ready files={} bytes={} elapsed_ms={} root={}",
                    self.log_tag,
                    self.ordinary_file_count(),
                    self.total_file_bytes(),
                    started.elapsed().as_millis(),
                    root.display()
                ));
                Ok((root, paths))
            }
            Err(error) => {
                let _ = fs::remove_dir_all(&root);
                Err(error)
            }
        }
    }

    #[must_use]
    pub(crate) fn served(&self) -> bool {
        self.served.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(crate) fn is_materialized(&self) -> bool {
        self.state
            .lock()
            .is_ok_and(|guard| matches!(&*guard, MaterializedState::Complete { .. }))
    }

    fn log(&self, line: &str) {
        if let Some(logger) = self.logger.as_ref() {
            logger(line);
        }
    }

    fn allocate_root(&self) -> io::Result<PathBuf> {
        for suffix in 0..100_u32 {
            let name = if suffix == 0 {
                self.directory_name.clone()
            } else {
                format!("{}-{suffix}", self.directory_name)
            };
            let path = self.cache_root.join(name);
            match fs::create_dir(&path) {
                Ok(()) => return Ok(path),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not allocate an isolated local cache directory",
        ))
    }

    fn write_entries_into(&self, root: &Path, overwrite_existing: bool) -> io::Result<()> {
        for entry in self
            .entries
            .iter()
            .filter(|entry| entry.descriptor.is_directory())
        {
            fs::create_dir_all(root.join(entry.descriptor.file_name.as_ref()))?;
        }
        for entry in self
            .entries
            .iter()
            .filter(|entry| !entry.descriptor.is_directory())
        {
            let path = root.join(entry.descriptor.file_name.as_ref());
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut options = OpenOptions::new();
            options.write(true);
            if overwrite_existing {
                options.create(true).truncate(true);
            } else {
                options.create_new(true);
            }
            let mut file = options.open(&path)?;
            let source_factory = entry.source_factory.as_ref().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "file entry has no content source",
                )
            })?;
            let source = source_factory();
            if source.len() != entry.descriptor.size {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "local cache source size does not match its authenticated manifest",
                ));
            }
            let mut offset = 0_u64;
            let mut buffer = vec![0_u8; LOCAL_CACHE_READ_BUFFER];
            while offset < entry.descriptor.size {
                let remaining = entry.descriptor.size - offset;
                let requested = usize::try_from(remaining)
                    .unwrap_or(usize::MAX)
                    .min(buffer.len());
                let read = source
                    .read_at(offset, &mut buffer[..requested])
                    .map_err(|error| io::Error::other(error.to_string()))?;
                if read == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "remote file ended before its authenticated size",
                    ));
                }
                file.write_all(&buffer[..read])?;
                offset = offset
                    .checked_add(u64::try_from(read).map_err(io::Error::other)?)
                    .ok_or_else(|| io::Error::other("local cache file offset overflow"))?;
            }
            file.flush()?;
            let mut eof_probe = [0_u8; 1];
            let eof = source
                .read_at(entry.descriptor.size, &mut eof_probe)
                .map_err(|error| io::Error::other(error.to_string()))?;
            if eof != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "remote file exceeded its authenticated size",
                ));
            }
        }
        Ok(())
    }

    fn collect_top_level_paths(&self, root: &Path) -> io::Result<Vec<PathBuf>> {
        let mut seen = HashSet::new();
        let mut paths = Vec::new();
        for entry in self.entries.iter() {
            let top = entry
                .descriptor
                .file_name
                .split('\\')
                .next()
                .ok_or_else(|| io::Error::other("empty local cache path"))?;
            let key = top.to_lowercase();
            if seen.insert(key) {
                paths.push(root.join(top));
            }
        }
        if paths.is_empty() || paths.iter().any(|path| !path.exists()) {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "local cache paths did not materialize completely",
            ));
        }
        Ok(paths)
    }
}

fn mark_file_sparse(file: &File) -> io::Result<()> {
    let mut returned = 0_u32;
    let handle = HANDLE(file.as_raw_handle());
    // SAFETY: `handle` is an open Win32 file handle owned by `file` for the duration of the call.
    unsafe {
        DeviceIoControl(
            handle,
            FSCTL_SET_SPARSE,
            None,
            0,
            None,
            0,
            Some(&raw mut returned),
            None,
        )
    }
    .map_err(io::Error::other)
}

fn cleanup_local_cache(cache_root: &Path, prefix: &str) {
    let cutoff = SystemTime::now()
        .checked_sub(LOCAL_CACHE_RETENTION)
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let Ok(entries) = fs::read_dir(cache_root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let managed = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(prefix));
        let expired = entry
            .metadata()
            .ok()
            .filter(std::fs::Metadata::is_dir)
            .and_then(|metadata| metadata.modified().ok())
            .is_some_and(|modified| modified < cutoff);
        if managed && expired {
            let _ = fs::remove_dir_all(path);
        }
    }
}

#[must_use]
pub(crate) const fn hover_fallback_allowed(total_bytes: u64, ordinary_files: usize) -> bool {
    total_bytes <= DRAG_HOVER_FALLBACK_MAX_BYTES && ordinary_files <= DRAG_HOVER_FALLBACK_MAX_FILES
}

#[must_use]
pub(crate) fn is_explorer_process(name: &str) -> bool {
    name.eq_ignore_ascii_case("explorer.exe")
}

#[must_use]
fn is_excluded_clipboard_hdrop_process(name: &str) -> bool {
    is_explorer_process(name)
        || name.eq_ignore_ascii_case("svchost.exe")
        || name.eq_ignore_ascii_case("textinputhost.exe")
        || name.eq_ignore_ascii_case("shellexperiencehost.exe")
        || name.eq_ignore_ascii_case("searchhost.exe")
        || name.eq_ignore_ascii_case("startmenuexperiencehost.exe")
        || name.eq_ignore_ascii_case("rdpclip.exe")
        || name.eq_ignore_ascii_case("dwm.exe")
}

#[must_use]
pub(crate) fn should_offer_drag_hdrop_process(
    process_name: Option<&str>,
    owned_by_current_process: bool,
) -> bool {
    if owned_by_current_process {
        return false;
    }
    !process_name.is_some_and(is_explorer_process)
}

#[must_use]
pub(crate) fn should_offer_clipboard_hdrop_process(
    registering_clipboard: bool,
    window_present: bool,
    process_name: Option<&str>,
    owned_by_current_process: bool,
) -> bool {
    if registering_clipboard {
        return true;
    }
    if !window_present || owned_by_current_process {
        return false;
    }
    !process_name.is_some_and(is_excluded_clipboard_hdrop_process)
}

#[must_use]
pub(crate) fn window_process_id(window: HWND) -> Option<u32> {
    if window.0.is_null() {
        return None;
    }
    let mut process_id = 0_u32;
    unsafe { GetWindowThreadProcessId(window, Some(&raw mut process_id)) };
    (process_id != 0).then_some(process_id)
}

#[must_use]
pub(crate) fn window_process_name(window: HWND) -> Option<String> {
    let process_id = window_process_id(window)?;
    let image = process_image(process_id).ok()?;
    image
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
}

#[must_use]
pub(crate) fn window_belongs_to_current_process(window: HWND) -> bool {
    if window.0.is_null() {
        return false;
    }
    let current = std::process::id();
    if window_process_id(window) == Some(current) {
        return true;
    }
    let root = unsafe { GetAncestor(window, GA_ROOT) };
    !root.0.is_null() && window_process_id(root) == Some(current)
}

#[must_use]
pub(crate) fn drag_hdrop_policy(window: HWND) -> (bool, Option<String>) {
    let owned = window_belongs_to_current_process(window);
    let process = window_process_name(window);
    let offered = should_offer_drag_hdrop_process(process.as_deref(), owned);
    (offered, process)
}

#[must_use]
pub(crate) fn clipboard_hdrop_policy(registering_clipboard: bool) -> (bool, Option<String>) {
    if registering_clipboard {
        return (true, Some("clipboard-register".to_owned()));
    }
    let target_window = unsafe { GetOpenClipboardWindow() }
        .ok()
        .filter(|window| !window.0.is_null())
        .unwrap_or_else(|| unsafe { GetForegroundWindow() });
    let present = !target_window.0.is_null();
    let owned = window_belongs_to_current_process(target_window);
    let process = window_process_name(target_window);
    let offered =
        should_offer_clipboard_hdrop_process(false, present, process.as_deref(), owned);
    (offered, process)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use super::*;
    use crate::clipboard::data_object::VirtualFileDescriptor;
    use crate::clipboard::source::{MemorySource, ReadAtSource};

    fn unique_temp_root(prefix: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("{prefix}-{}-{nonce}", std::process::id()))
    }

    struct CountingFailingSource {
        len: u64,
        reads: Arc<AtomicUsize>,
    }

    impl ReadAtSource for CountingFailingSource {
        fn len(&self) -> u64 {
            self.len
        }

        fn read_at(&self, _offset: u64, _destination: &mut [u8]) -> windows::core::Result<usize> {
            self.reads.fetch_add(1, Ordering::AcqRel);
            Err(windows::core::Error::from_hresult(
                windows::Win32::Foundation::E_FAIL,
            ))
        }
    }

    #[test]
    fn hover_fallback_policy_allows_typical_files_and_rejects_oversized_offers() {
        assert!(hover_fallback_allowed(512 * 1024, 1));
        assert!(hover_fallback_allowed(
            DRAG_HOVER_FALLBACK_MAX_BYTES,
            DRAG_HOVER_FALLBACK_MAX_FILES
        ));
        assert!(!hover_fallback_allowed(
            DRAG_HOVER_FALLBACK_MAX_BYTES + 1,
            1
        ));
        assert!(!hover_fallback_allowed(
            1024,
            DRAG_HOVER_FALLBACK_MAX_FILES + 1
        ));
    }

    #[test]
    fn drag_hdrop_policy_excludes_explorer_and_current_process_windows() {
        assert!(!should_offer_drag_hdrop_process(Some("explorer.exe"), false));
        assert!(!should_offer_drag_hdrop_process(Some("EXPLORER.EXE"), false));
        assert!(!should_offer_drag_hdrop_process(
            Some("msedgewebview2.exe"),
            true
        ));
        assert!(!should_offer_drag_hdrop_process(Some("pierdesk.exe"), true));
        assert!(should_offer_drag_hdrop_process(Some("Weixin.exe"), false));
        assert!(should_offer_drag_hdrop_process(Some("WeChat.exe"), false));
        assert!(should_offer_drag_hdrop_process(Some("codex.exe"), false));
        assert!(should_offer_drag_hdrop_process(None, false));
    }

    #[test]
    fn clipboard_hdrop_policy_registers_globally_and_excludes_explorer_and_shell_hosts() {
        assert!(should_offer_clipboard_hdrop_process(
            true,
            true,
            Some("explorer.exe"),
            false
        ));
        assert!(should_offer_clipboard_hdrop_process(true, false, None, true));
        assert!(!should_offer_clipboard_hdrop_process(
            false,
            false,
            None,
            false
        ));
        assert!(!should_offer_clipboard_hdrop_process(
            false,
            true,
            Some("explorer.exe"),
            false
        ));
        assert!(!should_offer_clipboard_hdrop_process(
            false,
            true,
            Some("TextInputHost.exe"),
            false
        ));
        assert!(!should_offer_clipboard_hdrop_process(
            false,
            true,
            Some("svchost.exe"),
            false
        ));
        assert!(!should_offer_clipboard_hdrop_process(
            false,
            true,
            Some("pierdesk.exe"),
            true
        ));
        assert!(should_offer_clipboard_hdrop_process(
            false,
            true,
            Some("Weixin.exe"),
            false
        ));
        assert!(should_offer_clipboard_hdrop_process(
            false,
            true,
            Some("WorkBuddy.exe"),
            false
        ));
        assert!(should_offer_clipboard_hdrop_process(
            false,
            true,
            Some("codex.exe"),
            false
        ));
    }

    #[test]
    fn sparse_hover_placeholder_exposes_large_file_size_with_zero_remote_reads_and_cleans_up() {
        let root = unique_temp_root("pierdesk-sparse-placeholder-test");
        let reads = Arc::new(AtomicUsize::new(0));
        let large_size = 203_389_919_u64;
        let source: Arc<dyn ReadAtSource> = Arc::new(CountingFailingSource {
            len: large_size,
            reads: Arc::clone(&reads),
        });
        let entry = VirtualFileEntry::file(
            VirtualFileDescriptor::basic(Arc::from("逐跳之间 v1.pdf"), large_size),
            Arc::new(move || Arc::clone(&source)),
        );
        let materializer = LocalFileMaterializer::new(
            root.join("drag-cache"),
            "drop-",
            "drop-large-pdf".to_owned(),
            vec![entry],
            "local_drop",
            None,
        );

        let hovered = materializer.hdrop_paths_for_drag(false).unwrap();
        assert_eq!(reads.load(Ordering::Acquire), 0);
        assert!(materializer.has_unhydrated_placeholder());
        assert!(!materializer.served());
        assert_eq!(fs::metadata(&hovered[0]).unwrap().len(), large_size);

        // Hovering again reuses the exact same placeholder path without remote reads.
        let hovered_again = materializer.hdrop_paths_for_drag(false).unwrap();
        assert!(Arc::ptr_eq(&hovered, &hovered_again));
        assert_eq!(reads.load(Ordering::Acquire), 0);

        // Cancelling or dropping elsewhere removes the unhydrated placeholder directory.
        materializer.cleanup_unhydrated();
        assert!(!materializer.has_unhydrated_placeholder());
        assert!(!hovered[0].exists());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn failed_hydration_deletes_sparse_placeholder_so_targets_never_receive_zeroes() {
        let root = unique_temp_root("pierdesk-sparse-hydrate-fail-test");
        let reads = Arc::new(AtomicUsize::new(0));
        let source: Arc<dyn ReadAtSource> = Arc::new(CountingFailingSource {
            len: 4096,
            reads: Arc::clone(&reads),
        });
        let entry = VirtualFileEntry::file(
            VirtualFileDescriptor::basic(Arc::from("broken.bin"), 4096),
            Arc::new(move || Arc::clone(&source)),
        );
        let materializer = LocalFileMaterializer::new(
            root.join("drag-cache"),
            "drop-",
            "drop-fail".to_owned(),
            vec![entry],
            "local_drop",
            None,
        );

        let hovered = materializer.hdrop_paths_for_drag(false).unwrap();
        assert!(hovered[0].exists());
        assert!(materializer.hydrate_for_drop().is_err());
        assert!(!materializer.served());
        assert!(!materializer.has_unhydrated_placeholder());
        assert!(!hovered[0].exists());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn clipboard_materializer_caches_and_rebuilds_if_cached_file_is_removed() {
        let root = unique_temp_root("pierdesk-clipboard-cache-test");
        let source: Arc<dyn ReadAtSource> = Arc::new(MemorySource::new(&b"image-bytes"[..]));
        let entry = VirtualFileEntry::file(
            VirtualFileDescriptor::basic(Arc::from("photo.png"), 11),
            Arc::new(move || Arc::clone(&source)),
        );
        let materializer = LocalFileMaterializer::new(
            root.join("clipboard-cache"),
            "paste-",
            "paste-test".to_owned(),
            vec![entry],
            "clipboard_hdrop",
            None,
        );

        let first = materializer.hdrop_paths_for_clipboard().unwrap();
        let second = materializer.hdrop_paths_for_clipboard().unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(fs::read(&first[0]).unwrap(), b"image-bytes");

        // Simulate a target moving or deleting the cached file: a subsequent paste rematerializes.
        fs::remove_file(&first[0]).unwrap();
        let third = materializer.hdrop_paths_for_clipboard().unwrap();
        assert!(!Arc::ptr_eq(&first, &third));
        assert_eq!(fs::read(&third[0]).unwrap(), b"image-bytes");

        fs::remove_dir_all(root).unwrap();
    }
}
