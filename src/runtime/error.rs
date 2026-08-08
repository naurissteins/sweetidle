use std::{error::Error, fmt, io};

use crate::{
    command::{CommandDispatchError, CommandId, ProcessError},
    listener::{ListenerError, ListenerId},
    wayland::IdleWaylandError,
};

#[derive(Debug)]
pub enum RuntimeError {
    Wayland(IdleWaylandError),
    CreateEventLoop(calloop::Error),
    RegisterWayland(calloop::Error),
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
                write!(formatter, "failed to initialize idle protocol: {source}")
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
            Self::Listener(ListenerError::UnknownListener(listener_id)) => {
                display_unknown_listener(formatter, *listener_id)
            }
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
            | Self::Dispatch(source)
            | Self::RegisterProcessMonitor { source, .. } => Some(source),
            Self::CreateSignalPipe { source, .. } | Self::RegisterSignalHandler { source, .. } => {
                Some(source)
            }
            Self::RegisterSignalSource { source, .. } => Some(source),
            Self::Process(source) => Some(source),
            Self::CommandDispatch(source) => Some(source),
            Self::MonitorProcess { source, .. } => Some(source),
            Self::Listener(_) | Self::InvalidProcessId(_) | Self::ProcessNotReady(_) => None,
        }
    }
}

impl From<IdleWaylandError> for RuntimeError {
    fn from(source: IdleWaylandError) -> Self {
        Self::Wayland(source)
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
