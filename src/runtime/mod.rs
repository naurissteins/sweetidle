mod commands;
mod error;
mod inhibition;
mod reload;
mod shutdown;
mod state;
#[cfg(test)]
mod test_support;

use std::{path::PathBuf, time::Duration};

use calloop::{EventLoop, channel::Event as ChannelEvent};
use calloop_wayland_source::WaylandSource;

pub use error::RuntimeError;
pub use shutdown::ShutdownSignal;

use self::{
    reload::{ConfigReloadMonitor, ConfigReloadRequester},
    shutdown::register_sources,
    state::RuntimeState,
};
use crate::{
    command::{FinishedCommand, ProcessCompletion, ProcessError, ProcessStart, SkipDecision},
    config::{Config, ConfigLoadError},
    control::ControlMonitor,
    listener::ListenerSet,
    session::{DbusInhibitMonitor, LogindFeatures, LogindMonitor, SystemdIdleInhibitor},
    wayland::{IdleWaylandError, IdleWaylandParts},
};

#[derive(Debug)]
pub enum RuntimeEvent {
    ShutdownRequested(ShutdownSignal),
    ConfigReloadRequested,
    ConfigReloadReady(Config),
    ConfigReloadRejected(ConfigLoadError),
    IdlePaused,
    IdleResumed,
    PreparingForSleep,
    ResumedFromSleep,
    LockRequested,
    UnlockRequested,
    IdleHintChanged {
        idle: bool,
    },
    DbusInhibitorAdded {
        cookie: u32,
        application: String,
        reason: String,
        active_count: usize,
    },
    DbusInhibitorRemoved {
        cookie: u32,
        application: String,
        reason: String,
        active_count: usize,
    },
    DbusInhibitorOwnerDisconnected {
        owner: String,
        released_count: usize,
        active_count: usize,
    },
    SystemdIdleInhibitorsChanged {
        inhibitors: Vec<SystemdIdleInhibitor>,
        active_count: usize,
    },
    IdleSuppressed {
        listener_id: crate::listener::ListenerId,
        active_dbus_inhibitors: usize,
        active_systemd_inhibitors: usize,
    },
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

#[derive(Debug)]
pub enum ConfigReloadOutcome {
    Applied,
    Rejected(IdleWaylandError),
    RestartRequired(&'static str),
}

pub struct IdleRuntime {
    event_loop: EventLoop<'static, RuntimeState>,
    state: RuntimeState,
    integration_features: IntegrationFeatures,
    _reload: ConfigReloadMonitor,
    logind: Option<LogindMonitor>,
    _dbus_inhibit: Option<DbusInhibitMonitor>,
    _control: ControlMonitor,
}

impl IdleRuntime {
    pub fn connect(config: &Config, config_path: PathBuf) -> Result<Self, RuntimeError> {
        let listeners = ListenerSet::from_config(config);
        let idle_hint_timeout = config.idle_hint_timeout_seconds.map(Duration::from_secs);
        let parts = IdleWaylandParts::connect(&listeners.timeout_requests(), idle_hint_timeout)?;
        let event_loop = EventLoop::try_new().map_err(RuntimeError::CreateEventLoop)?;
        let source = WaylandSource::new(parts.connection, parts.event_queue);

        event_loop
            .handle()
            .insert_source(source, |_, event_queue, state: &mut RuntimeState| {
                event_queue.dispatch_pending(&mut state.wayland)
            })
            .map_err(|error| RuntimeError::RegisterWayland(error.error))?;
        let (reload, reload_requester) = register_reload(config_path, &event_loop)?;
        register_sources(&event_loop.handle())?;
        let logind = register_logind(config, &event_loop)?;
        let dbus_inhibit = register_dbus_inhibit(config, &event_loop)?;
        let control = register_control(&event_loop)?;

        Ok(Self {
            event_loop,
            state: RuntimeState::new(parts.state, listeners, config, Some(reload_requester)),
            integration_features: IntegrationFeatures::from_config(config),
            _reload: reload,
            logind,
            _dbus_inhibit: dbus_inhibit,
            _control: control,
        })
    }

    pub fn dispatch(
        &mut self,
        timeout: Option<Duration>,
    ) -> Result<Vec<RuntimeEvent>, RuntimeError> {
        // route inhibitor changes before idle events from the same wakeup
        self.event_loop
            .dispatch(timeout, &mut self.state)
            .map_err(RuntimeError::Dispatch)?;
        self.state.collect_ready_processes()?;

        if self.state.shutdown_requested() {
            self.state.discard_pending_events();
            return Ok(self.state.take_runtime_events());
        }

        self.state.route_dbus_inhibit_events()?;
        self.state.route_reload_events()?;
        self.state.route_session_events()?;
        self.state.route_wayland_events()?;
        self.state.route_control_events()?;
        self.flush_idle_hint_updates()?;
        self.state
            .dispatch_command_intents(&self.event_loop.handle())?;

        Ok(self.state.take_runtime_events())
    }

    pub fn reload_config(
        &mut self,
        candidate: &Config,
    ) -> Result<ConfigReloadOutcome, RuntimeError> {
        // changing integration workers needs a full restart
        let candidate_features = IntegrationFeatures::from_config(candidate);
        if let Some(reason) = reload_restart_reason(self.integration_features, candidate_features) {
            return Ok(ConfigReloadOutcome::RestartRequired(reason));
        }

        let listeners = ListenerSet::from_config(candidate);
        if let Err(error) = self.state.wayland.validate_notifications(
            &listeners.timeout_requests(),
            candidate.idle_hint_timeout_seconds.map(Duration::from_secs),
        ) {
            return Ok(ConfigReloadOutcome::Rejected(error));
        }

        self.state.apply_config(candidate)?;
        self.flush_idle_hint_updates()?;
        Ok(ConfigReloadOutcome::Applied)
    }

    fn flush_idle_hint_updates(&mut self) -> Result<(), RuntimeError> {
        for idle in self.state.take_idle_hint_updates() {
            self.logind
                .as_ref()
                .ok_or(RuntimeError::IdleHintMonitorMissing)?
                .set_idle_hint(idle)?;
        }

        Ok(())
    }
}

fn register_control(
    event_loop: &EventLoop<'static, RuntimeState>,
) -> Result<ControlMonitor, RuntimeError> {
    let (monitor, source) = ControlMonitor::connect().map_err(RuntimeError::Control)?;
    event_loop
        .handle()
        .insert_source(source, |event, _, state: &mut RuntimeState| match event {
            ChannelEvent::Msg(event) => state.record_control_event(event),
            ChannelEvent::Closed => state.mark_control_channel_closed(),
        })
        .map_err(|error| RuntimeError::RegisterControl(error.error))?;
    Ok(monitor)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct IntegrationFeatures {
    dbus_inhibit: bool,
    logind: Option<LogindFeatures>,
}

impl IntegrationFeatures {
    fn from_config(config: &Config) -> Self {
        Self {
            dbus_inhibit: dbus_inhibit_enabled(config),
            logind: logind_features(config),
        }
    }
}

fn reload_restart_reason(
    active: IntegrationFeatures,
    candidate: IntegrationFeatures,
) -> Option<&'static str> {
    if active.dbus_inhibit != candidate.dbus_inhibit {
        Some("changing D-Bus inhibitor monitoring")
    } else if active.logind != candidate.logind {
        Some("changing logind integration features")
    } else {
        None
    }
}

fn register_reload(
    config_path: PathBuf,
    event_loop: &EventLoop<'static, RuntimeState>,
) -> Result<(ConfigReloadMonitor, ConfigReloadRequester), RuntimeError> {
    let (monitor, requester, source) = ConfigReloadMonitor::connect(config_path)?;
    event_loop
        .handle()
        .insert_source(source, |event, _, state: &mut RuntimeState| match event {
            ChannelEvent::Msg(event) => state.record_reload_event(event),
            ChannelEvent::Closed => state.mark_reload_channel_closed(),
        })
        .map_err(|error| RuntimeError::RegisterReload(error.error))?;

    Ok((monitor, requester))
}

fn register_dbus_inhibit(
    config: &Config,
    event_loop: &EventLoop<'static, RuntimeState>,
) -> Result<Option<DbusInhibitMonitor>, RuntimeError> {
    if !dbus_inhibit_enabled(config) {
        return Ok(None);
    }

    let (monitor, source) = DbusInhibitMonitor::connect()?;
    event_loop
        .handle()
        .insert_source(source, |event, _, state: &mut RuntimeState| match event {
            ChannelEvent::Msg(event) => state.record_dbus_inhibit_event(event),
            ChannelEvent::Closed => state.mark_dbus_inhibit_channel_closed(),
        })
        .map_err(|error| RuntimeError::RegisterDbusInhibit(error.error))?;

    Ok(Some(monitor))
}

fn dbus_inhibit_enabled(config: &Config) -> bool {
    !config.general.ignore_dbus_inhibit
}

fn register_logind(
    config: &Config,
    event_loop: &EventLoop<'static, RuntimeState>,
) -> Result<Option<LogindMonitor>, RuntimeError> {
    let Some(features) = logind_features(config) else {
        return Ok(None);
    };

    let (monitor, source) = LogindMonitor::connect(features)?;
    event_loop
        .handle()
        .insert_source(source, |event, _, state: &mut RuntimeState| match event {
            ChannelEvent::Msg(event) => state.record_session_event(event),
            ChannelEvent::Closed => state.mark_session_channel_closed(),
        })
        .map_err(|error| RuntimeError::RegisterSession(error.error))?;

    Ok(Some(monitor))
}

fn logind_features(config: &Config) -> Option<LogindFeatures> {
    let has_before_sleep = config.general.before_sleep_command.is_some();
    let has_after_resume = config.general.after_resume_command.is_some();
    let session_events =
        config.general.lock_command.is_some() || config.general.unlock_command.is_some();
    let sleep_events = has_before_sleep || has_after_resume;
    let idle_inhibitors = !config.general.ignore_systemd_inhibit;
    let idle_hint = config.idle_hint_timeout_seconds.is_some();

    (sleep_events || session_events || idle_inhibitors || idle_hint).then_some(LogindFeatures {
        sleep_events,
        delay_inhibitor: has_before_sleep && config.general.wait_before_sleep,
        session_events,
        idle_inhibitors,
        idle_hint,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        IntegrationFeatures, dbus_inhibit_enabled, logind_features, reload_restart_reason,
    };
    use crate::config::Config;
    use crate::session::LogindFeatures;

    #[test]
    fn starts_logind_without_an_inhibitor_for_fire_and_forget_hooks() {
        let mut config = Config::default();
        config.general.before_sleep_command = Some("lock".to_string());
        config.general.wait_before_sleep = false;

        assert_eq!(
            logind_features(&config),
            Some(LogindFeatures {
                sleep_events: true,
                delay_inhibitor: false,
                session_events: false,
                idle_inhibitors: true,
                idle_hint: false,
            })
        );
    }

    #[test]
    fn starts_logind_for_a_lock_command_without_sleep_monitoring() {
        let mut config = Config::default();
        config.general.lock_command = Some("lock".to_string());

        assert_eq!(
            logind_features(&config),
            Some(LogindFeatures {
                sleep_events: false,
                delay_inhibitor: false,
                session_events: true,
                idle_inhibitors: true,
                idle_hint: false,
            })
        );
    }

    #[test]
    fn starts_logind_for_an_unlock_command_without_sleep_monitoring() {
        let mut config = Config::default();
        config.general.unlock_command = Some("unlock".to_string());

        assert_eq!(
            logind_features(&config),
            Some(LogindFeatures {
                sleep_events: false,
                delay_inhibitor: false,
                session_events: true,
                idle_inhibitors: true,
                idle_hint: false,
            })
        );
    }

    #[test]
    fn starts_logind_for_systemd_idle_inhibitors_by_default() {
        assert_eq!(
            logind_features(&Config::default()),
            Some(LogindFeatures {
                sleep_events: false,
                delay_inhibitor: false,
                session_events: false,
                idle_inhibitors: true,
                idle_hint: false,
            })
        );
    }

    #[test]
    fn skips_logind_when_systemd_inhibitors_and_session_hooks_are_disabled() {
        let mut config = Config::default();
        config.general.ignore_systemd_inhibit = true;

        assert_eq!(logind_features(&config), None);
    }

    #[test]
    fn keeps_logind_hooks_when_systemd_inhibitors_are_disabled() {
        let mut config = Config::default();
        config.general.ignore_systemd_inhibit = true;
        config.general.lock_command = Some("lock".to_string());

        assert_eq!(
            logind_features(&config),
            Some(LogindFeatures {
                sleep_events: false,
                delay_inhibitor: false,
                session_events: true,
                idle_inhibitors: false,
                idle_hint: false,
            })
        );
    }

    #[test]
    fn starts_logind_for_idle_hint_when_inhibitor_monitoring_is_disabled() {
        let mut config = Config {
            idle_hint_timeout_seconds: Some(300),
            ..Config::default()
        };
        config.general.ignore_systemd_inhibit = true;

        assert_eq!(
            logind_features(&config),
            Some(LogindFeatures {
                sleep_events: false,
                delay_inhibitor: false,
                session_events: false,
                idle_inhibitors: false,
                idle_hint: true,
            })
        );
    }

    #[test]
    fn enables_dbus_inhibitors_by_default() {
        assert!(dbus_inhibit_enabled(&Config::default()));
    }

    #[test]
    fn honors_the_ignore_dbus_inhibit_option() {
        let mut config = Config::default();
        config.general.ignore_dbus_inhibit = true;

        assert!(!dbus_inhibit_enabled(&config));
    }

    #[test]
    fn listener_changes_can_reload_without_restarting_integrations() {
        let active = Config::default();
        let mut candidate = active.clone();
        candidate.listeners.push(crate::config::ListenerConfig {
            timeout_seconds: 30,
            on_timeout: Some("lock".to_string()),
            on_resume: None,
            ignore_inhibit: true,
        });

        assert_eq!(
            reload_restart_reason(
                IntegrationFeatures::from_config(&active),
                IntegrationFeatures::from_config(&candidate),
            ),
            None
        );
    }

    #[test]
    fn changing_dbus_monitoring_requires_restart() {
        let active = Config::default();
        let mut candidate = active.clone();
        candidate.general.ignore_dbus_inhibit = true;

        assert_eq!(
            reload_restart_reason(
                IntegrationFeatures::from_config(&active),
                IntegrationFeatures::from_config(&candidate),
            ),
            Some("changing D-Bus inhibitor monitoring")
        );
    }

    #[test]
    fn enabling_a_new_logind_feature_requires_restart() {
        let mut active = Config::default();
        active.general.ignore_systemd_inhibit = true;
        let mut candidate = active.clone();
        candidate.general.lock_command = Some("lock".to_string());

        assert_eq!(
            reload_restart_reason(
                IntegrationFeatures::from_config(&active),
                IntegrationFeatures::from_config(&candidate),
            ),
            Some("changing logind integration features")
        );
    }
}
