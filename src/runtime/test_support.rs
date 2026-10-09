use std::{
    io::{self, Read},
    os::{fd::OwnedFd, unix::net::UnixStream},
    thread,
    time::{Duration, Instant},
};

use calloop::EventLoop;

use super::{RuntimeEvent, state::RuntimeState};
use crate::session::DelayInhibitor;
use crate::test_support::TestDeadline;

pub(super) fn new_event_loop() -> EventLoop<'static, RuntimeState> {
    match EventLoop::try_new() {
        Ok(event_loop) => event_loop,
        Err(error) => panic!("{error}"),
    }
}

pub(super) fn wait_for_process_events(
    event_loop: &mut EventLoop<'static, RuntimeState>,
    state: &mut RuntimeState,
) -> Vec<RuntimeEvent> {
    let deadline = TestDeadline::new();
    loop {
        let wait = deadline
            .remaining("command lifecycle event")
            .min(Duration::from_millis(100));
        if let Err(error) = event_loop.dispatch(Some(wait), state) {
            panic!("{error}");
        }
        if let Err(error) = state.collect_ready_processes() {
            panic!("{error}");
        }
        let events = state.take_runtime_events();
        if !events.is_empty() {
            return events;
        }
    }
}

pub(super) fn assert_inhibitor_released(observer: &mut UnixStream) {
    // concurrent forks can briefly hold a CLOEXEC FD until the child execs
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match observer.read(&mut [0]) {
            Ok(0) => return,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            result => panic!("unexpected inhibitor observation: {result:?}"),
        }
        assert!(
            Instant::now() < deadline,
            "sleep inhibitor FD is still open"
        );
        thread::sleep(Duration::from_millis(1));
    }
}

pub(super) fn observable_inhibitor() -> (UnixStream, DelayInhibitor) {
    let (observer, held) = UnixStream::pair()
        .unwrap_or_else(|error| panic!("failed to create test socket pair: {error}"));
    observer
        .set_nonblocking(true)
        .unwrap_or_else(|error| panic!("failed to set test socket nonblocking: {error}"));
    let fd: OwnedFd = held.into();
    (observer, DelayInhibitor::new(fd.into()))
}
