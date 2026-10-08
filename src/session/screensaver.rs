use std::{
    error::Error,
    fmt, io,
    sync::{Arc, Mutex, mpsc},
};

use async_channel::Receiver;
use calloop::channel::{Channel, Sender as EventSender, channel};
use futures_lite::{StreamExt, future};
use zbus::{
    Connection,
    fdo::{self, DBusProxy, RequestNameFlags},
    message::Header,
};

use super::{
    inhibit::{InhibitorInfo, InhibitorRegistry, InhibitorRegistryError},
    worker::{Worker, WorkerError, report_initialization_error},
};

const SERVICE_NAME: &str = "org.freedesktop.ScreenSaver";
const OBJECT_PATH: &str = "/org/freedesktop/ScreenSaver";
const LEGACY_OBJECT_PATH: &str = "/ScreenSaver";

#[derive(Debug)]
pub(crate) enum DbusInhibitEvent {
    Added {
        inhibitor: InhibitorInfo,
        active_count: usize,
    },
    Removed {
        inhibitor: InhibitorInfo,
        active_count: usize,
    },
    OwnerDisconnected {
        owner: String,
        released_count: usize,
        active_count: usize,
    },
    Failed(DbusInhibitError),
}

pub(crate) struct DbusInhibitMonitor {
    _worker: Worker,
}

#[derive(Debug)]
pub enum DbusInhibitError {
    SpawnWorker(io::Error),
    WorkerInitializationStopped,
    Connect(Box<zbus::Error>),
    RegisterObject(Box<zbus::Error>),
    CreateBusProxy(Box<zbus::Error>),
    Subscribe(Box<zbus::Error>),
    OwnName(Box<zbus::Error>),
    DecodeSignal(Box<zbus::Error>),
    StatePoisoned,
    Disconnected,
}

impl DbusInhibitMonitor {
    pub(crate) fn connect() -> Result<(Self, Channel<DbusInhibitEvent>), DbusInhibitError> {
        let (event_sender, event_channel) = channel();
        let worker = Worker::spawn("sweetsidle-dbus-inhibit", move |ready, shutdown| {
            run_worker(event_sender, ready, shutdown)
        })?;

        Ok((Self { _worker: worker }, event_channel))
    }
}

impl From<WorkerError> for DbusInhibitError {
    fn from(error: WorkerError) -> Self {
        match error {
            WorkerError::Spawn(source) => Self::SpawnWorker(source),
            WorkerError::InitializationStopped => Self::WorkerInitializationStopped,
        }
    }
}

impl fmt::Display for DbusInhibitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SpawnWorker(source) => {
                write!(
                    formatter,
                    "failed to start D-Bus inhibitor worker: {source}"
                )
            }
            Self::WorkerInitializationStopped => {
                write!(
                    formatter,
                    "D-Bus inhibitor worker stopped during initialization"
                )
            }
            Self::Connect(source) => {
                write!(formatter, "failed to connect to session D-Bus: {source}")
            }
            Self::RegisterObject(source) => {
                write!(formatter, "failed to register ScreenSaver object: {source}")
            }
            Self::CreateBusProxy(source) => {
                write!(formatter, "failed to create session bus proxy: {source}")
            }
            Self::Subscribe(source) => {
                write!(
                    formatter,
                    "failed to monitor D-Bus client lifetimes: {source}"
                )
            }
            Self::OwnName(source) => {
                write!(formatter, "failed to own `{SERVICE_NAME}`: {source}")
            }
            Self::DecodeSignal(source) => {
                write!(formatter, "invalid D-Bus name-owner signal: {source}")
            }
            Self::StatePoisoned => write!(formatter, "D-Bus inhibitor state is unavailable"),
            Self::Disconnected => write!(formatter, "session D-Bus disconnected"),
        }
    }
}

impl Error for DbusInhibitError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::SpawnWorker(source) => Some(source),
            Self::Connect(source)
            | Self::RegisterObject(source)
            | Self::CreateBusProxy(source)
            | Self::Subscribe(source)
            | Self::OwnName(source)
            | Self::DecodeSignal(source) => Some(source.as_ref()),
            Self::WorkerInitializationStopped | Self::StatePoisoned | Self::Disconnected => None,
        }
    }
}

struct ScreenSaverService {
    shared: Arc<SharedState>,
}

struct SharedState {
    registry: Mutex<InhibitorRegistry>,
    events: EventSender<DbusInhibitEvent>,
}

#[zbus::interface(name = "org.freedesktop.ScreenSaver")]
impl ScreenSaverService {
    #[zbus(name = "Inhibit")]
    async fn inhibit(
        &self,
        application_name: &str,
        reason_for_inhibit: &str,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &Connection,
    ) -> fdo::Result<u32> {
        let owner = caller(&header)?;
        let owner_name = zbus::names::BusName::try_from(owner.as_str())
            .map_err(|_| fdo::Error::Failed("invalid D-Bus caller name".to_string()))?;
        let cookie = {
            let mut registry = self.shared.registry.lock().map_err(state_unavailable)?;
            let inhibitor = registry
                .add(&owner, application_name, reason_for_inhibit)
                .map_err(registry_error)?;
            let active_count = registry.len();
            let cookie = inhibitor.cookie;
            if self
                .shared
                .events
                .send(DbusInhibitEvent::Added {
                    inhibitor,
                    active_count,
                })
                .is_err()
            {
                let _ = registry.remove(cookie, &owner);
                return Err(fdo::Error::Failed(
                    "sweetsidle is shutting down".to_string(),
                ));
            }
            cookie
        };

        // the disconnect signal can arrive before this method inserts the cookie
        let owner_check = match DBusProxy::new(connection).await {
            Ok(bus) => bus.name_has_owner(owner_name).await,
            Err(error) => Err(fdo::Error::ZBus(error)),
        };
        let failure = match owner_check {
            Ok(true) => return Ok(cookie),
            Ok(false) => "D-Bus caller disconnected during Inhibit".to_string(),
            Err(error) => format!("failed to verify D-Bus caller during Inhibit: {error}"),
        };
        remove_cookie(&self.shared, cookie, &owner)?;
        Err(fdo::Error::Failed(failure))
    }

    #[zbus(name = "UnInhibit")]
    fn un_inhibit(&self, cookie: u32, #[zbus(header)] header: Header<'_>) -> fdo::Result<()> {
        let owner = caller(&header)?;
        {
            let mut registry = self.shared.registry.lock().map_err(state_unavailable)?;
            let inhibitor = registry.remove(cookie, &owner).map_err(registry_error)?;
            let active_count = registry.len();
            let _ = self.shared.events.send(DbusInhibitEvent::Removed {
                inhibitor,
                active_count,
            });
        }

        Ok(())
    }
}

fn remove_cookie(shared: &SharedState, cookie: u32, owner: &str) -> fdo::Result<()> {
    let mut registry = shared.registry.lock().map_err(state_unavailable)?;
    let inhibitor = match registry.remove(cookie, owner) {
        Ok(inhibitor) => inhibitor,
        Err(InhibitorRegistryError::UnknownCookie) => return Ok(()),
        Err(error) => return Err(registry_error(error)),
    };
    let _ = shared.events.send(DbusInhibitEvent::Removed {
        inhibitor,
        active_count: registry.len(),
    });

    Ok(())
}

fn run_worker(
    events: EventSender<DbusInhibitEvent>,
    ready: mpsc::SyncSender<Result<(), DbusInhibitError>>,
    shutdown: Receiver<()>,
) {
    if let Err(error) = async_io::block_on(worker_loop(events.clone(), &ready, shutdown)) {
        let _ = events.send(DbusInhibitEvent::Failed(error));
    }
}

async fn worker_loop(
    events: EventSender<DbusInhibitEvent>,
    ready: &mpsc::SyncSender<Result<(), DbusInhibitError>>,
    shutdown: Receiver<()>,
) -> Result<(), DbusInhibitError> {
    let connection = match Connection::session().await {
        Ok(connection) => connection,
        Err(source) => {
            return report_initialization_error(ready, DbusInhibitError::Connect(Box::new(source)));
        }
    };
    let shared = Arc::new(SharedState {
        registry: Mutex::new(InhibitorRegistry::default()),
        events,
    });
    for path in [OBJECT_PATH, LEGACY_OBJECT_PATH] {
        if let Err(source) = connection
            .object_server()
            .at(
                path,
                ScreenSaverService {
                    shared: shared.clone(),
                },
            )
            .await
        {
            return report_initialization_error(
                ready,
                DbusInhibitError::RegisterObject(Box::new(source)),
            );
        }
    }
    let bus = match DBusProxy::new(&connection).await {
        Ok(proxy) => proxy,
        Err(source) => {
            return report_initialization_error(
                ready,
                DbusInhibitError::CreateBusProxy(Box::new(source)),
            );
        }
    };
    // subscribe before owning the name to observe early client exits
    let mut owner_changes = match bus.receive_name_owner_changed().await {
        Ok(stream) => stream,
        Err(source) => {
            return report_initialization_error(
                ready,
                DbusInhibitError::Subscribe(Box::new(source)),
            );
        }
    };
    if let Err(source) = connection
        .request_name_with_flags(SERVICE_NAME, RequestNameFlags::DoNotQueue.into())
        .await
    {
        return report_initialization_error(ready, DbusInhibitError::OwnName(Box::new(source)));
    }

    if ready.send(Ok(())).is_err() {
        return Ok(());
    }

    loop {
        let next = future::race(
            async { WorkerInput::OwnerChanged(owner_changes.next().await) },
            async {
                let _ = shutdown.recv().await;
                WorkerInput::Shutdown
            },
        )
        .await;

        match next {
            WorkerInput::OwnerChanged(Some(signal)) => {
                let args = signal
                    .args()
                    .map_err(|source| DbusInhibitError::DecodeSignal(Box::new(source)))?;
                if args.new_owner().as_ref().is_none() {
                    release_owner(&shared, args.name().as_str())?;
                }
            }
            WorkerInput::OwnerChanged(None) => return Err(DbusInhibitError::Disconnected),
            WorkerInput::Shutdown => return Ok(()),
        }
    }
}

fn release_owner(shared: &SharedState, owner: &str) -> Result<(), DbusInhibitError> {
    {
        let mut registry = shared
            .registry
            .lock()
            .map_err(|_| DbusInhibitError::StatePoisoned)?;
        let released_count = registry.remove_owner(owner).len();
        if released_count > 0 {
            let _ = shared.events.send(DbusInhibitEvent::OwnerDisconnected {
                owner: owner.to_string(),
                released_count,
                active_count: registry.len(),
            });
        }
    }

    Ok(())
}

fn caller(header: &Header<'_>) -> fdo::Result<String> {
    header
        .sender()
        .map(|sender| sender.as_str().to_string())
        .ok_or_else(|| fdo::Error::Failed("D-Bus caller has no unique name".to_string()))
}

fn registry_error(error: InhibitorRegistryError) -> fdo::Error {
    match error {
        InhibitorRegistryError::Full => {
            fdo::Error::LimitsExceeded("too many active idle inhibitors".to_string())
        }
        InhibitorRegistryError::UnknownCookie => {
            fdo::Error::InvalidArgs("unknown inhibitor cookie".to_string())
        }
        InhibitorRegistryError::WrongOwner => {
            fdo::Error::AccessDenied("inhibitor cookie belongs to another client".to_string())
        }
    }
}

fn state_unavailable<T>(_: std::sync::PoisonError<T>) -> fdo::Error {
    fdo::Error::Failed("idle inhibitor state is unavailable".to_string())
}

enum WorkerInput<T> {
    OwnerChanged(Option<T>),
    Shutdown,
}

#[cfg(test)]
mod tests {
    use std::{
        sync::mpsc::TryRecvError,
        thread,
        time::{Duration, Instant},
    };

    use super::{DbusInhibitEvent, DbusInhibitMonitor};

    #[zbus::proxy(
        interface = "org.freedesktop.ScreenSaver",
        default_service = "org.freedesktop.ScreenSaver",
        default_path = "/org/freedesktop/ScreenSaver",
        gen_blocking = false
    )]
    trait ScreenSaverClient {
        fn inhibit(&self, application_name: &str, reason_for_inhibit: &str) -> zbus::Result<u32>;

        #[zbus(name = "UnInhibit")]
        fn un_inhibit(&self, cookie: u32) -> zbus::Result<()>;
    }

    #[test]
    #[ignore = "requires a private D-Bus session"]
    fn readiness_and_shutdown_preserve_exclusive_name_ownership() {
        let (monitor, events) = connect_monitor();
        assert!(matches!(
            DbusInhibitMonitor::connect(),
            Err(super::DbusInhibitError::OwnName(_))
        ));

        drop(monitor);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match events.try_recv() {
                Err(TryRecvError::Disconnected) => break,
                Err(TryRecvError::Empty) => {
                    assert!(
                        Instant::now() < deadline,
                        "worker retained the event sender"
                    );
                    thread::sleep(Duration::from_millis(1));
                }
                event => panic!("unexpected shutdown event: {event:?}"),
            }
        }
        async_io::block_on(async {
            let observer = connect_client().await;
            let bus = zbus::fdo::DBusProxy::new(&observer)
                .await
                .unwrap_or_else(|error| panic!("failed to create bus proxy: {error}"));
            let deadline = Instant::now() + Duration::from_secs(5);
            while bus
                .name_has_owner(
                    super::SERVICE_NAME
                        .try_into()
                        .unwrap_or_else(|error| panic!("invalid service name: {error}")),
                )
                .await
                .unwrap_or_else(|error| panic!("name ownership query failed: {error}"))
            {
                assert!(
                    Instant::now() < deadline,
                    "worker retained the bus name after shutdown"
                );
                async_io::Timer::after(Duration::from_millis(1)).await;
            }
        });
        let (restarted, _events) = connect_monitor();
        drop(restarted);
    }

    #[test]
    #[ignore = "requires a private D-Bus session"]
    fn serves_inhibit_and_uninhibit_requests() {
        let (monitor, events) = connect_monitor();

        async_io::block_on(async {
            let connection = connect_client().await;
            let proxy = build_proxy(&connection).await;
            let cookie = match proxy.inhibit("test.video", "playing media").await {
                Ok(cookie) => cookie,
                Err(error) => panic!("failed to inhibit idle: {error}"),
            };
            assert!(matches!(
                events.recv(),
                Ok(DbusInhibitEvent::Added {
                    inhibitor,
                    active_count: 1,
                }) if inhibitor.cookie == cookie
                    && inhibitor.application == "test.video"
                    && inhibitor.reason == "playing media"
            ));

            if let Err(error) = proxy.un_inhibit(cookie).await {
                panic!("failed to release idle inhibitor: {error}");
            }
            assert!(matches!(
                events.recv(),
                Ok(DbusInhibitEvent::Removed {
                    inhibitor,
                    active_count: 0,
                }) if inhibitor.cookie == cookie
            ));
        });

        drop(monitor);
    }

    #[test]
    #[ignore = "requires a private D-Bus session"]
    fn both_object_paths_share_inhibitor_cookies() {
        let (monitor, events) = connect_monitor();

        async_io::block_on(async {
            let connection = connect_client().await;
            let standard = build_proxy(&connection).await;
            let legacy = zbus::Proxy::new(
                &connection,
                super::SERVICE_NAME,
                super::LEGACY_OBJECT_PATH,
                super::SERVICE_NAME,
            )
            .await
            .unwrap_or_else(|error| panic!("failed to create legacy client: {error}"));

            let cookie: u32 = legacy
                .call("Inhibit", &("test.video", "playing media"))
                .await
                .unwrap_or_else(|error| panic!("legacy Inhibit failed: {error}"));
            assert!(matches!(
                events.recv(),
                Ok(DbusInhibitEvent::Added {
                    inhibitor,
                    active_count: 1,
                }) if inhibitor.cookie == cookie
            ));
            standard
                .un_inhibit(cookie)
                .await
                .unwrap_or_else(|error| panic!("standard UnInhibit failed: {error}"));
            assert!(matches!(
                events.recv(),
                Ok(DbusInhibitEvent::Removed {
                    inhibitor,
                    active_count: 0,
                }) if inhibitor.cookie == cookie
            ));

            let cookie = standard
                .inhibit("test.video", "playing media")
                .await
                .unwrap_or_else(|error| panic!("standard Inhibit failed: {error}"));
            assert!(matches!(
                events.recv(),
                Ok(DbusInhibitEvent::Added {
                    inhibitor,
                    active_count: 1,
                }) if inhibitor.cookie == cookie
            ));
            let _: () = legacy
                .call("UnInhibit", &(cookie,))
                .await
                .unwrap_or_else(|error| panic!("legacy UnInhibit failed: {error}"));
            assert!(matches!(
                events.recv(),
                Ok(DbusInhibitEvent::Removed {
                    inhibitor,
                    active_count: 0,
                }) if inhibitor.cookie == cookie
            ));
        });

        drop(monitor);
    }

    #[test]
    #[ignore = "requires a private D-Bus session"]
    fn releases_inhibitors_when_the_client_disconnects() {
        let (monitor, events) = connect_monitor();

        async_io::block_on(async {
            let connection = connect_client().await;
            let proxy = build_proxy(&connection).await;
            if let Err(error) = proxy.inhibit("test.video", "playing media").await {
                panic!("failed to inhibit idle: {error}");
            }
        });
        assert!(matches!(
            events.recv(),
            Ok(DbusInhibitEvent::Added {
                active_count: 1,
                ..
            })
        ));

        let event = wait_for_event(&events);
        assert!(matches!(
            event,
            DbusInhibitEvent::OwnerDisconnected {
                released_count: 1,
                active_count: 0,
                ..
            }
        ));

        drop(monitor);
    }

    #[test]
    #[ignore = "requires a private D-Bus session"]
    fn rapid_client_disconnects_leave_no_inhibitors() {
        const CLIENTS: usize = 500;
        let (monitor, events) = connect_monitor();

        async_io::block_on(async {
            for _ in 0..CLIENTS {
                let connection = connect_client().await;
                let proxy = zbus::Proxy::new(
                    &connection,
                    super::SERVICE_NAME,
                    super::OBJECT_PATH,
                    super::SERVICE_NAME,
                )
                .await
                .unwrap_or_else(|error| panic!("failed to create client proxy: {error}"));
                proxy
                    .call_noreply("Inhibit", &("test.video", "playing media"))
                    .await
                    .unwrap_or_else(|error| panic!("failed to send inhibitor request: {error}"));
                drop(proxy);
                connection
                    .close()
                    .await
                    .unwrap_or_else(|error| panic!("failed to disconnect client: {error}"));
            }
        });

        let mut added = 0;
        let mut active_count = 0;
        let deadline = Instant::now() + Duration::from_secs(15);
        while (added < CLIENTS || active_count != 0) && Instant::now() < deadline {
            match events.try_recv() {
                Ok(DbusInhibitEvent::Added {
                    active_count: count,
                    ..
                }) => {
                    added += 1;
                    active_count = count;
                }
                Ok(DbusInhibitEvent::Removed {
                    active_count: count,
                    ..
                })
                | Ok(DbusInhibitEvent::OwnerDisconnected {
                    active_count: count,
                    ..
                }) => {
                    active_count = count;
                }
                Ok(DbusInhibitEvent::Failed(error)) => panic!("D-Bus service failed: {error}"),
                Err(TryRecvError::Empty) => thread::sleep(Duration::from_millis(10)),
                Err(TryRecvError::Disconnected) => panic!("D-Bus inhibitor channel closed"),
            }
        }

        assert_eq!(added, CLIENTS);
        assert_eq!(active_count, 0);
        drop(monitor);
    }

    fn connect_monitor() -> (
        DbusInhibitMonitor,
        calloop::channel::Channel<DbusInhibitEvent>,
    ) {
        match DbusInhibitMonitor::connect() {
            Ok(parts) => parts,
            Err(error) => panic!("failed to start D-Bus inhibitor service: {error}"),
        }
    }

    async fn connect_client() -> zbus::Connection {
        match zbus::Connection::session().await {
            Ok(connection) => connection,
            Err(error) => panic!("failed to connect D-Bus client: {error}"),
        }
    }

    async fn build_proxy(connection: &zbus::Connection) -> ScreenSaverClientProxy<'_> {
        match ScreenSaverClientProxy::new(connection).await {
            Ok(proxy) => proxy,
            Err(error) => panic!("failed to create ScreenSaver client: {error}"),
        }
    }

    fn wait_for_event(events: &calloop::channel::Channel<DbusInhibitEvent>) -> DbusInhibitEvent {
        for _ in 0..100 {
            match events.try_recv() {
                Ok(event) => return event,
                Err(TryRecvError::Empty) => thread::sleep(Duration::from_millis(10)),
                Err(TryRecvError::Disconnected) => panic!("D-Bus inhibitor channel closed"),
            }
        }

        panic!("timed out waiting for D-Bus inhibitor event")
    }
}
