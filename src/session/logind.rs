use std::{
    error::Error,
    fmt,
    future::pending,
    io,
    sync::mpsc::{self, SyncSender},
    thread::{self, JoinHandle},
};

use async_channel::{Receiver, Sender, bounded, unbounded};
use calloop::channel::{Channel, Sender as EventSender, channel};
use futures_lite::{StreamExt, future};
use zbus::{
    Connection, proxy,
    zvariant::{OwnedFd, OwnedObjectPath},
};

use super::{LogindInhibitor, SystemdIdleInhibitor, idle_block_inhibitors};

#[proxy(
    interface = "org.freedesktop.login1.Manager",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1",
    gen_blocking = false
)]
trait LogindManager {
    fn get_session(&self, session_id: &str) -> zbus::Result<OwnedObjectPath>;

    fn get_session_by_pid(&self, pid: u32) -> zbus::Result<OwnedObjectPath>;

    fn inhibit(&self, what: &str, who: &str, why: &str, mode: &str) -> zbus::Result<OwnedFd>;

    fn list_inhibitors(&self) -> zbus::Result<Vec<LogindInhibitor>>;

    #[zbus(signal)]
    fn prepare_for_sleep(&self, active: bool) -> zbus::Result<()>;

    #[zbus(property)]
    fn preparing_for_sleep(&self) -> zbus::Result<bool>;

    #[zbus(property)]
    fn block_inhibited(&self) -> zbus::Result<String>;
}

#[proxy(
    interface = "org.freedesktop.login1.Session",
    default_service = "org.freedesktop.login1",
    gen_blocking = false
)]
trait LogindSession {
    fn set_idle_hint(&self, idle: bool) -> zbus::Result<()>;

    #[zbus(signal)]
    fn lock(&self) -> zbus::Result<()>;

    #[zbus(signal)]
    fn unlock(&self) -> zbus::Result<()>;
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
    shutdown: Option<Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

#[derive(Debug)]
pub enum SessionError {
    SpawnWorker(io::Error),
    WorkerInitializationStopped,
    Connect(zbus::Error),
    CreateProxy(zbus::Error),
    ResolveSession {
        by_session_id: Option<Box<zbus::Error>>,
        by_process_id: Box<zbus::Error>,
    },
    CreateSessionProxy(zbus::Error),
    Subscribe(zbus::Error),
    SubscribeSession(zbus::Error),
    QueryState(zbus::Error),
    QueryInhibitors(zbus::Error),
    Inhibit(zbus::Error),
    SetIdleHint(zbus::Error),
    RequestChannelClosed,
    DecodeSignal(zbus::Error),
    Disconnected,
}

impl LogindMonitor {
    pub(crate) fn connect(
        features: LogindFeatures,
    ) -> Result<(Self, Channel<SessionEvent>), SessionError> {
        let (event_sender, event_channel) = channel();
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        let (shutdown_sender, shutdown_receiver) = bounded(1);
        let (request_sender, request_receiver) = unbounded();
        let thread = thread::Builder::new()
            .name("sweetsidle-logind".to_string())
            .spawn(move || {
                run_worker(
                    features,
                    event_sender,
                    ready_sender,
                    request_receiver,
                    shutdown_receiver,
                );
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
                requests: Some(request_sender),
                shutdown: Some(shutdown_sender),
                thread: Some(thread),
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
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.try_send(());
        }
        self.requests.take();
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
            Self::ResolveSession {
                by_session_id: Some(by_session_id),
                by_process_id,
            } => write!(
                formatter,
                "failed to resolve logind session from XDG_SESSION_ID ({by_session_id}) or process id ({by_process_id})"
            ),
            Self::ResolveSession {
                by_session_id: None,
                by_process_id,
            } => write!(
                formatter,
                "failed to resolve logind session from process id: {by_process_id}"
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
            | Self::DecodeSignal(source) => Some(source),
            Self::ResolveSession { by_process_id, .. } => Some(by_process_id.as_ref()),
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
    // dbus work stays off the Wayland event loop
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
    let mut sleep_signals = if features.sleep_events {
        match proxy.receive_prepare_for_sleep().await {
            Ok(signals) => Some(signals),
            Err(source) => {
                return report_initialization_error(ready, SessionError::Subscribe(source));
            }
        }
    } else {
        None
    };
    let session_proxy = if features.session_events || features.idle_hint {
        let path = match resolve_session_path(&proxy).await {
            Ok(path) => path,
            Err(error) => return report_initialization_error(ready, error),
        };
        let builder = match LogindSessionProxy::builder(&connection).path(path) {
            Ok(builder) => builder,
            Err(source) => {
                return report_initialization_error(
                    ready,
                    SessionError::CreateSessionProxy(source),
                );
            }
        };
        match builder.build().await {
            Ok(proxy) => Some(proxy),
            Err(source) => {
                return report_initialization_error(
                    ready,
                    SessionError::CreateSessionProxy(source),
                );
            }
        }
    } else {
        None
    };
    let mut lock_signals = match (features.session_events, session_proxy.as_ref()) {
        (true, Some(proxy)) => match proxy.receive_lock().await {
            Ok(signals) => Some(signals),
            Err(source) => {
                return report_initialization_error(ready, SessionError::SubscribeSession(source));
            }
        },
        (false, _) | (true, None) => None,
    };
    let mut unlock_signals = match (features.session_events, session_proxy.as_ref()) {
        (true, Some(proxy)) => match proxy.receive_unlock().await {
            Ok(signals) => Some(signals),
            Err(source) => {
                return report_initialization_error(ready, SessionError::SubscribeSession(source));
            }
        },
        (false, _) | (true, None) => None,
    };
    let mut idle_inhibitor_changes = if features.idle_inhibitors {
        Some(proxy.receive_block_inhibited_changed().await)
    } else {
        None
    };
    let mut idle_inhibitors = if features.idle_inhibitors {
        match query_idle_inhibitors(&proxy).await {
            Ok(inhibitors) => inhibitors,
            Err(error) => return report_initialization_error(ready, error),
        }
    } else {
        Vec::new()
    };
    let mut inhibitor = if features.delay_inhibitor {
        match acquire_delay_inhibitor(&proxy).await {
            Ok(inhibitor) => Some(inhibitor),
            Err(error) => return report_initialization_error(ready, error),
        }
    } else {
        None
    };
    let already_preparing = if features.sleep_events {
        match proxy.preparing_for_sleep().await {
            Ok(preparing) => preparing,
            Err(source) => {
                return report_initialization_error(ready, SessionError::QueryState(source));
            }
        }
    } else {
        false
    };
    if features.idle_hint
        && let Err(error) = set_idle_hint(session_proxy.as_ref(), false).await
    {
        return report_initialization_error(ready, error);
    }

    if !idle_inhibitors.is_empty()
        && events
            .send(SessionEvent::SystemdIdleInhibitorsChanged(
                idle_inhibitors.clone(),
            ))
            .is_err()
    {
        return Ok(());
    }
    // report readiness only after subscriptions and the initial state are set
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
        let next = future::race(
            future::race(
                async {
                    match sleep_signals.as_mut() {
                        Some(signals) => match signals.next().await {
                            Some(signal) => WorkerInput::Sleep(
                                signal
                                    .args()
                                    .map(|args| Some(*args.active()))
                                    .map_err(SessionError::DecodeSignal),
                            ),
                            None => WorkerInput::Sleep(Ok(None)),
                        },
                        None => pending().await,
                    }
                },
                async {
                    match lock_signals.as_mut() {
                        Some(signals) => WorkerInput::Lock(signals.next().await.is_some()),
                        None => pending().await,
                    }
                },
            ),
            future::race(
                future::race(
                    async {
                        match unlock_signals.as_mut() {
                            Some(signals) => WorkerInput::Unlock(signals.next().await.is_some()),
                            None => pending().await,
                        }
                    },
                    async {
                        match idle_inhibitor_changes.as_mut() {
                            Some(changes) => {
                                WorkerInput::IdleInhibitorsChanged(changes.next().await.is_some())
                            }
                            None => pending().await,
                        }
                    },
                ),
                future::race(
                    async {
                        match requests.recv().await {
                            Ok(request) => WorkerInput::Request(request),
                            Err(_) => WorkerInput::Shutdown,
                        }
                    },
                    async {
                        let _ = shutdown.recv().await;
                        WorkerInput::Shutdown
                    },
                ),
            ),
        )
        .await;

        match next {
            WorkerInput::Sleep(Ok(Some(true))) => {
                if events
                    .send(SessionEvent::PrepareForSleep(inhibitor.take()))
                    .is_err()
                {
                    return Ok(());
                }
            }
            WorkerInput::Sleep(Ok(Some(false))) => {
                if features.delay_inhibitor {
                    inhibitor = Some(acquire_delay_inhibitor(&proxy).await?);
                }
                if events.send(SessionEvent::Resumed).is_err() {
                    return Ok(());
                }
            }
            WorkerInput::Sleep(Ok(None))
            | WorkerInput::Lock(false)
            | WorkerInput::Unlock(false)
            | WorkerInput::IdleInhibitorsChanged(false) => {
                return Err(SessionError::Disconnected);
            }
            WorkerInput::Sleep(Err(error)) => return Err(error),
            WorkerInput::Lock(true) => {
                if events.send(SessionEvent::LockRequested).is_err() {
                    return Ok(());
                }
            }
            WorkerInput::Unlock(true) => {
                if events.send(SessionEvent::UnlockRequested).is_err() {
                    return Ok(());
                }
            }
            WorkerInput::IdleInhibitorsChanged(true) => {
                let next = query_idle_inhibitors(&proxy).await?;
                if next != idle_inhibitors {
                    idle_inhibitors = next;
                    if events
                        .send(SessionEvent::SystemdIdleInhibitorsChanged(
                            idle_inhibitors.clone(),
                        ))
                        .is_err()
                    {
                        return Ok(());
                    }
                }
            }
            WorkerInput::Request(SessionRequest::SetIdleHint(idle)) => {
                set_idle_hint(session_proxy.as_ref(), idle).await?;
            }
            WorkerInput::Shutdown => {
                if features.idle_hint {
                    set_idle_hint(session_proxy.as_ref(), false).await?;
                }
                return Ok(());
            }
        }
    }
}

async fn resolve_session_path(
    manager: &LogindManagerProxy<'_>,
) -> Result<OwnedObjectPath, SessionError> {
    let mut by_session_id = None;
    if let Some(session_id) = std::env::var_os("XDG_SESSION_ID")
        .and_then(|value| value.into_string().ok())
        .filter(|value| !value.is_empty())
    {
        match manager.get_session(&session_id).await {
            Ok(path) => return Ok(path),
            Err(error) => by_session_id = Some(Box::new(error)),
        }
    }

    manager
        .get_session_by_pid(std::process::id())
        .await
        .map_err(|by_process_id| SessionError::ResolveSession {
            by_session_id,
            by_process_id: Box::new(by_process_id),
        })
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

async fn set_idle_hint(
    proxy: Option<&LogindSessionProxy<'_>>,
    idle: bool,
) -> Result<(), SessionError> {
    let Some(proxy) = proxy else {
        return Err(SessionError::RequestChannelClosed);
    };

    proxy
        .set_idle_hint(idle)
        .await
        .map_err(SessionError::SetIdleHint)
}

async fn query_idle_inhibitors(
    proxy: &LogindManagerProxy<'_>,
) -> Result<Vec<SystemdIdleInhibitor>, SessionError> {
    proxy
        .list_inhibitors()
        .await
        .map(idle_block_inhibitors)
        .map_err(SessionError::QueryInhibitors)
}

fn report_initialization_error<T>(
    ready: &SyncSender<Result<(), SessionError>>,
    error: SessionError,
) -> Result<T, SessionError> {
    let _ = ready.send(Err(error));
    Err(SessionError::WorkerInitializationStopped)
}

enum WorkerInput {
    Sleep(Result<Option<bool>, SessionError>),
    Lock(bool),
    Unlock(bool),
    IdleInhibitorsChanged(bool),
    Request(SessionRequest),
    Shutdown,
}

#[derive(Debug, Clone, Copy)]
enum SessionRequest {
    SetIdleHint(bool),
}

#[cfg(test)]
mod tests {
    use std::{sync::mpsc::TryRecvError, thread, time::Duration};

    use super::{LogindFeatures, LogindManagerProxy, LogindMonitor, SessionEvent};

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
                    "sweetsidle integration test",
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
        for _ in 0..100 {
            match events.try_recv() {
                Ok(SessionEvent::SystemdIdleInhibitorsChanged(inhibitors))
                    if inhibitors
                        .iter()
                        .any(|inhibitor| inhibitor.who == "sweetsidle integration test")
                        == expected_active =>
                {
                    return;
                }
                Ok(SessionEvent::Failed(error)) => panic!("logind monitor failed: {error}"),
                Ok(_) | Err(TryRecvError::Empty) => thread::sleep(Duration::from_millis(10)),
                Err(TryRecvError::Disconnected) => panic!("logind event channel closed"),
            }
        }

        panic!("timed out waiting for systemd idle inhibitor state {expected_active}");
    }
}
