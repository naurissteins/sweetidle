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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandIntent {
    pub listener_id: ListenerId,
    pub kind: CommandKind,
    pub command: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

    pub fn mark_active(&mut self) -> Vec<CommandIntent> {
        let mut intents = Vec::new();

        for (index, listener) in self.listeners.iter_mut().enumerate() {
            if listener.phase == ListenerPhase::Armed {
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
    phase: ListenerPhase,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ListenerPhase {
    Armed,
    TimedOut,
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{CommandIntent, CommandKind, ListenerError, ListenerId, ListenerSet};
    use crate::config::{Config, GeneralConfig, ListenerConfig};

    #[test]
    fn exposes_timeout_requests_in_config_order() {
        let set = ListenerSet::from_config(&sample_config());

        assert_eq!(set.len(), 2);
        assert_eq!(set.timeout_requests()[0].listener_id.index(), 0);
        assert_eq!(set.timeout_requests()[0].timeout, Duration::from_secs(300));
        assert_eq!(set.timeout_requests()[1].listener_id.index(), 1);
        assert_eq!(set.timeout_requests()[1].timeout, Duration::from_secs(600));
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

        let resume = set.mark_active();
        assert_eq!(resume, Vec::<CommandIntent>::new());

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

        assert_eq!(set.mark_active(), Vec::<CommandIntent>::new());
        assert_eq!(
            set.mark_timed_out(ListenerId(1)),
            Ok(Some(CommandIntent {
                listener_id: ListenerId(1),
                kind: CommandKind::Timeout,
                command: "sweetsmsg output '*' power off".to_string()
            }))
        );

        assert_eq!(
            set.mark_active(),
            vec![CommandIntent {
                listener_id: ListenerId(1),
                kind: CommandKind::Resume,
                command: "sweetsmsg output '*' power on".to_string()
            }]
        );
        assert_eq!(set.mark_active(), Vec::<CommandIntent>::new());
    }

    #[test]
    fn listener_without_timeout_command_still_tracks_resume() {
        let config = Config {
            general: GeneralConfig::default(),
            listeners: vec![ListenerConfig {
                timeout_seconds: 5,
                on_timeout: None,
                on_resume: Some("resume".to_string()),
            }],
        };
        let mut set = ListenerSet::from_config(&config);

        assert_eq!(set.mark_timed_out(ListenerId(0)), Ok(None));
        assert_eq!(
            set.mark_active(),
            vec![CommandIntent {
                listener_id: ListenerId(0),
                kind: CommandKind::Resume,
                command: "resume".to_string()
            }]
        );
    }

    #[test]
    fn rejects_unknown_listener_ids() {
        let mut set = ListenerSet::from_config(&sample_config());

        assert_eq!(
            set.mark_timed_out(ListenerId(99)),
            Err(ListenerError::UnknownListener(ListenerId(99)))
        );
    }

    fn sample_config() -> Config {
        Config {
            general: GeneralConfig::default(),
            listeners: vec![
                ListenerConfig {
                    timeout_seconds: 300,
                    on_timeout: Some("sweetslock".to_string()),
                    on_resume: None,
                },
                ListenerConfig {
                    timeout_seconds: 600,
                    on_timeout: Some("sweetsmsg output '*' power off".to_string()),
                    on_resume: Some("sweetsmsg output '*' power on".to_string()),
                },
            ],
        }
    }
}
