use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

use super::{Worker, WorkerError, report_initialization_error};

#[derive(Debug)]
enum TestError {
    Worker(WorkerError),
    Startup,
}

impl From<WorkerError> for TestError {
    fn from(error: WorkerError) -> Self {
        Self::Worker(error)
    }
}

#[test]
fn spawn_waits_for_readiness_and_drop_joins_cleanup() {
    let (entered, entry) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    let (returned, result) = mpsc::channel();
    let cleaned = Arc::new(AtomicBool::new(false));
    let worker_cleaned = Arc::clone(&cleaned);
    let caller = thread::spawn(move || {
        let worker = Worker::spawn::<TestError>("sweetsidle-test", move |ready, shutdown| {
            assert_eq!(thread::current().name(), Some("sweetsidle-test"));
            entered.send(()).unwrap_or_else(|error| panic!("{error}"));
            gate.recv().unwrap_or_else(|error| panic!("{error}"));
            ready.send(Ok(())).unwrap_or_else(|error| panic!("{error}"));
            shutdown
                .recv_blocking()
                .unwrap_or_else(|error| panic!("{error}"));
            worker_cleaned.store(true, Ordering::SeqCst);
        });
        assert!(returned.send(worker).is_ok());
    });
    entry
        .recv_timeout(Duration::from_secs(5))
        .unwrap_or_else(|error| panic!("{error}"));
    assert!(matches!(result.try_recv(), Err(mpsc::TryRecvError::Empty)));
    release.send(()).unwrap_or_else(|error| panic!("{error}"));
    let worker = result
        .recv_timeout(Duration::from_secs(5))
        .unwrap_or_else(|error| panic!("{error}"))
        .unwrap_or_else(|error| panic!("{error:?}"));
    assert!(!cleaned.load(Ordering::SeqCst));
    drop(worker);
    assert!(cleaned.load(Ordering::SeqCst));
    caller
        .join()
        .unwrap_or_else(|_| panic!("spawn caller panicked"));
}

#[test]
fn startup_error_is_returned_after_worker_cleanup() {
    let cleaned = Arc::new(AtomicBool::new(false));
    let worker_cleaned = Arc::clone(&cleaned);
    let result = Worker::spawn("sweetsidle-test", move |ready, _shutdown| {
        let result: Result<(), TestError> = report_initialization_error(&ready, TestError::Startup);
        assert!(matches!(
            result,
            Err(TestError::Worker(WorkerError::InitializationStopped))
        ));
        worker_cleaned.store(true, Ordering::SeqCst);
    });
    assert!(matches!(result, Err(TestError::Startup)));
    assert!(cleaned.load(Ordering::SeqCst));
}

#[test]
fn missing_readiness_is_returned_after_worker_cleanup() {
    let cleaned = Arc::new(AtomicBool::new(false));
    let worker_cleaned = Arc::clone(&cleaned);
    let result = Worker::spawn::<TestError>("sweetsidle-test", move |ready, _shutdown| {
        drop(ready);
        worker_cleaned.store(true, Ordering::SeqCst);
    });
    assert!(matches!(
        result,
        Err(TestError::Worker(WorkerError::InitializationStopped))
    ));
    assert!(cleaned.load(Ordering::SeqCst));
}

#[test]
fn explicit_shutdown_then_drop_sends_once_and_joins() {
    let cleaned = Arc::new(AtomicBool::new(false));
    let worker_cleaned = Arc::clone(&cleaned);
    let mut worker = Worker::spawn::<TestError>("sweetsidle-test", move |ready, shutdown| {
        ready.send(Ok(())).unwrap_or_else(|error| panic!("{error}"));
        shutdown
            .recv_blocking()
            .unwrap_or_else(|error| panic!("{error}"));
        assert!(matches!(
            shutdown.try_recv(),
            Err(async_channel::TryRecvError::Closed)
        ));
        worker_cleaned.store(true, Ordering::SeqCst);
    })
    .unwrap_or_else(|error| panic!("{error:?}"));
    worker.request_shutdown();
    worker.request_shutdown();
    drop(worker);
    assert!(cleaned.load(Ordering::SeqCst));
}

#[test]
fn initialization_report_handles_a_closed_receiver() {
    let (ready, receiver) = mpsc::sync_channel(1);
    drop(receiver);
    let result: Result<(), TestError> = report_initialization_error(&ready, TestError::Startup);
    assert!(matches!(
        result,
        Err(TestError::Worker(WorkerError::InitializationStopped))
    ));
}
