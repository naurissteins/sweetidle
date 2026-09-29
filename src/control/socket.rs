use std::{
    env,
    ffi::OsString,
    fmt, fs,
    io::{self, Read, Write},
    os::unix::{
        fs::{FileTypeExt, MetadataExt, PermissionsExt},
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    sync::mpsc,
    thread::{self, JoinHandle},
    time::Duration,
};

use calloop::channel::{Channel, Sender as EventSender, channel};
use rustix::event::{PollFd, PollFlags, poll};

use super::{ControlError, ControlEvent, ControlRequest, StatusFormat};

const SOCKET_NAME: &str = "sweetsidle.sock";
const STATUS_COMMAND: &[u8] = b"status\n";
const STATUS_JSON_COMMAND: &[u8] = b"status-json\n";
const PAUSE_COMMAND: &[u8] = b"pause\n";
const RESUME_COMMAND: &[u8] = b"resume\n";
const RELOAD_COMMAND: &[u8] = b"reload\n";
const IO_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_REQUEST_BYTES: u64 = 64;
const MAX_RESPONSE_BYTES: u64 = 8 * 1024 * 1024;

pub(crate) struct ControlMonitor {
    shutdown: Option<UnixStream>,
    thread: Option<JoinHandle<()>>,
}

impl ControlMonitor {
    pub(crate) fn connect() -> Result<(Self, Channel<ControlEvent>), ControlError> {
        Self::connect_at(control_socket_path()?)
    }

    fn connect_at(socket_path: PathBuf) -> Result<(Self, Channel<ControlEvent>), ControlError> {
        let listener = bind_socket(&socket_path)?;
        let socket_file = SocketFile::new(socket_path.clone())?;
        let (shutdown, worker_shutdown) =
            UnixStream::pair().map_err(|source| ControlError::CreateShutdownChannel { source })?;
        let (event_sender, event_channel) = channel();
        let thread = thread::Builder::new()
            .name("sweetsidle-control".to_string())
            .spawn(move || run_worker(listener, socket_file, event_sender, worker_shutdown))
            .map_err(|source| ControlError::SpawnWorker {
                path: socket_path.clone(),
                source,
            })?;

        Ok((
            Self {
                shutdown: Some(shutdown),
                thread: Some(thread),
            },
            event_channel,
        ))
    }
}

impl Drop for ControlMonitor {
    fn drop(&mut self) {
        if let Some(mut shutdown) = self.shutdown.take() {
            let _ = shutdown.write_all(&[1]);
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub fn query_status() -> Result<String, ControlError> {
    send_command_at(&control_socket_path()?, STATUS_COMMAND)
}

pub fn query_status_json() -> Result<String, ControlError> {
    send_command_at(&control_socket_path()?, STATUS_JSON_COMMAND)
}

pub fn pause() -> Result<String, ControlError> {
    send_command_at(&control_socket_path()?, PAUSE_COMMAND)
}

pub fn resume() -> Result<String, ControlError> {
    send_command_at(&control_socket_path()?, RESUME_COMMAND)
}

pub fn reload() -> Result<String, ControlError> {
    send_command_at(&control_socket_path()?, RELOAD_COMMAND)
}

fn send_command_at(socket_path: &Path, command: &[u8]) -> Result<String, ControlError> {
    let mut stream = UnixStream::connect(socket_path).map_err(|source| ControlError::Connect {
        path: socket_path.to_path_buf(),
        source,
    })?;
    stream
        .set_read_timeout(Some(IO_TIMEOUT))
        .map_err(|source| ControlError::ConfigureClient { source })?;
    stream
        .set_write_timeout(Some(IO_TIMEOUT))
        .map_err(|source| ControlError::ConfigureClient { source })?;
    stream
        .write_all(command)
        .map_err(|source| ControlError::WriteRequest { source })?;
    stream
        .shutdown(std::net::Shutdown::Write)
        .map_err(|source| ControlError::WriteRequest { source })?;

    let mut response = Vec::new();
    stream
        .take(MAX_RESPONSE_BYTES + 1)
        .read_to_end(&mut response)
        .map_err(|source| ControlError::ReadResponse { source })?;
    if response.len() as u64 > MAX_RESPONSE_BYTES {
        return Err(ControlError::ResponseTooLarge);
    }
    let response = String::from_utf8(response).map_err(|_| ControlError::InvalidResponse)?;
    if response.is_empty() {
        return Err(ControlError::EmptyResponse);
    }
    if let Some(message) = response.strip_prefix("error: ") {
        return Err(ControlError::ServerRejected(message.trim_end().to_string()));
    }
    Ok(response)
}

fn control_socket_path() -> Result<PathBuf, ControlError> {
    let runtime_dir =
        env::var_os("XDG_RUNTIME_DIR").ok_or(ControlError::MissingRuntimeDirectory)?;
    runtime_socket_path(runtime_dir)
}

fn runtime_socket_path(runtime_dir: OsString) -> Result<PathBuf, ControlError> {
    let runtime_dir = PathBuf::from(runtime_dir);
    if !runtime_dir.is_absolute() {
        return Err(ControlError::RelativeRuntimeDirectory(runtime_dir));
    }
    Ok(runtime_dir.join(SOCKET_NAME))
}

fn bind_socket(path: &Path) -> Result<UnixListener, ControlError> {
    match UnixListener::bind(path) {
        Ok(listener) => secure_socket(listener, path),
        Err(source) if source.kind() == io::ErrorKind::AddrInUse => {
            replace_stale_socket(path)?;
            UnixListener::bind(path)
                .map_err(|source| ControlError::Bind {
                    path: path.to_path_buf(),
                    source,
                })
                .and_then(|listener| secure_socket(listener, path))
        }
        Err(source) => Err(ControlError::Bind {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn replace_stale_socket(path: &Path) -> Result<(), ControlError> {
    // only a refused connection proves an existing socket has no listener
    match UnixStream::connect(path) {
        Ok(_) => return Err(ControlError::AlreadyRunning(path.to_path_buf())),
        Err(source) if source.kind() == io::ErrorKind::ConnectionRefused => {}
        Err(source) => {
            return Err(ControlError::ProbeSocket {
                path: path.to_path_buf(),
                source,
            });
        }
    }

    let metadata = fs::symlink_metadata(path).map_err(|source| ControlError::InspectSocket {
        path: path.to_path_buf(),
        source,
    })?;
    if !metadata.file_type().is_socket() {
        return Err(ControlError::PathOccupied(path.to_path_buf()));
    }
    fs::remove_file(path).map_err(|source| ControlError::RemoveStaleSocket {
        path: path.to_path_buf(),
        source,
    })
}

fn secure_socket(listener: UnixListener, path: &Path) -> Result<UnixListener, ControlError> {
    if let Err(source) = fs::set_permissions(path, fs::Permissions::from_mode(0o600)) {
        let _ = fs::remove_file(path);
        return Err(ControlError::SetPermissions {
            path: path.to_path_buf(),
            source,
        });
    }
    Ok(listener)
}

fn run_worker(
    listener: UnixListener,
    _socket_file: SocketFile,
    events: EventSender<ControlEvent>,
    shutdown: UnixStream,
) {
    // poll shutdown and clients together so monitor drop can stop this thread
    loop {
        let mut poll_fds = [
            PollFd::new(&listener, PollFlags::IN),
            PollFd::new(&shutdown, PollFlags::IN),
        ];
        if let Err(source) = poll(&mut poll_fds, None) {
            let _ = events.send(ControlEvent::Failed(ControlError::Poll { source }));
            return;
        }
        if poll_fds[1]
            .revents()
            .intersects(PollFlags::IN | PollFlags::HUP)
        {
            return;
        }
        if !poll_fds[0].revents().contains(PollFlags::IN) {
            continue;
        }

        let (stream, _) = match listener.accept() {
            Ok(connection) => connection,
            Err(source) => {
                let _ = events.send(ControlEvent::Failed(ControlError::Accept { source }));
                return;
            }
        };
        if !serve_client(stream, &events) {
            return;
        }
    }
}

fn serve_client(mut stream: UnixStream, events: &EventSender<ControlEvent>) -> bool {
    let result = handle_client(&mut stream, events);
    match result {
        Ok(()) => true,
        Err(ClientRequestError::RuntimeStopped) => false,
        Err(error) => {
            let _ = writeln!(stream, "error: {error}");
            true
        }
    }
}

fn handle_client(
    stream: &mut UnixStream,
    events: &EventSender<ControlEvent>,
) -> Result<(), ClientRequestError> {
    // bound each exchange so a client cannot hold the worker indefinitely
    stream
        .set_read_timeout(Some(IO_TIMEOUT))
        .map_err(ClientRequestError::Io)?;
    stream
        .set_write_timeout(Some(IO_TIMEOUT))
        .map_err(ClientRequestError::Io)?;

    let mut request = Vec::new();
    stream
        .take(MAX_REQUEST_BYTES + 1)
        .read_to_end(&mut request)
        .map_err(ClientRequestError::Io)?;
    if request.len() as u64 > MAX_REQUEST_BYTES {
        return Err(ClientRequestError::TooLarge);
    }
    let command = request_command(&request)?;
    let (reply_sender, reply_receiver) = mpsc::sync_channel(1);
    let control_request = ControlRequest::new(reply_sender);
    let event = match command {
        ControlCommand::Status(format) => ControlEvent::Status {
            request: control_request,
            format,
        },
        ControlCommand::Pause => ControlEvent::Pause(control_request),
        ControlCommand::Resume => ControlEvent::Resume(control_request),
        ControlCommand::Reload => ControlEvent::Reload(control_request),
    };
    events
        .send(event)
        .map_err(|_| ClientRequestError::RuntimeStopped)?;
    let response = reply_receiver
        .recv_timeout(IO_TIMEOUT)
        .map_err(|error| match error {
            mpsc::RecvTimeoutError::Timeout => ClientRequestError::RuntimeTimeout,
            mpsc::RecvTimeoutError::Disconnected => ClientRequestError::RuntimeStopped,
        })?;
    stream
        .write_all(response.as_bytes())
        .map_err(ClientRequestError::Io)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ControlCommand {
    Status(StatusFormat),
    Pause,
    Resume,
    Reload,
}

fn request_command(request: &[u8]) -> Result<ControlCommand, ClientRequestError> {
    match request {
        STATUS_COMMAND => Ok(ControlCommand::Status(StatusFormat::Text)),
        STATUS_JSON_COMMAND => Ok(ControlCommand::Status(StatusFormat::Json)),
        PAUSE_COMMAND => Ok(ControlCommand::Pause),
        RESUME_COMMAND => Ok(ControlCommand::Resume),
        RELOAD_COMMAND => Ok(ControlCommand::Reload),
        _ => Err(ClientRequestError::UnknownCommand),
    }
}

struct SocketFile {
    path: PathBuf,
    device: u64,
    inode: u64,
}

impl SocketFile {
    fn new(path: PathBuf) -> Result<Self, ControlError> {
        let metadata =
            fs::symlink_metadata(&path).map_err(|source| ControlError::InspectSocket {
                path: path.clone(),
                source,
            })?;
        Ok(Self {
            path,
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
}

impl Drop for SocketFile {
    fn drop(&mut self) {
        let owned = fs::symlink_metadata(&self.path)
            .map(|metadata| metadata.dev() == self.device && metadata.ino() == self.inode)
            .unwrap_or(false);
        if owned {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[derive(Debug)]
enum ClientRequestError {
    Io(io::Error),
    TooLarge,
    UnknownCommand,
    RuntimeTimeout,
    RuntimeStopped,
}

impl fmt::Display for ClientRequestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(source) => write!(formatter, "control request I/O failed: {source}"),
            Self::TooLarge => write!(formatter, "control request is too large"),
            Self::UnknownCommand => write!(formatter, "unknown control command"),
            Self::RuntimeTimeout => write!(formatter, "daemon did not answer in time"),
            Self::RuntimeStopped => write!(formatter, "daemon stopped before answering"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        io::Read,
        os::unix::{
            fs::PermissionsExt,
            net::{UnixListener, UnixStream},
        },
        thread,
    };

    use tempfile::TempDir;

    use super::{
        ControlError, ControlEvent, ControlMonitor, PAUSE_COMMAND, STATUS_COMMAND,
        STATUS_JSON_COMMAND, StatusFormat, send_command_at,
    };

    #[test]
    fn status_request_round_trips_through_the_worker() {
        let temp = TempDir::new().unwrap_or_else(|error| panic!("temp dir failed: {error}"));
        let path = temp.path().join("control.sock");
        let (monitor, events) = ControlMonitor::connect_at(path.clone())
            .unwrap_or_else(|error| panic!("control monitor failed: {error}"));
        let client_path = path.clone();
        let client = thread::spawn(move || send_command_at(&client_path, STATUS_COMMAND));

        let event = events
            .recv()
            .unwrap_or_else(|error| panic!("control event missing: {error}"));
        match event {
            ControlEvent::Status { request, format } => {
                assert_eq!(format, StatusFormat::Text);
                request.respond("status response\n".to_string());
            }
            ControlEvent::Pause(_) | ControlEvent::Resume(_) | ControlEvent::Reload(_) => {
                panic!("unexpected control command")
            }
            ControlEvent::Failed(error) => panic!("control worker failed: {error}"),
        }

        let response = client
            .join()
            .unwrap_or_else(|_| panic!("control client panicked"))
            .unwrap_or_else(|error| panic!("status request failed: {error}"));
        assert_eq!(response, "status response\n");
        let mode = fs::metadata(&path)
            .unwrap_or_else(|error| panic!("socket metadata failed: {error}"))
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        drop(monitor);
        assert!(!path.exists());
    }

    #[test]
    fn json_status_request_round_trips_through_the_worker() {
        let temp = TempDir::new().unwrap_or_else(|error| panic!("temp dir failed: {error}"));
        let path = temp.path().join("control.sock");
        let (monitor, events) = ControlMonitor::connect_at(path.clone())
            .unwrap_or_else(|error| panic!("control monitor failed: {error}"));
        let client_path = path.clone();
        let client = thread::spawn(move || send_command_at(&client_path, STATUS_JSON_COMMAND));

        let event = events
            .recv()
            .unwrap_or_else(|error| panic!("control event missing: {error}"));
        match event {
            ControlEvent::Status { request, format } => {
                assert_eq!(format, StatusFormat::Json);
                request.respond("{\"paused\":false}\n".to_string());
            }
            ControlEvent::Pause(_) | ControlEvent::Resume(_) | ControlEvent::Reload(_) => {
                panic!("unexpected control command")
            }
            ControlEvent::Failed(error) => panic!("control worker failed: {error}"),
        }

        let response = client
            .join()
            .unwrap_or_else(|_| panic!("control client panicked"))
            .unwrap_or_else(|error| panic!("JSON status request failed: {error}"));
        assert_eq!(response, "{\"paused\":false}\n");
        drop(monitor);
    }

    #[test]
    fn unknown_command_returns_an_error_without_stopping_the_worker() {
        let temp = TempDir::new().unwrap_or_else(|error| panic!("temp dir failed: {error}"));
        let path = temp.path().join("control.sock");
        let (monitor, events) = ControlMonitor::connect_at(path.clone())
            .unwrap_or_else(|error| panic!("control monitor failed: {error}"));
        let mut stream = UnixStream::connect(&path)
            .unwrap_or_else(|error| panic!("control connection failed: {error}"));
        std::io::Write::write_all(&mut stream, b"banana\n")
            .unwrap_or_else(|error| panic!("control write failed: {error}"));
        stream
            .shutdown(std::net::Shutdown::Write)
            .unwrap_or_else(|error| panic!("control shutdown failed: {error}"));

        let mut response = String::new();
        stream
            .read_to_string(&mut response)
            .unwrap_or_else(|error| panic!("control read failed: {error}"));

        assert_eq!(response, "error: unknown control command\n");

        let client_path = path.clone();
        let client = thread::spawn(move || send_command_at(&client_path, STATUS_COMMAND));
        let event = events
            .recv()
            .unwrap_or_else(|error| panic!("control event missing: {error}"));
        match event {
            ControlEvent::Status { request, format } => {
                assert_eq!(format, StatusFormat::Text);
                request.respond("still running\n".to_string());
            }
            ControlEvent::Pause(_) | ControlEvent::Resume(_) | ControlEvent::Reload(_) => {
                panic!("unexpected control command")
            }
            ControlEvent::Failed(error) => panic!("control worker failed: {error}"),
        }
        let follow_up = client
            .join()
            .unwrap_or_else(|_| panic!("control client panicked"))
            .unwrap_or_else(|error| panic!("follow-up status failed: {error}"));
        assert_eq!(follow_up, "still running\n");
        drop(monitor);
    }

    #[test]
    fn pause_command_round_trips_through_the_worker() {
        let temp = TempDir::new().unwrap_or_else(|error| panic!("temp dir failed: {error}"));
        let path = temp.path().join("control.sock");
        let (monitor, events) = ControlMonitor::connect_at(path.clone())
            .unwrap_or_else(|error| panic!("control monitor failed: {error}"));
        let client_path = path.clone();
        let client = thread::spawn(move || send_command_at(&client_path, PAUSE_COMMAND));

        let event = events
            .recv()
            .unwrap_or_else(|error| panic!("control event missing: {error}"));
        match event {
            ControlEvent::Pause(request) => request.respond("sweetsidle: paused\n".to_string()),
            ControlEvent::Status { .. } | ControlEvent::Resume(_) | ControlEvent::Reload(_) => {
                panic!("unexpected control command")
            }
            ControlEvent::Failed(error) => panic!("control worker failed: {error}"),
        }

        let response = client
            .join()
            .unwrap_or_else(|_| panic!("control client panicked"))
            .unwrap_or_else(|error| panic!("pause request failed: {error}"));
        assert_eq!(response, "sweetsidle: paused\n");
        drop(monitor);
    }

    #[test]
    fn active_socket_prevents_a_second_monitor() {
        let temp = TempDir::new().unwrap_or_else(|error| panic!("temp dir failed: {error}"));
        let path = temp.path().join("control.sock");
        let (monitor, _events) = ControlMonitor::connect_at(path.clone())
            .unwrap_or_else(|error| panic!("control monitor failed: {error}"));

        let error = match ControlMonitor::connect_at(path.clone()) {
            Ok(_) => panic!("second control monitor unexpectedly started"),
            Err(error) => error,
        };

        assert!(matches!(error, ControlError::AlreadyRunning(found) if found == path));
        drop(monitor);
    }

    #[test]
    fn shutdown_does_not_unlink_a_replacement_socket() {
        let temp = TempDir::new().unwrap_or_else(|error| panic!("temp dir failed: {error}"));
        let path = temp.path().join("control.sock");
        let (monitor, _events) = ControlMonitor::connect_at(path.clone())
            .unwrap_or_else(|error| panic!("control monitor failed: {error}"));
        fs::remove_file(&path)
            .unwrap_or_else(|error| panic!("failed to unlink original socket: {error}"));
        let replacement = UnixListener::bind(&path)
            .unwrap_or_else(|error| panic!("replacement socket failed: {error}"));

        drop(monitor);

        assert!(path.exists());
        drop(replacement);
        fs::remove_file(path)
            .unwrap_or_else(|error| panic!("replacement socket cleanup failed: {error}"));
    }
}
