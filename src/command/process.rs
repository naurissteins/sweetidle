use std::{
    error::Error,
    fmt, io,
    os::unix::process::CommandExt,
    process::{Child, Command, ExitStatus, Stdio},
};

use super::{CommandContext, CommandId, SpawnRequest};

#[derive(Debug, Default)]
pub struct ProcessAdapter {
    running: Vec<RunningProcess>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessStart {
    pub command_id: CommandId,
    pub context: CommandContext,
    pub process_id: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessCompletion {
    pub command_id: CommandId,
    pub status: ExitStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunningProcessInfo {
    pub command_id: CommandId,
    pub process_id: u32,
}

#[derive(Debug)]
pub enum ProcessError {
    Spawn {
        command_id: CommandId,
        source: io::Error,
    },
    Wait {
        command_id: CommandId,
        source: io::Error,
    },
}

impl ProcessAdapter {
    pub fn spawn(&mut self, request: &SpawnRequest) -> Result<ProcessStart, ProcessError> {
        // configured commands use the shell and discard standard streams
        let child = Command::new("sh")
            .arg("-c")
            .arg(&request.command)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            // keep terminal interrupts from reaching running lockers and helpers
            .process_group(0)
            .spawn()
            .map_err(|source| ProcessError::Spawn {
                command_id: request.command_id,
                source,
            })?;

        let process_id = child.id();
        self.running.push(RunningProcess {
            command_id: request.command_id,
            process_id,
            child,
        });

        Ok(ProcessStart {
            command_id: request.command_id,
            context: request.context,
            process_id,
        })
    }

    pub fn collect_finished(&mut self) -> Result<Vec<ProcessCompletion>, ProcessError> {
        // try_wait reaps children without blocking the event loop
        let mut finished = Vec::new();
        let mut index = 0;

        while index < self.running.len() {
            let Some(status) =
                self.running[index]
                    .child
                    .try_wait()
                    .map_err(|source| ProcessError::Wait {
                        command_id: self.running[index].command_id,
                        source,
                    })?
            else {
                index += 1;
                continue;
            };

            let process = self.running.remove(index);
            finished.push(ProcessCompletion {
                command_id: process.command_id,
                status,
            });
        }

        Ok(finished)
    }

    pub fn running_processes(&self) -> Vec<RunningProcessInfo> {
        self.running
            .iter()
            .map(|process| RunningProcessInfo {
                command_id: process.command_id,
                process_id: process.process_id,
            })
            .collect()
    }

    pub fn running_count(&self) -> usize {
        self.running.len()
    }

    pub fn is_empty(&self) -> bool {
        self.running.is_empty()
    }
}

impl fmt::Display for ProcessError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spawn { command_id, source } => write!(
                formatter,
                "failed to spawn command {}: {source}",
                command_id.index()
            ),
            Self::Wait { command_id, source } => {
                write!(
                    formatter,
                    "failed to poll command {}: {source}",
                    command_id.index()
                )
            }
        }
    }
}

impl Error for ProcessError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Spawn { source, .. } | Self::Wait { source, .. } => Some(source),
        }
    }
}

#[derive(Debug)]
struct RunningProcess {
    command_id: CommandId,
    process_id: u32,
    child: Child,
}

#[cfg(test)]
mod tests {
    use std::{thread, time::Duration};

    use rustix::process::{Pid, getpgid};

    use super::{CommandContext, CommandId, ProcessAdapter, SpawnRequest};
    use crate::listener::{CommandKind, ListenerId};

    #[test]
    fn spawns_and_collects_a_finished_command() {
        let mut processes = ProcessAdapter::default();

        let start = match processes.spawn(&request(0, "exit 0")) {
            Ok(start) => start,
            Err(error) => panic!("{error}"),
        };

        assert_eq!(start.command_id, CommandId(0));
        assert_eq!(processes.running_count(), 1);

        let completions = wait_for_completion(&mut processes);
        assert_eq!(completions.len(), 1);
        assert_eq!(completions[0].command_id, CommandId(0));
        assert!(completions[0].status.success());
        assert!(processes.is_empty());
    }

    #[test]
    fn preserves_non_zero_exit_status() {
        let mut processes = ProcessAdapter::default();

        match processes.spawn(&request(1, "exit 7")) {
            Ok(_) => {}
            Err(error) => panic!("{error}"),
        }

        let completions = wait_for_completion(&mut processes);
        assert_eq!(completions.len(), 1);
        assert_eq!(completions[0].command_id, CommandId(1));
        assert_eq!(completions[0].status.code(), Some(7));
    }

    #[test]
    fn reports_running_processes() {
        let mut processes = ProcessAdapter::default();

        let start = match processes.spawn(&request(2, "sleep 0.05")) {
            Ok(start) => start,
            Err(error) => panic!("{error}"),
        };

        assert_eq!(
            processes.running_processes(),
            vec![super::RunningProcessInfo {
                command_id: CommandId(2),
                process_id: start.process_id
            }]
        );

        let _ = wait_for_completion(&mut processes);
    }

    #[test]
    fn starts_commands_in_their_own_process_group() -> Result<(), Box<dyn std::error::Error>> {
        let mut processes = ProcessAdapter::default();
        processes.spawn(&request(3, "exec sleep 1"))?;
        let child = &mut processes.running[0].child;
        let pid = Pid::from_child(child);
        let group = getpgid(Some(pid));

        // reap the helper before asserting so a regression cannot leave it running
        child.kill()?;
        child.wait()?;

        assert_eq!(group?, pid);
        Ok(())
    }

    #[test]
    fn spawn_failure_keeps_io_details_without_command_text()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut processes = ProcessAdapter::default();
        let error = match processes.spawn(&request(4, "printf secret-command-token\0")) {
            Ok(start) => panic!("unexpected process start: {start:?}"),
            Err(error) => error,
        };

        assert!(matches!(
            &error,
            super::ProcessError::Spawn { command_id, .. } if *command_id == CommandId(4)
        ));
        assert!(error.to_string().contains("failed to spawn command 4"));
        assert!(!error.to_string().contains("secret-command-token"));
        assert!(!format!("{error:?}").contains("secret-command-token"));
        let source = std::error::Error::source(&error)
            .and_then(|source| source.downcast_ref::<std::io::Error>())
            .ok_or("spawn failure lost its I/O source")?;
        assert_eq!(source.kind(), std::io::ErrorKind::InvalidInput);
        assert!(processes.is_empty());
        Ok(())
    }

    fn request(command_index: usize, command: &str) -> SpawnRequest {
        SpawnRequest {
            command_id: CommandId(command_index),
            context: CommandContext::Listener {
                listener_id: ListenerId::from_index(0),
                kind: CommandKind::Timeout,
            },
            command: command.to_string(),
        }
    }

    fn wait_for_completion(processes: &mut ProcessAdapter) -> Vec<super::ProcessCompletion> {
        for _ in 0..50 {
            let completions = match processes.collect_finished() {
                Ok(completions) => completions,
                Err(error) => panic!("{error}"),
            };

            if !completions.is_empty() {
                return completions;
            }

            thread::sleep(Duration::from_millis(10));
        }

        panic!("command did not finish");
    }
}
