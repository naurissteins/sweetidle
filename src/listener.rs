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
    TimedOutListenerChanged(ListenerId),
}

pub(crate) struct ListenerReload {
    pub(crate) listeners: ListenerSet,
    pub(crate) matching_ids: Vec<Option<ListenerId>>,
    pub(crate) retained_ids: Vec<(ListenerId, ListenerId)>,
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

        // duplicate idle notifications must not rerun a timeout action
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

        // resume actions belong only to listeners that already timed out
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

    #[must_use = "returned resume intents must be dispatched"]
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

    pub(crate) fn matching_listener_ids(&self, candidate: &Self) -> Vec<Option<ListenerId>> {
        let mut matched = vec![false; candidate.listeners.len()];
        let mut matching = vec![None; self.listeners.len()];
        // timed-out duplicates get first claim so removing an armed copy cannot discard their cycle
        for phase in [ListenerPhase::TimedOut, ListenerPhase::Armed] {
            for (old_index, listener) in self.listeners.iter().enumerate() {
                if listener.phase != phase {
                    continue;
                }
                let index = candidate
                    .listeners
                    .iter()
                    .enumerate()
                    .find_map(|(index, next)| {
                        (!matched[index] && listener.same_definition(next)).then_some(index)
                    });
                if let Some(index) = index {
                    matched[index] = true;
                }
                matching[old_index] = index.map(ListenerId);
            }
        }
        matching
    }

    pub(crate) fn prepare_reload(&self, config: &Config) -> Result<ListenerReload, ListenerError> {
        let mut listeners = Self::from_config(config);
        let matching_ids = self.matching_listener_ids(&listeners);
        let mut retained_ids = Vec::new();
        for (index, (old, next)) in self.listeners.iter().zip(&matching_ids).enumerate() {
            if old.phase != ListenerPhase::TimedOut {
                continue;
            }
            let next = next.ok_or(ListenerError::TimedOutListenerChanged(ListenerId(index)))?;
            listeners.listeners[next.index()].phase = ListenerPhase::TimedOut;
            retained_ids.push((ListenerId(index), next));
        }
        Ok(ListenerReload {
            listeners,
            matching_ids,
            retained_ids,
        })
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ListenerState {
    timeout: Duration,
    on_timeout: Option<String>,
    on_resume: Option<String>,
    inhibit_policy: InhibitPolicy,
    phase: ListenerPhase,
}

impl ListenerState {
    fn same_definition(&self, other: &Self) -> bool {
        self.timeout == other.timeout
            && self.on_timeout == other.on_timeout
            && self.on_resume == other.on_resume
            && self.inhibit_policy == other.inhibit_policy
    }
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

        assert_eq!(set.timeout_requests().len(), 2);
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
    fn matches_duplicate_definitions_once_each_without_resetting_idle_phase() {
        let mut config = sample_config();
        config.listeners.push(config.listeners[0].clone());
        let mut active = ListenerSet::from_config(&config);
        assert!(active.mark_timed_out(ListenerId(0)).is_ok());
        config.listeners.swap(0, 1);

        assert_eq!(
            active.matching_listener_ids(&ListenerSet::from_config(&config)),
            vec![
                Some(ListenerId(1)),
                Some(ListenerId(0)),
                Some(ListenerId(2))
            ]
        );
        config.listeners.remove(0);
        config.listeners.pop();
        assert_eq!(
            active.matching_listener_ids(&ListenerSet::from_config(&config)),
            vec![Some(ListenerId(0)), None, None]
        );
        assert_eq!(active.status()[0].phase, super::ListenerPhase::TimedOut);
    }

    #[test]
    fn timeout_command_fires_once_until_activity() {
        let mut set = ListenerSet::from_config(&sample_config());

        assert_eq!(
            set.mark_timed_out(ListenerId(0)),
            Ok(Some(CommandIntent {
                listener_id: ListenerId(0),
                kind: CommandKind::Timeout,
                command: "veila lock --wait-ready".to_string()
            }))
        );
        assert_eq!(set.mark_timed_out(ListenerId(0)), Ok(None));

        assert_eq!(set.mark_resumed(ListenerId(0)), Ok(None));

        assert_eq!(
            set.mark_timed_out(ListenerId(0)),
            Ok(Some(CommandIntent {
                listener_id: ListenerId(0),
                kind: CommandKind::Timeout,
                command: "veila lock --wait-ready".to_string()
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
                command: "sweets msg output-power '*' off".to_string()
            }))
        );

        assert_eq!(
            set.mark_resumed(ListenerId(1)),
            Ok(Some(CommandIntent {
                listener_id: ListenerId(1),
                kind: CommandKind::Resume,
                command: "sweets msg output-power '*' on".to_string()
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
                command: "sweets msg output-power '*' on".to_string(),
            }]
        );
        assert_eq!(
            set.mark_timed_out(ListenerId(1)),
            Ok(Some(CommandIntent {
                listener_id: ListenerId(1),
                kind: CommandKind::Timeout,
                command: "sweets msg output-power '*' off".to_string(),
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
                    on_timeout: Some("veila lock --wait-ready".to_string()),
                    on_resume: None,
                    ignore_inhibit: true,
                },
                ListenerConfig {
                    timeout_seconds: 600,
                    on_timeout: Some("sweets msg output-power '*' off".to_string()),
                    on_resume: Some("sweets msg output-power '*' on".to_string()),
                    ignore_inhibit: false,
                },
            ],
        }
    }

    #[test]
    fn reload_retains_a_timed_out_listener_until_its_own_resume() {
        let mut config = sample_config();
        config.listeners[0].on_resume = Some("resume".into());
        let mut active = ListenerSet::from_config(&config);
        assert!(active.mark_timed_out(ListenerId(0)).is_ok());
        let mut reload = match active.prepare_reload(&config) {
            Ok(reload) => reload,
            Err(error) => panic!("reload rejected: {error:?}"),
        };

        assert_eq!(reload.retained_ids, [(ListenerId(0), ListenerId(0))]);
        assert_eq!(reload.listeners.mark_timed_out(ListenerId(0)), Ok(None));
        assert!(matches!(
            reload.listeners.mark_resumed(ListenerId(0)),
            Ok(Some(_))
        ));
        assert_eq!(reload.listeners.mark_resumed(ListenerId(0)), Ok(None));
    }

    #[test]
    fn reload_rejects_every_definition_change_to_a_timed_out_listener() {
        let config = sample_config();
        let mut active = ListenerSet::from_config(&config);
        assert!(active.mark_timed_out(ListenerId(0)).is_ok());
        for change in 0..4 {
            let mut candidate = config.clone();
            match change {
                0 => candidate.listeners[0].timeout_seconds += 1,
                1 => candidate.listeners[0].on_timeout = Some("changed timeout".into()),
                2 => candidate.listeners[0].on_resume = Some("changed resume".into()),
                _ => candidate.listeners[0].ignore_inhibit = !candidate.listeners[0].ignore_inhibit,
            }
            assert!(matches!(
                active.prepare_reload(&candidate),
                Err(super::ListenerError::TimedOutListenerChanged(ListenerId(0)))
            ));
        }
        assert_eq!(active.status()[0].phase, super::ListenerPhase::TimedOut);
    }

    #[test]
    fn reload_rejects_removal_of_a_timed_out_listener_without_commands() {
        let mut config =
            Config::parse("[[listener]]\ntimeout = 30\n").unwrap_or_else(|e| panic!("{e}"));
        let mut active = ListenerSet::from_config(&config);
        assert_eq!(active.mark_timed_out(ListenerId(0)), Ok(None));
        config.listeners.clear();

        assert!(matches!(
            active.prepare_reload(&config),
            Err(super::ListenerError::TimedOutListenerChanged(ListenerId(0)))
        ));
    }

    #[test]
    fn reload_matches_timed_out_duplicates_before_armed_copies() {
        let mut config = sample_config();
        config.listeners = vec![config.listeners[0].clone(); 2];
        let mut active = ListenerSet::from_config(&config);
        assert!(active.mark_timed_out(ListenerId(1)).is_ok());
        config.listeners.pop();
        let reload = match active.prepare_reload(&config) {
            Ok(reload) => reload,
            Err(error) => panic!("reload rejected: {error:?}"),
        };

        assert_eq!(reload.matching_ids, [None, Some(ListenerId(0))]);
        assert_eq!(reload.retained_ids, [(ListenerId(1), ListenerId(0))]);
        assert_eq!(
            reload.listeners.status()[0].phase,
            super::ListenerPhase::TimedOut
        );
    }

    #[test]
    fn reload_requires_a_copy_for_every_timed_out_duplicate() {
        let mut config = sample_config();
        config.listeners = vec![config.listeners[0].clone(); 2];
        let mut active = ListenerSet::from_config(&config);
        assert!(active.mark_timed_out(ListenerId(0)).is_ok());
        assert!(active.mark_timed_out(ListenerId(1)).is_ok());
        config.listeners.pop();

        assert!(matches!(
            active.prepare_reload(&config),
            Err(ListenerError::TimedOutListenerChanged(ListenerId(1)))
        ));
    }
}
