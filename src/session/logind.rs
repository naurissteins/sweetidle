use std::{
    error::Error,
    fmt, io,
    sync::mpsc::{self, SyncSender},
    thread::{self, JoinHandle},
};

use async_channel::{Receiver, Sender as ShutdownSender, bounded};
use calloop::channel::{Channel, Sender as EventSender, channel};
use futures_lite::{StreamExt, future};
use zbus::{Connection, proxy, zvariant::OwnedFd};

#[proxy(
    interface = "org.freedesktop.login1.Manager",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1",
    gen_blocking = false
)]
trait LogindManager {
    fn inhibit(&self, what: &str, who: &str, why: &str, mode: &str) -> zbus::Result<OwnedFd>;

    #[zbus(signal)]
    fn prepare_for_sleep(&self, active: bool) -> zbus::Result<()>;

    #[zbus(property)]
    fn preparing_for_sleep(&self) -> zbus::Result<bool>;
}

#[derive(Debug)]
pub(crate) struct DelayInhibitor {
    _fd: OwnedFd,
}

impl DelayInhibitor {
    pub(crate) fn new(fd: OwnedFd) -> Self {
        Self { _fd: fd }
    }
}

#[derive(Debug)]
pub(crate) enum SessionEvent {
    PrepareForSleep(Option<DelayInhibitor>),
    Resumed,
    Failed(SessionError),
}

pub(crate) struct LogindMonitor {
    shutdown: Option<ShutdownSender<()>>,
    thread: Option<JoinHandle<()>>,
}

#[derive(Debug)]
pub enum SessionError {
    SpawnWorker(io::Error),
    WorkerInitializationStopped,
    Connect(zbus::Error),
    CreateProxy(zbus::Error),
    Subscribe(zbus::Error),
    QueryState(zbus::Error),
    Inhibit(zbus::Error),
    DecodeSignal(zbus::Error),
    Disconnected,
}

impl LogindMonitor {
    pub(crate) fn connect(
        use_delay_inhibitor: bool,
    ) -> Result<(Self, Channel<SessionEvent>), SessionError> {
        let (event_sender, event_channel) = channel();
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        let (shutdown_sender, shutdown_receiver) = bounded(1);
        let thread = thread::Builder::new()
            .name("sweetsidle-logind".to_string())
            .spawn(move || {
                run_worker(
                    use_delay_inhibitor,
                    event_sender,
                    ready_sender,
                    shutdown_receiver,
                )
            })
            .map_err(SessionError::SpawnWorker)?;
        match ready_receiver.recv() {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                let _ = thread.join();
                return Err(error);
            }
            Err(_) => {
                let _ = thread.join();
                return Err(SessionError::WorkerInitializationStopped);
            }
        };

        Ok((
            Self {
                shutdown: Some(shutdown_sender),
                thread: Some(thread),
            },
            event_channel,
        ))
    }
}

impl Drop for LogindMonitor {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.try_send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl fmt::Display for SessionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SpawnWorker(source) => {
                write!(formatter, "failed to start logind worker: {source}")
            }
            Self::WorkerInitializationStopped => {
                write!(formatter, "logind worker stopped during initialization")
            }
            Self::Connect(source) => {
                write!(formatter, "failed to connect to system D-Bus: {source}")
            }
            Self::CreateProxy(source) => {
                write!(formatter, "failed to create logind proxy: {source}")
            }
            Self::Subscribe(source) => {
                write!(
                    formatter,
                    "failed to subscribe to PrepareForSleep: {source}"
                )
            }
            Self::QueryState(source) => {
                write!(formatter, "failed to query logind sleep state: {source}")
            }
            Self::Inhibit(source) => {
                write!(
                    formatter,
                    "failed to acquire logind sleep delay inhibitor: {source}"
                )
            }
            Self::DecodeSignal(source) => {
                write!(formatter, "invalid PrepareForSleep signal: {source}")
            }
            Self::Disconnected => write!(formatter, "logind signal stream disconnected"),
        }
    }
}

impl Error for SessionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::SpawnWorker(source) => Some(source),
            Self::Connect(source)
            | Self::CreateProxy(source)
            | Self::Subscribe(source)
            | Self::QueryState(source)
            | Self::Inhibit(source)
            | Self::DecodeSignal(source) => Some(source),
            Self::WorkerInitializationStopped | Self::Disconnected => None,
        }
    }
}

fn run_worker(
    use_delay_inhibitor: bool,
    events: EventSender<SessionEvent>,
    ready: SyncSender<Result<(), SessionError>>,
    shutdown: Receiver<()>,
) {
    if let Err(error) =
        async_io::block_on(worker_loop(use_delay_inhibitor, &events, &ready, shutdown))
    {
        let _ = events.send(SessionEvent::Failed(error));
    }
}

async fn worker_loop(
    use_delay_inhibitor: bool,
    events: &EventSender<SessionEvent>,
    ready: &SyncSender<Result<(), SessionError>>,
    shutdown: Receiver<()>,
) -> Result<(), SessionError> {
    let connection = match Connection::system().await {
        Ok(connection) => connection,
        Err(source) => return report_initialization_error(ready, SessionError::Connect(source)),
    };
    let proxy = match LogindManagerProxy::new(&connection).await {
        Ok(proxy) => proxy,
        Err(source) => {
            return report_initialization_error(ready, SessionError::CreateProxy(source));
        }
    };
    let mut signals = match proxy.receive_prepare_for_sleep().await {
        Ok(signals) => signals,
        Err(source) => return report_initialization_error(ready, SessionError::Subscribe(source)),
    };
    let mut inhibitor = if use_delay_inhibitor {
        match acquire_delay_inhibitor(&proxy).await {
            Ok(inhibitor) => Some(inhibitor),
            Err(error) => return report_initialization_error(ready, error),
        }
    } else {
        None
    };
    let already_preparing = match proxy.preparing_for_sleep().await {
        Ok(preparing) => preparing,
        Err(source) => {
            return report_initialization_error(ready, SessionError::QueryState(source));
        }
    };

    if ready.send(Ok(())).is_err() {
        return Ok(());
    }
    if already_preparing
        && events
            .send(SessionEvent::PrepareForSleep(inhibitor.take()))
            .is_err()
    {
        return Ok(());
    }

    loop {
        let next = future::race(async { WorkerInput::Signal(signals.next().await) }, async {
            let _ = shutdown.recv().await;
            WorkerInput::Shutdown
        })
        .await;
        let signal = match next {
            WorkerInput::Signal(Some(signal)) => signal,
            WorkerInput::Signal(None) => return Err(SessionError::Disconnected),
            WorkerInput::Shutdown => return Ok(()),
        };
        let args = signal.args().map_err(SessionError::DecodeSignal)?;

        if *args.active() {
            if events
                .send(SessionEvent::PrepareForSleep(inhibitor.take()))
                .is_err()
            {
                return Ok(());
            }
        } else {
            if use_delay_inhibitor {
                inhibitor = Some(acquire_delay_inhibitor(&proxy).await?);
            }
            if events.send(SessionEvent::Resumed).is_err() {
                return Ok(());
            }
        }
    }
}

async fn acquire_delay_inhibitor(
    proxy: &LogindManagerProxy<'_>,
) -> Result<DelayInhibitor, SessionError> {
    proxy
        .inhibit(
            "sleep",
            "sweetsidle",
            "run the configured before-sleep command",
            "delay",
        )
        .await
        .map(DelayInhibitor::new)
        .map_err(SessionError::Inhibit)
}

fn report_initialization_error<T>(
    ready: &SyncSender<Result<(), SessionError>>,
    error: SessionError,
) -> Result<T, SessionError> {
    let _ = ready.send(Err(error));
    Err(SessionError::WorkerInitializationStopped)
}

enum WorkerInput<T> {
    Signal(Option<T>),
    Shutdown,
}

#[cfg(test)]
mod tests {
    use super::LogindMonitor;

    #[test]
    #[ignore = "requires access to systemd-logind"]
    fn connects_subscribes_and_acquires_the_delay_inhibitor() {
        let (monitor, _events) = match LogindMonitor::connect(true) {
            Ok(parts) => parts,
            Err(error) => panic!("failed to initialize logind monitor: {error}"),
        };

        drop(monitor);
    }
}
