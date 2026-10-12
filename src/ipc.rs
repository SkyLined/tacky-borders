use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::cell::Cell;
use std::collections::{HashSet, VecDeque};
use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::APP_STATE;
use crate::border_registry::{BorderRegistry, TrackingWindowFingerprint};
use crate::border_updates::{AppearanceSequencer, IpcUpdate};
use crate::colors::ColorBrushConfig;
use crate::config::{Config, OffsetConfig, RadiusConfig, WidthConfig};
use crate::iocp::{SocketId, UnixListener, UnixStream, shutdown_socket};
use crate::ipc_security::restrict_socket_path;
use crate::utils::{LogIfErr, get_foreground_window, remove_file_if_exists};

const IPC_WORKER_COUNT: usize = 4;
const IPC_QUEUE_CAPACITY: usize = 4;
const IPC_ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(50);
const IPC_SOCKET_POLL_INTERVAL: Duration = Duration::from_millis(50);
const IPC_OVERLOAD_LOG_INTERVAL: Duration = Duration::from_secs(5);
const MAX_COMMAND_BYTES: usize = 64 * 1024;
const MAX_RESPONSE_BYTES: usize = 64 * 1024;
const MAX_COMMANDS_PER_CONNECTION: usize = 32;
const IPC_IDLE_TIMEOUT: Duration = Duration::from_secs(5);
const IPC_COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
const IPC_CONNECTION_TIMEOUT: Duration = Duration::from_secs(30);
const IPC_WRITE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy)]
struct IpcLimits {
    max_command_bytes: usize,
    max_response_bytes: usize,
    max_commands_per_connection: usize,
    idle_timeout: Duration,
    command_timeout: Duration,
    connection_timeout: Duration,
    write_timeout: Duration,
}

impl Default for IpcLimits {
    fn default() -> Self {
        Self {
            max_command_bytes: MAX_COMMAND_BYTES,
            max_response_bytes: MAX_RESPONSE_BYTES,
            max_commands_per_connection: MAX_COMMANDS_PER_CONNECTION,
            idle_timeout: IPC_IDLE_TIMEOUT,
            command_timeout: IPC_COMMAND_TIMEOUT,
            connection_timeout: IPC_CONNECTION_TIMEOUT,
            write_timeout: IPC_WRITE_TIMEOUT,
        }
    }
}

pub fn socket_path() -> anyhow::Result<PathBuf> {
    Config::get_dir().map(|dir| dir.join("tacky-borders.sock"))
}

/// IPC Server that handles communication between a CLI and daemon.
/// Changes made via IPC are not written back to the config file.
pub struct IpcServer {
    socket_path: PathBuf,
    stop: Arc<AtomicBool>,
    queue: Arc<ClientQueue<UnixStream>>,
    active: Arc<ActiveClients>,
    rejected: Arc<AtomicU64>,
    acceptor: Option<JoinHandle<()>>,
    workers: Vec<JoinHandle<()>>,
}

impl IpcServer {
    pub fn new(socket_path: &Path) -> anyhow::Result<Self> {
        // Remove a stale socket file left over from a previous run; bind fails otherwise
        remove_file_if_exists(socket_path).context("could not remove stale ipc socket")?;

        let listener = UnixListener::bind(socket_path).context("could not bind ipc socket")?;
        listener
            .set_nonblocking(true)
            .context("could not make ipc listener nonblocking")?;
        let socket_path_restricted = match restrict_socket_path(socket_path) {
            Ok(()) => true,
            Err(error) => {
                warn!(
                    "could not restrict the ipc socket path; continuing with protocol limits: {error:#}"
                );
                false
            }
        };

        let stop = Arc::new(AtomicBool::new(false));
        let queue = Arc::new(ClientQueue::new(IPC_QUEUE_CAPACITY));
        let active = Arc::new(ActiveClients::default());
        let rejected = Arc::new(AtomicU64::new(0));
        let mut workers = Vec::with_capacity(IPC_WORKER_COUNT);

        for index in 0..IPC_WORKER_COUNT {
            let worker_queue = queue.clone();
            let worker_active = active.clone();
            let worker_stop = stop.clone();
            match thread::Builder::new()
                .name(format!("ipc-worker-{index}"))
                .spawn(move || run_worker(worker_queue, worker_active, worker_stop))
            {
                Ok(worker) => workers.push(worker),
                Err(error) => {
                    stop.store(true, Ordering::Release);
                    queue.close_and_drain();
                    join_threads(&mut workers, "ipc worker");
                    drop(listener);
                    remove_file_if_exists(socket_path)
                        .context("could not remove ipc socket after startup failure")
                        .log_if_err();
                    return Err(error).context("could not spawn ipc worker");
                }
            }
        }

        let acceptor_queue = queue.clone();
        let acceptor_stop = stop.clone();
        let acceptor_rejected = rejected.clone();
        let acceptor = match thread::Builder::new()
            .name("ipc-acceptor".into())
            .spawn(move || run_acceptor(listener, acceptor_queue, acceptor_stop, acceptor_rejected))
        {
            Ok(acceptor) => acceptor,
            Err(error) => {
                stop.store(true, Ordering::Release);
                queue.close_and_drain();
                active.shutdown_all();
                join_threads(&mut workers, "ipc worker");
                remove_file_if_exists(socket_path)
                    .context("could not remove ipc socket after startup failure")
                    .log_if_err();
                return Err(error).context("could not spawn ipc acceptor");
            }
        };

        info!(
            "ipc server listening on {} (socket path restricted: {socket_path_restricted})",
            socket_path.display()
        );

        Ok(Self {
            socket_path: socket_path.to_owned(),
            stop,
            queue,
            active,
            rejected,
            acceptor: Some(acceptor),
            workers,
        })
    }
}

impl Drop for IpcServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);

        if let Some(acceptor) = self.acceptor.take() {
            acceptor.thread().unpark();
            if let Err(error) = acceptor.join() {
                error!("could not join ipc acceptor: {error:?}");
            }
        } else {
            error!("could not take ipc acceptor thread handle");
        }

        self.queue.close_and_drain();
        self.active.shutdown_all();
        join_threads(&mut self.workers, "ipc worker");
        let rejected = self.rejected.swap(0, Ordering::Relaxed);
        if rejected != 0 {
            debug!("rejected {rejected} ipc clients before shutdown");
        }

        remove_file_if_exists(&self.socket_path)
            .context("could not remove ipc socket")
            .log_if_err();

        debug!("ipc server stopped");
    }
}

enum PushResult<T> {
    Queued,
    Full(T),
    Closed(T),
}

struct ClientQueueState<T> {
    closed: bool,
    clients: VecDeque<T>,
}

struct ClientQueue<T> {
    capacity: usize,
    state: Mutex<ClientQueueState<T>>,
    available: Condvar,
}

impl<T> ClientQueue<T> {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            state: Mutex::new(ClientQueueState {
                closed: false,
                clients: VecDeque::with_capacity(capacity),
            }),
            available: Condvar::new(),
        }
    }

    fn try_push(&self, client: T) -> PushResult<T> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if state.closed {
            return PushResult::Closed(client);
        }
        if state.clients.len() == self.capacity {
            return PushResult::Full(client);
        }

        state.clients.push_back(client);
        self.available.notify_one();
        PushResult::Queued
    }

    fn pop(&self) -> Option<T> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        loop {
            if let Some(client) = state.clients.pop_front() {
                return Some(client);
            }
            if state.closed {
                return None;
            }
            state = self
                .available
                .wait(state)
                .unwrap_or_else(|poison| poison.into_inner());
        }
    }

    fn close_and_drain(&self) {
        let clients = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            state.closed = true;
            self.available.notify_all();
            state.clients.drain(..).collect::<Vec<_>>()
        };
        drop(clients);
    }
}

#[derive(Default)]
struct ActiveClients {
    state: Mutex<ActiveClientsState>,
}

#[derive(Default)]
struct ActiveClientsState {
    stopping: bool,
    sockets: HashSet<SocketId>,
}

impl ActiveClients {
    fn register(self: &Arc<Self>, stream: UnixStream) -> Result<ActiveClient, UnixStream> {
        let socket_id = stream.socket_id();
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if state.stopping {
            return Err(stream);
        }
        state.sockets.insert(socket_id);
        drop(state);

        Ok(ActiveClient {
            _registration: ActiveClientRegistration {
                clients: self.clone(),
                socket_id,
            },
            stream,
        })
    }

    fn shutdown_all(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        state.stopping = true;
        for socket_id in state.sockets.iter().copied() {
            shutdown_socket(socket_id)
                .context("could not shut down active ipc client")
                .log_if_err();
        }
    }
}

struct ActiveClientRegistration {
    clients: Arc<ActiveClients>,
    socket_id: SocketId,
}

impl Drop for ActiveClientRegistration {
    fn drop(&mut self) {
        self.clients
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .sockets
            .remove(&self.socket_id);
    }
}

struct ActiveClient {
    _registration: ActiveClientRegistration,
    stream: UnixStream,
}

fn run_worker(
    queue: Arc<ClientQueue<UnixStream>>,
    active: Arc<ActiveClients>,
    stop: Arc<AtomicBool>,
) {
    while let Some(stream) = queue.pop() {
        if stop.load(Ordering::Acquire) {
            break;
        }

        let Ok(client) = active.register(stream) else {
            break;
        };
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            handle_client(&client.stream, &stop)
        }));
        match result {
            Ok(Ok(())) => {}
            Ok(Err(error)) => debug!("ipc client disconnected: {error:#}"),
            Err(error) => error!("ipc client handler panicked: {error:?}"),
        }
    }
}

fn run_acceptor(
    listener: UnixListener,
    queue: Arc<ClientQueue<UnixStream>>,
    stop: Arc<AtomicBool>,
    rejected: Arc<AtomicU64>,
) {
    debug!("entering ipc acceptor thread");
    let mut last_overload_log = Instant::now();

    loop {
        if stop.load(Ordering::Acquire) {
            break;
        }

        match listener.accept() {
            Ok(stream) => {
                if stop.load(Ordering::Acquire) {
                    break;
                }
                if let Err(error) = stream.set_nonblocking(false) {
                    error!("could not make accepted ipc client blocking: {error}");
                    continue;
                }
                match queue.try_push(stream) {
                    PushResult::Queued => {}
                    PushResult::Full(_) | PushResult::Closed(_) => {
                        rejected.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if last_overload_log.elapsed() >= IPC_OVERLOAD_LOG_INTERVAL {
                    let count = rejected.swap(0, Ordering::Relaxed);
                    if count != 0 {
                        warn!("rejected {count} ipc clients while the worker queue was full");
                    }
                    last_overload_log = Instant::now();
                }
                thread::park_timeout(IPC_ACCEPT_POLL_INTERVAL);
            }
            Err(error) => {
                if !stop.load(Ordering::Acquire) {
                    error!("could not accept ipc client: {error}");
                }
                break;
            }
        }
    }

    debug!("exiting ipc acceptor thread");
}

fn join_threads(handles: &mut Vec<JoinHandle<()>>, description: &str) {
    for handle in handles.drain(..) {
        if let Err(error) = handle.join() {
            error!("could not join {description}: {error:?}");
        }
    }
}

#[cfg(test)]
pub(crate) fn handle_test_client(stream: UnixStream) -> anyhow::Result<()> {
    handle_client(&stream, &AtomicBool::new(false))
}

enum CommandRead {
    Command(Vec<u8>),
    Eof,
}

#[derive(Debug)]
enum CommandReadError {
    Io(io::Error),
    TooLarge,
    Deadline(&'static str),
}

impl std::fmt::Display for CommandReadError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "could not read ipc command: {error}"),
            Self::TooLarge => formatter.write_str("ipc command exceeds the byte limit"),
            Self::Deadline(name) => write!(formatter, "ipc {name} deadline exceeded"),
        }
    }
}

impl std::error::Error for CommandReadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::TooLarge | Self::Deadline(_) => None,
        }
    }
}

fn read_bounded_command<R, F, P>(
    reader: &mut R,
    max_bytes: usize,
    mut before_read: F,
    mut after_progress: P,
) -> Result<CommandRead, CommandReadError>
where
    R: BufRead,
    F: FnMut() -> Result<(), CommandReadError>,
    P: FnMut(),
{
    let mut command = Vec::with_capacity(max_bytes.min(1024));

    loop {
        before_read()?;
        let available = match reader.fill_buf() {
            Ok(available) => available,
            Err(error) if is_socket_timeout(&error) => continue,
            Err(error) => return Err(CommandReadError::Io(error)),
        };
        if available.is_empty() {
            return if command.is_empty() {
                Ok(CommandRead::Eof)
            } else {
                Ok(CommandRead::Command(command))
            };
        }

        let newline = available.iter().position(|byte| *byte == b'\n');
        let segment_len = newline.unwrap_or(available.len());
        let new_len = command
            .len()
            .checked_add(segment_len)
            .ok_or(CommandReadError::TooLarge)?;
        if new_len > max_bytes {
            return Err(CommandReadError::TooLarge);
        }
        command.extend_from_slice(&available[..segment_len]);
        if segment_len != 0 {
            after_progress();
        }
        reader.consume(segment_len + usize::from(newline.is_some()));

        if newline.is_some() {
            return Ok(CommandRead::Command(command));
        }
    }
}

fn is_socket_timeout(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
    ) || matches!(error.raw_os_error(), Some(10035 | 10060))
}

fn remaining(deadline: Instant, name: &'static str) -> Result<Duration, CommandReadError> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or(CommandReadError::Deadline(name))
}

fn handle_client(stream: &UnixStream, stop: &AtomicBool) -> anyhow::Result<()> {
    handle_client_with_limits(stream, IpcLimits::default(), stop)
}

fn handle_client_with_limits(
    stream: &UnixStream,
    limits: IpcLimits,
    stop: &AtomicBool,
) -> anyhow::Result<()> {
    let connection_deadline = Instant::now()
        .checked_add(limits.connection_timeout)
        .context("ipc connection deadline overflow")?;
    let mut reader = BufReader::with_capacity(8 * 1024, stream);

    for _ in 0..limits.max_commands_per_connection {
        let idle_deadline = Cell::new(
            Instant::now()
                .checked_add(limits.idle_timeout)
                .context("ipc idle deadline overflow")?,
        );
        let command_deadline = Instant::now()
            .checked_add(limits.command_timeout)
            .context("ipc command deadline overflow")?;
        let command = read_bounded_command(
            &mut reader,
            limits.max_command_bytes,
            || {
                let timeout = IPC_SOCKET_POLL_INTERVAL
                    .min(remaining(idle_deadline.get(), "idle")?)
                    .min(remaining(command_deadline, "command")?)
                    .min(remaining(connection_deadline, "connection")?);
                stream
                    .set_read_timeout(timeout)
                    .map_err(CommandReadError::Io)
            },
            || {
                idle_deadline.set(
                    Instant::now()
                        .checked_add(limits.idle_timeout)
                        .unwrap_or_else(Instant::now),
                );
            },
        );

        let command = match command {
            Ok(CommandRead::Command(command)) => command,
            Ok(CommandRead::Eof) => return Ok(()),
            Err(CommandReadError::TooLarge) => {
                write_protocol_error(
                    stream,
                    "command byte limit exceeded",
                    limits,
                    connection_deadline,
                    stop,
                )
                .log_if_err();
                return Ok(());
            }
            Err(error) => return Err(error.into()),
        };

        let command = match std::str::from_utf8(&command) {
            Ok(command) => command,
            Err(_) => {
                write_protocol_error(
                    stream,
                    "command is not valid UTF-8",
                    limits,
                    connection_deadline,
                    stop,
                )
                .log_if_err();
                return Ok(());
            }
        };
        let trimmed = command.trim();
        if trimmed.is_empty() {
            continue;
        }

        remaining(connection_deadline, "connection")?;
        let response = process_command(trimmed);
        write_response(stream, response, limits, connection_deadline, stop)?;
    }

    write_protocol_error(
        stream,
        "command count limit exceeded",
        limits,
        connection_deadline,
        stop,
    )
    .log_if_err();
    Ok(())
}

fn write_protocol_error(
    stream: &UnixStream,
    message: &str,
    limits: IpcLimits,
    connection_deadline: Instant,
    stop: &AtomicBool,
) -> anyhow::Result<()> {
    write_response(
        stream,
        json!({"ok": false, "error": message}).to_string(),
        limits,
        connection_deadline,
        stop,
    )
}

trait ResponseWriter {
    fn set_write_timeout(&self, timeout: Duration) -> io::Result<()>;
    fn write_bytes(&self, bytes: &[u8]) -> io::Result<usize>;
}

impl ResponseWriter for UnixStream {
    fn set_write_timeout(&self, timeout: Duration) -> io::Result<()> {
        UnixStream::set_write_timeout(self, timeout)
    }

    fn write_bytes(&self, bytes: &[u8]) -> io::Result<usize> {
        io::Write::write(&mut &*self, bytes)
    }
}

fn write_response<W: ResponseWriter>(
    writer: &W,
    mut response: String,
    limits: IpcLimits,
    connection_deadline: Instant,
    stop: &AtomicBool,
) -> anyhow::Result<()> {
    anyhow::ensure!(!stop.load(Ordering::Acquire), "ipc server is stopping");
    remaining(connection_deadline, "connection")?;
    if response.len() > limits.max_response_bytes {
        anyhow::bail!("ipc response exceeds the byte limit");
    }
    response.push('\n');
    let write_deadline = Instant::now()
        .checked_add(limits.write_timeout)
        .context("ipc write deadline overflow")?;
    let mut remaining_bytes = response.as_bytes();
    while !remaining_bytes.is_empty() {
        anyhow::ensure!(!stop.load(Ordering::Acquire), "ipc server is stopping");
        let timeout = IPC_SOCKET_POLL_INTERVAL
            .min(remaining(write_deadline, "write")?)
            .min(remaining(connection_deadline, "connection")?);
        writer
            .set_write_timeout(timeout)
            .context("could not set ipc client write timeout")?;
        match writer.write_bytes(remaining_bytes) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "ipc socket write returned zero bytes",
                )
                .into());
            }
            Ok(written) => remaining_bytes = &remaining_bytes[written..],
            Err(error) if is_socket_timeout(&error) => {
                anyhow::ensure!(!stop.load(Ordering::Acquire), "ipc server is stopping");
            }
            Err(error) => return Err(error).context("could not write response to ipc client"),
        }
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
    use std::io::{Read, Write};
    use std::sync::atomic::AtomicUsize;
    use std::time::{Duration, Instant};

    static NEXT_SOCKET_PATH: AtomicU64 = AtomicU64::new(0);

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

    fn test_socket_path(name: &str) -> PathBuf {
        let id = NEXT_SOCKET_PATH.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "tacky-borders-ipc-{name}-{}-{id}.sock",
            std::process::id()
        ))
    }

    #[test]
    fn client_queue_enforces_capacity_and_fifo_order() {
        let queue = ClientQueue::new(2);
        assert!(matches!(queue.try_push(1), PushResult::Queued));
        assert!(matches!(queue.try_push(2), PushResult::Queued));
        assert!(matches!(queue.try_push(3), PushResult::Full(3)));
        assert_eq!(queue.pop(), Some(1));
        assert_eq!(queue.pop(), Some(2));
    }

    #[test]
    fn client_queue_rejects_pushes_after_close() {
        let queue = ClientQueue::new(1);
        queue.close_and_drain();

        assert!(matches!(queue.try_push(1), PushResult::Closed(1)));
        assert_eq!(queue.pop(), None);
    }

    #[test]
    fn client_queue_close_wakes_waiter() {
        let queue = Arc::new(ClientQueue::<usize>::new(1));
        let waiter_queue = queue.clone();
        let waiter = thread::spawn(move || waiter_queue.pop());

        queue.close_and_drain();

        assert_eq!(waiter.join().unwrap(), None);
    }

    fn read_test_command(
        input: &[u8],
        capacity: usize,
        limit: usize,
    ) -> Result<CommandRead, CommandReadError> {
        let cursor = io::Cursor::new(input);
        let mut reader = BufReader::with_capacity(capacity, cursor);
        read_bounded_command(&mut reader, limit, || Ok(()), || {})
    }

    #[test]
    fn bounded_reader_preserves_newline_and_eof_commands() {
        let CommandRead::Command(command) = read_test_command(b"first\n", 8, 16).unwrap() else {
            panic!("newline command was not returned");
        };
        assert_eq!(command, b"first");

        let CommandRead::Command(command) = read_test_command(b"final", 8, 16).unwrap() else {
            panic!("EOF command was not returned");
        };
        assert_eq!(command, b"final");
        assert!(matches!(
            read_test_command(b"", 8, 16).unwrap(),
            CommandRead::Eof
        ));
    }

    #[test]
    fn bounded_reader_enforces_exact_payload_limit() {
        let CommandRead::Command(command) = read_test_command(b"1234\n", 2, 4).unwrap() else {
            panic!("limit-sized command was not returned");
        };
        assert_eq!(command, b"1234");
        assert!(matches!(
            read_test_command(b"12345\n", 2, 4),
            Err(CommandReadError::TooLarge)
        ));
    }

    #[test]
    fn bounded_reader_stops_at_each_buffered_newline() {
        let cursor = io::Cursor::new(b"one\ntwo\n");
        let mut reader = BufReader::with_capacity(16, cursor);
        let CommandRead::Command(first) =
            read_bounded_command(&mut reader, 16, || Ok(()), || {}).unwrap()
        else {
            panic!("first command was not returned");
        };
        let CommandRead::Command(second) =
            read_bounded_command(&mut reader, 16, || Ok(()), || {}).unwrap()
        else {
            panic!("second command was not returned");
        };
        assert_eq!(first, b"one");
        assert_eq!(second, b"two");
    }

    #[test]
    fn server_bounds_clients_and_rebinds_after_active_shutdown() -> anyhow::Result<()> {
        let socket_path = test_socket_path("bounds");
        let server = IpcServer::new(&socket_path)?;
        assert_eq!(server.workers.len(), IPC_WORKER_COUNT);

        let mut clients = Vec::new();
        for _ in 0..IPC_WORKER_COUNT {
            clients.push(UnixStream::connect(&socket_path)?);
        }

        let deadline = Instant::now() + Duration::from_secs(2);
        while server
            .active
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .sockets
            .len()
            != IPC_WORKER_COUNT
        {
            anyhow::ensure!(
                Instant::now() < deadline,
                "server did not assign one client to each worker"
            );
            thread::yield_now();
        }

        for _ in 0..IPC_QUEUE_CAPACITY {
            clients.push(UnixStream::connect(&socket_path)?);
        }
        while server
            .queue
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clients
            .len()
            != IPC_QUEUE_CAPACITY
        {
            anyhow::ensure!(
                Instant::now() < deadline,
                "server did not fill its bounded client queue"
            );
            thread::yield_now();
        }

        clients.push(UnixStream::connect(&socket_path)?);
        while server.rejected.load(Ordering::Relaxed) == 0 {
            anyhow::ensure!(
                Instant::now() < deadline,
                "server did not reject a client beyond its bounded capacity"
            );
            thread::yield_now();
        }

        assert_eq!(
            server
                .active
                .state
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .sockets
                .len(),
            IPC_WORKER_COUNT
        );
        assert_eq!(
            server
                .queue
                .state
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .clients
                .len(),
            IPC_QUEUE_CAPACITY
        );

        let shutdown_started = Instant::now();
        drop(server);
        anyhow::ensure!(
            shutdown_started.elapsed() < Duration::from_secs(2),
            "server shutdown exceeded its bounded wait"
        );
        anyhow::ensure!(!socket_path.exists(), "ipc socket path was not removed");
        drop(clients);

        let restarted = IpcServer::new(&socket_path)?;
        drop(restarted);
        anyhow::ensure!(
            !socket_path.exists(),
            "restarted server left its socket path"
        );
        Ok(())
    }

    #[test]
    fn server_rejects_oversized_command_and_closes_client() -> anyhow::Result<()> {
        let socket_path = test_socket_path("oversized");
        let server = IpcServer::new(&socket_path)?;
        let mut client = UnixStream::connect(&socket_path)?;
        client.set_read_timeout(Duration::from_secs(2))?;
        let mut command = vec![b'x'; MAX_COMMAND_BYTES + 1];
        command.push(b'\n');

        client.write_all(&command)?;
        let mut response = String::new();
        client.read_to_string(&mut response)?;

        assert!(response.contains("command byte limit exceeded"));
        drop(server);
        Ok(())
    }

    #[test]
    fn blank_commands_count_toward_connection_limit() -> anyhow::Result<()> {
        let socket_path = test_socket_path("command-count");
        let server = IpcServer::new(&socket_path)?;
        let mut client = UnixStream::connect(&socket_path)?;
        client.set_read_timeout(Duration::from_secs(2))?;

        client.write_all(&[b'\n'; MAX_COMMANDS_PER_CONNECTION])?;
        let mut response = String::new();
        client.read_to_string(&mut response)?;

        assert!(response.contains("command count limit exceeded"));
        drop(server);
        Ok(())
    }

    #[test]
    fn idle_deadline_releases_handler() -> anyhow::Result<()> {
        let socket_path = test_socket_path("idle-deadline");
        remove_file_if_exists(&socket_path)?;
        let listener = UnixListener::bind(&socket_path)?;
        let mut client = UnixStream::connect(&socket_path)?;
        client.set_read_timeout(Duration::from_secs(2))?;
        let server_stream = listener.accept()?;
        let limits = IpcLimits {
            idle_timeout: Duration::from_millis(100),
            command_timeout: Duration::from_secs(1),
            connection_timeout: Duration::from_secs(1),
            ..IpcLimits::default()
        };
        let handler = thread::spawn(move || {
            handle_client_with_limits(&server_stream, limits, &AtomicBool::new(false))
        });
        let started = Instant::now();

        let mut buffer = [0u8; 1];
        assert_eq!(client.read(&mut buffer)?, 0);
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(handler.join().unwrap().is_err());

        drop(listener);
        remove_file_if_exists(&socket_path)?;
        Ok(())
    }

    #[test]
    fn response_limit_rejects_before_writing() -> anyhow::Result<()> {
        let socket_path = test_socket_path("response-limit");
        remove_file_if_exists(&socket_path)?;
        let listener = UnixListener::bind(&socket_path)?;
        let _client = UnixStream::connect(&socket_path)?;
        let server_stream = listener.accept()?;
        let limits = IpcLimits {
            max_response_bytes: 3,
            ..IpcLimits::default()
        };
        let connection_deadline = Instant::now() + Duration::from_secs(1);

        let error = write_response(
            &server_stream,
            "1234".to_owned(),
            limits,
            connection_deadline,
            &AtomicBool::new(false),
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("response exceeds the byte limit")
        );
        drop(listener);
        remove_file_if_exists(&socket_path)?;
        Ok(())
    }

    struct TimeoutWriter<'a> {
        stop_on_write: Option<&'a AtomicBool>,
        writes: AtomicUsize,
    }

    impl ResponseWriter for TimeoutWriter<'_> {
        fn set_write_timeout(&self, timeout: Duration) -> io::Result<()> {
            assert!(!timeout.is_zero());
            assert!(timeout <= IPC_SOCKET_POLL_INTERVAL);
            Ok(())
        }

        fn write_bytes(&self, _bytes: &[u8]) -> io::Result<usize> {
            self.writes.fetch_add(1, Ordering::Relaxed);
            if let Some(stop) = self.stop_on_write {
                stop.store(true, Ordering::Release);
            } else {
                thread::sleep(Duration::from_millis(1));
            }
            Err(io::Error::new(io::ErrorKind::TimedOut, "test timeout"))
        }
    }

    #[test]
    fn write_timeouts_end_at_write_deadline() {
        let stop = AtomicBool::new(false);
        let writer = TimeoutWriter {
            stop_on_write: None,
            writes: AtomicUsize::new(0),
        };
        let limits = IpcLimits {
            write_timeout: Duration::from_millis(20),
            connection_timeout: Duration::from_secs(1),
            ..IpcLimits::default()
        };

        let error = write_response(
            &writer,
            "response".to_owned(),
            limits,
            Instant::now() + limits.connection_timeout,
            &stop,
        )
        .unwrap_err();

        assert!(error.to_string().contains("write deadline exceeded"));
        assert!(writer.writes.load(Ordering::Relaxed) >= 1);
    }

    #[test]
    fn stop_cancels_write_after_bounded_timeout() {
        let stop = AtomicBool::new(false);
        let writer = TimeoutWriter {
            stop_on_write: Some(&stop),
            writes: AtomicUsize::new(0),
        };
        let limits = IpcLimits {
            write_timeout: Duration::from_secs(5),
            connection_timeout: Duration::from_secs(10),
            ..IpcLimits::default()
        };

        let error = write_response(
            &writer,
            "response".to_owned(),
            limits,
            Instant::now() + limits.connection_timeout,
            &stop,
        )
        .unwrap_err();

        assert!(error.to_string().contains("ipc server is stopping"));
        assert_eq!(writer.writes.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn slow_trickle_cannot_extend_command_deadline() -> anyhow::Result<()> {
        let socket_path = test_socket_path("command-deadline");
        remove_file_if_exists(&socket_path)?;
        let listener = UnixListener::bind(&socket_path)?;
        let mut client = UnixStream::connect(&socket_path)?;
        client.set_read_timeout(Duration::from_secs(2))?;
        let server_stream = listener.accept()?;
        let limits = IpcLimits {
            idle_timeout: Duration::from_millis(500),
            command_timeout: Duration::from_millis(200),
            connection_timeout: Duration::from_secs(1),
            ..IpcLimits::default()
        };
        let handler = thread::spawn(move || {
            handle_client_with_limits(&server_stream, limits, &AtomicBool::new(false))
        });

        for _ in 0..3 {
            client.write_all(b"x")?;
            thread::sleep(Duration::from_millis(50));
        }

        let mut buffer = [0u8; 1];
        assert_eq!(client.read(&mut buffer)?, 0);
        let error = handler.join().unwrap().unwrap_err();
        assert!(error.to_string().contains("command deadline exceeded"));

        drop(listener);
        remove_file_if_exists(&socket_path)?;
        Ok(())
    }

    #[test]
    fn activity_cannot_extend_connection_deadline() -> anyhow::Result<()> {
        let socket_path = test_socket_path("connection-deadline");
        remove_file_if_exists(&socket_path)?;
        let listener = UnixListener::bind(&socket_path)?;
        let mut client = UnixStream::connect(&socket_path)?;
        client.set_read_timeout(Duration::from_secs(2))?;
        let server_stream = listener.accept()?;
        let limits = IpcLimits {
            idle_timeout: Duration::from_millis(500),
            command_timeout: Duration::from_millis(500),
            connection_timeout: Duration::from_millis(200),
            ..IpcLimits::default()
        };
        let handler = thread::spawn(move || {
            handle_client_with_limits(&server_stream, limits, &AtomicBool::new(false))
        });

        for _ in 0..3 {
            client.write_all(b"\n")?;
            thread::sleep(Duration::from_millis(50));
        }

        let mut buffer = [0u8; 1];
        assert_eq!(client.read(&mut buffer)?, 0);
        let error = handler.join().unwrap().unwrap_err();
        assert!(error.to_string().contains("connection deadline exceeded"));

        drop(listener);
        remove_file_if_exists(&socket_path)?;
        Ok(())
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
