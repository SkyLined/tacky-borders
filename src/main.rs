#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

#[macro_use]
extern crate log;
extern crate sp_log;

use anyhow::{Context, anyhow};
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Write};
use std::process::ExitCode;
use std::sync::LazyLock;
use tacky_borders::border_coordinator::{
    LifecycleReceipt, MAX_COORDINATOR_BATCH, OperationOutcome,
};
use tacky_borders::colors::ColorBrushConfig;
use tacky_borders::config::{OffsetConfig, RadiusConfig, WidthConfig};
use tacky_borders::iocp::UnixStream;
use tacky_borders::ipc::{IpcCommand, socket_path};
use tacky_borders::sys_tray_icon::create_tray_icon;
use tacky_borders::utils::{LogIfErr, imm_disable_ime, set_process_dpi_awareness_context};
use tacky_borders::{
    APP_STATE, BG_SERVICES, HostAction, attach_parent_console, initialize_host_requests,
    is_unwanted_instance, register_border_window_class, reload_borders, set_event_hook,
};
use windows::Win32::Foundation::WAIT_FAILED;
use windows::Win32::UI::Accessibility::UnhookWinEvent;
use windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2;
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, MSG, MWMO_INPUTAVAILABLE, MsgWaitForMultipleObjectsEx, PM_REMOVE,
    PeekMessageW, QS_ALLINPUT, TranslateMessage, WM_QUIT,
};

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();

    // When invoked with arguments, act as an IPC client for a running instance
    // instead of starting as the border daemon itself.
    if !args.is_empty() {
        let args = pico_args::Arguments::from_vec(args);
        return match run_cli(args) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                // eprintln because the logger isn't initialized in client mode
                eprintln!("error: {err:#}");
                ExitCode::FAILURE
            }
        };
    }

    run_daemon();
    ExitCode::SUCCESS
}

fn run_daemon() {
    if is_unwanted_instance() {
        return;
    }

    // Force initialization of our app state
    let _ = LazyLock::force(&APP_STATE);

    info!("starting tacky-borders");

    // xFFFFFFFF (-1) is used to disable IME windows for all threads in the current process.
    imm_disable_ime(0xFFFFFFFF)
        .ok()
        .context("could not disable ime")
        .log_if_err();

    set_process_dpi_awareness_context(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)
        .context("could not make process dpi aware")
        .log_if_err();

    if let Err(error) = register_border_window_class() {
        error!("could not register border class: {error:#}");
        return;
    }
    let requests = match initialize_host_requests() {
        Ok(requests) => requests,
        Err(error) => {
            error!("could not initialize host queue: {error:#}");
            return;
        }
    };
    let hook_work = match tacky_borders::event_hook::initialize_hook_pump() {
        Ok(event) => event,
        Err(error) => {
            error!("could not initialize hook pump: {error:#}");
            return;
        }
    };
    let coordinator = match APP_STATE.start_lifecycle() {
        Ok(coordinator) => coordinator,
        Err(error) => {
            error!("could not start border coordinator: {error:#}");
            return;
        }
    };
    let hwineventhook = set_event_hook();
    let mut shutting_down = hwineventhook.0.is_null();
    let services_ready = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        LazyLock::force(&BG_SERVICES)
    })) {
        Ok(_) => true,
        Err(payload) => {
            tacky_borders::border_registry::discard_panic(payload);
            error!("background service startup panicked; shutting down");
            shutting_down = true;
            false
        }
    };

    // This owns the tray icon window, so it must be kept in scope
    let tray_icon_res = create_tray_icon(hwineventhook);
    if let Err(ref err) = tray_icon_res {
        error!("could not create tray icon: {err:#}");
        shutting_down = true;
    }

    let mut receipts: Vec<(LifecycleReceipt, bool)> = Vec::with_capacity(MAX_COORDINATOR_BATCH);
    if !shutting_down {
        match reload_borders() {
            Ok(receipt) => receipts.push((receipt, true)),
            Err(error) => {
                error!("initial baseline submission failed: {error:#}");
                shutting_down = true;
            }
        }
    }
    run_host_loop(
        &coordinator,
        &requests,
        hook_work,
        hwineventhook,
        shutting_down,
        receipts,
        None,
    );
    if services_ready {
        BG_SERVICES
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .shutdown();
    }
    info!("exiting tacky-borders");
}

fn run_host_loop(
    coordinator: &std::sync::Arc<tacky_borders::border_coordinator::BorderLifecycleCoordinator>,
    requests: &tacky_borders::HostRequests,
    hook_work: windows::Win32::Foundation::HANDLE,
    mut hwineventhook: windows::Win32::UI::Accessibility::HWINEVENTHOOK,
    mut shutting_down: bool,
    mut receipts: Vec<(LifecycleReceipt, bool)>,
    mut config_reload: Option<std::thread::JoinHandle<anyhow::Result<()>>>,
) {
    let mut stop_submitted = false;
    let mut wait_failed = false;
    let mut reconciler = tacky_borders::utils::WindowReconciler::default();
    #[cfg(test)]
    let silent_completion = tacky_borders::border_registry::BorderEvent::new(true).unwrap();
    loop {
        let turn = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            tacky_borders::utils::flush_border_work();
            let hook_pending = !shutting_down
                && tacky_borders::event_hook::pump_hook_events(MAX_COORDINATOR_BATCH);
            let reconciliation_pending =
                !shutting_down && reconciler.tick(std::time::Instant::now());
            let mut message = MSG::default();
            let mut message_count = 0;
            for _ in 0..MAX_COORDINATOR_BATCH {
                if !unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
                    break;
                }
                message_count += 1;
                if message.message == WM_QUIT {
                    shutting_down = true;
                    continue;
                }
                unsafe {
                    let _ = TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
            }
            if shutting_down && !stop_submitted {
                if !hwineventhook.0.is_null() {
                    unsafe { UnhookWinEvent(hwineventhook) }
                        .ok()
                        .context("unhook win event")
                        .log_if_err();
                    hwineventhook = Default::default();
                }
                coordinator.request_shutdown();
                stop_submitted = true;
            }
            if config_reload
                .as_ref()
                .is_some_and(std::thread::JoinHandle::is_finished)
                && let Some(worker) = config_reload.take()
            {
                match worker.join() {
                    Ok(Ok(())) if !shutting_down => {
                        if receipts.len() < MAX_COORDINATOR_BATCH {
                            match reload_borders() {
                                Ok(receipt) => receipts.push((receipt, false)),
                                Err(error) => {
                                    error!("config reload border admission rejected: {error:#}")
                                }
                            }
                        } else {
                            error!("config reload border admission storage full");
                        }
                    }
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => error!("config reload failed: {error:#}"),
                    Err(payload) => {
                        tacky_borders::border_registry::discard_panic(payload);
                        error!("config reload worker panicked");
                    }
                }
            }
            if !shutting_down {
                for _ in 0..MAX_COORDINATOR_BATCH {
                    let Ok(action) = requests.receiver.try_recv() else {
                        break;
                    };
                    if receipts.len() == MAX_COORDINATOR_BATCH {
                        error!("host operation storage full");
                        break;
                    }
                    let submitted = match action {
                        HostAction::ReloadConfig => {
                            if config_reload.is_some() {
                                error!("config reload already in progress");
                            } else {
                                match std::thread::Builder::new()
                                    .name("host-config-reload".into())
                                    .spawn(tacky_borders::config::Config::reload)
                                {
                                    Ok(worker) => config_reload = Some(worker),
                                    Err(error) => {
                                        error!("could not spawn config reload worker: {error}")
                                    }
                                }
                            }
                            continue;
                        }
                        HostAction::ReloadBorders => reload_borders(),
                    };
                    match submitted {
                        Ok(receipt) => receipts.push((receipt, false)),
                        Err(error) => error!("host operation rejected: {error:#}"),
                    }
                }
            }
            for index in (0..receipts.len()).rev() {
                match receipts[index].0.try_result() {
                    Ok(None) => {}
                    Ok(Some(Ok(OperationOutcome::Reloaded { .. }))) => {
                        receipts.swap_remove(index);
                    }
                    Ok(Some(result)) => {
                        error!("host lifecycle result: {result:?}");
                        if receipts[index].1 {
                            shutting_down = true;
                        }
                        receipts.swap_remove(index);
                    }
                    Err(error) => {
                        error!("host lifecycle receipt disconnected: {error}");
                        if receipts[index].1 {
                            shutting_down = true;
                        }
                        receipts.swap_remove(index);
                    }
                }
            }
            if coordinator.is_terminal() {
                let result = coordinator.join_terminal();
                if coordinator.has_joined() && config_reload.is_none() {
                    result.log_if_err();
                    return true;
                }
            }
            if wait_failed {
                std::thread::park_timeout(std::time::Duration::from_millis(100));
            } else {
                let completion = coordinator.completion_event();
                #[cfg(test)]
                let completion = if host_tests::completion_is_suppressed() {
                    silent_completion.handle()
                } else {
                    completion
                };
                let result = unsafe {
                    MsgWaitForMultipleObjectsEx(
                        Some(&[hook_work, completion, requests.wake.handle()]),
                        if hook_pending
                            || reconciliation_pending
                            || message_count == MAX_COORDINATOR_BATCH
                        {
                            0
                        } else {
                            100
                        },
                        QS_ALLINPUT,
                        MWMO_INPUTAVAILABLE,
                    )
                };
                if result == WAIT_FAILED {
                    error!("native host wait failed");
                    shutting_down = true;
                    wait_failed = true;
                }
            }
            false
        }));
        match turn {
            Ok(true) => break,
            Ok(false) => {}
            Err(payload) => {
                tacky_borders::border_registry::discard_panic(payload);
                error!("host service turn panicked; retaining ownership through shutdown");
                shutting_down = true;
                coordinator.request_shutdown();
            }
        }
    }
    if !hwineventhook.0.is_null() {
        unsafe { UnhookWinEvent(hwineventhook) }.ok().log_if_err();
    }
}

const HELP_TEXT: &str = "\
tacky-borders - customizable borders for Windows 11 and 10

USAGE:
  tacky-borders.exe                start the border daemon
  tacky-borders.exe <command>      send a command to a running instance

COMMANDS:
  set-color [OPTIONS]              change border color
  set-width <width> [OPTIONS]      change border width
  set-offset <offset> [OPTIONS]    change border offset
  set-radius <radius> [OPTIONS]    change border radius
  reload                           reload config.yaml and recreate borders
  get-state                        print runtime state as json
  msg <json>                       send a raw json command
  help                             show this help

OPTIONS:
  -h, --help                show this help

OPTIONS for set-color:
  -a, --active   <color>    set the active (focused) border color
  -i, --inactive <color>    set the inactive (unfocused) border color
  <color> is a hex string like \"#RRGGBB\" or \"#RRGGBBAA\", \"accent\", or a JSON gradient object:
    '{\"colors\":[\"#ffffff\",\"#000000\"],\"direction\":\"90deg\"}'

OPTIONS for set-offset:
  <offset> is a number applied to all sides (e.g. -1), or a JSON object:
    '{\"top\":-1,\"left\":0,\"right\":0,\"bottom\":-1}'

OPTIONS for set-color, set-width, set-offset, set-radius:
  -f, --focused             only update the currently focused window's border;
                            all other borders are left unchanged
";

fn run_cli(mut args: pico_args::Arguments) -> anyhow::Result<()> {
    // Console is disabled by default in release mode so we need to attach it
    let _ = attach_parent_console();

    // Help flags have higher priority and should be handled separately
    if args.contains(["-h", "--help"]) {
        print!("{HELP_TEXT}");
        return Ok(());
    }

    let command_json = match args
        .subcommand()?
        .ok_or(anyhow!("missing subcommand"))?
        .as_str()
    {
        "set-color" | "set_color" => {
            let active = args.opt_value_from_fn(["-a", "--active"], parse_color_arg)?;
            let inactive = args.opt_value_from_fn(["-i", "--inactive"], parse_color_arg)?;

            if active.is_none() && inactive.is_none() {
                anyhow::bail!("set-color requires at least one of --active or --inactive");
            }

            let command = IpcCommand::SetColor {
                active,
                inactive,
                focused: args.contains(["-f", "--focused"]),
            };
            serde_json::to_string(&command)?
        }
        "set-width" | "set_width" => {
            let command = IpcCommand::SetWidth {
                width: args.free_from_fn(parse_width_arg)?,
                focused: args.contains(["-f", "--focused"]),
            };
            serde_json::to_string(&command)?
        }
        "set-offset" | "set_offset" => {
            let command = IpcCommand::SetOffset {
                offset: args.free_from_fn(parse_offset_arg)?,
                focused: args.contains(["-f", "--focused"]),
            };
            serde_json::to_string(&command)?
        }
        "set-radius" | "set_radius" => {
            let command = IpcCommand::SetRadius {
                radius: args.free_from_fn(parse_radius_arg)?,
                focused: args.contains(["-f", "--focused"]),
            };
            serde_json::to_string(&command)?
        }
        "reload" => serde_json::to_string(&IpcCommand::Reload)?,
        "get-state" | "get_state" => serde_json::to_string(&IpcCommand::GetState)?,
        "msg" => args.free_from_str()?,
        "help" => {
            print!("{HELP_TEXT}");
            return Ok(());
        }
        other => anyhow::bail!("unknown command '{other}'; see 'tacky-borders help'"),
    };

    let remaining = args.finish();
    if !remaining.is_empty() {
        anyhow::bail!("unknown arguments: {:?}", remaining);
    }

    let response = send_command(&command_json)?;
    let trimmed = response.trim_end();
    validate_response(trimmed)?;
    println!("{trimmed}");

    Ok(())
}

fn validate_response(response: &str) -> anyhow::Result<()> {
    let value: serde_json::Value =
        serde_json::from_str(response).context("could not parse ipc response")?;

    if value.get("ok").and_then(|v| v.as_bool()) == Some(true) {
        return Ok(());
    }

    let error = value
        .get("error")
        .and_then(|v| v.as_str())
        .unwrap_or("ipc command failed");

    anyhow::bail!("{error}")
}

fn send_command(command_json: &str) -> anyhow::Result<String> {
    let socket_path = socket_path().context("could not get socket path")?;

    let stream = UnixStream::connect(&socket_path).with_context(|| {
        format!(
            "could not connect to {}; is tacky-borders running with the ipc server enabled?",
            socket_path.display()
        )
    })?;

    let message = format!("{command_json}\n");
    (&stream)
        .write_all(message.as_bytes())
        .context("could not send command")?;

    let mut reader = BufReader::new(&stream);
    let mut response = String::new();
    reader
        .read_line(&mut response)
        .context("could not read response")?;

    Ok(response)
}

fn parse_color_arg(s: &str) -> anyhow::Result<ColorBrushConfig> {
    // Try parsing as JSON object first (e.g. gradients), but fallback to treating
    // it as a string (e.g. hex codes) which need to be wrapped in double quotes.
    let color = serde_json::from_str(s).or_else(|_| serde_json::from_str(&format!("\"{s}\"")))?;
    Ok(color)
}

fn parse_width_arg(s: &str) -> anyhow::Result<WidthConfig> {
    let width: f32 = s.parse()?;
    Ok(WidthConfig::new(width))
}

fn parse_offset_arg(s: &str) -> anyhow::Result<OffsetConfig> {
    serde_json::from_str(s).or_else(|_| {
        let offset: i32 = s.parse()?;
        Ok(OffsetConfig::new(offset))
    })
}

fn parse_radius_arg(s: &str) -> anyhow::Result<RadiusConfig> {
    let radius = serde_json::from_str(s).or_else(|_| serde_json::from_str(&format!("\"{s}\"")))?;
    Ok(radius)
}

#[cfg(test)]
mod host_tests {
    use super::*;

    static REQUESTS: std::sync::LazyLock<std::sync::Mutex<tacky_borders::HostRequests>> =
        std::sync::LazyLock::new(|| std::sync::Mutex::new(initialize_host_requests().unwrap()));

    #[test]
    fn wm_quit_joins_coordinator_while_configuration_remains_locked() {
        assert_host_shutdown(false);
    }

    #[test]
    fn startup_rollback_joins_coordinator_and_retained_job() {
        assert_host_shutdown(true);
    }

    thread_local! {
        static SUPPRESS_COMPLETION: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    pub(super) fn completion_is_suppressed() -> bool {
        SUPPRESS_COMPLETION.get()
    }

    #[test]
    fn wm_quit_joins_native_workers_without_completion_wakes() {
        struct RestoreCompletion;
        impl Drop for RestoreCompletion {
            fn drop(&mut self) {
                SUPPRESS_COMPLETION.set(false);
            }
        }
        SUPPRESS_COMPLETION.set(true);
        let _restore = RestoreCompletion;
        assert_host_shutdown(false);
    }

    fn assert_host_shutdown(shutting_down: bool) {
        use tacky_borders::border_coordinator::BorderLifecycleCoordinator;
        use tacky_borders::border_registry::BorderRegistry;
        let requests = REQUESTS.lock().unwrap();
        let hook_work = tacky_borders::event_hook::initialize_hook_pump().unwrap();
        let registry = std::sync::Arc::new(std::sync::Mutex::new(BorderRegistry::default()));
        tacky_borders::register_border_window_class().unwrap();
        let tracking = tacky_borders::utils::OwnedHWND(
            unsafe {
                windows::Win32::UI::WindowsAndMessaging::CreateWindowExW(
                    Default::default(),
                    windows::core::w!("STATIC"),
                    windows::core::w!("fix01-host-owned"),
                    windows::Win32::UI::WindowsAndMessaging::WS_POPUP,
                    -32000,
                    -32000,
                    100,
                    100,
                    None,
                    None,
                    None,
                    None,
                )
            }
            .unwrap(),
        );
        let instance = {
            use tacky_borders::border_registry::{
                BorderAdmission, BorderControl, ReserveResult, TrackingWindowFingerprint,
            };
            use tacky_borders::border_updates::BorderUpdateMailbox;
            let mut registry = registry.lock().unwrap();
            registry.admission = BorderAdmission::Open;
            let identity = registry
                .observe(TrackingWindowFingerprint::capture(tracking.0).unwrap())
                .unwrap();
            let updates = std::sync::Arc::new(
                BorderUpdateMailbox::new(std::sync::Arc::new(BorderControl::new().unwrap()))
                    .unwrap(),
            );
            match registry
                .reserve(identity, updates, std::time::Instant::now())
                .unwrap()
            {
                ReserveResult::Reserved(instance) => instance,
                _ => panic!("host fixture did not reserve a native worker"),
            }
        };
        let coordinator = BorderLifecycleCoordinator::start(registry.clone()).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let border = loop {
            use windows::Win32::UI::WindowsAndMessaging::{
                DispatchMessageW, MSG, PM_REMOVE, PeekMessageW,
            };
            let mut message = MSG::default();
            for _ in 0..64 {
                if !unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
                    break;
                }
                unsafe {
                    DispatchMessageW(&message);
                }
            }
            let registry = registry.lock().unwrap();
            if let Some(entry) = registry.entries.get(&instance)
                && entry.phase == tacky_borders::border_registry::BorderPhase::Running
            {
                break windows::Win32::Foundation::HWND(entry.border_hwnd.unwrap() as _);
            }
            assert!(
                std::time::Instant::now() < deadline,
                "host native worker did not run"
            );
            drop(registry);
            std::thread::yield_now();
        };
        let config = APP_STATE.get_config_mut();
        let (release, waiting) = std::sync::mpsc::channel();
        let finished = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let completed = finished.clone();
        let job = std::thread::spawn(move || {
            waiting.recv().unwrap();
            completed.store(true, std::sync::atomic::Ordering::Release);
            Ok(())
        });
        let retiring = registry.clone();
        let unblock = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while !retiring.lock().unwrap().shutdown_requested {
                assert!(std::time::Instant::now() < deadline);
                std::thread::yield_now();
            }
            release.send(()).unwrap();
        });
        if !shutting_down {
            unsafe {
                windows::Win32::UI::WindowsAndMessaging::PostQuitMessage(0);
            }
        }
        let started = std::time::Instant::now();
        run_host_loop(
            &coordinator,
            &requests,
            hook_work,
            Default::default(),
            shutting_down,
            Vec::new(),
            Some(job),
        );
        unblock.join().unwrap();
        assert!(finished.load(std::sync::atomic::Ordering::Acquire));
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        assert!(coordinator.is_terminal());
        assert!(coordinator.has_joined());
        let registry = registry.lock().unwrap();
        assert!(registry.shutdown_requested);
        assert!(registry.entries.is_empty());
        assert!(
            !unsafe { windows::Win32::UI::WindowsAndMessaging::IsWindow(Some(border)) }.as_bool()
        );
        assert!(
            unsafe { windows::Win32::UI::WindowsAndMessaging::IsWindow(Some(tracking.0)) }
                .as_bool()
        );
        drop(config);
    }
}
