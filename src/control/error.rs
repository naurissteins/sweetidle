use std::{error::Error, fmt, io, path::PathBuf};

#[derive(Debug)]
pub enum ControlError {
    MissingRuntimeDirectory,
    RelativeRuntimeDirectory(PathBuf),
    AlreadyRunning(PathBuf),
    PathOccupied(PathBuf),
    Bind { path: PathBuf, source: io::Error },
    ProbeSocket { path: PathBuf, source: io::Error },
    InspectSocket { path: PathBuf, source: io::Error },
    RemoveStaleSocket { path: PathBuf, source: io::Error },
    SetPermissions { path: PathBuf, source: io::Error },
    CreateShutdownChannel { source: io::Error },
    SpawnWorker { path: PathBuf, source: io::Error },
    Poll { source: rustix::io::Errno },
    Accept { source: io::Error },
    Connect { path: PathBuf, source: io::Error },
    ConfigureClient { source: io::Error },
    WriteRequest { source: io::Error },
    ReadResponse { source: io::Error },
    ResponseTooLarge,
    InvalidResponse,
    EmptyResponse,
    ServerRejected(String),
}

impl fmt::Display for ControlError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingRuntimeDirectory => write!(formatter, "XDG_RUNTIME_DIR is not set"),
            Self::RelativeRuntimeDirectory(path) => write!(
                formatter,
                "XDG_RUNTIME_DIR must be absolute, got `{}`",
                path.display()
            ),
            Self::AlreadyRunning(path) => write!(
                formatter,
                "another sweetsidle instance is listening at `{}`",
                path.display()
            ),
            Self::PathOccupied(path) => write!(
                formatter,
                "control socket path `{}` exists and is not a socket",
                path.display()
            ),
            Self::Bind { path, source } => write!(
                formatter,
                "failed to bind control socket `{}`: {source}",
                path.display()
            ),
            Self::ProbeSocket { path, source } => write!(
                formatter,
                "failed to check existing control socket `{}`: {source}",
                path.display()
            ),
            Self::InspectSocket { path, source } => write!(
                formatter,
                "failed to inspect control socket `{}`: {source}",
                path.display()
            ),
            Self::RemoveStaleSocket { path, source } => write!(
                formatter,
                "failed to remove stale control socket `{}`: {source}",
                path.display()
            ),
            Self::SetPermissions { path, source } => write!(
                formatter,
                "failed to secure control socket `{}`: {source}",
                path.display()
            ),
            Self::CreateShutdownChannel { source } => {
                write!(
                    formatter,
                    "failed to create control shutdown channel: {source}"
                )
            }
            Self::SpawnWorker { path, source } => write!(
                formatter,
                "failed to start control worker for `{}`: {source}",
                path.display()
            ),
            Self::Poll { source } => write!(formatter, "control socket poll failed: {source}"),
            Self::Accept { source } => write!(formatter, "control socket accept failed: {source}"),
            Self::Connect { path, source } => write!(
                formatter,
                "failed to connect to sweetsidle at `{}`: {source}",
                path.display()
            ),
            Self::ConfigureClient { source } => {
                write!(
                    formatter,
                    "failed to configure control connection: {source}"
                )
            }
            Self::WriteRequest { source } => {
                write!(formatter, "failed to send control request: {source}")
            }
            Self::ReadResponse { source } => {
                write!(formatter, "failed to read control response: {source}")
            }
            Self::ResponseTooLarge => write!(formatter, "control response is too large"),
            Self::InvalidResponse => write!(formatter, "control response is not valid UTF-8"),
            Self::EmptyResponse => write!(formatter, "daemon closed the control connection"),
            Self::ServerRejected(message) => {
                write!(formatter, "daemon rejected the control request: {message}")
            }
        }
    }
}

impl Error for ControlError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Bind { source, .. }
            | Self::ProbeSocket { source, .. }
            | Self::InspectSocket { source, .. }
            | Self::RemoveStaleSocket { source, .. }
            | Self::SetPermissions { source, .. }
            | Self::CreateShutdownChannel { source }
            | Self::SpawnWorker { source, .. }
            | Self::Accept { source }
            | Self::Connect { source, .. }
            | Self::ConfigureClient { source }
            | Self::WriteRequest { source }
            | Self::ReadResponse { source } => Some(source),
            Self::Poll { source } => Some(source),
            Self::MissingRuntimeDirectory
            | Self::RelativeRuntimeDirectory(_)
            | Self::AlreadyRunning(_)
            | Self::PathOccupied(_)
            | Self::ResponseTooLarge
            | Self::InvalidResponse
            | Self::EmptyResponse
            | Self::ServerRejected(_) => None,
        }
    }
}
