use std::{
    collections::{HashMap, HashSet},
    io,
    time::Duration,
};

use calloop::{Interest, LoopHandle, Mode, PostAction, generic::Generic};
use rustix::process::{Pid, PidfdFlags, pidfd_open};

use super::{
    RuntimeError, RuntimeEvent, ShutdownSignal,
    reload::{ConfigReloadEvent, ConfigReloadRequester},
};
use crate::{
    command::{
        CommandContext, CommandDispatcher, CommandId, DispatchDecision, ProcessAdapter,
        ProcessStart, SkipReason,
    },
    config::Config,
    control::{ControlEvent, DbusInhibitorStatus, StatusSnapshot},
    listener::{CommandIntent, InhibitPolicy, ListenerSet},
    session::{
        DbusInhibitEvent, DelayInhibitor, InhibitorInfo, SessionEvent, SystemdIdleInhibitor,
    },
    wayland::{IdleEvent, IdleWaylandState},
};

pub(super) struct RuntimeState {
    pub(super) wayland: IdleWaylandState,
    listeners: ListenerSet,
    command_intents: Vec<CommandIntent>,
    session_command_intents: Vec<SessionCommandIntent>,
    lock_command: Option<String>,
    unlock_command: Option<String>,
    before_sleep_command: Option<String>,
    after_resume_command: Option<String>,
    session_events: Vec<SessionEvent>,
    session_channel_closed: bool,
    dbus_inhibit_events: Vec<DbusInhibitEvent>,
    dbus_inhibit_channel_closed: bool,
    control_events: Vec<ControlEvent>,
    control_channel_closed: bool,
    paused: bool,
    reload_requester: Option<ConfigReloadRequester>,
    reload_events: Vec<ConfigReloadEvent>,
    reload_request_failed: bool,
    reload_channel_closed: bool,
    reload_pending: bool,
    active_dbus_inhibitors: usize,
    active_systemd_inhibitors: usize,
    active_dbus_inhibitor_details: HashMap<u32, InhibitorInfo>,
    active_systemd_inhibitor_details: Vec<SystemdIdleInhibitor>,
    idle_hint_state: Option<bool>,
    idle_hint_updates: Vec<bool>,
    preparing_for_sleep: bool,
    delay_inhibitors: HashMap<CommandId, Vec<DelayInhibitor>>,
    dispatcher: CommandDispatcher,
    processes: ProcessAdapter,
    ready_processes: Vec<CommandId>,
    completed_before_monitor: HashSet<CommandId>,
    runtime_events: Vec<RuntimeEvent>,
    shutdown: Option<ShutdownSignal>,
}

impl RuntimeState {
    pub(super) fn new(
        wayland: IdleWaylandState,
        listeners: ListenerSet,
        config: &Config,
        reload_requester: Option<ConfigReloadRequester>,
    ) -> Self {
        Self {
            wayland,
            listeners,
            command_intents: Vec::new(),
            session_command_intents: Vec::new(),
            lock_command: config.general.lock_command.clone(),
            unlock_command: config.general.unlock_command.clone(),
            before_sleep_command: config.general.before_sleep_command.clone(),
            after_resume_command: config.general.after_resume_command.clone(),
            session_events: Vec::new(),
            session_channel_closed: false,
            dbus_inhibit_events: Vec::new(),
            dbus_inhibit_channel_closed: false,
            control_events: Vec::new(),
            control_channel_closed: false,
            paused: false,
            reload_requester,
            reload_events: Vec::new(),
            reload_request_failed: false,
            reload_channel_closed: false,
            reload_pending: false,
            active_dbus_inhibitors: 0,
            active_systemd_inhibitors: 0,
            active_dbus_inhibitor_details: HashMap::new(),
            active_systemd_inhibitor_details: Vec::new(),
            idle_hint_state: config.idle_hint_timeout_seconds.map(|_| false),
            idle_hint_updates: Vec::new(),
            preparing_for_sleep: false,
            delay_inhibitors: HashMap::new(),
            dispatcher: CommandDispatcher::default(),
            processes: ProcessAdapter::default(),
            ready_processes: Vec::new(),
            completed_before_monitor: HashSet::new(),
            runtime_events: Vec::new(),
            shutdown: None,
        }
    }

    pub(super) fn route_wayland_events(&mut self) -> Result<(), RuntimeError> {
        if self.paused {
            self.wayland.take_events();
            return Ok(());
        }

        for event in self.wayland.take_events() {
            self.route_idle_event(event)?;
        }

        Ok(())
    }

    pub(super) fn apply_config(&mut self, config: &Config) -> Result<(), RuntimeError> {
        let listeners = ListenerSet::from_config(config);
        self.wayland.reconfigure_notifications(
            &listeners.timeout_requests(),
            config.idle_hint_timeout_seconds.map(Duration::from_secs),
        )?;
        if self.paused {
            self.wayland.pause_notifications();
        }

        self.listeners = listeners;
        self.lock_command = config.general.lock_command.clone();
        self.unlock_command = config.general.unlock_command.clone();
        self.before_sleep_command = config.general.before_sleep_command.clone();
        self.after_resume_command = config.general.after_resume_command.clone();
        self.set_idle_hint(false);

        Ok(())
    }

    pub(super) fn dispatch_command_intents(
        &mut self,
        handle: &LoopHandle<'static, RuntimeState>,
    ) -> Result<(), RuntimeError> {
        for intent in std::mem::take(&mut self.session_command_intents) {
            let decision = self
                .dispatcher
                .dispatch_command(intent.context, intent.command);
            self.dispatch_decision(handle, decision, intent.inhibitor)?;
        }

        for intent in std::mem::take(&mut self.command_intents) {
            let decision = self.dispatcher.dispatch(intent);
            self.dispatch_decision(handle, decision, None)?;
        }

        Ok(())
    }

    pub(super) fn collect_ready_processes(&mut self) -> Result<(), RuntimeError> {
        // children may finish before their pidfd source is registered
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
            self.delay_inhibitors.remove(&completion.command_id);
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

    pub(super) fn take_idle_hint_updates(&mut self) -> Vec<bool> {
        std::mem::take(&mut self.idle_hint_updates)
    }

    pub(super) fn record_session_event(&mut self, event: SessionEvent) {
        self.session_events.push(event);
    }

    pub(super) fn request_reload(&mut self) {
        if self.reload_pending || self.shutdown.is_some() {
            return;
        }
        let Some(requester) = self.reload_requester.as_ref() else {
            return;
        };

        match requester.request() {
            Ok(()) => {
                self.reload_pending = true;
                self.runtime_events
                    .push(RuntimeEvent::ConfigReloadRequested);
            }
            Err(_) => self.reload_request_failed = true,
        }
    }

    pub(super) fn record_reload_event(&mut self, event: ConfigReloadEvent) {
        self.reload_events.push(event);
    }

    pub(super) fn mark_reload_channel_closed(&mut self) {
        self.reload_channel_closed = true;
    }

    pub(super) fn route_reload_events(&mut self) -> Result<(), RuntimeError> {
        for event in std::mem::take(&mut self.reload_events) {
            self.reload_pending = false;
            match event {
                ConfigReloadEvent::Ready(config) => {
                    self.runtime_events
                        .push(RuntimeEvent::ConfigReloadReady(config));
                }
                ConfigReloadEvent::Rejected(error) => {
                    self.runtime_events
                        .push(RuntimeEvent::ConfigReloadRejected(error));
                }
            }
        }

        if self.reload_request_failed {
            return Err(RuntimeError::ReloadRequestChannelClosed);
        }
        if self.reload_channel_closed {
            return Err(RuntimeError::ReloadResultChannelClosed);
        }

        Ok(())
    }

    pub(super) fn mark_session_channel_closed(&mut self) {
        self.session_channel_closed = true;
    }

    pub(super) fn record_dbus_inhibit_event(&mut self, event: DbusInhibitEvent) {
        self.dbus_inhibit_events.push(event);
    }

    pub(super) fn mark_dbus_inhibit_channel_closed(&mut self) {
        self.dbus_inhibit_channel_closed = true;
    }

    pub(super) fn record_control_event(&mut self, event: ControlEvent) {
        self.control_events.push(event);
    }

    pub(super) fn mark_control_channel_closed(&mut self) {
        self.control_channel_closed = true;
    }

    pub(super) fn route_control_events(&mut self) -> Result<(), RuntimeError> {
        for event in std::mem::take(&mut self.control_events) {
            match event {
                ControlEvent::Status { request, format } => {
                    let response = self
                        .status_snapshot()
                        .render(format)
                        .unwrap_or_else(|error| {
                            format!("error: failed to serialize status: {error}\n")
                        });
                    request.respond(response);
                }
                ControlEvent::Pause(request) => {
                    let changed = self.pause_idle();
                    request.respond(if changed {
                        "sweetsidle: paused\n".to_string()
                    } else {
                        "sweetsidle: already paused\n".to_string()
                    });
                }
                ControlEvent::Resume(request) => {
                    let changed = self.resume_idle()?;
                    request.respond(if changed {
                        "sweetsidle: resumed\n".to_string()
                    } else {
                        "sweetsidle: already running\n".to_string()
                    });
                }
                ControlEvent::Failed(error) => return Err(RuntimeError::Control(error)),
            }
        }

        if self.control_channel_closed {
            return Err(RuntimeError::ControlChannelClosed);
        }
        Ok(())
    }

    pub(super) fn route_dbus_inhibit_events(&mut self) -> Result<(), RuntimeError> {
        for event in std::mem::take(&mut self.dbus_inhibit_events) {
            match event {
                DbusInhibitEvent::Added {
                    inhibitor,
                    active_count,
                } => {
                    self.active_dbus_inhibitor_details
                        .insert(inhibitor.cookie, inhibitor.clone());
                    self.update_dbus_inhibit_count(active_count)?;
                    self.runtime_events.push(RuntimeEvent::DbusInhibitorAdded {
                        cookie: inhibitor.cookie,
                        application: inhibitor.application,
                        reason: inhibitor.reason,
                        active_count,
                    });
                }
                DbusInhibitEvent::Removed {
                    inhibitor,
                    active_count,
                } => {
                    self.active_dbus_inhibitor_details.remove(&inhibitor.cookie);
                    self.update_dbus_inhibit_count(active_count)?;
                    self.runtime_events
                        .push(RuntimeEvent::DbusInhibitorRemoved {
                            cookie: inhibitor.cookie,
                            application: inhibitor.application,
                            reason: inhibitor.reason,
                            active_count,
                        });
                }
                DbusInhibitEvent::OwnerDisconnected {
                    owner,
                    released_count,
                    active_count,
                } => {
                    self.active_dbus_inhibitor_details
                        .retain(|_, inhibitor| inhibitor.owner != owner);
                    self.update_dbus_inhibit_count(active_count)?;
                    self.runtime_events
                        .push(RuntimeEvent::DbusInhibitorOwnerDisconnected {
                            owner,
                            released_count,
                            active_count,
                        });
                }
                DbusInhibitEvent::Failed(error) => {
                    return Err(RuntimeError::DbusInhibit(error));
                }
            }
        }

        if self.dbus_inhibit_channel_closed {
            return Err(RuntimeError::DbusInhibitChannelClosed);
        }

        Ok(())
    }

    pub(super) fn route_session_events(&mut self) -> Result<(), RuntimeError> {
        for event in std::mem::take(&mut self.session_events) {
            match event {
                SessionEvent::PrepareForSleep(inhibitor) => {
                    if self.preparing_for_sleep {
                        continue;
                    }
                    self.preparing_for_sleep = true;
                    self.runtime_events.push(RuntimeEvent::PreparingForSleep);
                    if let Some(command) = self.before_sleep_command.clone() {
                        self.session_command_intents.push(SessionCommandIntent {
                            context: CommandContext::BeforeSleep,
                            command,
                            inhibitor,
                        });
                    }
                }
                SessionEvent::Resumed => {
                    self.preparing_for_sleep = false;
                    self.set_idle_hint(false);
                    self.runtime_events.push(RuntimeEvent::ResumedFromSleep);
                    if let Some(command) = self.after_resume_command.clone() {
                        self.session_command_intents.push(SessionCommandIntent {
                            context: CommandContext::AfterResume,
                            command,
                            inhibitor: None,
                        });
                    }
                }
                SessionEvent::LockRequested => {
                    self.runtime_events.push(RuntimeEvent::LockRequested);
                    if let Some(command) = self.lock_command.clone() {
                        self.session_command_intents.push(SessionCommandIntent {
                            context: CommandContext::Lock,
                            command,
                            inhibitor: None,
                        });
                    }
                }
                SessionEvent::UnlockRequested => {
                    self.set_idle_hint(false);
                    self.runtime_events.push(RuntimeEvent::UnlockRequested);
                    if let Some(command) = self.unlock_command.clone() {
                        self.session_command_intents.push(SessionCommandIntent {
                            context: CommandContext::Unlock,
                            command,
                            inhibitor: None,
                        });
                    }
                }
                SessionEvent::SystemdIdleInhibitorsChanged(inhibitors) => {
                    let active_count = inhibitors.len();
                    self.active_systemd_inhibitor_details = inhibitors.clone();
                    self.update_systemd_inhibit_count(active_count)?;
                    self.runtime_events
                        .push(RuntimeEvent::SystemdIdleInhibitorsChanged {
                            inhibitors,
                            active_count,
                        });
                }
                SessionEvent::Failed(error) => return Err(RuntimeError::Session(error)),
            }
        }

        if self.session_channel_closed {
            return Err(RuntimeError::SessionChannelClosed);
        }

        Ok(())
    }

    pub(super) fn discard_pending_events(&mut self) {
        self.wayland.take_events();
        self.session_events.clear();
        self.dbus_inhibit_events.clear();
        self.control_events.clear();
        self.reload_events.clear();
        self.command_intents.clear();
        self.session_command_intents.clear();
        self.idle_hint_updates.clear();
    }

    pub(super) fn request_shutdown(&mut self, signal: ShutdownSignal) {
        if self.shutdown.is_some() {
            return;
        }

        self.shutdown = Some(signal);
        self.runtime_events
            .push(RuntimeEvent::ShutdownRequested(signal));
    }

    pub(super) fn shutdown_requested(&self) -> bool {
        self.shutdown.is_some()
    }

    fn route_idle_event(&mut self, event: IdleEvent) -> Result<(), RuntimeError> {
        if self.paused {
            return Ok(());
        }

        match event {
            IdleEvent::Idled(listener_id) => {
                let inhibit_policy = self
                    .listeners
                    .inhibit_policy(listener_id)
                    .map_err(RuntimeError::Listener)?;
                if self.idle_inhibited() && inhibit_policy == InhibitPolicy::Respect {
                    self.runtime_events.push(RuntimeEvent::IdleSuppressed {
                        listener_id,
                        active_dbus_inhibitors: self.active_dbus_inhibitors,
                        active_systemd_inhibitors: self.active_systemd_inhibitors,
                    });
                    return Ok(());
                }
                if let Some(intent) = self
                    .listeners
                    .mark_timed_out(listener_id)
                    .map_err(RuntimeError::Listener)?
                {
                    self.command_intents.push(intent);
                }
            }
            IdleEvent::Resumed(listener_id) => {
                if let Some(intent) = self
                    .listeners
                    .mark_resumed(listener_id)
                    .map_err(RuntimeError::Listener)?
                {
                    self.command_intents.push(intent);
                }
            }
            IdleEvent::IdleHintIdled => {
                if !self.idle_inhibited() {
                    self.set_idle_hint(true);
                }
            }
            IdleEvent::IdleHintResumed => self.set_idle_hint(false),
        }

        Ok(())
    }

    fn mark_process_ready(&mut self, command_id: CommandId) {
        self.ready_processes.push(command_id);
    }

    fn update_dbus_inhibit_count(&mut self, active_count: usize) -> Result<(), RuntimeError> {
        let was_inhibited = self.idle_inhibited();
        self.active_dbus_inhibitors = active_count;
        self.apply_idle_inhibition_transition(was_inhibited)
    }

    fn update_systemd_inhibit_count(&mut self, active_count: usize) -> Result<(), RuntimeError> {
        let was_inhibited = self.idle_inhibited();
        self.active_systemd_inhibitors = active_count;
        self.apply_idle_inhibition_transition(was_inhibited)
    }

    fn idle_inhibited(&self) -> bool {
        self.active_dbus_inhibitors > 0 || self.active_systemd_inhibitors > 0
    }

    fn status_snapshot(&self) -> StatusSnapshot {
        let mut dbus_inhibitors = self
            .active_dbus_inhibitor_details
            .values()
            .map(|inhibitor| DbusInhibitorStatus {
                cookie: inhibitor.cookie,
                application: inhibitor.application.clone(),
                reason: inhibitor.reason.clone(),
            })
            .collect::<Vec<_>>();
        dbus_inhibitors.sort_unstable_by_key(|inhibitor| inhibitor.cookie);

        StatusSnapshot {
            paused: self.paused,
            listeners: self.listeners.status(),
            dbus_inhibitors,
            systemd_inhibitors: self.active_systemd_inhibitor_details.clone(),
            idle_hint: self.idle_hint_state,
        }
    }

    fn apply_idle_inhibition_transition(
        &mut self,
        was_inhibited: bool,
    ) -> Result<(), RuntimeError> {
        // a change in inhibitor presence rearms Wayland notifications
        let is_inhibited = self.idle_inhibited();

        if was_inhibited == is_inhibited || self.paused {
            return Ok(());
        }

        self.wayland.reset_inhibited_notifications()?;
        if is_inhibited {
            self.command_intents
                .extend(self.listeners.mark_active_respecting_inhibitors());
            self.set_idle_hint(false);
        }

        Ok(())
    }

    fn pause_idle(&mut self) -> bool {
        if self.paused {
            return false;
        }

        self.wayland.pause_notifications();
        self.listeners.reset();
        self.command_intents.clear();
        self.set_idle_hint(false);
        self.paused = true;
        self.runtime_events.push(RuntimeEvent::IdlePaused);
        true
    }

    fn resume_idle(&mut self) -> Result<bool, RuntimeError> {
        if !self.paused {
            return Ok(false);
        }

        self.wayland.reset_notifications()?;
        self.paused = false;
        self.runtime_events.push(RuntimeEvent::IdleResumed);
        Ok(true)
    }

    fn set_idle_hint(&mut self, idle: bool) {
        let Some(current) = self.idle_hint_state.as_mut() else {
            return;
        };
        if *current == idle {
            return;
        }

        *current = idle;
        self.idle_hint_updates.push(idle);
        self.runtime_events
            .push(RuntimeEvent::IdleHintChanged { idle });
    }

    fn dispatch_decision(
        &mut self,
        handle: &LoopHandle<'static, RuntimeState>,
        decision: DispatchDecision,
        inhibitor: Option<DelayInhibitor>,
    ) -> Result<(), RuntimeError> {
        match decision {
            DispatchDecision::Skip(decision) => {
                if let Some(inhibitor) = inhibitor {
                    let SkipReason::AlreadyRunning(command_id) = decision.reason;
                    self.delay_inhibitors
                        .entry(command_id)
                        .or_default()
                        .push(inhibitor);
                }
                self.runtime_events
                    .push(RuntimeEvent::CommandSkipped(decision));
            }
            DispatchDecision::Spawn(request) => {
                if let Some(inhibitor) = inhibitor {
                    self.delay_inhibitors
                        .entry(request.command_id)
                        .or_default()
                        .push(inhibitor);
                }

                match self.processes.spawn(request.clone()) {
                    Ok(start) => {
                        register_process_monitor(handle, start)?;
                        self.runtime_events
                            .push(RuntimeEvent::CommandStarted(start));
                    }
                    Err(error) => {
                        self.delay_inhibitors.remove(&request.command_id);
                        let command = self
                            .dispatcher
                            .finish(request.command_id)
                            .map_err(RuntimeError::CommandDispatch)?;
                        self.runtime_events
                            .push(RuntimeEvent::CommandFailed { command, error });
                    }
                }
            }
        }

        Ok(())
    }
}

struct SessionCommandIntent {
    context: CommandContext,
    command: String,
    inhibitor: Option<DelayInhibitor>,
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
    use std::{fs::File, os::fd::OwnedFd, sync::mpsc, time::Duration};

    use calloop::EventLoop;

    use super::{ConfigReloadEvent, RuntimeState, SessionCommandIntent};
    use crate::{
        command::CommandContext,
        config::{Config, GeneralConfig, ListenerConfig},
        control::{ControlEvent, ControlRequest},
        listener::{
            CommandIntent, CommandKind, ListenerError, ListenerId, ListenerPhase, ListenerSet,
        },
        runtime::{RuntimeError, RuntimeEvent, ShutdownSignal},
        session::{
            DbusInhibitEvent, DelayInhibitor, InhibitorInfo, SessionEvent, SystemdIdleInhibitor,
        },
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
    fn compositor_inhibition_does_not_resume_an_ignored_listener() {
        let config = Config {
            general: GeneralConfig::default(),
            idle_hint_timeout_seconds: None,
            listeners: vec![
                ListenerConfig {
                    timeout_seconds: 300,
                    on_timeout: Some("ignored timeout".to_string()),
                    on_resume: Some("ignored resume".to_string()),
                    ignore_inhibit: true,
                },
                ListenerConfig {
                    timeout_seconds: 600,
                    on_timeout: Some("respecting timeout".to_string()),
                    on_resume: Some("respecting resume".to_string()),
                    ignore_inhibit: false,
                },
            ],
        };
        let mut state = RuntimeState::new(
            IdleWaylandState::default(),
            ListenerSet::from_config(&config),
            &config,
            None,
        );
        assert_runtime_ok(state.route_idle_event(IdleEvent::Idled(ListenerId::from_index(0))));
        assert_runtime_ok(state.route_idle_event(IdleEvent::Idled(ListenerId::from_index(1))));
        state.command_intents.clear();

        assert_runtime_ok(state.route_idle_event(IdleEvent::Resumed(ListenerId::from_index(1))));

        assert_eq!(
            state.command_intents,
            vec![intent(1, CommandKind::Resume, "respecting resume")]
        );
        assert_eq!(
            state.status_snapshot().listeners[0].phase,
            ListenerPhase::TimedOut
        );

        assert_runtime_ok(state.route_idle_event(IdleEvent::Resumed(ListenerId::from_index(0))));
        assert_eq!(
            state.command_intents,
            vec![
                intent(1, CommandKind::Resume, "respecting resume"),
                intent(0, CommandKind::Resume, "ignored resume"),
            ]
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
    fn applying_config_replaces_listener_policy() {
        let mut state = runtime_state();
        let config = Config {
            general: GeneralConfig::default(),
            idle_hint_timeout_seconds: None,
            listeners: vec![ListenerConfig {
                timeout_seconds: 45,
                on_timeout: Some("new lock".to_string()),
                on_resume: None,
                ignore_inhibit: false,
            }],
        };

        assert_runtime_ok(state.apply_config(&config));
        assert_runtime_ok(state.route_idle_event(IdleEvent::Idled(ListenerId::from_index(0))));

        assert_eq!(
            state.command_intents,
            vec![intent(0, CommandKind::Timeout, "new lock")]
        );
    }

    #[test]
    fn applying_config_restarts_an_active_idle_hint() {
        let mut state = idle_hint_runtime_state();
        assert_runtime_ok(state.route_idle_event(IdleEvent::IdleHintIdled));
        state.take_idle_hint_updates();
        state.take_runtime_events();
        let config = Config {
            idle_hint_timeout_seconds: Some(600),
            ..Config::default()
        };

        assert_runtime_ok(state.apply_config(&config));

        assert_eq!(state.take_idle_hint_updates(), vec![false]);
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
        assert!(matches!(
            state.route_idle_event(IdleEvent::Resumed(ListenerId::from_index(9))),
            Err(RuntimeError::Listener(ListenerError::UnknownListener(
                listener_id
            ))) if listener_id == ListenerId::from_index(9)
        ));
    }

    #[test]
    fn reports_idle_hint_transitions_once() {
        let mut state = idle_hint_runtime_state();

        assert_runtime_ok(state.route_idle_event(IdleEvent::IdleHintIdled));
        assert_runtime_ok(state.route_idle_event(IdleEvent::IdleHintIdled));

        assert_eq!(state.take_idle_hint_updates(), vec![true]);
        assert!(matches!(
            state.take_runtime_events().as_slice(),
            [RuntimeEvent::IdleHintChanged { idle: true }]
        ));

        assert_runtime_ok(state.route_idle_event(IdleEvent::IdleHintResumed));

        assert_eq!(state.take_idle_hint_updates(), vec![false]);
        assert!(matches!(
            state.take_runtime_events().as_slice(),
            [RuntimeEvent::IdleHintChanged { idle: false }]
        ));
    }

    #[test]
    fn inhibition_clears_an_active_idle_hint() {
        let mut state = idle_hint_runtime_state();
        assert_runtime_ok(state.route_idle_event(IdleEvent::IdleHintIdled));
        state.take_idle_hint_updates();
        state.take_runtime_events();
        state.record_dbus_inhibit_event(DbusInhibitEvent::Added {
            inhibitor: inhibitor_info(1),
            active_count: 1,
        });

        assert_runtime_ok(state.route_dbus_inhibit_events());

        assert_eq!(state.take_idle_hint_updates(), vec![false]);
        assert!(
            state
                .take_runtime_events()
                .iter()
                .any(|event| matches!(event, RuntimeEvent::IdleHintChanged { idle: false }))
        );
    }

    #[test]
    fn unlock_clears_an_active_idle_hint() {
        let mut state = idle_hint_runtime_state();
        assert_runtime_ok(state.route_idle_event(IdleEvent::IdleHintIdled));
        state.take_idle_hint_updates();
        state.take_runtime_events();
        state.record_session_event(SessionEvent::UnlockRequested);

        assert_runtime_ok(state.route_session_events());

        assert_eq!(state.take_idle_hint_updates(), vec![false]);
        assert!(matches!(
            state.take_runtime_events().as_slice(),
            [
                RuntimeEvent::IdleHintChanged { idle: false },
                RuntimeEvent::UnlockRequested
            ]
        ));
    }

    #[test]
    fn suppresses_timeout_commands_while_dbus_inhibited() {
        let mut state = runtime_state();
        state.record_dbus_inhibit_event(DbusInhibitEvent::Added {
            inhibitor: inhibitor_info(1),
            active_count: 1,
        });
        assert_runtime_ok(state.route_dbus_inhibit_events());
        state.take_runtime_events();

        assert_runtime_ok(state.route_idle_event(IdleEvent::Idled(ListenerId::from_index(0))));

        assert!(state.command_intents.is_empty());
        assert!(matches!(
            state.take_runtime_events().as_slice(),
            [RuntimeEvent::IdleSuppressed {
                listener_id,
                active_dbus_inhibitors: 1,
                active_systemd_inhibitors: 0,
            }] if *listener_id == ListenerId::from_index(0)
        ));
    }

    #[test]
    fn suppresses_timeout_commands_while_systemd_inhibited() {
        let mut state = runtime_state();
        state.record_session_event(SessionEvent::SystemdIdleInhibitorsChanged(vec![
            systemd_inhibitor(20),
        ]));
        assert_runtime_ok(state.route_session_events());
        state.take_runtime_events();

        assert_runtime_ok(state.route_idle_event(IdleEvent::Idled(ListenerId::from_index(0))));

        assert!(state.command_intents.is_empty());
        assert!(matches!(
            state.take_runtime_events().as_slice(),
            [RuntimeEvent::IdleSuppressed {
                listener_id,
                active_dbus_inhibitors: 0,
                active_systemd_inhibitors: 1,
            }] if *listener_id == ListenerId::from_index(0)
        ));
    }

    #[test]
    fn opted_out_listener_runs_while_inhibited() {
        let mut state = runtime_state_with_ignored_listener();
        state.record_dbus_inhibit_event(DbusInhibitEvent::Added {
            inhibitor: inhibitor_info(1),
            active_count: 1,
        });
        assert_runtime_ok(state.route_dbus_inhibit_events());
        state.take_runtime_events();

        assert_runtime_ok(state.route_idle_event(IdleEvent::Idled(ListenerId::from_index(0))));

        assert_eq!(
            state.command_intents,
            vec![intent(0, CommandKind::Timeout, "lock")]
        );
        assert!(state.take_runtime_events().is_empty());
    }

    #[test]
    fn starting_inhibition_preserves_opted_out_listener_state() {
        let mut state = runtime_state_with_ignored_listener();
        assert_runtime_ok(state.route_idle_event(IdleEvent::Idled(ListenerId::from_index(0))));
        state.command_intents.clear();
        state.record_dbus_inhibit_event(DbusInhibitEvent::Added {
            inhibitor: inhibitor_info(1),
            active_count: 1,
        });

        assert_runtime_ok(state.route_dbus_inhibit_events());
        assert!(state.command_intents.is_empty());

        assert_runtime_ok(state.route_idle_event(IdleEvent::Idled(ListenerId::from_index(0))));
        assert!(state.command_intents.is_empty());
        assert_runtime_ok(state.route_idle_event(IdleEvent::Resumed(ListenerId::from_index(0))));
        assert_eq!(
            state.command_intents,
            vec![intent(0, CommandKind::Resume, "unlock")]
        );
    }

    #[test]
    fn status_snapshot_contains_live_listener_and_inhibitor_state() {
        let mut state = runtime_state_with_ignored_listener();
        assert_runtime_ok(state.route_idle_event(IdleEvent::Idled(ListenerId::from_index(0))));
        state.record_dbus_inhibit_event(DbusInhibitEvent::Added {
            inhibitor: inhibitor_info(7),
            active_count: 1,
        });
        assert_runtime_ok(state.route_dbus_inhibit_events());
        state.record_session_event(SessionEvent::SystemdIdleInhibitorsChanged(vec![
            systemd_inhibitor(42),
        ]));
        assert_runtime_ok(state.route_session_events());

        let status = state.status_snapshot().to_string();

        assert!(status.contains("id=0 state=timed-out timeout=300s inhibitors=ignored"));
        assert!(status.contains("source=dbus cookie=7 application=\"test application\""));
        assert!(status.contains("source=systemd pid=42 uid=1000 who=\"test application\""));
    }

    #[test]
    fn pause_control_resets_listener_state_without_resume_actions() {
        let mut state = runtime_state();
        assert_runtime_ok(state.route_idle_event(IdleEvent::Idled(ListenerId::from_index(1))));
        state.command_intents.clear();
        let (reply, response) = mpsc::sync_channel(1);
        state.record_control_event(ControlEvent::Pause(ControlRequest::new(reply)));

        assert_runtime_ok(state.route_control_events());
        assert_runtime_ok(state.route_idle_event(IdleEvent::Resumed(ListenerId::from_index(1))));

        assert_eq!(response.recv(), Ok("sweetsidle: paused\n".to_string()));
        assert!(state.command_intents.is_empty());
        assert!(
            state
                .status_snapshot()
                .to_string()
                .contains("sweetsidle: paused")
        );
    }

    #[test]
    fn pause_and_resume_controls_are_idempotent() {
        let mut state = runtime_state();
        assert!(state.pause_idle());
        assert!(!state.pause_idle());
        assert!(matches!(state.resume_idle(), Ok(true)));

        assert!(!state.paused);
        assert!(matches!(state.resume_idle(), Ok(false)));
    }

    #[test]
    fn paused_runtime_suppresses_idle_events() {
        let mut state = runtime_state();
        assert!(state.pause_idle());
        state.take_runtime_events();

        assert_runtime_ok(state.route_idle_event(IdleEvent::Idled(ListenerId::from_index(0))));

        assert!(state.command_intents.is_empty());
        assert!(state.take_runtime_events().is_empty());
    }

    #[test]
    fn pausing_clears_an_active_idle_hint() {
        let mut state = idle_hint_runtime_state();
        assert_runtime_ok(state.route_idle_event(IdleEvent::IdleHintIdled));
        state.take_idle_hint_updates();

        assert!(state.pause_idle());

        assert_eq!(state.take_idle_hint_updates(), vec![false]);
    }

    #[test]
    fn config_reload_preserves_paused_state() {
        let mut state = runtime_state();
        assert!(state.pause_idle());
        let config = Config {
            listeners: vec![ListenerConfig {
                timeout_seconds: 45,
                on_timeout: Some("new lock".to_string()),
                on_resume: None,
                ignore_inhibit: false,
            }],
            ..Config::default()
        };

        assert_runtime_ok(state.apply_config(&config));

        assert!(
            state
                .status_snapshot()
                .to_string()
                .contains("sweetsidle: paused")
        );
    }

    #[test]
    fn keeps_inhibition_active_until_both_sources_are_clear() {
        let mut state = runtime_state();
        state.record_dbus_inhibit_event(DbusInhibitEvent::Added {
            inhibitor: inhibitor_info(1),
            active_count: 1,
        });
        assert_runtime_ok(state.route_dbus_inhibit_events());
        state.record_session_event(SessionEvent::SystemdIdleInhibitorsChanged(vec![
            systemd_inhibitor(20),
        ]));
        assert_runtime_ok(state.route_session_events());

        state.record_dbus_inhibit_event(DbusInhibitEvent::Removed {
            inhibitor: inhibitor_info(1),
            active_count: 0,
        });
        assert_runtime_ok(state.route_dbus_inhibit_events());
        assert_runtime_ok(state.route_idle_event(IdleEvent::Idled(ListenerId::from_index(0))));
        assert!(state.command_intents.is_empty());

        state.record_session_event(SessionEvent::SystemdIdleInhibitorsChanged(Vec::new()));
        assert_runtime_ok(state.route_session_events());
        assert_runtime_ok(state.route_idle_event(IdleEvent::Idled(ListenerId::from_index(0))));

        assert_eq!(
            state.command_intents,
            vec![intent(0, CommandKind::Timeout, "lock")]
        );
    }

    #[test]
    fn starting_inhibition_resumes_listeners_that_already_fired() {
        let mut state = runtime_state();
        assert_runtime_ok(state.route_idle_event(IdleEvent::Idled(ListenerId::from_index(1))));
        state.command_intents.clear();
        state.record_dbus_inhibit_event(DbusInhibitEvent::Added {
            inhibitor: inhibitor_info(1),
            active_count: 1,
        });

        assert_runtime_ok(state.route_dbus_inhibit_events());

        assert_eq!(
            state.command_intents,
            vec![intent(1, CommandKind::Resume, "outputs on")]
        );
    }

    #[test]
    fn final_inhibitor_release_allows_fresh_timeouts() {
        let mut state = runtime_state();
        state.record_dbus_inhibit_event(DbusInhibitEvent::Added {
            inhibitor: inhibitor_info(1),
            active_count: 1,
        });
        state.record_dbus_inhibit_event(DbusInhibitEvent::Added {
            inhibitor: inhibitor_info(2),
            active_count: 2,
        });
        state.record_dbus_inhibit_event(DbusInhibitEvent::Removed {
            inhibitor: inhibitor_info(1),
            active_count: 1,
        });
        assert_runtime_ok(state.route_dbus_inhibit_events());

        assert_runtime_ok(state.route_idle_event(IdleEvent::Idled(ListenerId::from_index(0))));
        assert!(state.command_intents.is_empty());

        state.record_dbus_inhibit_event(DbusInhibitEvent::OwnerDisconnected {
            owner: ":1.2".to_string(),
            released_count: 1,
            active_count: 0,
        });
        assert_runtime_ok(state.route_dbus_inhibit_events());
        assert_runtime_ok(state.route_idle_event(IdleEvent::Idled(ListenerId::from_index(0))));

        assert_eq!(
            state.command_intents,
            vec![intent(0, CommandKind::Timeout, "lock")]
        );
    }

    #[test]
    fn records_only_the_first_shutdown_signal() {
        let mut state = runtime_state();

        state.request_shutdown(ShutdownSignal::Interrupt);
        state.request_shutdown(ShutdownSignal::Terminate);

        assert!(state.shutdown_requested());
        assert!(matches!(
            state.take_runtime_events().as_slice(),
            [RuntimeEvent::ShutdownRequested(ShutdownSignal::Interrupt)]
        ));
    }

    #[test]
    fn rejected_reload_keeps_runtime_available_for_another_request() {
        let mut state = runtime_state();
        state.reload_pending = true;
        let error = match Config::load(std::path::Path::new(
            "/definitely-missing-sweetsidle-config.toml",
        )) {
            Ok(config) => panic!("unexpected config: {config:?}"),
            Err(error) => error,
        };
        state.record_reload_event(ConfigReloadEvent::Rejected(error));

        assert_runtime_ok(state.route_reload_events());

        assert!(!state.reload_pending);
        assert!(matches!(
            state.take_runtime_events().as_slice(),
            [RuntimeEvent::ConfigReloadRejected(_)]
        ));
    }

    #[test]
    fn routes_prepare_for_sleep_to_the_configured_command() {
        let mut state = runtime_state();
        state.before_sleep_command = Some("lock before sleep".to_string());
        state.record_session_event(SessionEvent::PrepareForSleep(None));

        assert_runtime_ok(state.route_session_events());

        assert!(matches!(
            state.take_runtime_events().as_slice(),
            [RuntimeEvent::PreparingForSleep]
        ));
        assert!(matches!(
            state.session_command_intents.as_slice(),
            [SessionCommandIntent {
                context: CommandContext::BeforeSleep,
                command,
                inhibitor: None,
            }] if command == "lock before sleep"
        ));
    }

    #[test]
    fn ignores_duplicate_prepare_for_sleep_events() {
        let mut state = runtime_state();
        state.before_sleep_command = Some("lock before sleep".to_string());
        state.record_session_event(SessionEvent::PrepareForSleep(None));
        state.record_session_event(SessionEvent::PrepareForSleep(None));

        assert_runtime_ok(state.route_session_events());

        assert_eq!(state.session_command_intents.len(), 1);
        assert!(matches!(
            state.take_runtime_events().as_slice(),
            [RuntimeEvent::PreparingForSleep]
        ));
    }

    #[test]
    fn routes_resume_to_the_configured_command() {
        let mut state = runtime_state();
        state.after_resume_command = Some("outputs on".to_string());
        state.record_session_event(SessionEvent::Resumed);

        assert_runtime_ok(state.route_session_events());

        assert!(matches!(
            state.take_runtime_events().as_slice(),
            [RuntimeEvent::ResumedFromSleep]
        ));
        assert!(matches!(
            state.session_command_intents.as_slice(),
            [SessionCommandIntent {
                context: CommandContext::AfterResume,
                command,
                inhibitor: None,
            }] if command == "outputs on"
        ));
    }

    #[test]
    fn routes_lock_request_to_the_configured_command() {
        let mut state = runtime_state();
        state.lock_command = Some("lock session".to_string());
        state.record_session_event(SessionEvent::LockRequested);

        assert_runtime_ok(state.route_session_events());

        assert!(matches!(
            state.take_runtime_events().as_slice(),
            [RuntimeEvent::LockRequested]
        ));
        assert!(matches!(
            state.session_command_intents.as_slice(),
            [SessionCommandIntent {
                context: CommandContext::Lock,
                command,
                inhibitor: None,
            }] if command == "lock session"
        ));
    }

    #[test]
    fn routes_unlock_request_to_the_configured_command() {
        let mut state = runtime_state();
        state.unlock_command = Some("unlock session".to_string());
        state.record_session_event(SessionEvent::UnlockRequested);

        assert_runtime_ok(state.route_session_events());

        assert!(matches!(
            state.take_runtime_events().as_slice(),
            [RuntimeEvent::UnlockRequested]
        ));
        assert!(matches!(
            state.session_command_intents.as_slice(),
            [SessionCommandIntent {
                context: CommandContext::Unlock,
                command,
                inhibitor: None,
            }] if command == "unlock session"
        ));
    }

    #[test]
    fn records_unlock_without_a_configured_command() {
        let mut state = runtime_state();
        state.record_session_event(SessionEvent::UnlockRequested);

        assert_runtime_ok(state.route_session_events());

        assert!(matches!(
            state.take_runtime_events().as_slice(),
            [RuntimeEvent::UnlockRequested]
        ));
        assert!(state.session_command_intents.is_empty());
    }

    #[test]
    fn holds_the_sleep_inhibitor_until_the_command_finishes() {
        let mut event_loop = new_event_loop();
        let mut state = runtime_state();
        state.before_sleep_command = Some("exit 0".to_string());
        state.record_session_event(SessionEvent::PrepareForSleep(Some(test_inhibitor())));

        assert_runtime_ok(state.route_session_events());
        assert_runtime_ok(state.dispatch_command_intents(&event_loop.handle()));
        assert_eq!(state.delay_inhibitors.len(), 1);
        state.take_runtime_events();

        let _ = wait_for_process_events(&mut event_loop, &mut state);
        assert!(state.delay_inhibitors.is_empty());
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
            idle_hint_timeout_seconds: None,
            listeners: vec![
                ListenerConfig {
                    timeout_seconds: 300,
                    on_timeout: Some("lock".to_string()),
                    on_resume: None,
                    ignore_inhibit: false,
                },
                ListenerConfig {
                    timeout_seconds: 600,
                    on_timeout: Some("outputs off".to_string()),
                    on_resume: Some("outputs on".to_string()),
                    ignore_inhibit: false,
                },
            ],
        };

        RuntimeState::new(
            IdleWaylandState::default(),
            ListenerSet::from_config(&config),
            &config,
            None,
        )
    }

    fn runtime_state_with_ignored_listener() -> RuntimeState {
        let config = Config {
            general: GeneralConfig::default(),
            idle_hint_timeout_seconds: None,
            listeners: vec![ListenerConfig {
                timeout_seconds: 300,
                on_timeout: Some("lock".to_string()),
                on_resume: Some("unlock".to_string()),
                ignore_inhibit: true,
            }],
        };

        RuntimeState::new(
            IdleWaylandState::default(),
            ListenerSet::from_config(&config),
            &config,
            None,
        )
    }

    fn idle_hint_runtime_state() -> RuntimeState {
        let mut state = runtime_state();
        state.idle_hint_state = Some(false);
        state
    }

    fn intent(listener_index: usize, kind: CommandKind, command: &str) -> CommandIntent {
        CommandIntent {
            listener_id: ListenerId::from_index(listener_index),
            kind,
            command: command.to_string(),
        }
    }

    fn inhibitor_info(cookie: u32) -> InhibitorInfo {
        InhibitorInfo {
            cookie,
            owner: format!(":1.{cookie}"),
            application: "test application".to_string(),
            reason: "test reason".to_string(),
        }
    }

    fn systemd_inhibitor(pid: u32) -> SystemdIdleInhibitor {
        SystemdIdleInhibitor {
            who: "test application".to_string(),
            why: "test reason".to_string(),
            uid: 1000,
            pid,
        }
    }

    fn assert_runtime_ok(result: Result<(), RuntimeError>) {
        if let Err(error) = result {
            panic!("{error}");
        }
    }

    fn test_inhibitor() -> DelayInhibitor {
        let file = match File::open("/dev/null") {
            Ok(file) => file,
            Err(error) => panic!("failed to open test file: {error}"),
        };
        let fd: OwnedFd = file.into();
        DelayInhibitor::new(fd.into())
    }
}
