use std::{error::Error, fmt};

use crate::listener::{CommandIntent, CommandKind, ListenerId};

mod process;

pub use process::{ProcessAdapter, ProcessCompletion, ProcessError, ProcessStart};

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
    Lock,
    Unlock,
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
        // one running command occupies each listener event or session hook slot
        let slot = Some(CommandSlot { context });

        if let Some(command) = self.running.iter().find(|command| command.slot == slot) {
            return DispatchDecision::Skip(SkipDecision {
                context,
                reason: SkipReason::AlreadyRunning(command.command_id),
            });
        }

        // ids keep process completion tied to the original dispatch
        let command_id = CommandId(self.next_id);
        self.next_id += 1;

        self.running.push(RunningCommand {
            command_id,
            context,
            slot,
        });

        DispatchDecision::Spawn(SpawnRequest {
            command_id,
            context,
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
            context: command.context,
        })
    }

    #[cfg(test)]
    pub fn running_count(&self) -> usize {
        self.running.len()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.running.is_empty()
    }

    pub(crate) fn remap_listener_slots(&mut self, mapping: &[Option<ListenerId>]) {
        for command in &mut self.running {
            let Some(CommandSlot {
                context: CommandContext::Listener { listener_id, kind },
            }) = command.slot
            else {
                continue;
            };

            // removed or changed listeners retain their process but release their slot
            command.slot = mapping
                .get(listener_id.index())
                .copied()
                .flatten()
                .map(|listener_id| CommandSlot {
                    context: CommandContext::Listener { listener_id, kind },
                });
        }
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
    context: CommandContext,
    slot: Option<CommandSlot>,
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
    fn session_commands_have_independent_command_slots() {
        let mut dispatcher = CommandDispatcher::default();

        assert!(matches!(
            dispatcher.dispatch_command(CommandContext::Lock, "lock".to_string()),
            DispatchDecision::Spawn(_)
        ));
        assert!(matches!(
            dispatcher.dispatch_command(CommandContext::Unlock, "unlock".to_string()),
            DispatchDecision::Spawn(_)
        ));
        assert!(matches!(
            dispatcher.dispatch_command(CommandContext::BeforeSleep, "prepare".to_string()),
            DispatchDecision::Spawn(_)
        ));
        assert!(matches!(
            dispatcher.dispatch_command(CommandContext::AfterResume, "outputs on".to_string()),
            DispatchDecision::Spawn(_)
        ));
        assert_eq!(dispatcher.running_count(), 4);
    }

    #[test]
    fn listener_slots_follow_reordering_and_keep_their_original_metadata() {
        let mut dispatcher = CommandDispatcher::default();
        for kind in [CommandKind::Timeout, CommandKind::Resume] {
            let DispatchDecision::Spawn(start) = dispatcher.dispatch(intent(0, kind, "hook"))
            else {
                panic!("expected the listener hook to start");
            };
            dispatcher.remap_listener_slots(&[Some(ListenerId::from_index(1))]);

            assert_eq!(
                dispatcher.dispatch(intent(1, kind, "hook")),
                DispatchDecision::Skip(SkipDecision {
                    context: listener_context(1, kind),
                    reason: SkipReason::AlreadyRunning(start.command_id),
                })
            );
            assert_eq!(
                dispatcher.finish(start.command_id),
                Ok(FinishedCommand {
                    command_id: start.command_id,
                    context: listener_context(0, kind),
                })
            );
        }
    }

    #[test]
    fn session_slots_remain_busy_when_listeners_change() {
        let mut dispatcher = CommandDispatcher::default();
        for context in [
            CommandContext::Lock,
            CommandContext::Unlock,
            CommandContext::BeforeSleep,
            CommandContext::AfterResume,
        ] {
            let DispatchDecision::Spawn(start) =
                dispatcher.dispatch_command(context, "hook".to_string())
            else {
                panic!("expected the session hook to start");
            };
            dispatcher.remap_listener_slots(&[]);

            assert_eq!(
                dispatcher.dispatch_command(context, "new hook".to_string()),
                DispatchDecision::Skip(SkipDecision {
                    context,
                    reason: SkipReason::AlreadyRunning(start.command_id),
                })
            );
        }
    }

    #[test]
    fn completing_a_removed_listener_does_not_release_a_replacement_slot() {
        let mut dispatcher = CommandDispatcher::default();
        let DispatchDecision::Spawn(old) =
            dispatcher.dispatch(intent(0, CommandKind::Timeout, "old"))
        else {
            panic!("expected the old hook to start");
        };
        dispatcher.remap_listener_slots(&[None]);
        let DispatchDecision::Spawn(new) =
            dispatcher.dispatch(intent(0, CommandKind::Timeout, "new"))
        else {
            panic!("expected the replacement hook to start");
        };
        // later reloads must not attach a retired slot to a reintroduced listener
        dispatcher.remap_listener_slots(&[Some(ListenerId::from_index(0))]);
        assert_eq!(
            dispatcher.finish(old.command_id),
            Ok(FinishedCommand {
                command_id: old.command_id,
                context: listener_context(0, CommandKind::Timeout),
            })
        );

        assert_eq!(
            dispatcher.dispatch(intent(0, CommandKind::Timeout, "new")),
            DispatchDecision::Skip(SkipDecision {
                context: listener_context(0, CommandKind::Timeout),
                reason: SkipReason::AlreadyRunning(new.command_id),
            })
        );
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
