use std::{collections::HashSet, io};

use calloop::{Interest, LoopHandle, Mode, PostAction, generic::Generic};
use rustix::process::{Pid, PidfdFlags, pidfd_open};

use super::{RuntimeError, RuntimeEvent};
use crate::{
    command::{CommandDispatcher, CommandId, DispatchDecision, ProcessAdapter, ProcessStart},
    listener::{CommandIntent, ListenerSet},
    wayland::{IdleEvent, IdleWaylandState},
};

pub(super) struct RuntimeState {
    pub(super) wayland: IdleWaylandState,
    listeners: ListenerSet,
    command_intents: Vec<CommandIntent>,
    dispatcher: CommandDispatcher,
    processes: ProcessAdapter,
    ready_processes: Vec<CommandId>,
    completed_before_monitor: HashSet<CommandId>,
    runtime_events: Vec<RuntimeEvent>,
}

impl RuntimeState {
    pub(super) fn new(wayland: IdleWaylandState, listeners: ListenerSet) -> Self {
        Self {
            wayland,
            listeners,
            command_intents: Vec::new(),
            dispatcher: CommandDispatcher::default(),
            processes: ProcessAdapter::default(),
            ready_processes: Vec::new(),
            completed_before_monitor: HashSet::new(),
            runtime_events: Vec::new(),
        }
    }

    pub(super) fn route_wayland_events(&mut self) -> Result<(), RuntimeError> {
        for event in self.wayland.take_events() {
            self.route_idle_event(event)?;
        }

        Ok(())
    }

    pub(super) fn dispatch_command_intents(
        &mut self,
        handle: &LoopHandle<'static, RuntimeState>,
    ) -> Result<(), RuntimeError> {
        for intent in std::mem::take(&mut self.command_intents) {
            match self.dispatcher.dispatch(intent) {
                DispatchDecision::Skip(decision) => {
                    self.runtime_events
                        .push(RuntimeEvent::CommandSkipped(decision));
                }
                DispatchDecision::Spawn(request) => match self.processes.spawn(request.clone()) {
                    Ok(start) => {
                        register_process_monitor(handle, start)?;
                        self.runtime_events
                            .push(RuntimeEvent::CommandStarted(start));
                    }
                    Err(error) => {
                        let command = self
                            .dispatcher
                            .finish(request.command_id)
                            .map_err(RuntimeError::CommandDispatch)?;
                        self.runtime_events
                            .push(RuntimeEvent::CommandFailed { command, error });
                    }
                },
            }
        }

        Ok(())
    }

    pub(super) fn collect_ready_processes(&mut self) -> Result<(), RuntimeError> {
        let ready = std::mem::take(&mut self.ready_processes);
        if ready.is_empty() {
            return Ok(());
        }

        let completions = self
            .processes
            .collect_finished()
            .map_err(RuntimeError::Process)?;
        let completed_ids = completions
            .iter()
            .map(|completion| completion.command_id)
            .collect::<HashSet<_>>();

        for completion in completions {
            let command = self
                .dispatcher
                .finish(completion.command_id)
                .map_err(RuntimeError::CommandDispatch)?;
            if !ready.contains(&completion.command_id) {
                self.completed_before_monitor.insert(completion.command_id);
            }
            self.runtime_events.push(RuntimeEvent::CommandFinished {
                command,
                completion,
            });
        }

        for command_id in ready {
            if completed_ids.contains(&command_id)
                || self.completed_before_monitor.remove(&command_id)
            {
                continue;
            }
            return Err(RuntimeError::ProcessNotReady(command_id));
        }

        Ok(())
    }

    pub(super) fn take_runtime_events(&mut self) -> Vec<RuntimeEvent> {
        std::mem::take(&mut self.runtime_events)
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

    fn mark_process_ready(&mut self, command_id: CommandId) {
        self.ready_processes.push(command_id);
    }
}

fn register_process_monitor(
    handle: &LoopHandle<'static, RuntimeState>,
    start: ProcessStart,
) -> Result<(), RuntimeError> {
    let raw_pid = i32::try_from(start.process_id)
        .ok()
        .and_then(Pid::from_raw)
        .ok_or(RuntimeError::InvalidProcessId(start.process_id))?;
    let pidfd = pidfd_open(raw_pid, PidfdFlags::NONBLOCK).map_err(|source| {
        RuntimeError::MonitorProcess {
            command_id: start.command_id,
            process_id: start.process_id,
            source,
        }
    })?;
    let command_id = start.command_id;

    handle
        .insert_source(
            Generic::new(pidfd, Interest::READ, Mode::Level),
            move |_, _, state: &mut RuntimeState| {
                state.mark_process_ready(command_id);
                Ok::<PostAction, io::Error>(PostAction::Remove)
            },
        )
        .map_err(|error| RuntimeError::RegisterProcessMonitor {
            command_id,
            source: error.error,
        })?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use calloop::EventLoop;

    use super::RuntimeState;
    use crate::{
        config::{Config, GeneralConfig, ListenerConfig},
        listener::{CommandIntent, CommandKind, ListenerError, ListenerId, ListenerSet},
        runtime::{RuntimeError, RuntimeEvent},
        wayland::{IdleEvent, IdleWaylandState},
    };

    #[test]
    fn routes_timeout_once_until_listener_resumes() {
        let mut state = runtime_state();

        assert_runtime_ok(state.route_idle_event(IdleEvent::Idled(ListenerId::from_index(0))));
        assert_runtime_ok(state.route_idle_event(IdleEvent::Idled(ListenerId::from_index(0))));

        assert_eq!(
            state.command_intents,
            vec![intent(0, CommandKind::Timeout, "lock")]
        );
    }

    #[test]
    fn activity_resumes_only_listeners_that_timed_out() {
        let mut state = runtime_state();
        assert_runtime_ok(state.route_idle_event(IdleEvent::Idled(ListenerId::from_index(1))));
        state.command_intents.clear();

        assert_runtime_ok(state.route_idle_event(IdleEvent::Resumed(ListenerId::from_index(1))));

        assert_eq!(
            state.command_intents,
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

        assert_runtime_ok(state.route_wayland_events());

        assert_eq!(
            state.command_intents,
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

    #[test]
    fn launches_and_reaps_commands_through_pidfds() {
        let mut event_loop = new_event_loop();
        let mut state = runtime_state();
        state.command_intents = vec![intent(0, CommandKind::Timeout, "exit 0")];

        assert_runtime_ok(state.dispatch_command_intents(&event_loop.handle()));
        assert!(matches!(
            state.take_runtime_events().as_slice(),
            [RuntimeEvent::CommandStarted(_)]
        ));

        let events = wait_for_process_events(&mut event_loop, &mut state);
        assert!(matches!(
            events.as_slice(),
            [RuntimeEvent::CommandFinished { completion, .. }] if completion.status.success()
        ));
        assert!(state.dispatcher.is_empty());
        assert!(state.processes.is_empty());
    }

    #[test]
    fn skips_duplicate_commands_while_the_first_process_runs() {
        let mut event_loop = new_event_loop();
        let mut state = runtime_state();
        state.command_intents = vec![
            intent(0, CommandKind::Timeout, "sleep 0.05"),
            intent(0, CommandKind::Timeout, "sleep 0.05"),
        ];

        assert_runtime_ok(state.dispatch_command_intents(&event_loop.handle()));
        let events = state.take_runtime_events();
        assert!(matches!(
            events.as_slice(),
            [
                RuntimeEvent::CommandStarted(_),
                RuntimeEvent::CommandSkipped(_)
            ]
        ));

        let _ = wait_for_process_events(&mut event_loop, &mut state);
        assert!(state.dispatcher.is_empty());
        assert!(state.processes.is_empty());
    }

    fn new_event_loop() -> EventLoop<'static, RuntimeState> {
        match EventLoop::try_new() {
            Ok(event_loop) => event_loop,
            Err(error) => panic!("{error}"),
        }
    }

    fn wait_for_process_events(
        event_loop: &mut EventLoop<'static, RuntimeState>,
        state: &mut RuntimeState,
    ) -> Vec<RuntimeEvent> {
        for _ in 0..20 {
            if let Err(error) = event_loop.dispatch(Some(Duration::from_millis(100)), state) {
                panic!("{error}");
            }
            assert_runtime_ok(state.collect_ready_processes());
            let events = state.take_runtime_events();
            if !events.is_empty() {
                return events;
            }
        }

        panic!("command did not finish");
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

    fn assert_runtime_ok(result: Result<(), RuntimeError>) {
        if let Err(error) = result {
            panic!("{error}");
        }
    }
}
