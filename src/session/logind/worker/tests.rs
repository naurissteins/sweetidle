use std::{
    io::{self, Read},
    sync::{Arc, Mutex, mpsc},
    time::Duration,
};

use async_channel::{Receiver, bounded, unbounded};
use calloop::channel::channel;
use futures_lite::future;
use zbus::Connection;

use super::{
    LogindFeatures, LogindWorker, SessionError, SessionEvent, SessionRequest, WorkerInput,
};
use fixture::{PATH, SESSION_INTERFACE, SharedState, State, service, state};

mod fixture;

fn features() -> LogindFeatures {
    LogindFeatures {
        sleep_events: true,
        delay_inhibitor: true,
        session_events: true,
        idle_inhibitors: true,
        idle_hint: true,
    }
}

async fn open(features: LogindFeatures) -> Result<LogindWorker, SessionError> {
    let connection = Connection::session()
        .await
        .unwrap_or_else(|error| panic!("failed to connect worker: {error}"));
    LogindWorker::from_connection(connection, features).await
}

async fn next(
    worker: &mut LogindWorker,
    requests: &Receiver<SessionRequest>,
    shutdown: &Receiver<()>,
) -> WorkerInput {
    future::race(worker.next_input(requests, shutdown), async {
        async_io::Timer::after(Duration::from_secs(5)).await;
        panic!("timed out waiting for worker input");
    })
    .await
}

fn assert_fd_open(shared: &SharedState, index: usize) {
    assert!(matches!(state(shared).peers[index].read(&mut [0]),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock));
}

fn assert_fd_closed(shared: &SharedState, index: usize) {
    assert_eq!(
        state(shared).peers[index]
            .read(&mut [0])
            .unwrap_or_else(|error| panic!("failed to observe FD closure: {error}")),
        0
    );
}

#[test]
#[ignore = "requires a private D-Bus session"]
fn startup_and_sleep_cycles_keep_delay_inhibitor_ownership() {
    async_io::block_on(async {
        let shared = Arc::new(Mutex::new(State {
            preparing: true,
            inhibitors: vec![(
                "idle".into(),
                "video".into(),
                "playing".into(),
                "block".into(),
                1000,
                42,
            )],
            ..State::default()
        }));
        let service = service(&shared).await;
        let mut worker = open(features())
            .await
            .unwrap_or_else(|error| panic!("worker startup failed: {error}"));
        assert_eq!(state(&shared).calls, ["list", "delay", "hint"]);
        let (events, channel) = channel();
        let (ready, received) = mpsc::sync_channel(1);
        assert!(worker.start(&events, &ready).is_continue());
        assert!(
            received
                .recv()
                .unwrap_or_else(|error| panic!("no readiness: {error}"))
                .is_ok()
        );
        assert!(
            matches!(channel.try_recv(), Ok(SessionEvent::SystemdIdleInhibitorsChanged(snapshot)) if snapshot.len() == 1 && snapshot[0].who == "video")
        );
        let first = match channel.try_recv() {
            Ok(SessionEvent::PrepareForSleep(Some(inhibitor))) => inhibitor,
            event => panic!("missing initial sleep event: {event:?}"),
        };
        assert_fd_open(&shared, 0);
        let (_requests, requests) = unbounded();
        let (_shutdown, shutdown) = bounded(1);
        service
            .emit_signal(
                None::<&str>,
                "/org/freedesktop/login1",
                "org.freedesktop.login1.Manager",
                "PrepareForSleep",
                &false,
            )
            .await
            .unwrap_or_else(|error| panic!("failed to emit resume: {error}"));
        let input = loop {
            let input = next(&mut worker, &requests, &shutdown).await;
            if matches!(input, WorkerInput::Sleep(_)) {
                break input;
            }
            // the initial property-cache notification can precede the sleep signal
            assert!(matches!(input, WorkerInput::IdleInhibitorsChanged(true)));
            assert!(
                worker
                    .handle(input, &events)
                    .await
                    .unwrap_or_else(|error| panic!("initial snapshot refresh failed: {error}"))
                    .is_continue()
            );
        };
        assert!(matches!(input, WorkerInput::Sleep(Ok(Some(false)))));
        assert!(
            worker
                .handle(input, &events)
                .await
                .unwrap_or_else(|error| panic!("resume failed: {error}"))
                .is_continue()
        );
        assert_eq!(state(&shared).peers.len(), 2);
        assert!(matches!(channel.try_recv(), Ok(SessionEvent::Resumed)));
        assert_fd_open(&shared, 1);
        drop(first);
        assert_fd_closed(&shared, 0);
        assert!(
            worker
                .handle(WorkerInput::Sleep(Ok(Some(true))), &events)
                .await
                .unwrap_or_else(|error| panic!("prepare failed: {error}"))
                .is_continue()
        );
        let second = channel
            .try_recv()
            .unwrap_or_else(|error| panic!("no second prepare: {error}"));
        drop(worker);
        assert_fd_open(&shared, 1);
        drop(second);
        assert_fd_closed(&shared, 1);
    });
}

#[test]
#[ignore = "requires a private D-Bus session"]
fn worker_keeps_session_order_and_clears_idle_hint_on_request_channel_closure() {
    async_io::block_on(async {
        let shared = Arc::new(Mutex::new(State::default()));
        let service = service(&shared).await;
        let mut features = features();
        features.sleep_events = false;
        features.delay_inhibitor = false;
        features.idle_inhibitors = false;
        let mut worker = open(features)
            .await
            .unwrap_or_else(|error| panic!("worker startup failed: {error}"));
        let (events, channel) = channel();
        let (requests, request_receiver) = unbounded();
        let (_shutdown, shutdown) = bounded(1);
        for _ in 0..20 {
            for member in ["Lock", "Unlock"] {
                service
                    .emit_signal(None::<&str>, PATH, SESSION_INTERFACE, member, &())
                    .await
                    .unwrap_or_else(|error| panic!("failed to emit session signal: {error}"));
            }
        }
        for index in 0..40 {
            let input = next(&mut worker, &request_receiver, &shutdown).await;
            assert!(
                worker
                    .handle(input, &events)
                    .await
                    .unwrap_or_else(|error| panic!("session routing failed: {error}"))
                    .is_continue()
            );
            let event = channel
                .try_recv()
                .unwrap_or_else(|error| panic!("no session event: {error}"));
            assert!(matches!(
                (index % 2, event),
                (0, SessionEvent::LockRequested) | (1, SessionEvent::UnlockRequested)
            ));
        }
        requests
            .send(SessionRequest::SetIdleHint(true))
            .await
            .unwrap_or_else(|error| panic!("request failed: {error}"));
        let input = next(&mut worker, &request_receiver, &shutdown).await;
        assert!(
            worker
                .handle(input, &events)
                .await
                .unwrap_or_else(|error| panic!("hint update failed: {error}"))
                .is_continue()
        );
        drop(requests);
        let input = next(&mut worker, &request_receiver, &shutdown).await;
        assert!(matches!(input, WorkerInput::Shutdown));
        assert!(
            worker
                .handle(input, &events)
                .await
                .unwrap_or_else(|error| panic!("shutdown failed: {error}"))
                .is_break()
        );
        assert_eq!(state(&shared).hints, [false, true, false]);
    });
}

#[test]
#[ignore = "requires a private D-Bus session"]
fn worker_deduplicates_snapshots_and_preserves_fatal_input_errors() {
    async_io::block_on(async {
        let shared = Arc::new(Mutex::new(State::default()));
        let service = service(&shared).await;
        let mut worker = open(features())
            .await
            .unwrap_or_else(|error| panic!("worker startup failed: {error}"));
        let (events, channel) = channel();
        state(&shared).inhibitors.push((
            "idle".into(),
            "video".into(),
            "playing".into(),
            "block".into(),
            1000,
            42,
        ));
        let (_requests, requests) = unbounded();
        let (_shutdown, shutdown) = bounded(1);
        for _ in 0..2 {
            fixture::notify_idle_change(&service).await;
            let input = next(&mut worker, &requests, &shutdown).await;
            assert!(matches!(input, WorkerInput::IdleInhibitorsChanged(true)));
            assert!(
                worker
                    .handle(input, &events)
                    .await
                    .unwrap_or_else(|error| panic!("snapshot refresh failed: {error}"))
                    .is_continue()
            );
        }
        assert!(
            matches!(channel.try_recv(), Ok(SessionEvent::SystemdIdleInhibitorsChanged(snapshot)) if snapshot.len() == 1)
        );
        assert!(channel.try_recv().is_err());
        for input in [
            WorkerInput::Sleep(Ok(None)),
            WorkerInput::Session(None),
            WorkerInput::IdleInhibitorsChanged(false),
        ] {
            assert!(matches!(
                worker.handle(input, &events).await,
                Err(SessionError::Disconnected)
            ));
        }
        assert!(matches!(
            worker
                .handle(
                    WorkerInput::Sleep(Err(SessionError::RequestChannelClosed)),
                    &events
                )
                .await,
            Err(SessionError::RequestChannelClosed)
        ));
        drop(channel);
        assert!(
            worker
                .handle(WorkerInput::Sleep(Ok(Some(true))), &events)
                .await
                .unwrap_or_else(|error| panic!("prepare failed: {error}"))
                .is_break()
        );
        assert_fd_closed(&shared, 0);
    });
}

#[test]
#[ignore = "requires a private D-Bus session"]
fn startup_failure_keeps_the_typed_error_and_skips_later_initialization() {
    async_io::block_on(async {
        let shared = Arc::new(Mutex::new(State {
            fail_inhibit: true,
            ..State::default()
        }));
        let _service = service(&shared).await;
        let error = match open(features()).await {
            Ok(_) => panic!("startup unexpectedly succeeded"),
            Err(error) => error,
        };
        assert!(matches!(error, SessionError::Inhibit(_)));
        assert_eq!(state(&shared).calls, ["list", "delay"]);
    });
}

#[test]
#[ignore = "requires a private D-Bus session"]
fn unanswered_logind_call_times_out_with_a_typed_error() {
    async_io::block_on(async {
        let shared = Arc::new(Mutex::new(State {
            ignore_idle_hint: true,
            ..State::default()
        }));
        let _service = service(&shared).await;
        let features = LogindFeatures {
            sleep_events: false,
            delay_inhibitor: false,
            session_events: false,
            idle_inhibitors: false,
            idle_hint: true,
        };
        let builder = zbus::connection::Builder::session()
            .unwrap_or_else(|error| panic!("failed to configure worker bus: {error}"));

        let result = LogindWorker::connect(builder, Duration::from_millis(100), features).await;

        assert!(matches!(result, Err(SessionError::SetIdleHint(source))
            if matches!(&*source, zbus::Error::InputOutput(error) if error.kind() == io::ErrorKind::TimedOut)));
        assert_eq!(state(&shared).calls, ["hint"]);
    });
}
