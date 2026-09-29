use std::{
    io::{self, Read},
    os::{
        fd::{AsFd, BorrowedFd},
        unix::net::UnixStream,
    },
};

use calloop::{Interest, LoopHandle, Mode, PostAction, generic::Generic};
use signal_hook::{
    SigId,
    consts::signal::{SIGHUP, SIGINT, SIGTERM},
    low_level::{pipe, unregister},
};

use super::{RuntimeError, state::RuntimeState};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownSignal {
    Interrupt,
    Terminate,
}

impl ShutdownSignal {
    pub fn name(self) -> &'static str {
        match self {
            Self::Interrupt => "SIGINT",
            Self::Terminate => "SIGTERM",
        }
    }
}

pub(super) fn register_sources(
    handle: &LoopHandle<'static, RuntimeState>,
    reload_enabled: bool,
) -> Result<(), RuntimeError> {
    register_source(handle, RuntimeSignal::Interrupt)?;
    register_source(handle, RuntimeSignal::Terminate)?;
    if reload_enabled {
        register_source(handle, RuntimeSignal::Reload)?;
    }

    Ok(())
}

fn register_source(
    handle: &LoopHandle<'static, RuntimeState>,
    signal: RuntimeSignal,
) -> Result<(), RuntimeError> {
    // a pipe turns signals into ordinary event loop readiness
    let source = SignalPipe::new(signal)?;

    handle
        .insert_source(
            Generic::new(source, Interest::READ, Mode::Level),
            move |_, source, state: &mut RuntimeState| {
                source.drain()?;
                match signal {
                    RuntimeSignal::Interrupt => {
                        state.request_shutdown(ShutdownSignal::Interrupt);
                    }
                    RuntimeSignal::Terminate => {
                        state.request_shutdown(ShutdownSignal::Terminate);
                    }
                    RuntimeSignal::Reload => {
                        state.request_reload();
                    }
                }
                Ok::<PostAction, io::Error>(PostAction::Continue)
            },
        )
        .map_err(|error| RuntimeError::RegisterSignalSource {
            signal: signal.name(),
            source: error.error,
        })?;

    Ok(())
}

#[derive(Debug, Clone, Copy)]
enum RuntimeSignal {
    Interrupt,
    Terminate,
    Reload,
}

impl RuntimeSignal {
    fn name(self) -> &'static str {
        match self {
            Self::Interrupt => "SIGINT",
            Self::Terminate => "SIGTERM",
            Self::Reload => "SIGHUP",
        }
    }

    fn number(self) -> i32 {
        match self {
            Self::Interrupt => SIGINT,
            Self::Terminate => SIGTERM,
            Self::Reload => SIGHUP,
        }
    }
}

struct SignalPipe {
    read: UnixStream,
    registration: SigId,
}

impl SignalPipe {
    fn new(signal: RuntimeSignal) -> Result<Self, RuntimeError> {
        let (read, write) =
            UnixStream::pair().map_err(|source| RuntimeError::CreateSignalPipe {
                signal: signal.name(),
                source,
            })?;
        read.set_nonblocking(true)
            .map_err(|source| RuntimeError::CreateSignalPipe {
                signal: signal.name(),
                source,
            })?;
        let registration = pipe::register(signal.number(), write).map_err(|source| {
            RuntimeError::RegisterSignalHandler {
                signal: signal.name(),
                source,
            }
        })?;

        Ok(Self { read, registration })
    }

    fn drain(&self) -> io::Result<()> {
        let mut buffer = [0_u8; 64];
        let mut read = &self.read;

        loop {
            match read.read(&mut buffer) {
                Ok(0) => return Ok(()),
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
    }
}

impl AsFd for SignalPipe {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.read.as_fd()
    }
}

impl Drop for SignalPipe {
    fn drop(&mut self) {
        unregister(self.registration);
    }
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, time::Duration};

    use calloop::EventLoop;
    use signal_hook::{
        consts::signal::{SIGHUP, SIGINT},
        low_level::raise,
    };

    use super::{RuntimeSignal, ShutdownSignal, register_source};
    use crate::{
        config::Config,
        listener::ListenerSet,
        runtime::{RuntimeEvent, reload::ConfigReloadMonitor, state::RuntimeState},
        wayland::IdleWaylandState,
    };

    #[test]
    fn sigint_wakes_the_event_loop_and_requests_shutdown() {
        let mut event_loop = match EventLoop::try_new() {
            Ok(event_loop) => event_loop,
            Err(error) => panic!("failed to create test event loop: {error}"),
        };
        let config = Config::default();
        let listeners = ListenerSet::from_config(&config);
        let mut state = RuntimeState::new(IdleWaylandState::default(), listeners, &config, None);

        if let Err(error) = register_source(&event_loop.handle(), RuntimeSignal::Interrupt) {
            panic!("failed to register signal sources: {error}");
        }
        if let Err(error) = raise(SIGINT) {
            panic!("failed to raise SIGINT: {error}");
        }
        if let Err(error) = event_loop.dispatch(Some(Duration::from_secs(1)), &mut state) {
            panic!("failed to dispatch signal event: {error}");
        }

        assert!(state.shutdown_requested());
        assert!(matches!(
            state.take_runtime_events().as_slice(),
            [RuntimeEvent::ShutdownRequested(ShutdownSignal::Interrupt)]
        ));
    }

    #[test]
    fn sighup_wakes_the_event_loop_and_requests_reload() {
        let mut event_loop = match EventLoop::try_new() {
            Ok(event_loop) => event_loop,
            Err(error) => panic!("failed to create test event loop: {error}"),
        };
        let config = Config::default();
        let listeners = ListenerSet::from_config(&config);
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("config/example.toml");
        let (_monitor, requester, _events) = match ConfigReloadMonitor::connect(path) {
            Ok(parts) => parts,
            Err(error) => panic!("failed to start reload monitor: {error}"),
        };
        let mut state = RuntimeState::new(
            IdleWaylandState::default(),
            listeners,
            &config,
            Some(requester),
        );

        if let Err(error) = register_source(&event_loop.handle(), RuntimeSignal::Reload) {
            panic!("failed to register reload signal source: {error}");
        }
        if let Err(error) = raise(SIGHUP) {
            panic!("failed to raise SIGHUP: {error}");
        }
        if let Err(error) = event_loop.dispatch(Some(Duration::from_secs(1)), &mut state) {
            panic!("failed to dispatch signal event: {error}");
        }

        assert!(matches!(
            state.take_runtime_events().as_slice(),
            [RuntimeEvent::ConfigReloadRequested]
        ));
    }
}
