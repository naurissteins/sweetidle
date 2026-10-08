use std::{
    collections::{HashMap, HashSet},
    io,
};

use calloop::{Interest, LoopHandle, Mode, PostAction, generic::Generic};
use rustix::process::{Pid, PidfdFlags, pidfd_open};

use super::{RuntimeError, RuntimeEvent, state::RuntimeState};
use crate::{
    command::{
        CommandContext, CommandDispatcher, CommandId, DispatchDecision, ProcessAdapter,
        ProcessStart, SkipReason,
    },
    listener::{CommandIntent, ListenerId},
    session::DelayInhibitor,
};

#[derive(Default)]
pub(super) struct CommandRuntime {
    dispatcher: CommandDispatcher,
    processes: ProcessAdapter,
    ready_processes: Vec<CommandId>,
    // reaping can precede another child's pidfd callback, which must not fail or repeat completion
    completed_before_monitor: HashSet<CommandId>,
    delay_inhibitors: HashMap<CommandId, Vec<DelayInhibitor>>,
}

impl CommandRuntime {
    pub(super) fn dispatch_listener(
        &mut self,
        handle: &LoopHandle<'static, RuntimeState>,
        intent: CommandIntent,
        events: &mut Vec<RuntimeEvent>,
    ) -> Result<(), RuntimeError> {
        let decision = self.dispatcher.dispatch(intent);
        self.dispatch_decision(handle, decision, None, events)
    }

    pub(super) fn dispatch_session(
        &mut self,
        handle: &LoopHandle<'static, RuntimeState>,
        context: CommandContext,
        command: String,
        inhibitor: Option<DelayInhibitor>,
        events: &mut Vec<RuntimeEvent>,
    ) -> Result<(), RuntimeError> {
        let decision = self.dispatcher.dispatch_command(context, command);
        self.dispatch_decision(handle, decision, inhibitor, events)
    }

    pub(super) fn remap_listener_slots(&mut self, mapping: &[Option<ListenerId>]) {
        self.dispatcher.remap_listener_slots(mapping);
    }

    pub(super) fn release_sleep_inhibitors(&mut self) {
        self.delay_inhibitors.clear();
    }

    #[cfg(test)]
    pub(super) fn reserve_listener(&mut self, intent: CommandIntent) -> DispatchDecision {
        // reload policy tests reserve slots without spawning configured helpers
        self.dispatcher.dispatch(intent)
    }

    #[cfg(test)]
    pub(super) fn running_count(&self) -> usize {
        self.processes.running_count()
    }

    #[cfg(test)]
    pub(super) fn is_empty(&self) -> bool {
        self.dispatcher.is_empty() && self.processes.is_empty() && self.delay_inhibitors.is_empty()
    }

    pub(super) fn collect_ready_processes(
        &mut self,
        events: &mut Vec<RuntimeEvent>,
    ) -> Result<(), RuntimeError> {
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
            events.push(RuntimeEvent::CommandFinished {
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

    fn mark_process_ready(&mut self, command_id: CommandId) {
        self.ready_processes.push(command_id);
    }

    fn dispatch_decision(
        &mut self,
        handle: &LoopHandle<'static, RuntimeState>,
        decision: DispatchDecision,
        inhibitor: Option<DelayInhibitor>,
        events: &mut Vec<RuntimeEvent>,
    ) -> Result<(), RuntimeError> {
        match decision {
            DispatchDecision::Skip(decision) => {
                if let Some(inhibitor) = inhibitor {
                    // a new sleep attempt must still wait for its already-running before-sleep hook
                    let SkipReason::AlreadyRunning(command_id) = decision.reason;
                    self.delay_inhibitors
                        .entry(command_id)
                        .or_default()
                        .push(inhibitor);
                }
                events.push(RuntimeEvent::CommandSkipped(decision));
            }
            DispatchDecision::Spawn(request) => {
                if let Some(inhibitor) = inhibitor {
                    self.delay_inhibitors
                        .entry(request.command_id)
                        .or_default()
                        .push(inhibitor);
                }

                match self.processes.spawn(&request) {
                    Ok(start) => {
                        register_process_monitor(handle, start)?;
                        events.push(RuntimeEvent::CommandStarted(start));
                    }
                    Err(error) => {
                        self.delay_inhibitors.remove(&request.command_id);
                        let command = self
                            .dispatcher
                            .finish(request.command_id)
                            .map_err(RuntimeError::CommandDispatch)?;
                        events.push(RuntimeEvent::CommandFailed { command, error });
                    }
                }
            }
        }

        Ok(())
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

    // level readiness catches prior exits; removing the source prevents repeated wakeups
    handle
        .insert_source(
            Generic::new(pidfd, Interest::READ, Mode::Level),
            move |_, _, state: &mut RuntimeState| {
                state.commands.mark_process_ready(command_id);
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
mod tests;
