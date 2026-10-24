use std::{
    os::{fd::OwnedFd as StdOwnedFd, unix::net::UnixStream},
    sync::{Arc, Mutex, MutexGuard},
};

use zbus::{
    Connection, fdo,
    object_server::SignalEmitter,
    zvariant::{OwnedFd, OwnedObjectPath},
};

use crate::session::LogindInhibitor;

pub(super) const PATH: &str = "/org/freedesktop/login1/session/test";
pub(super) const SESSION_INTERFACE: &str = "org.freedesktop.login1.Session";

#[derive(Default)]
pub(super) struct State {
    pub calls: Vec<&'static str>,
    pub hints: Vec<bool>,
    pub peers: Vec<UnixStream>,
    pub preparing: bool,
    pub inhibitors: Vec<LogindInhibitor>,
    pub fail_inhibit: bool,
}

pub(super) type SharedState = Arc<Mutex<State>>;

pub(super) fn state(shared: &SharedState) -> MutexGuard<'_, State> {
    shared
        .lock()
        .unwrap_or_else(|error| panic!("test state poisoned: {error}"))
}

pub(super) async fn service(shared: &SharedState) -> Connection {
    zbus::connection::Builder::session()
        .and_then(|builder| builder.name("org.freedesktop.login1"))
        .and_then(|builder| builder.serve_at("/org/freedesktop/login1", Manager(shared.clone())))
        .and_then(|builder| builder.serve_at(PATH, Session(shared.clone())))
        .unwrap_or_else(|error| panic!("failed to configure test logind: {error}"))
        .build()
        .await
        .unwrap_or_else(|error| panic!("failed to start test logind: {error}"))
}

pub(super) async fn notify_idle_change(service: &Connection) {
    let interface = service
        .object_server()
        .interface::<_, Manager>("/org/freedesktop/login1")
        .await
        .unwrap_or_else(|error| panic!("test manager missing: {error}"));
    interface
        .get()
        .await
        .block_inhibited_changed(interface.signal_emitter())
        .await
        .unwrap_or_else(|error| panic!("failed to emit property change: {error}"));
}

struct Manager(SharedState);

#[zbus::interface(name = "org.freedesktop.login1.Manager")]
impl Manager {
    fn get_session(&self, _session_id: &str) -> OwnedObjectPath {
        OwnedObjectPath::try_from(PATH).unwrap_or_else(|error| panic!("invalid test path: {error}"))
    }

    fn get_session_by_pid(&self, _pid: u32) -> OwnedObjectPath {
        self.get_session("")
    }

    fn list_inhibitors(&self) -> Vec<LogindInhibitor> {
        let mut state = state(&self.0);
        state.calls.push("list");
        state.inhibitors.clone()
    }

    fn inhibit(&self, what: &str, who: &str, _why: &str, mode: &str) -> fdo::Result<OwnedFd> {
        assert_eq!((what, who, mode), ("sleep", "sweetidle", "delay"));
        let mut state = state(&self.0);
        state.calls.push("delay");
        if state.fail_inhibit {
            return Err(fdo::Error::Failed("test inhibitor failure".to_string()));
        }
        let (child, peer) =
            UnixStream::pair().map_err(|error| fdo::Error::Failed(error.to_string()))?;
        peer.set_nonblocking(true)
            .map_err(|error| fdo::Error::Failed(error.to_string()))?;
        state.peers.push(peer);
        Ok(OwnedFd::from(StdOwnedFd::from(child)))
    }

    #[zbus(property)]
    fn preparing_for_sleep(&self) -> bool {
        state(&self.0).preparing
    }

    #[zbus(property)]
    fn block_inhibited(&self) -> &str {
        ""
    }

    #[zbus(signal)]
    async fn prepare_for_sleep(emitter: &SignalEmitter<'_>, active: bool) -> zbus::Result<()>;
}

struct Session(SharedState);

#[zbus::interface(name = "org.freedesktop.login1.Session")]
impl Session {
    fn set_idle_hint(&self, idle: bool) {
        let mut state = state(&self.0);
        state.calls.push("hint");
        state.hints.push(idle);
    }
}
