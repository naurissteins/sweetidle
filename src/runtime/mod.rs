mod error;
mod shutdown;
mod state;

use std::time::Duration;

use calloop::{EventLoop, channel::Event as ChannelEvent};
use calloop_wayland_source::WaylandSource;

pub use error::RuntimeError;
pub use shutdown::ShutdownSignal;

use self::{shutdown::register_sources, state::RuntimeState};
use crate::{
    command::{FinishedCommand, ProcessCompletion, ProcessError, ProcessStart, SkipDecision},
    config::Config,
    listener::ListenerSet,
    session::LogindMonitor,
    wayland::IdleWaylandConnection,
};

#[derive(Debug)]
pub enum RuntimeEvent {
    ShutdownRequested(ShutdownSignal),
    PreparingForSleep,
    ResumedFromSleep,
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
    _logind: Option<LogindMonitor>,
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
        let logind = register_logind(config, &event_loop)?;

        Ok(Self {
            event_loop,
            state: RuntimeState::new(parts.state, listeners, config),
            _logind: logind,
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
            self.state.discard_pending_events();
            return Ok(self.state.take_runtime_events());
        }

        self.state.route_session_events()?;
        self.state.route_wayland_events()?;
        self.state
            .dispatch_command_intents(&self.event_loop.handle())?;

        Ok(self.state.take_runtime_events())
    }
}

fn register_logind(
    config: &Config,
    event_loop: &EventLoop<'static, RuntimeState>,
) -> Result<Option<LogindMonitor>, RuntimeError> {
    let Some(use_delay_inhibitor) = logind_delay_policy(config) else {
        return Ok(None);
    };

    let (monitor, source) = LogindMonitor::connect(use_delay_inhibitor)?;
    event_loop
        .handle()
        .insert_source(source, |event, _, state: &mut RuntimeState| match event {
            ChannelEvent::Msg(event) => state.record_session_event(event),
            ChannelEvent::Closed => state.mark_session_channel_closed(),
        })
        .map_err(|error| RuntimeError::RegisterSession(error.error))?;

    Ok(Some(monitor))
}

fn logind_delay_policy(config: &Config) -> Option<bool> {
    let has_before_sleep = config.general.before_sleep_command.is_some();
    let has_after_resume = config.general.after_resume_command.is_some();

    (has_before_sleep || has_after_resume)
        .then_some(has_before_sleep && config.general.wait_before_sleep)
}

#[cfg(test)]
mod tests {
    use super::logind_delay_policy;
    use crate::config::Config;

    #[test]
    fn starts_logind_without_an_inhibitor_for_fire_and_forget_hooks() {
        let mut config = Config::default();
        config.general.before_sleep_command = Some("lock".to_string());
        config.general.wait_before_sleep = false;

        assert_eq!(logind_delay_policy(&config), Some(false));
    }

    #[test]
    fn skips_logind_when_no_sleep_hooks_are_configured() {
        assert_eq!(logind_delay_policy(&Config::default()), None);
    }
}
