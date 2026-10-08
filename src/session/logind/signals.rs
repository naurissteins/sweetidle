use zbus::{Message, proxy::SignalStream};

use super::{SessionError, SessionEvent, proxy::LogindSessionProxy};

pub(super) async fn subscribe(
    proxy: &LogindSessionProxy<'_>,
) -> Result<SignalStream<'static>, SessionError> {
    // one queue preserves session signal order and retains the proxy's sender filters
    proxy
        .inner()
        .receive_all_signals()
        .await
        .map_err(|source| SessionError::SubscribeSession(Box::new(source)))
}

pub(super) fn decode(message: &Message) -> Result<Option<SessionEvent>, SessionError> {
    let header = message.header();
    let event = match header.member().map(|member| member.as_str()) {
        Some("Lock") => SessionEvent::LockRequested,
        Some("Unlock") => SessionEvent::UnlockRequested,
        _ => return Ok(None),
    };
    message
        .body()
        .deserialize::<()>()
        .map_err(|source| SessionError::DecodeSignal(Box::new(source)))?;
    Ok(Some(event))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use futures_lite::{Stream, StreamExt, future};
    use zbus::{Connection, Message};

    use super::{LogindSessionProxy, SessionError, SessionEvent, decode, subscribe};

    const PATH: &str = "/org/freedesktop/login1/session/test";
    const INTERFACE: &str = "org.freedesktop.login1.Session";

    fn message(member: &str) -> Message {
        Message::signal(PATH, INTERFACE, member)
            .and_then(|builder| builder.build(&()))
            .unwrap_or_else(|error| panic!("failed to build test signal: {error}"))
    }

    #[test]
    fn decodes_lock_and_unlock_signals() {
        assert!(matches!(
            decode(&message("Lock")),
            Ok(Some(SessionEvent::LockRequested))
        ));
        assert!(matches!(
            decode(&message("Unlock")),
            Ok(Some(SessionEvent::UnlockRequested))
        ));
    }

    #[test]
    fn ignores_unsupported_session_signals() {
        assert!(matches!(decode(&message("Other")), Ok(None)));
    }

    #[test]
    fn rejects_lock_and_unlock_signals_with_arguments() {
        for member in ["Lock", "Unlock"] {
            let signal = Message::signal(PATH, INTERFACE, member)
                .and_then(|builder| builder.build(&"unexpected argument"))
                .unwrap_or_else(|error| panic!("failed to build test signal: {error}"));
            assert!(matches!(
                decode(&signal),
                Err(SessionError::DecodeSignal(_))
            ));
        }
    }

    #[test]
    #[ignore = "requires a private D-Bus session"]
    fn buffered_session_signals_keep_emission_order_and_proxy_filters() {
        async_io::block_on(async {
            let service = Connection::session()
                .await
                .unwrap_or_else(|error| panic!("failed to connect test service: {error}"));
            service
                .request_name("org.freedesktop.login1")
                .await
                .unwrap_or_else(|error| panic!("failed to own test logind name: {error}"));
            let client = Connection::session()
                .await
                .unwrap_or_else(|error| panic!("failed to connect test client: {error}"));
            let proxy = LogindSessionProxy::builder(&client)
                .path(PATH)
                .unwrap_or_else(|error| panic!("invalid test session path: {error}"))
                .build()
                .await
                .unwrap_or_else(|error| panic!("failed to build session proxy: {error}"));
            let mut signals = subscribe(&proxy)
                .await
                .unwrap_or_else(|error| panic!("failed to subscribe to session signals: {error}"));
            let mut barrier = proxy
                .inner()
                .receive_signal("Barrier")
                .await
                .unwrap_or_else(|error| panic!("failed to subscribe to test barrier: {error}"));

            let impostor = Connection::session()
                .await
                .unwrap_or_else(|error| panic!("failed to connect test impostor: {error}"));
            emit(&impostor, PATH, INTERFACE, "Lock").await;
            zbus::fdo::DBusProxy::new(&impostor)
                .await
                .unwrap_or_else(|error| panic!("failed to create bus proxy: {error}"))
                .get_id()
                .await
                .unwrap_or_else(|error| panic!("bus barrier failed: {error}"));
            emit(
                &service,
                "/org/freedesktop/login1/session/other",
                INTERFACE,
                "Lock",
            )
            .await;
            emit(&service, PATH, "org.example.Other", "Lock").await;
            emit(&service, PATH, INTERFACE, "Other").await;
            for _ in 0..20 {
                emit(&service, PATH, INTERFACE, "Lock").await;
                emit(&service, PATH, INTERFACE, "Unlock").await;
            }
            emit(&service, PATH, INTERFACE, "Barrier").await;
            // the barrier proves the socket reader buffered the whole burst before consumption
            let _ = next_signal(&mut barrier).await;

            for index in 0..40 {
                let event = loop {
                    let signal = next_signal(&mut signals).await;
                    if let Some(event) = decode(&signal)
                        .unwrap_or_else(|error| panic!("failed to decode test signal: {error}"))
                    {
                        break event;
                    }
                };
                if index % 2 == 0 {
                    assert!(
                        matches!(event, SessionEvent::LockRequested),
                        "signal {index}: {event:?}"
                    );
                } else {
                    assert!(
                        matches!(event, SessionEvent::UnlockRequested),
                        "signal {index}: {event:?}"
                    );
                }
            }
        });
    }

    async fn emit(connection: &Connection, path: &str, interface: &str, member: &str) {
        connection
            .emit_signal(None::<&str>, path, interface, member, &())
            .await
            .unwrap_or_else(|error| panic!("failed to emit test signal: {error}"));
    }

    async fn next_signal(signals: &mut (impl Stream<Item = Message> + Unpin)) -> Message {
        future::race(signals.next(), async {
            async_io::Timer::after(Duration::from_secs(5)).await;
            panic!("timed out waiting for test signal");
        })
        .await
        .unwrap_or_else(|| panic!("session signal stream closed"))
    }
}
