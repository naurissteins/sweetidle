use std::{error::Error, fmt};

use crate::listener::{CommandIntent, CommandKind, ListenerId};

mod process;

pub use process::{
    ProcessAdapter, ProcessCompletion, ProcessError, ProcessStart, RunningProcessInfo,
};

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
    pub context: CommandContext,
    pub command: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CommandContext {
    Listener {
        listener_id: ListenerId,
        kind: CommandKind,
    },
    BeforeSleep,
    AfterResume,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DispatchDecision {
    Spawn(SpawnRequest),
    Skip(SkipDecision),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkipDecision {
    pub context: CommandContext,
    pub reason: SkipReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    AlreadyRunning(CommandId),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinishedCommand {
    pub command_id: CommandId,
    pub context: CommandContext,
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
        self.dispatch_command(
            CommandContext::Listener {
                listener_id: intent.listener_id,
                kind: intent.kind,
            },
            intent.command,
        )
    }

    pub fn dispatch_command(
        &mut self,
        context: CommandContext,
        command: String,
    ) -> DispatchDecision {
        let slot = CommandSlot { context };

        if let Some(command) = self.running.iter().find(|command| command.slot == slot) {
            return DispatchDecision::Skip(SkipDecision {
                context: slot.context,
                reason: SkipReason::AlreadyRunning(command.command_id),
            });
        }

        let command_id = CommandId(self.next_id);
        self.next_id += 1;

        self.running.push(RunningCommand {
            command_id,
            slot,
            command: command.clone(),
        });

        DispatchDecision::Spawn(SpawnRequest {
            command_id,
            context: slot.context,
            command,
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
            context: command.slot.context,
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
    context: CommandContext,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RunningCommand {
    command_id: CommandId,
    slot: CommandSlot,
    command: String,
}

#[cfg(test)]
mod tests {
    use super::{
        CommandContext, CommandDispatchError, CommandDispatcher, CommandId, DispatchDecision,
        FinishedCommand, SkipDecision, SkipReason, SpawnRequest,
    };
    use crate::listener::{CommandIntent, CommandKind, ListenerId};

    #[test]
    fn dispatch_creates_spawn_requests_with_stable_ids() {
        let mut dispatcher = CommandDispatcher::default();

        assert_eq!(
            dispatcher.dispatch(intent(0, CommandKind::Timeout, "lock")),
            DispatchDecision::Spawn(SpawnRequest {
                command_id: CommandId(0),
                context: listener_context(0, CommandKind::Timeout),
                command: "lock".to_string()
            })
        );
        assert_eq!(
            dispatcher.dispatch(intent(1, CommandKind::Timeout, "dpms off")),
            DispatchDecision::Spawn(SpawnRequest {
                command_id: CommandId(1),
                context: listener_context(1, CommandKind::Timeout),
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
                context: listener_context(0, CommandKind::Timeout),
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
    fn sleep_hooks_have_independent_command_slots() {
        let mut dispatcher = CommandDispatcher::default();

        assert!(matches!(
            dispatcher.dispatch_command(CommandContext::BeforeSleep, "lock".to_string()),
            DispatchDecision::Spawn(_)
        ));
        assert!(matches!(
            dispatcher.dispatch_command(CommandContext::AfterResume, "outputs on".to_string()),
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
                context: listener_context(0, CommandKind::Timeout),
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

    fn listener_context(listener_index: usize, kind: CommandKind) -> CommandContext {
        CommandContext::Listener {
            listener_id: ListenerId::from_index(listener_index),
            kind,
        }
    }
}
