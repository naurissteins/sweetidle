use std::{error::Error, fmt, io, sync::mpsc::SyncSender};

use async_channel::{Receiver, Sender, unbounded};
use calloop::channel::{Channel, Sender as EventSender, channel};
use zbus::zvariant::OwnedFd;

use super::{
    SystemdIdleInhibitor,
    worker::{Worker, WorkerError, report_initialization_error},
};

mod proxy;
mod signals;
mod worker;

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
    LockRequested,
    UnlockRequested,
    SystemdIdleInhibitorsChanged(Vec<SystemdIdleInhibitor>),
    Failed(SessionError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LogindFeatures {
    pub(crate) sleep_events: bool,
    pub(crate) delay_inhibitor: bool,
    pub(crate) session_events: bool,
    pub(crate) idle_inhibitors: bool,
    pub(crate) idle_hint: bool,
}

pub(crate) struct LogindMonitor {
    requests: Option<Sender<SessionRequest>>,
    worker: Worker,
}

#[derive(Debug)]
pub enum SessionError {
    SpawnWorker(io::Error),
    WorkerInitializationStopped,
    Connect(Box<zbus::Error>),
    CreateProxy(Box<zbus::Error>),
    ResolveSession {
        by_session_id: Option<Box<zbus::Error>>,
        by_auto: Box<zbus::Error>,
    },
    CreateSessionProxy(Box<zbus::Error>),
    Subscribe(Box<zbus::Error>),
    SubscribeSession(Box<zbus::Error>),
    QueryState(Box<zbus::Error>),
    QueryInhibitors(Box<zbus::Error>),
    Inhibit(Box<zbus::Error>),
    SetIdleHint(Box<zbus::Error>),
    RequestChannelClosed,
    DecodeSignal(Box<zbus::Error>),
    Disconnected,
}

impl LogindMonitor {
    pub(crate) fn connect(
        features: LogindFeatures,
    ) -> Result<(Self, Channel<SessionEvent>), SessionError> {
        let (event_sender, event_channel) = channel();
        let (request_sender, request_receiver) = unbounded();
        let worker = Worker::spawn("sweetidle-logind", move |ready, shutdown| {
            run_worker(features, event_sender, ready, request_receiver, shutdown);
        })?;

        Ok((
            Self {
                requests: Some(request_sender),
                worker,
            },
            event_channel,
        ))
    }

    pub(crate) fn set_idle_hint(&self, idle: bool) -> Result<(), SessionError> {
        self.requests
            .as_ref()
            .ok_or(SessionError::RequestChannelClosed)?
            .try_send(SessionRequest::SetIdleHint(idle))
            .map_err(|_| SessionError::RequestChannelClosed)
    }
}

impl Drop for LogindMonitor {
    fn drop(&mut self) {
        // close requests between the shutdown notification and the worker's join
        self.worker.request_shutdown();
        self.requests.take();
    }
}

impl From<WorkerError> for SessionError {
    fn from(error: WorkerError) -> Self {
        match error {
            WorkerError::Spawn(source) => Self::SpawnWorker(source),
            WorkerError::InitializationStopped => Self::WorkerInitializationStopped,
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
            Self::ResolveSession {
                by_session_id: Some(by_session_id),
                by_auto,
            } => write!(
                formatter,
                "failed to resolve logind session from XDG_SESSION_ID ({by_session_id}) or the user's display session ({by_auto})"
            ),
            Self::ResolveSession {
                by_session_id: None,
                by_auto,
            } => write!(
                formatter,
                "failed to resolve the user's logind display session: {by_auto}"
            ),
            Self::CreateSessionProxy(source) => {
                write!(formatter, "failed to create logind session proxy: {source}")
            }
            Self::Subscribe(source) => {
                write!(
                    formatter,
                    "failed to subscribe to PrepareForSleep: {source}"
                )
            }
            Self::SubscribeSession(source) => {
                write!(formatter, "failed to subscribe to logind session: {source}")
            }
            Self::QueryState(source) => {
                write!(formatter, "failed to query logind sleep state: {source}")
            }
            Self::QueryInhibitors(source) => {
                write!(
                    formatter,
                    "failed to query logind idle inhibitors: {source}"
                )
            }
            Self::Inhibit(source) => {
                write!(
                    formatter,
                    "failed to acquire logind sleep delay inhibitor: {source}"
                )
            }
            Self::SetIdleHint(source) => {
                write!(
                    formatter,
                    "failed to update logind session idle hint: {source}"
                )
            }
            Self::RequestChannelClosed => write!(formatter, "logind request channel closed"),
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
            | Self::CreateSessionProxy(source)
            | Self::Subscribe(source)
            | Self::SubscribeSession(source)
            | Self::QueryState(source)
            | Self::QueryInhibitors(source)
            | Self::Inhibit(source)
            | Self::SetIdleHint(source)
            | Self::DecodeSignal(source) => Some(source.as_ref()),
            Self::ResolveSession { by_auto, .. } => Some(by_auto.as_ref()),
            Self::WorkerInitializationStopped | Self::RequestChannelClosed | Self::Disconnected => {
                None
            }
        }
    }
}

fn run_worker(
    features: LogindFeatures,
    events: EventSender<SessionEvent>,
    ready: SyncSender<Result<(), SessionError>>,
    requests: Receiver<SessionRequest>,
    shutdown: Receiver<()>,
) {
    if let Err(error) =
        async_io::block_on(worker_loop(features, &events, &ready, requests, shutdown))
    {
        let _ = events.send(SessionEvent::Failed(error));
    }
}

async fn worker_loop(
    features: LogindFeatures,
    events: &EventSender<SessionEvent>,
    ready: &SyncSender<Result<(), SessionError>>,
    requests: Receiver<SessionRequest>,
    shutdown: Receiver<()>,
) -> Result<(), SessionError> {
    let mut worker = match worker::LogindWorker::open(features).await {
        Ok(worker) => worker,
        Err(error) => return report_initialization_error(ready, error),
    };
    if worker.start(events, ready).is_break() {
        return Ok(());
    }
    loop {
        let input = worker.next_input(&requests, &shutdown).await;
        if worker.handle(input, events).await?.is_break() {
            return Ok(());
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum SessionRequest {
    SetIdleHint(bool),
}

#[cfg(test)]
mod tests {
    use std::{
        sync::mpsc::TryRecvError,
        thread,
        time::{Duration, Instant},
    };

    use super::{
        LogindFeatures, LogindMonitor, SessionError, SessionEvent, Worker,
        proxy::LogindManagerProxy,
    };

    #[test]
    fn monitor_drop_closes_requests_before_joining_the_worker() {
        let (requests, receiver) = async_channel::unbounded::<super::SessionRequest>();
        let (finished, completed) = std::sync::mpsc::channel();
        let worker =
            Worker::spawn::<SessionError>("sweetidle-test-logind", move |ready, shutdown| {
                ready.send(Ok(())).unwrap_or_else(|error| panic!("{error}"));
                async_io::block_on(futures_lite::future::race(
                    async {
                        shutdown
                            .recv()
                            .await
                            .unwrap_or_else(|error| panic!("{error}"));
                        assert!(receiver.recv().await.is_err());
                        finished.send(()).unwrap_or_else(|error| panic!("{error}"));
                    },
                    async {
                        async_io::Timer::after(Duration::from_secs(5)).await;
                        panic!("monitor did not close its channels before joining");
                    },
                ));
            })
            .unwrap_or_else(|error| panic!("worker startup failed: {error}"));

        drop(LogindMonitor {
            requests: Some(requests),
            worker,
        });

        assert!(completed.try_recv().is_ok());
    }

    #[test]
    #[ignore = "requires access to systemd-logind"]
    fn connects_and_subscribes_to_configured_logind_events() {
        let (monitor, _events) = match LogindMonitor::connect(LogindFeatures {
            sleep_events: true,
            delay_inhibitor: true,
            session_events: true,
            idle_inhibitors: true,
            idle_hint: true,
        }) {
            Ok(parts) => parts,
            Err(error) => panic!("failed to initialize logind monitor: {error}"),
        };

        drop(monitor);
    }

    #[test]
    #[ignore = "requires systemd-logind and requests session lock/unlock"]
    fn observes_session_lock_unlock_in_order() {
        let (monitor, events) = LogindMonitor::connect(LogindFeatures {
            sleep_events: false,
            delay_inhibitor: false,
            session_events: true,
            idle_inhibitors: false,
            idle_hint: false,
        })
        .unwrap_or_else(|error| panic!("failed to initialize logind monitor: {error}"));
        async_io::block_on(async {
            let connection = zbus::Connection::system()
                .await
                .unwrap_or_else(|error| panic!("failed to connect test client: {error}"));
            let manager = LogindManagerProxy::new(&connection)
                .await
                .unwrap_or_else(|error| panic!("failed to create manager proxy: {error}"));
            let path = super::proxy::resolve_session_path(&manager)
                .await
                .unwrap_or_else(|error| panic!("failed to resolve test session: {error}"));
            let session = super::proxy::LogindSessionProxy::builder(&connection)
                .path(path)
                .unwrap_or_else(|error| panic!("invalid session path: {error}"))
                .build()
                .await
                .unwrap_or_else(|error| panic!("failed to create session proxy: {error}"));
            for _ in 0..20 {
                let _: () = session
                    .inner()
                    .call("Lock", &())
                    .await
                    .unwrap_or_else(|error| panic!("session lock request failed: {error}"));
                let _: () = session
                    .inner()
                    .call("Unlock", &())
                    .await
                    .unwrap_or_else(|error| panic!("session unlock request failed: {error}"));
            }
        });

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut received = 0;
        while received < 40 {
            match events.try_recv() {
                Ok(SessionEvent::LockRequested) => {
                    assert_eq!(
                        received % 2,
                        0,
                        "lock signal {received} arrived out of order"
                    );
                    received += 1;
                }
                Ok(SessionEvent::UnlockRequested) => {
                    assert_eq!(
                        received % 2,
                        1,
                        "unlock signal {received} arrived out of order"
                    );
                    received += 1;
                }
                Ok(event) => panic!("unexpected session event: {event:?}"),
                Err(TryRecvError::Disconnected) => panic!("logind worker disconnected"),
                Err(TryRecvError::Empty) => {
                    assert!(
                        Instant::now() < deadline,
                        "logind did not deliver all 40 signals"
                    );
                    thread::sleep(Duration::from_millis(1));
                }
            }
        }
        drop(monitor);
    }

    #[test]
    #[ignore = "requires access to systemd-logind"]
    fn observes_idle_block_inhibitor_lifecycle() {
        let (monitor, events) = match LogindMonitor::connect(LogindFeatures {
            sleep_events: false,
            delay_inhibitor: false,
            session_events: false,
            idle_inhibitors: true,
            idle_hint: false,
        }) {
            Ok(parts) => parts,
            Err(error) => panic!("failed to initialize logind monitor: {error}"),
        };

        let inhibitor = async_io::block_on(async {
            let connection = match zbus::Connection::system().await {
                Ok(connection) => connection,
                Err(error) => panic!("failed to connect test client to system D-Bus: {error}"),
            };
            let proxy = match LogindManagerProxy::new(&connection).await {
                Ok(proxy) => proxy,
                Err(error) => panic!("failed to create test logind proxy: {error}"),
            };
            match proxy
                .inhibit(
                    "idle",
                    "sweetidle integration test",
                    "verify idle inhibitor monitoring",
                    "block",
                )
                .await
            {
                Ok(inhibitor) => inhibitor,
                Err(error) => panic!("failed to acquire test idle inhibitor: {error}"),
            }
        });

        wait_for_inhibitor_state(&events, true);
        drop(inhibitor);
        wait_for_inhibitor_state(&events, false);
        drop(monitor);
    }

    fn wait_for_inhibitor_state(
        events: &calloop::channel::Channel<SessionEvent>,
        expected_active: bool,
    ) {
        let deadline = crate::test_support::TestDeadline::new();
        let waiting_for = format!("systemd idle inhibitor state {expected_active}");
        loop {
            match events.try_recv() {
                Ok(SessionEvent::SystemdIdleInhibitorsChanged(inhibitors))
                    if inhibitors
                        .iter()
                        .any(|inhibitor| inhibitor.who == "sweetidle integration test")
                        == expected_active =>
                {
                    return;
                }
                Ok(SessionEvent::Failed(error)) => panic!("logind monitor failed: {error}"),
                Ok(_) | Err(TryRecvError::Empty) => deadline.wait(&waiting_for),
                Err(TryRecvError::Disconnected) => panic!("logind event channel closed"),
            }
        }
    }
}
