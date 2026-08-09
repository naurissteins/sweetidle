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
    consts::signal::{SIGINT, SIGTERM},
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

    fn number(self) -> i32 {
        match self {
            Self::Interrupt => SIGINT,
            Self::Terminate => SIGTERM,
        }
    }
}

pub(super) fn register_sources(
    handle: &LoopHandle<'static, RuntimeState>,
) -> Result<(), RuntimeError> {
    register_source(handle, ShutdownSignal::Interrupt)?;
    register_source(handle, ShutdownSignal::Terminate)
}

fn register_source(
    handle: &LoopHandle<'static, RuntimeState>,
    signal: ShutdownSignal,
) -> Result<(), RuntimeError> {
    let source = SignalPipe::new(signal)?;

    handle
        .insert_source(
            Generic::new(source, Interest::READ, Mode::Level),
            move |_, source, state: &mut RuntimeState| {
                source.drain()?;
                state.request_shutdown(signal);
                Ok::<PostAction, io::Error>(PostAction::Continue)
            },
        )
        .map_err(|error| RuntimeError::RegisterSignalSource {
            signal: signal.name(),
            source: error.error,
        })?;

    Ok(())
}

struct SignalPipe {
    read: UnixStream,
    registration: SigId,
}

impl SignalPipe {
    fn new(signal: ShutdownSignal) -> Result<Self, RuntimeError> {
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
    use std::time::Duration;

    use calloop::EventLoop;
    use signal_hook::{consts::signal::SIGINT, low_level::raise};

    use super::{ShutdownSignal, register_sources};
    use crate::{
        config::Config,
        listener::ListenerSet,
        runtime::{RuntimeEvent, state::RuntimeState},
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
        let mut state = RuntimeState::new(IdleWaylandState::default(), listeners, &config);

        if let Err(error) = register_sources(&event_loop.handle()) {
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
}
