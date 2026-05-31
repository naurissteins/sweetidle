use std::{error::Error, fmt};

use sweetsidle_core::{CommandIntent, CommandKind, ListenerId};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CommandId(usize);

impl CommandId {
    pub fn index(self) -> usize {
        self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnRequest {
    pub command_id: CommandId,
    pub listener_id: ListenerId,
    pub kind: CommandKind,
    pub command: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DispatchDecision {
    Spawn(SpawnRequest),
    Skip(SkipDecision),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkipDecision {
    pub listener_id: ListenerId,
    pub kind: CommandKind,
    pub reason: SkipReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    AlreadyRunning(CommandId),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinishedCommand {
    pub command_id: CommandId,
    pub listener_id: ListenerId,
    pub kind: CommandKind,
    pub command: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandDispatchError {
    UnknownCommand(CommandId),
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CommandDispatcher {
    next_id: usize,
    running: Vec<RunningCommand>,
}

impl CommandDispatcher {
    pub fn dispatch(&mut self, intent: CommandIntent) -> DispatchDecision {
        let slot = CommandSlot {
            listener_id: intent.listener_id,
            kind: intent.kind,
        };

        if let Some(command) = self.running.iter().find(|command| command.slot == slot) {
            return DispatchDecision::Skip(SkipDecision {
                listener_id: slot.listener_id,
                kind: slot.kind,
                reason: SkipReason::AlreadyRunning(command.command_id),
            });
        }

        let command_id = CommandId(self.next_id);
        self.next_id += 1;

        self.running.push(RunningCommand {
            command_id,
            slot,
            command: intent.command.clone(),
        });

        DispatchDecision::Spawn(SpawnRequest {
            command_id,
            listener_id: slot.listener_id,
            kind: slot.kind,
            command: intent.command,
        })
    }

    pub fn finish(
        &mut self,
        command_id: CommandId,
    ) -> Result<FinishedCommand, CommandDispatchError> {
        let Some(position) = self
            .running
            .iter()
            .position(|command| command.command_id == command_id)
        else {
            return Err(CommandDispatchError::UnknownCommand(command_id));
        };

        let command = self.running.remove(position);

        Ok(FinishedCommand {
            command_id,
            listener_id: command.slot.listener_id,
            kind: command.slot.kind,
            command: command.command,
        })
    }

    pub fn running_count(&self) -> usize {
        self.running.len()
    }

    pub fn is_empty(&self) -> bool {
        self.running.is_empty()
    }
}

impl fmt::Display for CommandDispatchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownCommand(command_id) => {
                write!(formatter, "unknown command id {}", command_id.index())
            }
        }
    }
}

impl Error for CommandDispatchError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CommandSlot {
    listener_id: ListenerId,
    kind: CommandKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RunningCommand {
    command_id: CommandId,
    slot: CommandSlot,
    command: String,
}

#[cfg(test)]
mod tests {
    use sweetsidle_core::{CommandIntent, CommandKind, ListenerId};

    use super::{
        CommandDispatchError, CommandDispatcher, CommandId, DispatchDecision, FinishedCommand,
        SkipDecision, SkipReason, SpawnRequest,
    };

    #[test]
    fn dispatch_creates_spawn_requests_with_stable_ids() {
        let mut dispatcher = CommandDispatcher::default();

        assert_eq!(
            dispatcher.dispatch(intent(0, CommandKind::Timeout, "lock")),
            DispatchDecision::Spawn(SpawnRequest {
                command_id: CommandId(0),
                listener_id: ListenerId::from_index(0),
                kind: CommandKind::Timeout,
                command: "lock".to_string()
            })
        );
        assert_eq!(
            dispatcher.dispatch(intent(1, CommandKind::Timeout, "dpms off")),
            DispatchDecision::Spawn(SpawnRequest {
                command_id: CommandId(1),
                listener_id: ListenerId::from_index(1),
                kind: CommandKind::Timeout,
                command: "dpms off".to_string()
            })
        );
        assert_eq!(dispatcher.running_count(), 2);
    }

    #[test]
    fn duplicate_listener_kind_is_skipped_while_running() {
        let mut dispatcher = CommandDispatcher::default();

        assert!(matches!(
            dispatcher.dispatch(intent(0, CommandKind::Timeout, "lock")),
            DispatchDecision::Spawn(_)
        ));
        assert_eq!(
            dispatcher.dispatch(intent(0, CommandKind::Timeout, "lock again")),
            DispatchDecision::Skip(SkipDecision {
                listener_id: ListenerId::from_index(0),
                kind: CommandKind::Timeout,
                reason: SkipReason::AlreadyRunning(CommandId(0))
            })
        );
    }

    #[test]
    fn different_kinds_can_run_for_the_same_listener() {
        let mut dispatcher = CommandDispatcher::default();

        assert!(matches!(
            dispatcher.dispatch(intent(0, CommandKind::Timeout, "dpms off")),
            DispatchDecision::Spawn(_)
        ));
        assert!(matches!(
            dispatcher.dispatch(intent(0, CommandKind::Resume, "dpms on")),
            DispatchDecision::Spawn(_)
        ));
        assert_eq!(dispatcher.running_count(), 2);
    }

    #[test]
    fn finish_releases_the_command_slot() {
        let mut dispatcher = CommandDispatcher::default();

        assert!(matches!(
            dispatcher.dispatch(intent(0, CommandKind::Timeout, "lock")),
            DispatchDecision::Spawn(_)
        ));
        assert_eq!(
            dispatcher.finish(CommandId(0)),
            Ok(FinishedCommand {
                command_id: CommandId(0),
                listener_id: ListenerId::from_index(0),
                kind: CommandKind::Timeout,
                command: "lock".to_string()
            })
        );
        assert!(dispatcher.is_empty());
        assert!(matches!(
            dispatcher.dispatch(intent(0, CommandKind::Timeout, "lock")),
            DispatchDecision::Spawn(_)
        ));
    }

    #[test]
    fn rejects_unknown_command_completion() {
        let mut dispatcher = CommandDispatcher::default();

        assert_eq!(
            dispatcher.finish(CommandId(7)),
            Err(CommandDispatchError::UnknownCommand(CommandId(7)))
        );
    }

    fn intent(listener_index: usize, kind: CommandKind, command: &str) -> CommandIntent {
        CommandIntent {
            listener_id: ListenerId::from_index(listener_index),
            kind,
            command: command.to_string(),
        }
    }
}
