use std::time::Duration;

use crate::config::Config;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ListenerId(usize);

impl ListenerId {
    pub fn from_index(index: usize) -> Self {
        Self(index)
    }

    pub fn index(self) -> usize {
        self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimeoutRequest {
    pub listener_id: ListenerId,
    pub timeout: Duration,
    pub inhibit_policy: InhibitPolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InhibitPolicy {
    Respect,
    Ignore,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ListenerStatus {
    pub(crate) listener_id: ListenerId,
    pub(crate) timeout: Duration,
    pub(crate) inhibit_policy: InhibitPolicy,
    pub(crate) phase: ListenerPhase,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandIntent {
    pub listener_id: ListenerId,
    pub kind: CommandKind,
    pub command: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CommandKind {
    Timeout,
    Resume,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListenerError {
    UnknownListener(ListenerId),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListenerSet {
    listeners: Vec<ListenerState>,
}

impl ListenerSet {
    pub fn from_config(config: &Config) -> Self {
        let listeners = config
            .listeners
            .iter()
            .map(|listener| ListenerState {
                timeout: Duration::from_secs(listener.timeout_seconds),
                on_timeout: listener.on_timeout.clone(),
                on_resume: listener.on_resume.clone(),
                inhibit_policy: if listener.ignore_inhibit {
                    InhibitPolicy::Ignore
                } else {
                    InhibitPolicy::Respect
                },
                phase: ListenerPhase::Armed,
            })
            .collect();

        Self { listeners }
    }

    pub fn timeout_requests(&self) -> Vec<TimeoutRequest> {
        self.listeners
            .iter()
            .enumerate()
            .map(|(index, listener)| TimeoutRequest {
                listener_id: ListenerId(index),
                timeout: listener.timeout,
                inhibit_policy: listener.inhibit_policy,
            })
            .collect()
    }

    pub fn mark_timed_out(
        &mut self,
        listener_id: ListenerId,
    ) -> Result<Option<CommandIntent>, ListenerError> {
        let listener = self
            .listeners
            .get_mut(listener_id.index())
            .ok_or(ListenerError::UnknownListener(listener_id))?;

        if listener.phase == ListenerPhase::TimedOut {
            return Ok(None);
        }

        listener.phase = ListenerPhase::TimedOut;

        Ok(listener.on_timeout.clone().map(|command| CommandIntent {
            listener_id,
            kind: CommandKind::Timeout,
            command,
        }))
    }

    pub fn mark_resumed(
        &mut self,
        listener_id: ListenerId,
    ) -> Result<Option<CommandIntent>, ListenerError> {
        let listener = self
            .listeners
            .get_mut(listener_id.index())
            .ok_or(ListenerError::UnknownListener(listener_id))?;

        if listener.phase == ListenerPhase::Armed {
            return Ok(None);
        }

        listener.phase = ListenerPhase::Armed;

        Ok(listener.on_resume.clone().map(|command| CommandIntent {
            listener_id,
            kind: CommandKind::Resume,
            command,
        }))
    }

    pub fn mark_active_respecting_inhibitors(&mut self) -> Vec<CommandIntent> {
        self.mark_active_matching(|listener| listener.inhibit_policy == InhibitPolicy::Respect)
    }

    pub(crate) fn reset(&mut self) {
        for listener in &mut self.listeners {
            listener.phase = ListenerPhase::Armed;
        }
    }

    pub fn inhibit_policy(&self, listener_id: ListenerId) -> Result<InhibitPolicy, ListenerError> {
        self.listeners
            .get(listener_id.index())
            .map(|listener| listener.inhibit_policy)
            .ok_or(ListenerError::UnknownListener(listener_id))
    }

    pub(crate) fn status(&self) -> Vec<ListenerStatus> {
        self.listeners
            .iter()
            .enumerate()
            .map(|(index, listener)| ListenerStatus {
                listener_id: ListenerId(index),
                timeout: listener.timeout,
                inhibit_policy: listener.inhibit_policy,
                phase: listener.phase,
            })
            .collect()
    }

    fn mark_active_matching(
        &mut self,
        mut matches: impl FnMut(&ListenerState) -> bool,
    ) -> Vec<CommandIntent> {
        let mut intents = Vec::new();

        for (index, listener) in self.listeners.iter_mut().enumerate() {
            if listener.phase == ListenerPhase::Armed || !matches(listener) {
                continue;
            }

            listener.phase = ListenerPhase::Armed;

            if let Some(command) = listener.on_resume.clone() {
                intents.push(CommandIntent {
                    listener_id: ListenerId(index),
                    kind: CommandKind::Resume,
                    command,
                });
            }
        }

        intents
    }

    pub fn len(&self) -> usize {
        self.listeners.len()
    }

    pub fn is_empty(&self) -> bool {
        self.listeners.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ListenerState {
    timeout: Duration,
    on_timeout: Option<String>,
    on_resume: Option<String>,
    inhibit_policy: InhibitPolicy,
    phase: ListenerPhase,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ListenerPhase {
    Armed,
    TimedOut,
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{
        CommandIntent, CommandKind, InhibitPolicy, ListenerError, ListenerId, ListenerSet,
    };
    use crate::config::{Config, GeneralConfig, ListenerConfig};

    #[test]
    fn exposes_timeout_requests_in_config_order() {
        let set = ListenerSet::from_config(&sample_config());

        assert_eq!(set.len(), 2);
        assert_eq!(set.timeout_requests()[0].listener_id.index(), 0);
        assert_eq!(set.timeout_requests()[0].timeout, Duration::from_secs(300));
        assert_eq!(
            set.timeout_requests()[0].inhibit_policy,
            InhibitPolicy::Ignore
        );
        assert_eq!(set.timeout_requests()[1].listener_id.index(), 1);
        assert_eq!(set.timeout_requests()[1].timeout, Duration::from_secs(600));
        assert_eq!(
            set.timeout_requests()[1].inhibit_policy,
            InhibitPolicy::Respect
        );
    }

    #[test]
    fn timeout_command_fires_once_until_activity() {
        let mut set = ListenerSet::from_config(&sample_config());

        assert_eq!(
            set.mark_timed_out(ListenerId(0)),
            Ok(Some(CommandIntent {
                listener_id: ListenerId(0),
                kind: CommandKind::Timeout,
                command: "sweetslock".to_string()
            }))
        );
        assert_eq!(set.mark_timed_out(ListenerId(0)), Ok(None));

        assert_eq!(set.mark_resumed(ListenerId(0)), Ok(None));

        assert_eq!(
            set.mark_timed_out(ListenerId(0)),
            Ok(Some(CommandIntent {
                listener_id: ListenerId(0),
                kind: CommandKind::Timeout,
                command: "sweetslock".to_string()
            }))
        );
    }

    #[test]
    fn resume_command_fires_only_after_timeout() {
        let mut set = ListenerSet::from_config(&sample_config());

        assert_eq!(set.mark_resumed(ListenerId(1)), Ok(None));
        assert_eq!(
            set.mark_timed_out(ListenerId(1)),
            Ok(Some(CommandIntent {
                listener_id: ListenerId(1),
                kind: CommandKind::Timeout,
                command: "sweetsmsg output '*' power off".to_string()
            }))
        );

        assert_eq!(
            set.mark_resumed(ListenerId(1)),
            Ok(Some(CommandIntent {
                listener_id: ListenerId(1),
                kind: CommandKind::Resume,
                command: "sweetsmsg output '*' power on".to_string()
            }))
        );
        assert_eq!(set.mark_resumed(ListenerId(1)), Ok(None));
    }

    #[test]
    fn listener_without_timeout_command_still_tracks_resume() {
        let config = Config {
            general: GeneralConfig::default(),
            idle_hint_timeout_seconds: None,
            listeners: vec![ListenerConfig {
                timeout_seconds: 5,
                on_timeout: None,
                on_resume: Some("resume".to_string()),
                ignore_inhibit: false,
            }],
        };
        let mut set = ListenerSet::from_config(&config);

        assert_eq!(set.mark_timed_out(ListenerId(0)), Ok(None));
        assert_eq!(
            set.mark_resumed(ListenerId(0)),
            Ok(Some(CommandIntent {
                listener_id: ListenerId(0),
                kind: CommandKind::Resume,
                command: "resume".to_string()
            }))
        );
    }

    #[test]
    fn rejects_unknown_listener_ids() {
        let mut set = ListenerSet::from_config(&sample_config());

        assert_eq!(
            set.mark_timed_out(ListenerId(99)),
            Err(ListenerError::UnknownListener(ListenerId(99)))
        );
        assert_eq!(
            set.mark_resumed(ListenerId(99)),
            Err(ListenerError::UnknownListener(ListenerId(99)))
        );
    }

    #[test]
    fn reports_current_listener_status() {
        let mut set = ListenerSet::from_config(&sample_config());
        assert!(set.mark_timed_out(ListenerId(0)).is_ok());

        let status = set.status();

        assert_eq!(status[0].phase, super::ListenerPhase::TimedOut);
        assert_eq!(status[1].phase, super::ListenerPhase::Armed);
        assert_eq!(status[0].inhibit_policy, InhibitPolicy::Ignore);
    }

    #[test]
    fn reset_arms_listeners_without_resume_commands() {
        let mut set = ListenerSet::from_config(&sample_config());
        let listener = ListenerId::from_index(1);
        assert!(set.mark_timed_out(listener).is_ok());

        set.reset();

        assert_eq!(set.mark_resumed(listener), Ok(None));
    }

    #[test]
    fn inhibition_resumes_only_listeners_that_respect_it() {
        let mut set = ListenerSet::from_config(&sample_config());
        assert!(set.mark_timed_out(ListenerId(0)).is_ok());
        assert!(set.mark_timed_out(ListenerId(1)).is_ok());

        assert_eq!(
            set.mark_active_respecting_inhibitors(),
            vec![CommandIntent {
                listener_id: ListenerId(1),
                kind: CommandKind::Resume,
                command: "sweetsmsg output '*' power on".to_string(),
            }]
        );
        assert_eq!(
            set.mark_timed_out(ListenerId(1)),
            Ok(Some(CommandIntent {
                listener_id: ListenerId(1),
                kind: CommandKind::Timeout,
                command: "sweetsmsg output '*' power off".to_string(),
            }))
        );
        assert_eq!(set.mark_timed_out(ListenerId(0)), Ok(None));
    }

    fn sample_config() -> Config {
        Config {
            general: GeneralConfig::default(),
            idle_hint_timeout_seconds: None,
            listeners: vec![
                ListenerConfig {
                    timeout_seconds: 300,
                    on_timeout: Some("sweetslock".to_string()),
                    on_resume: None,
                    ignore_inhibit: true,
                },
                ListenerConfig {
                    timeout_seconds: 600,
                    on_timeout: Some("sweetsmsg output '*' power off".to_string()),
                    on_resume: Some("sweetsmsg output '*' power on".to_string()),
                    ignore_inhibit: false,
                },
            ],
        }
    }
}
