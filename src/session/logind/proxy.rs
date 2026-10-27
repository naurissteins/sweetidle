use zbus::{
    Connection, proxy,
    zvariant::{OwnedFd, OwnedObjectPath},
};

use super::{DelayInhibitor, SessionError};
use crate::session::{LogindInhibitor, SystemdIdleInhibitor, idle_block_inhibitors};

#[proxy(
    interface = "org.freedesktop.login1.Manager",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1",
    gen_blocking = false
)]
pub(super) trait LogindManager {
    fn get_session(&self, session_id: &str) -> zbus::Result<OwnedObjectPath>;

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
pub(super) trait LogindSession {
    fn set_idle_hint(&self, idle: bool) -> zbus::Result<()>;
}

pub(super) async fn create_session_proxy(
    connection: &Connection,
    manager: &LogindManagerProxy<'_>,
) -> Result<LogindSessionProxy<'static>, SessionError> {
    let path = resolve_session_path(manager).await?;
    LogindSessionProxy::builder(connection)
        .path(path)
        .map_err(|source| SessionError::CreateSessionProxy(Box::new(source)))?
        .build()
        .await
        .map_err(|source| SessionError::CreateSessionProxy(Box::new(source)))
}

pub(super) async fn resolve_session_path(
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

    // "auto" falls back to the user's display session, which user services need
    // because their processes belong to no session
    manager
        .get_session("auto")
        .await
        .map_err(|by_auto| SessionError::ResolveSession {
            by_session_id,
            by_auto: Box::new(by_auto),
        })
}

pub(super) async fn acquire_delay_inhibitor(
    proxy: &LogindManagerProxy<'_>,
) -> Result<DelayInhibitor, SessionError> {
    proxy
        .inhibit(
            "sleep",
            "sweetidle",
            "run the configured before-sleep command",
            "delay",
        )
        .await
        .map(DelayInhibitor::new)
        .map_err(|source| SessionError::Inhibit(Box::new(source)))
}

pub(super) async fn set_idle_hint(
    proxy: Option<&LogindSessionProxy<'_>>,
    idle: bool,
) -> Result<(), SessionError> {
    let Some(proxy) = proxy else {
        return Err(SessionError::RequestChannelClosed);
    };

    proxy
        .set_idle_hint(idle)
        .await
        .map_err(|source| SessionError::SetIdleHint(Box::new(source)))
}

pub(super) async fn query_idle_inhibitors(
    proxy: &LogindManagerProxy<'_>,
) -> Result<Vec<SystemdIdleInhibitor>, SessionError> {
    proxy
        .list_inhibitors()
        .await
        .map(idle_block_inhibitors)
        .map_err(|source| SessionError::QueryInhibitors(Box::new(source)))
}
