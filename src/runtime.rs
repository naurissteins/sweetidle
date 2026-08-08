use std::{error::Error, fmt, time::Duration};

use calloop::EventLoop;
use calloop_wayland_source::WaylandSource;

use crate::{
    config::Config,
    listener::{CommandIntent, ListenerError, ListenerSet},
    wayland::{IdleEvent, IdleWaylandConnection, IdleWaylandError},
};

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

        Ok(Self {
            event_loop,
            state: RuntimeState::new(parts.state, listeners),
        })
    }

    pub fn dispatch(
        &mut self,
        timeout: Option<Duration>,
    ) -> Result<Vec<CommandIntent>, RuntimeError> {
        self.event_loop
            .dispatch(timeout, &mut self.state)
            .map_err(RuntimeError::Dispatch)?;
        self.state.route_wayland_events()?;

        Ok(self.state.take_command_intents())
    }
}

#[derive(Debug)]
pub enum RuntimeError {
    Wayland(IdleWaylandError),
    CreateEventLoop(calloop::Error),
    RegisterWayland(calloop::Error),
    Dispatch(calloop::Error),
    Listener(ListenerError),
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
            Self::Dispatch(source) => write!(formatter, "event loop dispatch failed: {source}"),
            Self::Listener(ListenerError::UnknownListener(listener_id)) => write!(
                formatter,
                "idle event referenced unknown listener {}",
                listener_id.index()
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
            | Self::Dispatch(source) => Some(source),
            Self::Listener(_) => None,
        }
    }
}

impl From<IdleWaylandError> for RuntimeError {
    fn from(source: IdleWaylandError) -> Self {
        Self::Wayland(source)
    }
}

struct RuntimeState {
    wayland: crate::wayland::IdleWaylandState,
    listeners: ListenerSet,
    command_intents: Vec<CommandIntent>,
}

impl RuntimeState {
    fn new(wayland: crate::wayland::IdleWaylandState, listeners: ListenerSet) -> Self {
        Self {
            wayland,
            listeners,
            command_intents: Vec::new(),
        }
    }

    fn route_wayland_events(&mut self) -> Result<(), RuntimeError> {
        for event in self.wayland.take_events() {
            self.route_idle_event(event)?;
        }

        Ok(())
    }

    fn route_idle_event(&mut self, event: IdleEvent) -> Result<(), RuntimeError> {
        match event {
            IdleEvent::Idled(listener_id) => {
                if let Some(intent) = self
                    .listeners
                    .mark_timed_out(listener_id)
                    .map_err(RuntimeError::Listener)?
                {
                    self.command_intents.push(intent);
                }
            }
            IdleEvent::Resumed(_) => {
                self.command_intents.extend(self.listeners.mark_active());
            }
        }

        Ok(())
    }

    fn take_command_intents(&mut self) -> Vec<CommandIntent> {
        std::mem::take(&mut self.command_intents)
    }
}

#[cfg(test)]
mod tests {
    use super::{RuntimeError, RuntimeState};
    use crate::{
        config::{Config, GeneralConfig, ListenerConfig},
        listener::{CommandIntent, CommandKind, ListenerError, ListenerId, ListenerSet},
        wayland::{IdleEvent, IdleWaylandState},
    };

    #[test]
    fn routes_timeout_once_until_listener_resumes() {
        let mut state = runtime_state();

        state
            .route_idle_event(IdleEvent::Idled(ListenerId::from_index(0)))
            .assert_ok();
        state
            .route_idle_event(IdleEvent::Idled(ListenerId::from_index(0)))
            .assert_ok();

        assert_eq!(
            state.take_command_intents(),
            vec![intent(0, CommandKind::Timeout, "lock")]
        );
    }

    #[test]
    fn activity_resumes_only_listeners_that_timed_out() {
        let mut state = runtime_state();
        state
            .route_idle_event(IdleEvent::Idled(ListenerId::from_index(1)))
            .assert_ok();
        let _ = state.take_command_intents();

        state
            .route_idle_event(IdleEvent::Resumed(ListenerId::from_index(1)))
            .assert_ok();

        assert_eq!(
            state.take_command_intents(),
            vec![intent(1, CommandKind::Resume, "outputs on")]
        );
    }

    #[test]
    fn drains_wayland_events_in_arrival_order() {
        let mut state = runtime_state();
        state
            .wayland
            .record(IdleEvent::Idled(ListenerId::from_index(1)));
        state
            .wayland
            .record(IdleEvent::Idled(ListenerId::from_index(0)));

        state.route_wayland_events().assert_ok();

        assert_eq!(
            state.take_command_intents(),
            vec![
                intent(1, CommandKind::Timeout, "outputs off"),
                intent(0, CommandKind::Timeout, "lock")
            ]
        );
    }

    #[test]
    fn rejects_events_for_unknown_listeners() {
        let mut state = runtime_state();

        assert!(matches!(
            state.route_idle_event(IdleEvent::Idled(ListenerId::from_index(9))),
            Err(RuntimeError::Listener(ListenerError::UnknownListener(
                listener_id
            ))) if listener_id == ListenerId::from_index(9)
        ));
    }

    fn runtime_state() -> RuntimeState {
        let config = Config {
            general: GeneralConfig::default(),
            listeners: vec![
                ListenerConfig {
                    timeout_seconds: 300,
                    on_timeout: Some("lock".to_string()),
                    on_resume: None,
                },
                ListenerConfig {
                    timeout_seconds: 600,
                    on_timeout: Some("outputs off".to_string()),
                    on_resume: Some("outputs on".to_string()),
                },
            ],
        };

        RuntimeState::new(
            IdleWaylandState::default(),
            ListenerSet::from_config(&config),
        )
    }

    fn intent(listener_index: usize, kind: CommandKind, command: &str) -> CommandIntent {
        CommandIntent {
            listener_id: ListenerId::from_index(listener_index),
            kind,
            command: command.to_string(),
        }
    }

    trait AssertOk {
        fn assert_ok(self);
    }

    impl AssertOk for Result<(), RuntimeError> {
        fn assert_ok(self) {
            if let Err(error) = self {
                panic!("{error}");
            }
        }
    }
}
