use std::{
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, Sender},
    thread::{self, JoinHandle},
};

use calloop::channel::{Channel, Sender as EventSender, channel};

use super::RuntimeError;
use crate::config::{Config, ConfigLoadError};

pub(super) enum ConfigReloadEvent {
    Ready(Config),
    Rejected(ConfigLoadError),
}

#[derive(Clone)]
pub(super) struct ConfigReloadRequester {
    requests: Sender<WorkerRequest>,
}

pub(super) struct ConfigReloadMonitor {
    requests: Option<Sender<WorkerRequest>>,
    thread: Option<JoinHandle<()>>,
}

impl ConfigReloadMonitor {
    pub(super) fn connect(
        path: PathBuf,
    ) -> Result<(Self, ConfigReloadRequester, Channel<ConfigReloadEvent>), RuntimeError> {
        let (request_sender, request_receiver) = mpsc::channel();
        let (event_sender, event_channel) = channel();
        let thread = thread::Builder::new()
            .name("sweetsidle-config".to_string())
            .spawn(move || run_worker(&path, &request_receiver, &event_sender))
            .map_err(RuntimeError::SpawnReloadWorker)?;

        Ok((
            Self {
                requests: Some(request_sender.clone()),
                thread: Some(thread),
            },
            ConfigReloadRequester {
                requests: request_sender,
            },
            event_channel,
        ))
    }
}

impl ConfigReloadRequester {
    pub(super) fn request(&self) -> Result<(), RuntimeError> {
        self.requests
            .send(WorkerRequest::Reload)
            .map_err(|_| RuntimeError::ReloadRequestChannelClosed)
    }
}

impl Drop for ConfigReloadMonitor {
    fn drop(&mut self) {
        if let Some(requests) = self.requests.take() {
            let _ = requests.send(WorkerRequest::Shutdown);
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn run_worker(
    path: &Path,
    requests: &Receiver<WorkerRequest>,
    events: &EventSender<ConfigReloadEvent>,
) {
    while let Ok(request) = requests.recv() {
        match request {
            WorkerRequest::Reload => {
                let event = match Config::load(path) {
                    Ok(config) => ConfigReloadEvent::Ready(config),
                    Err(error) => ConfigReloadEvent::Rejected(error),
                };
                if events.send(event).is_err() {
                    return;
                }
            }
            WorkerRequest::Shutdown => return,
        }
    }
}

enum WorkerRequest {
    Reload,
    Shutdown,
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, sync::mpsc::TryRecvError, thread, time::Duration};

    use super::{ConfigReloadEvent, ConfigReloadMonitor};

    #[test]
    fn request_loads_config_on_the_worker() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("config/example.kdl");
        let (monitor, requester, events) = match ConfigReloadMonitor::connect(path) {
            Ok(parts) => parts,
            Err(error) => panic!("failed to start reload monitor: {error}"),
        };
        if let Err(error) = requester.request() {
            panic!("failed to request config reload: {error}");
        }

        for _ in 0..100 {
            match events.try_recv() {
                Ok(ConfigReloadEvent::Ready(config)) => {
                    assert_eq!(config.listeners.len(), 2);
                    drop(monitor);
                    return;
                }
                Ok(ConfigReloadEvent::Rejected(error)) => {
                    panic!("worker rejected example config: {error}");
                }
                Err(TryRecvError::Empty) => thread::sleep(Duration::from_millis(10)),
                Err(TryRecvError::Disconnected) => panic!("reload result channel closed"),
            }
        }

        panic!("timed out waiting for config reload");
    }
}
