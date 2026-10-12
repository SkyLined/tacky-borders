use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};

use crate::APP_STATE;
use crate::border_registry::{BorderRegistry, TrackingWindowFingerprint};
use crate::border_updates::{AppearanceSequencer, IpcUpdate};
use crate::colors::ColorBrushConfig;
use crate::config::{Config, OffsetConfig, RadiusConfig, WidthConfig};
use crate::iocp::{UnixListener, UnixStream};
use crate::utils::{LogIfErr, get_foreground_window, remove_file_if_exists};
use std::sync::{Mutex, RwLock};

pub fn socket_path() -> anyhow::Result<PathBuf> {
    Config::get_dir().map(|dir| dir.join("tacky-borders.sock"))
}

/// IPC Server that handles communication between a CLI and daemon.
/// Changes made via IPC are not written back to the config file.
pub struct IpcServer {
    socket_path: PathBuf,
    stop: Arc<AtomicBool>,
    thread_handle: Option<JoinHandle<()>>,
}

impl IpcServer {
    pub fn new(socket_path: &Path) -> anyhow::Result<Self> {
        // Remove a stale socket file left over from a previous run; bind fails otherwise
        remove_file_if_exists(socket_path).context("could not remove stale ipc socket")?;

        let listener = UnixListener::bind(socket_path).context("could not bind ipc socket")?;

        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = stop.clone();

        let thread_handle = thread::spawn(move || run_server(listener, stop_clone));

        info!("ipc server listening on {}", socket_path.display());

        Ok(Self {
            socket_path: socket_path.to_owned(),
            stop,
            thread_handle: Some(thread_handle),
        })
    }
}

impl Drop for IpcServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);

        // Unblock the accept() call in the server thread with a dummy connection
        let _ = UnixStream::connect(&self.socket_path);

        match self.thread_handle.take() {
            Some(handle) => {
                if let Err(err) = handle.join() {
                    error!("could not join ipc server thread handle: {err:?}");
                }
            }
            None => error!("could not take ipc server thread handle"),
        }

        // The listener has been dropped (its thread exited), so the socket file can go too
        remove_file_if_exists(&self.socket_path)
            .context("could not remove ipc socket")
            .log_if_err();

        debug!("ipc server stopped");
    }
}

fn run_server(listener: UnixListener, stop: Arc<AtomicBool>) {
    debug!("entering ipc server thread");

    loop {
        match listener.accept() {
            Ok(stream) => {
                // The dummy connection sent by Drop should not be processed
                if stop.load(Ordering::Relaxed) {
                    break;
                }

                thread::spawn(move || {
                    if let Err(err) = handle_client(stream) {
                        debug!("ipc client disconnected: {err:#}");
                    }
                });
            }
            Err(err) => {
                if !stop.load(Ordering::Relaxed) {
                    error!("could not accept ipc client: {err}");
                }
                break;
            }
        }
    }

    debug!("exiting ipc server thread");
}

#[cfg(test)]
pub(crate) fn handle_test_client(stream: UnixStream) -> anyhow::Result<()> {
    handle_client(stream)
}

fn handle_client(stream: UnixStream) -> anyhow::Result<()> {
    let reader = BufReader::new(&stream);

    for line in reader.lines() {
        let line = line.context("could not read line from ipc client")?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let mut response = process_command(trimmed);
        response.push('\n');

        (&stream)
            .write_all(response.as_bytes())
            .context("could not write response to ipc client")?;
    }

    Ok(())
}

/// All commands that can be sent through the IPC mechanism. When serialized,
/// the enum variant is in snake case and denoted with "cmd".
/// Example JSON format: {"cmd":"set_color","active":<color>,"inactive":<color>}
#[derive(Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum IpcCommand {
    SetColor {
        #[serde(default)]
        active: Option<ColorBrushConfig>,
        #[serde(default)]
        inactive: Option<ColorBrushConfig>,
        /// When true, only the currently focused window's border is updated.
        #[serde(default)]
        focused: bool,
    },
    SetWidth {
        width: WidthConfig,
        #[serde(default)]
        focused: bool,
    },
    SetOffset {
        offset: OffsetConfig,
        #[serde(default)]
        focused: bool,
    },
    SetRadius {
        radius: RadiusConfig,
        #[serde(default)]
        focused: bool,
    },
    Reload,
    GetState,
}

fn process_command(raw: &str) -> String {
    let command: IpcCommand = match serde_json::from_str(raw) {
        Ok(command) => command,
        Err(err) => {
            return json!({"ok": false, "error": format!("invalid command: {err}")}).to_string();
        }
    };

    match command {
        IpcCommand::SetColor {
            active,
            inactive,
            focused,
        } => {
            if active.is_none() && inactive.is_none() {
                return json!({"ok": false, "error": "no colors provided"}).to_string();
            }
            apply_update(IpcUpdate::Colors { active, inactive }, focused)
        }
        IpcCommand::SetWidth { width, focused } => apply_update(IpcUpdate::Width(width), focused),
        IpcCommand::SetOffset { offset, focused } => {
            apply_update(IpcUpdate::Offset(offset), focused)
        }
        IpcCommand::SetRadius { radius, focused } => {
            apply_update(IpcUpdate::Radius(radius), focused)
        }
        IpcCommand::Reload => {
            match Config::reload().and_then(|_| crate::reload_borders()).and_then(|receipt| receipt.wait()) {
                Ok(crate::border_coordinator::OperationOutcome::Reloaded { old_generation_retired, admitted_instances, skipped_invalid, skipped_ineligible, .. }) => json!({"ok": true, "old_generation_retired": old_generation_retired, "admitted_instances": admitted_instances.iter().map(|id| id.0).collect::<Vec<_>>(), "skipped_invalid": skipped_invalid, "skipped_ineligible": skipped_ineligible, "border_readiness": "not_awaited"}).to_string(),
                Ok(outcome) => json!({"ok": false, "error": format!("reload not admitted: {outcome:?}")}).to_string(),
                Err(error) => json!({"ok": false, "error": format!("reload failed: {error:#}")}).to_string(),
            }
        }
        IpcCommand::GetState => {
            let (active_color, inactive_color, border_width, border_offset, border_radius) = {
                let config = APP_STATE.config.read().unwrap();
                (
                    config.global.active_color.clone(),
                    config.global.inactive_color.clone(),
                    config.global.border_width,
                    config.global.border_offset,
                    config.global.border_radius,
                )
            };
            let active_window = {
                let isize = *APP_STATE.active_window.lock().unwrap();
                format!("{isize:#x}") // format as hex
            };
            let border_count = APP_STATE.border_windows().len();

            json!({
                "ok": true,
                "active_window": active_window,
                "border_count": border_count,
                "active_color": active_color,
                "inactive_color": inactive_color,
                "border_width": border_width,
                "border_offset": border_offset,
                "border_radius": border_radius,
            })
            .to_string()
        }
    }
}

#[derive(Default, Debug, Serialize)]
struct IpcDeliveryReport {
    config_accepted: bool,
    config_changed: bool,
    intended_targets: usize,
    accepted_targets: usize,
    failed_targets: usize,
    selection_failed: bool,
}

impl IpcDeliveryReport {
    fn response(&self) -> String {
        let accepted = self.config_accepted || self.accepted_targets != 0;
        let ok = accepted && self.failed_targets == 0 && !self.selection_failed;
        let mut value = serde_json::to_value(self).unwrap();
        value["ok"] = json!(ok);
        value["partial"] = json!(accepted && !ok);
        if !ok {
            value["error"] =
                json!("appearance update was not accepted by every intended destination");
        }
        value.to_string()
    }
}

enum TargetSelection {
    All,
    Focused(Option<TrackingWindowFingerprint>),
}

fn apply_update(update: IpcUpdate, focused: bool) -> String {
    let selection = if focused {
        TargetSelection::Focused(TrackingWindowFingerprint::capture(get_foreground_window()))
    } else {
        TargetSelection::All
    };
    match broadcast_update(&APP_STATE.appearance, &APP_STATE.config, &APP_STATE.border_registry, selection, update) {
        Ok(report) => report.response(),
        Err(error) => json!({"ok": false, "partial": false, "config_changed": false, "intended_targets": 0, "accepted_targets": 0, "failed_targets": 0, "error": format!("setter rejected: {error:#}")}).to_string(),
    }
}

fn broadcast_update(
    gate: &Mutex<AppearanceSequencer>,
    config: &RwLock<Config>,
    registry: &Mutex<BorderRegistry>,
    selection: TargetSelection,
    update: IpcUpdate,
) -> anyhow::Result<IpcDeliveryReport> {
    let mut sequence = gate
        .lock()
        .map_err(|_| anyhow::anyhow!("appearance gate poisoned"))?;
    let command = sequence.next_command().ok_or_else(|| {
        registry
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .quarantine();
        anyhow::anyhow!("appearance command IDs exhausted")
    })?;
    let mut report = IpcDeliveryReport::default();
    if matches!(selection, TargetSelection::All) {
        let revision = sequence.config_revision.checked_add(1).ok_or_else(|| {
            registry
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .quarantine();
            anyhow::anyhow!("configuration revisions exhausted")
        })?;
        let mut config = config
            .write()
            .map_err(|_| anyhow::anyhow!("configuration poisoned"))?;
        let global = &mut config.global;
        match &update {
            IpcUpdate::Colors { active, inactive } => {
                if let Some(color) = active {
                    report.config_changed |= global.active_color != *color;
                    global.active_color = color.clone();
                    sequence.global_stamps.active = command;
                }
                if let Some(color) = inactive {
                    report.config_changed |= global.inactive_color != *color;
                    global.inactive_color = color.clone();
                    sequence.global_stamps.inactive = command;
                }
            }
            IpcUpdate::Width(value) => {
                report.config_changed = global.border_width != *value;
                global.border_width = *value;
                sequence.global_stamps.width = command;
            }
            IpcUpdate::Offset(value) => {
                report.config_changed = global.border_offset != *value;
                global.border_offset = *value;
                sequence.global_stamps.offset = command;
            }
            IpcUpdate::Radius(value) => {
                report.config_changed = global.border_radius != *value;
                global.border_radius = *value;
                sequence.global_stamps.radius = command;
            }
        }
        sequence.config_revision = revision;
        report.config_accepted = true;
    }
    let targets = {
        let registry = match registry.lock() {
            Ok(registry) => registry,
            Err(poison) => {
                let mut registry = poison.into_inner();
                registry.quarantine();
                report.selection_failed = true;
                registry
            }
        };
        registry
            .entries
            .values()
            .filter(|entry| match selection {
                TargetSelection::All => true,
                TargetSelection::Focused(fingerprint) => {
                    fingerprint == Some(entry.tracking.fingerprint)
                        && registry
                            .current_by_hwnd
                            .get(&entry.tracking.fingerprint.hwnd)
                            == Some(&entry.id)
                }
            })
            .map(|entry| (entry.id, entry.updates.clone()))
            .collect::<Vec<_>>()
    };
    report.intended_targets = targets.len();
    for (_, mailbox) in targets {
        if mailbox.enqueue(command, update.clone()).is_ok() {
            report.accepted_targets += 1;
        } else {
            report.failed_targets += 1;
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::border_registry::{BorderAdmission, BorderControl, ReserveResult};
    use crate::border_updates::{BorderUpdateMailbox, MailboxDrain};
    use std::time::{Duration, Instant};

    type IpcFixture = (
        Arc<Mutex<AppearanceSequencer>>,
        Arc<RwLock<Config>>,
        Arc<Mutex<BorderRegistry>>,
        Arc<BorderUpdateMailbox>,
    );

    fn fixture() -> IpcFixture {
        let mailbox =
            Arc::new(BorderUpdateMailbox::new(Arc::new(BorderControl::new().unwrap())).unwrap());
        let mut registry = BorderRegistry::default();
        registry.admission = BorderAdmission::Open;
        let identity = registry
            .observe(TrackingWindowFingerprint {
                hwnd: 10,
                process_id: 11,
                thread_id: 12,
            })
            .unwrap();
        assert!(matches!(
            registry
                .reserve(identity, mailbox.clone(), Instant::now())
                .unwrap(),
            ReserveResult::Reserved(_)
        ));
        (
            Arc::new(Mutex::new(AppearanceSequencer::default())),
            Arc::new(RwLock::new(Config::default())),
            Arc::new(Mutex::new(registry)),
            mailbox,
        )
    }

    #[test]
    fn unchanged_global_with_zero_targets_is_complete_acceptance() {
        let config = RwLock::new(Config::default());
        let width = config.read().unwrap().global.border_width;
        let report = broadcast_update(
            &Mutex::default(),
            &config,
            &Mutex::new(BorderRegistry::default()),
            TargetSelection::All,
            IpcUpdate::Width(width),
        )
        .unwrap();
        assert!(report.config_accepted);
        assert!(!report.config_changed);
        assert_eq!(report.intended_targets, 0);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&report.response()).unwrap()["ok"],
            true
        );
    }

    #[test]
    fn focused_missing_is_total_failure_without_config_mutation() {
        let (gate, config, registry, _) = fixture();
        let original = config.read().unwrap().global.border_width;
        let report = broadcast_update(
            &gate,
            &config,
            &registry,
            TargetSelection::Focused(None),
            IpcUpdate::Width(WidthConfig::new(19.0)),
        )
        .unwrap();
        assert!(!report.config_accepted);
        assert_eq!(config.read().unwrap().global.border_width, original);
        let response: serde_json::Value = serde_json::from_str(&report.response()).unwrap();
        assert_eq!(response["ok"], false);
        assert_eq!(response["partial"], false);
    }

    #[test]
    fn revision_exhaustion_quarantines_without_mutation_or_delivery() {
        let (gate, config, registry, mailbox) = fixture();
        let original = config.read().unwrap().global.border_width;
        gate.lock().unwrap().config_revision = u64::MAX;
        assert!(
            broadcast_update(
                &gate,
                &config,
                &registry,
                TargetSelection::All,
                IpcUpdate::Width(WidthConfig::new(22.0))
            )
            .is_err()
        );
        assert_eq!(config.read().unwrap().global.border_width, original);
        assert_eq!(gate.lock().unwrap().config_revision, u64::MAX);
        assert!(registry.lock().unwrap().quarantined);
        assert!(mailbox.control().is_stopped());
    }

    #[test]
    fn focused_components_survive_unrelated_global_snapshot_refresh() {
        let (gate, config, registry, mailbox) = fixture();
        let fingerprint = TrackingWindowFingerprint {
            hwnd: 10,
            process_id: 11,
            thread_id: 12,
        };
        let original_active = config.read().unwrap().global.active_color.clone();
        broadcast_update(
            &gate,
            &config,
            &registry,
            TargetSelection::Focused(Some(fingerprint)),
            IpcUpdate::Colors {
                active: Some(ColorBrushConfig::Solid("red".into())),
                inactive: None,
            },
        )
        .unwrap();
        broadcast_update(
            &gate,
            &config,
            &registry,
            TargetSelection::All,
            IpcUpdate::Colors {
                active: None,
                inactive: Some(ColorBrushConfig::Solid("blue".into())),
            },
        )
        .unwrap();
        broadcast_update(
            &gate,
            &config,
            &registry,
            TargetSelection::Focused(Some(fingerprint)),
            IpcUpdate::Width(WidthConfig::new(11.0)),
        )
        .unwrap();
        let MailboxDrain::Updates(mut pending) = mailbox.try_drain() else {
            panic!("mailbox did not drain");
        };
        let config = config.read().unwrap();
        assert_eq!(config.global.active_color, original_active);
        let sequence = gate.lock().unwrap();
        let resolved = crate::border_config::BorderConfig::resolve(
            &crate::config::WindowRule::default(),
            &config.global,
            config.render_backend,
            false,
        );
        pending.merge(crate::border_updates::PendingIpcUpdates::from_config(
            &resolved,
            sequence.global_stamps,
        ));
        let colors = pending.colors.unwrap();
        assert_eq!(
            colors.active.unwrap().value,
            ColorBrushConfig::Solid("red".into())
        );
        assert_eq!(
            colors.inactive.unwrap().value,
            ColorBrushConfig::Solid("blue".into())
        );
        assert_eq!(pending.width.unwrap().value, WidthConfig::new(11.0));
        assert_eq!(sequence.config_revision, 1);
    }

    #[test]
    fn reused_tracking_identity_cannot_retarget_old_mailbox() {
        let (gate, config, registry, old_mailbox) = fixture();
        let fingerprint = TrackingWindowFingerprint {
            hwnd: 10,
            process_id: 11,
            thread_id: 12,
        };
        let new_mailbox =
            Arc::new(BorderUpdateMailbox::new(Arc::new(BorderControl::new().unwrap())).unwrap());
        {
            let mut registry = registry.lock().unwrap();
            registry.retire_observation(fingerprint.hwnd, None, Instant::now());
            let identity = registry.observe(fingerprint).unwrap();
            registry
                .reserve(identity, new_mailbox.clone(), Instant::now())
                .unwrap();
        }
        let report = broadcast_update(
            &gate,
            &config,
            &registry,
            TargetSelection::Focused(Some(fingerprint)),
            IpcUpdate::Width(WidthConfig::new(17.0)),
        )
        .unwrap();
        assert_eq!(
            (
                report.intended_targets,
                report.accepted_targets,
                report.failed_targets
            ),
            (1, 1, 0)
        );
        assert!(old_mailbox.control().is_stopped());
        assert!(matches!(old_mailbox.try_drain(), MailboxDrain::Failed));
        let MailboxDrain::Updates(pending) = new_mailbox.try_drain() else {
            panic!("replacement did not drain");
        };
        assert_eq!(pending.width.unwrap().value, WidthConfig::new(17.0));
    }

    #[test]
    fn reserved_target_accepts_and_stopping_target_counts_as_rejected() {
        let (gate, config, registry, mailbox) = fixture();
        let update = IpcUpdate::Width(WidthConfig::new(9.0));
        let report = broadcast_update(
            &gate,
            &config,
            &registry,
            TargetSelection::All,
            update.clone(),
        )
        .unwrap();
        assert_eq!(
            (
                report.intended_targets,
                report.accepted_targets,
                report.failed_targets
            ),
            (1, 1, 0)
        );
        mailbox.control().record_stop();
        let report =
            broadcast_update(&gate, &config, &registry, TargetSelection::All, update).unwrap();
        assert_eq!(
            (
                report.intended_targets,
                report.accepted_targets,
                report.failed_targets
            ),
            (1, 0, 1)
        );
        assert!(report.config_accepted && !report.config_changed);
        let response: serde_json::Value = serde_json::from_str(&report.response()).unwrap();
        assert_eq!(response["partial"], true);
        assert_eq!(response["ok"], false);
    }

    #[test]
    fn two_clients_commit_and_enqueue_in_the_same_order() {
        let (gate, config, registry, mailbox) = fixture();
        let mut workers = Vec::with_capacity(2);
        mailbox.while_locked(|| {
            let first_gate = gate.clone();
            let first_config = config.clone();
            let first_registry = registry.clone();
            workers.push(thread::spawn(move || {
                broadcast_update(
                    &first_gate,
                    &first_config,
                    &first_registry,
                    TargetSelection::All,
                    IpcUpdate::Width(WidthConfig::new(9.0)),
                )
                .unwrap()
            }));
            let deadline = Instant::now() + Duration::from_secs(2);
            while config.read().unwrap().global.border_width != WidthConfig::new(9.0) {
                assert!(Instant::now() < deadline);
                thread::yield_now();
            }
            assert!(gate.try_lock().is_err());
            let second_gate = gate.clone();
            let second_config = config.clone();
            let second_registry = registry.clone();
            workers.push(thread::spawn(move || {
                broadcast_update(
                    &second_gate,
                    &second_config,
                    &second_registry,
                    TargetSelection::All,
                    IpcUpdate::Width(WidthConfig::new(13.0)),
                )
                .unwrap()
            }));
        });
        for worker in workers {
            assert_eq!(worker.join().unwrap().accepted_targets, 1);
        }
        assert_eq!(
            config.read().unwrap().global.border_width,
            WidthConfig::new(13.0)
        );
        let MailboxDrain::Updates(pending) = mailbox.try_drain() else {
            panic!("mailbox did not drain");
        };
        let width = pending.width.unwrap();
        assert_eq!(width.value, WidthConfig::new(13.0));
        let sequence = gate.lock().unwrap();
        assert_eq!(width.command_id, sequence.global_stamps.width);
        assert_eq!(sequence.config_revision, 2);
    }
}
