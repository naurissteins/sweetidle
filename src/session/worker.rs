use std::{
    io,
    sync::mpsc::{self, SyncSender},
    thread::{self, JoinHandle},
};

use async_channel::{Receiver, Sender, bounded};

pub(super) struct Worker {
    shutdown: Option<Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

#[derive(Debug)]
pub(super) enum WorkerError {
    Spawn(io::Error),
    InitializationStopped,
}

impl Worker {
    pub(super) fn spawn<E>(
        name: &str,
        body: impl FnOnce(SyncSender<Result<(), E>>, Receiver<()>) + Send + 'static,
    ) -> Result<Self, E>
    where
        E: From<WorkerError> + Send + 'static,
    {
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        let (shutdown_sender, shutdown_receiver) = bounded(1);
        let thread = thread::Builder::new()
            .name(name.to_string())
            .spawn(move || body(ready_sender, shutdown_receiver))
            .map_err(|error| E::from(WorkerError::Spawn(error)))?;
        match ready_receiver.recv() {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                let _ = thread.join();
                return Err(error);
            }
            Err(_) => {
                let _ = thread.join();
                return Err(WorkerError::InitializationStopped.into());
            }
        }
        Ok(Self {
            shutdown: Some(shutdown_sender),
            thread: Some(thread),
        })
    }

    pub(super) fn request_shutdown(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.try_send(());
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.request_shutdown();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub(super) fn report_initialization_error<T, E>(
    ready: &SyncSender<Result<(), E>>,
    error: E,
) -> Result<T, E>
where
    E: From<WorkerError>,
{
    let _ = ready.send(Err(error));
    Err(WorkerError::InitializationStopped.into())
}

#[cfg(test)]
mod tests;
