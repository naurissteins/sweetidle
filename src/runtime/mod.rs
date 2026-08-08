mod error;
mod shutdown;
mod state;

use std::time::Duration;

use calloop::EventLoop;
use calloop_wayland_source::WaylandSource;

pub use error::RuntimeError;
pub use shutdown::ShutdownSignal;

use self::{shutdown::register_sources, state::RuntimeState};
use crate::{
    command::{FinishedCommand, ProcessCompletion, ProcessError, ProcessStart, SkipDecision},
    config::Config,
    listener::ListenerSet,
    wayland::IdleWaylandConnection,
};

#[derive(Debug)]
pub enum RuntimeEvent {
    ShutdownRequested(ShutdownSignal),
    CommandStarted(ProcessStart),
    CommandSkipped(SkipDecision),
    CommandFinished {
        command: FinishedCommand,
        completion: ProcessCompletion,
    },
    CommandFailed {
        command: FinishedCommand,
        error: ProcessError,
    },
}

pub struct IdleRuntime {
    event_loop: EventLoop<'static, RuntimeState>,
    state: RuntimeState,
}

impl IdleRuntime {
    pub fn connect(config: &Config) -> Result<Self, RuntimeError> {
        let listeners = ListenerSet::from_config(config);
        let wayland = IdleWaylandConnection::connect(&listeners.timeout_requests())?;
        let parts = wayland.into_parts();
        let event_loop = EventLoop::try_new().map_err(RuntimeError::CreateEventLoop)?;
        let source = WaylandSource::new(parts.connection, parts.event_queue);

        event_loop
            .handle()
            .insert_source(source, |_, event_queue, state: &mut RuntimeState| {
                event_queue.dispatch_pending(&mut state.wayland)
            })
            .map_err(|error| RuntimeError::RegisterWayland(error.error))?;
        register_sources(&event_loop.handle())?;

        Ok(Self {
            event_loop,
            state: RuntimeState::new(parts.state, listeners),
        })
    }

    pub fn dispatch(
        &mut self,
        timeout: Option<Duration>,
    ) -> Result<Vec<RuntimeEvent>, RuntimeError> {
        self.event_loop
            .dispatch(timeout, &mut self.state)
            .map_err(RuntimeError::Dispatch)?;
        self.state.collect_ready_processes()?;

        if self.state.shutdown_requested() {
            self.state.wayland.take_events();
            return Ok(self.state.take_runtime_events());
        }

        self.state.route_wayland_events()?;
        self.state
            .dispatch_command_intents(&self.event_loop.handle())?;

        Ok(self.state.take_runtime_events())
    }
}
