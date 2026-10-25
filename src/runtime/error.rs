use std::{error::Error, fmt, io};

use crate::{
    command::{CommandDispatchError, CommandId, ProcessError},
    control::ControlError,
    listener::{ListenerError, ListenerId},
    session::{DbusInhibitError, SessionError},
    wayland::IdleWaylandError,
};

// keep errors typed across event sources until the binary logs them
#[derive(Debug)]
pub enum RuntimeError {
    Wayland(IdleWaylandError),
    CreateEventLoop(calloop::Error),
    RegisterWayland(calloop::Error),
    SpawnReloadWorker(io::Error),
    RegisterReload(calloop::Error),
    ReloadRequestChannelClosed,
    ReloadResultChannelClosed,
    Session(SessionError),
    RegisterSession(calloop::Error),
    SessionChannelClosed,
    IdleHintMonitorMissing,
    DbusInhibit(DbusInhibitError),
    RegisterDbusInhibit(calloop::Error),
    DbusInhibitChannelClosed,
    Control(ControlError),
    RegisterControl(calloop::Error),
    ControlChannelClosed,
    CreateSignalPipe {
        signal: &'static str,
        source: io::Error,
    },
    RegisterSignalHandler {
        signal: &'static str,
        source: io::Error,
    },
    RegisterSignalSource {
        signal: &'static str,
        source: calloop::Error,
    },
    Dispatch(calloop::Error),
    CompositorDisconnected,
    Listener(ListenerError),
    Process(ProcessError),
    CommandDispatch(CommandDispatchError),
    InvalidProcessId(u32),
    MonitorProcess {
        command_id: CommandId,
        process_id: u32,
        source: rustix::io::Errno,
    },
    RegisterProcessMonitor {
        command_id: CommandId,
        source: calloop::Error,
    },
    ProcessNotReady(CommandId),
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Wayland(source) => {
                write!(formatter, "Wayland idle protocol failed: {source}")
            }
            Self::CreateEventLoop(source) => {
                write!(formatter, "failed to create event loop: {source}")
            }
            Self::RegisterWayland(source) => {
                write!(
                    formatter,
                    "failed to register Wayland event source: {source}"
                )
            }
            Self::SpawnReloadWorker(source) => {
                write!(formatter, "failed to start config reload worker: {source}")
            }
            Self::RegisterReload(source) => {
                write!(
                    formatter,
                    "failed to register config reload source: {source}"
                )
            }
            Self::ReloadRequestChannelClosed => {
                write!(formatter, "config reload request channel closed")
            }
            Self::ReloadResultChannelClosed => {
                write!(formatter, "config reload result channel closed")
            }
            Self::Session(source) => write!(formatter, "logind integration failed: {source}"),
            Self::RegisterSession(source) => {
                write!(
                    formatter,
                    "failed to register logind event source: {source}"
                )
            }
            Self::SessionChannelClosed => write!(formatter, "logind event channel closed"),
            Self::IdleHintMonitorMissing => {
                write!(formatter, "idle hint update has no active logind monitor")
            }
            Self::DbusInhibit(source) => {
                write!(formatter, "D-Bus idle inhibition failed: {source}")
            }
            Self::RegisterDbusInhibit(source) => {
                write!(
                    formatter,
                    "failed to register D-Bus inhibitor event source: {source}"
                )
            }
            Self::DbusInhibitChannelClosed => {
                write!(formatter, "D-Bus inhibitor event channel closed")
            }
            Self::Control(source) => write!(formatter, "control socket failed: {source}"),
            Self::RegisterControl(source) => {
                write!(
                    formatter,
                    "failed to register control socket source: {source}"
                )
            }
            Self::ControlChannelClosed => write!(formatter, "control socket event channel closed"),
            Self::CreateSignalPipe { signal, source } => {
                write!(formatter, "failed to create {signal} signal pipe: {source}")
            }
            Self::RegisterSignalHandler { signal, source } => {
                write!(formatter, "failed to register {signal} handler: {source}")
            }
            Self::RegisterSignalSource { signal, source } => {
                write!(
                    formatter,
                    "failed to register {signal} event source: {source}"
                )
            }
            Self::Dispatch(source) => write!(formatter, "event loop dispatch failed: {source}"),
            Self::CompositorDisconnected => {
                write!(formatter, "compositor closed the Wayland connection")
            }
            Self::Listener(ListenerError::UnknownListener(listener_id)) => {
                display_unknown_listener(formatter, *listener_id)
            }
            Self::Listener(ListenerError::TimedOutListenerChanged(listener_id)) => write!(
                formatter,
                "cannot change or remove timed-out listener {}; retry reload after activity",
                listener_id.index()
            ),
            Self::Process(source) => write!(formatter, "command process failed: {source}"),
            Self::CommandDispatch(source) => {
                write!(formatter, "command lifecycle state failed: {source}")
            }
            Self::InvalidProcessId(process_id) => {
                write!(
                    formatter,
                    "process {process_id} has an invalid Linux process id"
                )
            }
            Self::MonitorProcess {
                command_id,
                process_id,
                source,
            } => write!(
                formatter,
                "failed to monitor process {process_id} for command {}: {source}",
                command_id.index()
            ),
            Self::RegisterProcessMonitor { command_id, source } => write!(
                formatter,
                "failed to register process monitor for command {}: {source}",
                command_id.index()
            ),
            Self::ProcessNotReady(command_id) => write!(
                formatter,
                "process monitor fired before command {} exited",
                command_id.index()
            ),
        }
    }
}

impl Error for RuntimeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Wayland(source) => Some(source),
            Self::CreateEventLoop(source)
            | Self::RegisterWayland(source)
            | Self::RegisterReload(source)
            | Self::RegisterSession(source)
            | Self::RegisterDbusInhibit(source)
            | Self::RegisterControl(source)
            | Self::Dispatch(source)
            | Self::RegisterProcessMonitor { source, .. } => Some(source),
            Self::CreateSignalPipe { source, .. } | Self::RegisterSignalHandler { source, .. } => {
                Some(source)
            }
            Self::SpawnReloadWorker(source) => Some(source),
            Self::RegisterSignalSource { source, .. } => Some(source),
            Self::Process(source) => Some(source),
            Self::Session(source) => Some(source),
            Self::DbusInhibit(source) => Some(source),
            Self::Control(source) => Some(source),
            Self::CommandDispatch(source) => Some(source),
            Self::MonitorProcess { source, .. } => Some(source),
            Self::Listener(_)
            | Self::ReloadRequestChannelClosed
            | Self::ReloadResultChannelClosed
            | Self::SessionChannelClosed
            | Self::IdleHintMonitorMissing
            | Self::DbusInhibitChannelClosed
            | Self::ControlChannelClosed
            | Self::CompositorDisconnected
            | Self::InvalidProcessId(_)
            | Self::ProcessNotReady(_) => None,
        }
    }
}

impl From<IdleWaylandError> for RuntimeError {
    fn from(source: IdleWaylandError) -> Self {
        Self::Wayland(source)
    }
}

impl From<SessionError> for RuntimeError {
    fn from(source: SessionError) -> Self {
        Self::Session(source)
    }
}

impl From<DbusInhibitError> for RuntimeError {
    fn from(source: DbusInhibitError) -> Self {
        Self::DbusInhibit(source)
    }
}

fn display_unknown_listener(
    formatter: &mut fmt::Formatter<'_>,
    listener_id: ListenerId,
) -> fmt::Result {
    write!(
        formatter,
        "idle event referenced unknown listener {}",
        listener_id.index()
    )
}
