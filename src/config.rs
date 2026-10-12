use crate::animations::AnimationsConfig;
use crate::colors::ColorBrushConfig;
use crate::effects::EffectsConfig;
use crate::komorebi::KomorebiColorsConfig;
use crate::render_backend::RenderBackendConfig;
use crate::utils::{OwnedHANDLE, get_adjusted_radius, get_window_corner_preference};
use crate::{APP_STATE, BG_SERVICES, DirectXDevices, IS_WINDOWS_11, display_error_box};
use anyhow::{Context, anyhow};
use dirs::home_dir;
use serde::{Deserialize, Serialize};
use std::fs::{self, DirBuilder};
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::{env, iter, ptr, slice, thread, time};
use windows::Win32::Foundation::{ERROR_NOT_FOUND, HANDLE, HWND};
use windows::Win32::Graphics::Dwm::{
    DWMWCP_DEFAULT, DWMWCP_DONOTROUND, DWMWCP_ROUND, DWMWCP_ROUNDSMALL,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_LIST_DIRECTORY, FILE_NOTIFY_CHANGE_LAST_WRITE,
    FILE_NOTIFY_INFORMATION, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    ReadDirectoryChangesW,
};
use windows::Win32::System::IO::CancelIoEx;
use windows::core::PCWSTR;

const DEFAULT_CONFIG: &str = include_str!("resources/config.yaml");
static CONFIG_RELOAD_PENDING: AtomicBool = AtomicBool::new(false);

/// The config.yaml definition
#[derive(Debug, Default, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub watch_config_changes: bool,
    #[serde(default = "serde_default_bool::<true>")]
    pub enable_logging: bool,
    #[serde(default = "serde_default_bool::<true>")]
    pub enable_ipc_server: bool,
    #[serde(default)]
    #[serde(alias = "rendering_backend")]
    pub render_backend: RenderBackendConfig,
    #[serde(default = "serde_default_global")]
    pub global: Global,
    #[serde(default)]
    pub window_rules: Vec<WindowRule>,
}

// Show borders even if the config.yaml is completely empty
// NOTE: This is intentionally kept separate from the Default trait because I want the
// width/offset zeroed out when config deserialization fails and falls back to Config::default()
fn serde_default_global() -> Global {
    Global {
        border_width: WidthConfig::serde_default(),
        border_offset: OffsetConfig::serde_default(),
        ..Default::default()
    }
}

#[derive(Debug, Default, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Global {
    #[serde(default = "WidthConfig::serde_default")]
    pub border_width: WidthConfig,
    #[serde(default = "OffsetConfig::serde_default")]
    pub border_offset: OffsetConfig,
    #[serde(default)]
    pub border_radius: RadiusConfig,
    #[serde(default)]
    pub border_z_order: ZOrderMode,
    #[serde(default = "serde_default_bool::<true>")]
    pub follow_native_border: bool,
    #[serde(default)]
    pub active_color: ColorBrushConfig,
    #[serde(default)]
    pub inactive_color: ColorBrushConfig,
    #[serde(default)]
    pub komorebi_colors: KomorebiColorsConfig,
    #[serde(default)]
    pub animations: AnimationsConfig,
    #[serde(default)]
    pub effects: EffectsConfig,
    #[serde(alias = "init_delay")]
    #[serde(default = "serde_default_u64::<250>")]
    pub initialize_delay: u64, // Adjust delay when creating new windows/borders
    #[serde(alias = "restore_delay")]
    #[serde(default = "serde_default_u64::<200>")]
    pub unminimize_delay: u64, // Adjust delay when restoring minimized windows
}

pub fn serde_default_u64<const V: u64>() -> u64 {
    V
}

pub fn serde_default_i32<const V: i32>() -> i32 {
    V
}

// f32 cannot be a const, so we have to do the following instead
pub fn serde_default_f32<const V: i32>() -> f32 {
    V as f32
}

pub fn serde_default_bool<const V: bool>() -> bool {
    V
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct WindowRule {
    #[serde(rename = "match")]
    pub kind: Option<MatchKind>,
    pub name: Option<String>,
    pub strategy: Option<MatchStrategy>,
    pub border_width: Option<WidthConfig>,
    pub border_offset: Option<OffsetConfig>,
    pub border_radius: Option<RadiusConfig>,
    pub border_z_order: Option<ZOrderMode>,
    pub follow_native_border: Option<bool>,
    pub active_color: Option<ColorBrushConfig>,
    pub inactive_color: Option<ColorBrushConfig>,
    pub komorebi_colors: Option<KomorebiColorsConfig>,
    pub animations: Option<AnimationsConfig>,
    pub effects: Option<EffectsConfig>,
    #[serde(alias = "init_delay")]
    pub initialize_delay: Option<u64>,
    #[serde(alias = "restore_delay")]
    pub unminimize_delay: Option<u64>,
    pub enabled: Option<EnableMode>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub enum MatchKind {
    Title,
    Class,
    Process,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub enum MatchStrategy {
    Equals,
    Contains,
    Regex,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(transparent)]
pub struct WidthConfig(f32);

impl WidthConfig {
    pub fn new(width: f32) -> Self {
        Self(width)
    }

    // TODO: Maybe rename this and other to_x methods to to_raw or smth idk
    /// Returns a DPI-adjusted raw width value
    pub fn to_width(&self, dpi: f32) -> i32 {
        (self.0 * dpi / 96.0).round() as i32
    }

    fn serde_default() -> Self {
        Self(4.0)
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum OffsetConfig {
    Uniform(i32),
    PerSide(Offset),
}

impl OffsetConfig {
    pub fn new(offset: i32) -> Self {
        Self::Uniform(offset)
    }

    /// Returns a DPI-adjusted offset for each side
    pub fn to_offset(&self, dpi: f32) -> Offset {
        let scale = |value: i32| (value as f32 * dpi / 96.0).round() as i32;
        match *self {
            Self::Uniform(offset) => Offset::new(scale(offset)),
            Self::PerSide(offset) => Offset {
                top: scale(offset.top),
                left: scale(offset.left),
                right: scale(offset.right),
                bottom: scale(offset.bottom),
            },
        }
    }

    fn serde_default() -> Self {
        Self::Uniform(-1)
    }
}

impl Default for OffsetConfig {
    fn default() -> Self {
        Self::Uniform(0)
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Offset {
    #[serde(default)]
    pub top: i32,
    #[serde(default)]
    pub left: i32,
    #[serde(default)]
    pub right: i32,
    #[serde(default)]
    pub bottom: i32,
}

impl Offset {
    pub fn new(offset: i32) -> Self {
        Self {
            top: offset,
            left: offset,
            right: offset,
            bottom: offset,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq)]
pub enum RadiusConfig {
    #[default]
    Auto,
    Square,
    Round,
    RoundSmall,
    #[serde(untagged)]
    Custom(f32),
}

impl RadiusConfig {
    pub fn to_radius(&self, border_width: i32, dpi: u32, tracking_window: HWND) -> f32 {
        match self {
            // We also check Custom(-1.0) for legacy reasons (don't wanna break anyone's old config)
            RadiusConfig::Auto | RadiusConfig::Custom(-1.0) => {
                // I believe this will error on Windows 10, so we'll just use a default
                match get_window_corner_preference(tracking_window).unwrap_or(DWMWCP_DEFAULT) {
                    DWMWCP_DEFAULT => {
                        if *IS_WINDOWS_11 {
                            get_adjusted_radius(8.0, dpi, border_width)
                        } else {
                            0.0
                        }
                    }
                    DWMWCP_DONOTROUND => 0.0,
                    DWMWCP_ROUND => get_adjusted_radius(8.0, dpi, border_width),
                    DWMWCP_ROUNDSMALL => get_adjusted_radius(4.0, dpi, border_width),
                    _ => 0.0,
                }
            }
            RadiusConfig::Square => 0.0,
            RadiusConfig::Round => get_adjusted_radius(8.0, dpi, border_width),
            RadiusConfig::RoundSmall => get_adjusted_radius(4.0, dpi, border_width),
            RadiusConfig::Custom(radius) => get_adjusted_radius(*radius, dpi, border_width),
        }
    }
}
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq)]
pub enum EnableMode {
    #[default]
    Auto,
    #[serde(untagged)]
    Bool(bool),
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq)]
pub enum ZOrderMode {
    #[default]
    AboveWindow,
    BelowWindow,
}

impl Config {
    pub fn create() -> anyhow::Result<Self> {
        let config_dir = Self::get_dir()?;
        let config_path = config_dir.join("config.yaml");

        // When saving files with a text editor like VSCode or Neovim, there may be a small time
        // period where the target file is empty or doesn't exist, so we'll use some retries.
        let mut exists = fs::exists(&config_path).context("could not check if config exists")?;
        for _ in 0..2 {
            if !exists {
                debug!("config does not exist; attempting to check again");
                thread::sleep(time::Duration::from_millis(20));
                exists = fs::exists(&config_path).context("could not check if config exists")?;
            } else {
                break;
            }
        }

        // If the config.yaml does not exist, try to create it
        if !exists {
            let default_contents = DEFAULT_CONFIG.as_bytes();
            fs::write(&config_path, default_contents)
                .context("could not create default config.yaml")?;

            info!("generating default config in {}", config_dir.display());
        }

        // We also implement retries here for the same reasons listed earlier
        let mut contents = fs::read_to_string(&config_path).context("could not read config")?;
        for _ in 0..2 {
            if contents.is_empty() {
                debug!("config is empty; attempting to read again");
                thread::sleep(time::Duration::from_millis(20));
                contents = fs::read_to_string(&config_path).context("could not read config")?;
            } else {
                break;
            }
        }

        // Deserialize the config.yaml file
        serde_yaml_ng::from_str(&contents).map_err(anyhow::Error::new)
    }

    pub fn get_dir() -> anyhow::Result<PathBuf> {
        let config_dir = env::var("TACKY_BORDERS_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|env_err| {
                home_dir()
                    .map(|dir| dir.join(".config").join("tacky-borders"))
                    .ok_or(anyhow!("could not find home dir, and could not access TACKY_BORDERS_CONFIG_HOME env var: {env_err}"))
            })?;

        if !config_dir.exists() {
            DirBuilder::new()
                .recursive(true)
                .create(&config_dir)
                .context("could not create config directory")?;
        };

        Ok(config_dir)
    }

    pub fn reload() -> anyhow::Result<()> {
        let new_config = match Self::create() {
            Ok(config) => {
                BG_SERVICES.lock().unwrap().reload(&config);

                let mut directx_devices_opt = APP_STATE.directx_devices.write().unwrap();

                if config.render_backend == RenderBackendConfig::V2 && directx_devices_opt.is_none()
                {
                    let direct_x_devices = DirectXDevices::new(&APP_STATE.render_factory)
                        .unwrap_or_else(|err| {
                            error!("could not create directx devices: {err:#}");
                            panic!("could not create directx devices: {err:#}");
                        });

                    *directx_devices_opt = Some(direct_x_devices);
                } else if config.render_backend == RenderBackendConfig::Legacy
                    && directx_devices_opt.is_some()
                {
                    *directx_devices_opt = None;
                }

                config
            }
            Err(err) => {
                error!("could not reload config: {err:#}");
                thread::spawn(move || {
                    display_error_box(format!("could not reload config: {err:#}"), None);
                });

                Config::default()
            }
        };
        let mut sequence = APP_STATE
            .appearance
            .lock()
            .map_err(|_| anyhow!("appearance gate poisoned"))?;
        let revision = sequence.config_revision.checked_add(1).ok_or_else(|| {
            APP_STATE
                .border_registry
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .quarantine();
            anyhow!("configuration revisions exhausted")
        })?;
        let command = sequence.next_command().ok_or_else(|| {
            APP_STATE
                .border_registry
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .quarantine();
            anyhow!("appearance commands exhausted")
        })?;
        let mut config = APP_STATE
            .config
            .write()
            .map_err(|_| anyhow!("configuration poisoned"))?;
        *config = new_config;
        sequence.config_revision = revision;
        sequence.global_stamps = crate::border_updates::AppearanceStamps {
            active: command,
            inactive: command,
            width: command,
            offset: command,
            radius: command,
        };
        Ok(())
    }

    pub fn is_config_watcher_enabled(&self) -> bool {
        self.watch_config_changes
    }

    pub fn is_komorebi_integration_enabled(&self) -> bool {
        self.global.komorebi_colors.enabled
            || self.window_rules.iter().any(|rule| {
                rule.komorebi_colors
                    .as_ref()
                    .map(|komocolors| komocolors.enabled)
                    .unwrap_or(false)
            })
    }

    pub fn is_theme_aware_enabled(&self) -> bool {
        Self::is_color_theme_aware(&self.global.active_color)
            || Self::is_color_theme_aware(&self.global.inactive_color)
            || self.window_rules.iter().any(|rule| {
                rule.active_color
                    .as_ref()
                    .is_some_and(Self::is_color_theme_aware)
                    || rule
                        .inactive_color
                        .as_ref()
                        .is_some_and(Self::is_color_theme_aware)
            })
    }

    fn is_color_theme_aware(config: &ColorBrushConfig) -> bool {
        matches!(config, ColorBrushConfig::ThemeAware(_))
    }

    pub fn is_ipc_server_enabled(&self) -> bool {
        self.enable_ipc_server
    }
}

#[derive(Debug)]
pub struct ConfigWatcher {
    dir_handle: OwnedHANDLE,
    stop_requested: Arc<AtomicBool>,
    thread_handle: Option<JoinHandle<()>>,
}

impl ConfigWatcher {
    pub fn new(
        config_path: PathBuf,
        debounce_time: u64,
        callback_fn: fn(),
    ) -> anyhow::Result<Self> {
        let config_dir = config_path
            .parent()
            .context("could not get parent dir for config watcher")?;
        let config_dir_vec: Vec<u16> = config_dir
            .as_os_str()
            .encode_wide()
            .chain(iter::once(0))
            .collect();

        let dir_handle = {
            let handle = unsafe {
                CreateFileW(
                    PCWSTR(config_dir_vec.as_ptr()),
                    FILE_LIST_DIRECTORY.0,
                    FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                    None,
                    OPEN_EXISTING,
                    FILE_FLAG_BACKUP_SEMANTICS,
                    None,
                )
            }
            .context("could not create dir handle for config watcher")?;
            OwnedHANDLE(handle)
        };

        // Convert HANDLE to isize so we can move it into the new thread
        let dir_handle_isize = dir_handle.0.0 as isize;

        // Also initialize these variables so we move them into the new thread
        let config_name = config_path
            .file_name()
            .context("could not get config name for config watcher")?
            .to_owned()
            .into_string()
            .map_err(|_| anyhow!("could not convert config name for config watcher"))?;

        let stop_requested = Arc::new(AtomicBool::new(false));
        let worker_stop_requested = stop_requested.clone();
        let thread_handle = thread::spawn(move || unsafe {
            debug!("entering config watcher thread");

            // Reconvert isize back to HANDLE
            let dir_handle = HANDLE(dir_handle_isize as _);

            let mut buffer = [0u8; 1024];
            let mut bytes_returned = 0u32;

            loop {
                if worker_stop_requested.load(Ordering::Acquire) {
                    break;
                }

                if let Err(e) = ReadDirectoryChangesW(
                    dir_handle,
                    buffer.as_mut_ptr() as _,
                    buffer.len() as u32,
                    false,
                    FILE_NOTIFY_CHANGE_LAST_WRITE,
                    Some(ptr::addr_of_mut!(bytes_returned)),
                    None,
                    None,
                ) {
                    if !worker_stop_requested.load(Ordering::Acquire) {
                        error!("could not check for changes in config dir: {e}");
                    }
                    break;
                }

                if worker_stop_requested.load(Ordering::Acquire) {
                    break;
                }

                Self::process_dir_change_notifs(&buffer, bytes_returned, &config_name, callback_fn);

                if worker_stop_requested.load(Ordering::Acquire) {
                    break;
                }

                // Prevent too many directory checks in quick succession
                // NOTE: if any dir changes are made while the thread is asleep, the OS will hold
                // the operations in queue, so we can immediately check them again after looping
                thread::park_timeout(time::Duration::from_millis(debounce_time));
            }

            debug!("exiting config watcher thread");
        });

        Ok(Self {
            dir_handle,
            stop_requested,
            thread_handle: Some(thread_handle),
        })
    }

    pub fn process_dir_change_notifs(
        buffer: &[u8; 1024],
        bytes_returned: u32,
        config_name: &str,
        callback_fn: fn(),
    ) {
        let mut offset = 0usize;

        while offset < bytes_returned as usize {
            let info = unsafe { &*(buffer.as_ptr().add(offset) as *const FILE_NOTIFY_INFORMATION) };

            // We divide FileNameLength by 2 because it's in bytes (u8), but FileName is in u16
            let name_slice = unsafe {
                slice::from_raw_parts(info.FileName.as_ptr(), info.FileNameLength as usize / 2)
            };
            let file_name = String::from_utf16_lossy(name_slice);
            debug!("file changed: {file_name}");

            if file_name == *config_name {
                callback_fn();
                break; // Prevent multiple callbacks from the same notification
            }

            // If NextEntryOffset = 0, then we have reached the end of the notification
            if info.NextEntryOffset == 0 {
                break;
            } else {
                offset += info.NextEntryOffset as usize
            }
        }
    }
}

impl Drop for ConfigWatcher {
    fn drop(&mut self) {
        self.stop_requested.store(true, Ordering::Release);

        let Some(handle) = self.thread_handle.take() else {
            error!("could not take config watcher thread handle");
            return;
        };
        handle.thread().unpark();

        let cancellation_failed = match unsafe { CancelIoEx(self.dir_handle.0, None) } {
            Ok(()) => false,
            Err(err) if is_operation_not_found(&err) => false,
            Err(err) => {
                error!(
                    "could not cancel i/o operations on {:?} for config watcher: {err:#}",
                    self.dir_handle.0
                );
                true
            }
        };

        join_or_reap_config_watcher(handle, cancellation_failed);
    }
}

fn is_operation_not_found(error: &windows::core::Error) -> bool {
    error.code() == ERROR_NOT_FOUND.to_hresult()
}

fn join_or_reap_config_watcher(handle: JoinHandle<()>, cancellation_failed: bool) {
    if cancellation_failed || handle.thread().id() == thread::current().id() {
        if let Err(err) = thread::Builder::new()
            .name("config-watcher-reaper".into())
            .spawn(move || log_config_watcher_join(handle.join()))
        {
            error!("could not spawn config watcher reaper: {err}");
        }
    } else {
        log_config_watcher_join(handle.join());
    }
}

fn log_config_watcher_join(result: thread::Result<()>) {
    if let Err(payload) = result {
        crate::border_registry::discard_panic(payload);
        error!("config watcher thread panicked");
    }
}

pub fn config_watcher_callback() {
    crate::utils::LogIfErr::log_if_err(&request_config_reload());
}

fn request_config_reload() -> anyhow::Result<()> {
    request_config_reload_with(|| crate::submit_host_action(crate::HostAction::ReloadConfig))
}

fn request_config_reload_with(submit: impl FnOnce() -> anyhow::Result<()>) -> anyhow::Result<()> {
    if CONFIG_RELOAD_PENDING.swap(true, Ordering::AcqRel) {
        return Ok(());
    }

    if let Err(error) = submit() {
        CONFIG_RELOAD_PENDING.store(false, Ordering::Release);
        return Err(error);
    }

    Ok(())
}

pub fn begin_config_reload() {
    CONFIG_RELOAD_PENDING.store(false, Ordering::Release);
}

#[cfg(test)]
mod config_watcher_tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;

    static CONFIG_RELOAD_TEST: Mutex<()> = Mutex::new(());
    static CONFIG_WATCHER_TEST: Mutex<()> = Mutex::new(());
    static CALLBACK_COUNT: AtomicUsize = AtomicUsize::new(0);
    static CALLBACK_STARTED: AtomicBool = AtomicBool::new(false);
    static CALLBACK_RELEASED: AtomicBool = AtomicBool::new(false);

    fn unused_callback() {}

    fn counting_callback() {
        CALLBACK_COUNT.fetch_add(1, Ordering::Release);
    }

    fn blocking_callback() {
        CALLBACK_STARTED.store(true, Ordering::Release);
        while !CALLBACK_RELEASED.load(Ordering::Acquire) {
            thread::park_timeout(time::Duration::from_millis(5));
        }
    }

    fn wait_until(mut condition: impl FnMut() -> bool) {
        let deadline = time::Instant::now() + time::Duration::from_secs(2);
        while !condition() && time::Instant::now() < deadline {
            thread::sleep(time::Duration::from_millis(5));
        }
        assert!(condition());
    }

    fn test_directory(name: &str) -> PathBuf {
        env::temp_dir().join(format!(
            "tacky-borders-config-watcher-{name}-{}-{:?}",
            std::process::id(),
            thread::current().id()
        ))
    }

    #[test]
    fn blocked_watcher_drop_cancels_and_joins_promptly() {
        let _guard = CONFIG_WATCHER_TEST.lock().unwrap();
        let directory = test_directory("blocked");
        fs::create_dir_all(&directory).unwrap();
        let watcher =
            ConfigWatcher::new(directory.join("config.yaml"), 60_000, unused_callback).unwrap();

        let started = time::Instant::now();
        drop(watcher);

        assert!(started.elapsed() < time::Duration::from_secs(2));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn debounce_wait_is_unparked_during_drop() {
        let _guard = CONFIG_WATCHER_TEST.lock().unwrap();
        CALLBACK_COUNT.store(0, Ordering::Release);
        let directory = test_directory("debounce");
        fs::create_dir_all(&directory).unwrap();
        let config_path = directory.join("config.yaml");
        let watcher = ConfigWatcher::new(config_path.clone(), 60_000, counting_callback).unwrap();

        fs::write(config_path, "watch_config_changes: true\n").unwrap();
        wait_until(|| CALLBACK_COUNT.load(Ordering::Acquire) == 1);

        let started = time::Instant::now();
        drop(watcher);

        assert!(started.elapsed() < time::Duration::from_secs(2));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn callback_active_drop_waits_for_callback_then_joins() {
        let _guard = CONFIG_WATCHER_TEST.lock().unwrap();
        CALLBACK_STARTED.store(false, Ordering::Release);
        CALLBACK_RELEASED.store(false, Ordering::Release);
        let directory = test_directory("callback");
        fs::create_dir_all(&directory).unwrap();
        let config_path = directory.join("config.yaml");
        let watcher = ConfigWatcher::new(config_path.clone(), 60_000, blocking_callback).unwrap();

        fs::write(config_path, "watch_config_changes: true\n").unwrap();
        wait_until(|| CALLBACK_STARTED.load(Ordering::Acquire));

        let release_thread = thread::spawn(|| {
            thread::sleep(time::Duration::from_millis(50));
            CALLBACK_RELEASED.store(true, Ordering::Release);
        });

        let started = time::Instant::now();
        drop(watcher);
        let elapsed = started.elapsed();
        release_thread.join().unwrap();

        assert!(elapsed >= time::Duration::from_millis(40));
        assert!(elapsed < time::Duration::from_secs(2));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn operation_not_found_is_expected_cancellation_race() {
        let expected = windows::core::Error::from_hresult(ERROR_NOT_FOUND.to_hresult());
        let unexpected = windows::core::Error::from_hresult(
            windows::Win32::Foundation::ERROR_ACCESS_DENIED.to_hresult(),
        );

        assert!(is_operation_not_found(&expected));
        assert!(!is_operation_not_found(&unexpected));
    }

    #[test]
    fn current_thread_join_is_deferred_to_reaper() {
        let (handle_sender, handle_receiver) = std::sync::mpsc::sync_channel(1);
        let returned = Arc::new(AtomicBool::new(false));
        let worker_returned = returned.clone();
        let worker = thread::spawn(move || {
            let own_handle = handle_receiver.recv().unwrap();
            join_or_reap_config_watcher(own_handle, false);
            worker_returned.store(true, Ordering::Release);
        });

        handle_sender.send(worker).unwrap();
        wait_until(|| returned.load(Ordering::Acquire));
    }

    #[test]
    fn cancellation_failure_defers_join_without_blocking_owner() {
        let release = Arc::new(AtomicBool::new(false));
        let exited = Arc::new(AtomicBool::new(false));
        let worker_release = release.clone();
        let worker_exited = exited.clone();
        let worker = thread::spawn(move || {
            while !worker_release.load(Ordering::Acquire) {
                thread::park_timeout(time::Duration::from_millis(5));
            }
            worker_exited.store(true, Ordering::Release);
        });

        let started = time::Instant::now();
        join_or_reap_config_watcher(worker, true);
        assert!(started.elapsed() < time::Duration::from_secs(1));

        release.store(true, Ordering::Release);
        wait_until(|| exited.load(Ordering::Acquire));
    }

    #[test]
    fn worker_panic_is_consumed_by_join_logging() {
        let worker = thread::spawn(|| panic!("config watcher test panic"));

        join_or_reap_config_watcher(worker, false);
    }

    #[test]
    fn config_reload_requests_coalesce_until_consumed() {
        let _guard = CONFIG_RELOAD_TEST.lock().unwrap();
        begin_config_reload();
        let submissions = AtomicUsize::new(0);

        request_config_reload_with(|| {
            submissions.fetch_add(1, Ordering::Relaxed);
            Ok(())
        })
        .unwrap();
        request_config_reload_with(|| {
            submissions.fetch_add(1, Ordering::Relaxed);
            Ok(())
        })
        .unwrap();
        assert_eq!(submissions.load(Ordering::Relaxed), 1);

        begin_config_reload();
        request_config_reload_with(|| {
            submissions.fetch_add(1, Ordering::Relaxed);
            Ok(())
        })
        .unwrap();
        assert_eq!(submissions.load(Ordering::Relaxed), 2);
        begin_config_reload();
    }

    #[test]
    fn rejected_config_reload_request_can_retry() {
        let _guard = CONFIG_RELOAD_TEST.lock().unwrap();
        begin_config_reload();

        assert!(request_config_reload_with(|| Err(anyhow!("rejected"))).is_err());
        let mut submitted = false;
        request_config_reload_with(|| {
            submitted = true;
            Ok(())
        })
        .unwrap();

        assert!(submitted);
        begin_config_reload();
    }
}
