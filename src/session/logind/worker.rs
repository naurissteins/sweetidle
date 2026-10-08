use std::{future::pending, ops::ControlFlow, sync::mpsc::SyncSender};

use async_channel::Receiver;
use calloop::channel::Sender as EventSender;
use futures_lite::{StreamExt, future};
use zbus::{
    Connection, Message,
    proxy::{PropertyStream, SignalStream},
};

use super::{
    DelayInhibitor, LogindFeatures, SessionError, SessionEvent, SessionRequest,
    proxy::{
        LogindManagerProxy, LogindSessionProxy, PrepareForSleepStream, acquire_delay_inhibitor,
        create_session_proxy, query_idle_inhibitors, set_idle_hint,
    },
    signals,
};
use crate::session::SystemdIdleInhibitor;

pub(super) struct LogindWorker {
    sleep_signals: Option<PrepareForSleepStream>,
    session_signals: Option<SignalStream<'static>>,
    idle_inhibitor_changes: Option<PropertyStream<'static, String>>,
    proxy: LogindManagerProxy<'static>,
    session_proxy: Option<LogindSessionProxy<'static>>,
    features: LogindFeatures,
    inhibitor: Option<DelayInhibitor>,
    idle_inhibitors: Vec<SystemdIdleInhibitor>,
    already_preparing: bool,
}

impl LogindWorker {
    pub(super) async fn open(features: LogindFeatures) -> Result<Self, SessionError> {
        // dbus work stays off the Wayland event loop
        let connection = Connection::system()
            .await
            .map_err(|source| SessionError::Connect(Box::new(source)))?;
        Self::from_connection(connection, features).await
    }

    async fn from_connection(
        connection: Connection,
        features: LogindFeatures,
    ) -> Result<Self, SessionError> {
        let proxy = LogindManagerProxy::new(&connection)
            .await
            .map_err(|source| SessionError::CreateProxy(Box::new(source)))?;
        let sleep_signals = if features.sleep_events {
            Some(
                proxy
                    .receive_prepare_for_sleep()
                    .await
                    .map_err(|source| SessionError::Subscribe(Box::new(source)))?,
            )
        } else {
            None
        };
        let session_proxy = if features.session_events || features.idle_hint {
            Some(create_session_proxy(&connection, &proxy).await?)
        } else {
            None
        };
        let session_signals = match (features.session_events, session_proxy.as_ref()) {
            (true, Some(proxy)) => Some(signals::subscribe(proxy).await?),
            (false, _) | (true, None) => None,
        };
        let idle_inhibitor_changes = if features.idle_inhibitors {
            Some(proxy.receive_block_inhibited_changed().await)
        } else {
            None
        };
        let mut worker = Self {
            sleep_signals,
            session_signals,
            idle_inhibitor_changes,
            proxy,
            session_proxy,
            features,
            inhibitor: None,
            idle_inhibitors: Vec::new(),
            already_preparing: false,
        };
        worker.initialize_state().await?;
        Ok(worker)
    }

    async fn initialize_state(&mut self) -> Result<(), SessionError> {
        if self.features.idle_inhibitors {
            self.idle_inhibitors = query_idle_inhibitors(&self.proxy).await?;
        }
        if self.features.delay_inhibitor {
            self.inhibitor = Some(acquire_delay_inhibitor(&self.proxy).await?);
        }
        if self.features.sleep_events {
            self.already_preparing = self
                .proxy
                .preparing_for_sleep()
                .await
                .map_err(|source| SessionError::QueryState(Box::new(source)))?;
        }
        if self.features.idle_hint {
            set_idle_hint(self.session_proxy.as_ref(), false).await?;
        }
        Ok(())
    }

    pub(super) fn start(
        &mut self,
        events: &EventSender<SessionEvent>,
        ready: &SyncSender<Result<(), SessionError>>,
    ) -> ControlFlow<()> {
        if !self.idle_inhibitors.is_empty()
            && events
                .send(SessionEvent::SystemdIdleInhibitorsChanged(
                    self.idle_inhibitors.clone(),
                ))
                .is_err()
        {
            return ControlFlow::Break(());
        }
        // report readiness only after subscriptions and the initial state are set
        if ready.send(Ok(())).is_err() {
            return ControlFlow::Break(());
        }
        if self.already_preparing {
            return self.prepare_for_sleep(events);
        }
        ControlFlow::Continue(())
    }

    pub(super) async fn next_input(
        &mut self,
        requests: &Receiver<SessionRequest>,
        shutdown: &Receiver<()>,
    ) -> WorkerInput {
        future::race(
            future::race(
                async {
                    match self.sleep_signals.as_mut() {
                        Some(signals) => match signals.next().await {
                            Some(signal) => WorkerInput::Sleep(
                                signal
                                    .args()
                                    .map(|args| Some(*args.active()))
                                    .map_err(|source| SessionError::DecodeSignal(Box::new(source))),
                            ),
                            None => WorkerInput::Sleep(Ok(None)),
                        },
                        None => pending().await,
                    }
                },
                async {
                    match self.session_signals.as_mut() {
                        Some(signals) => WorkerInput::Session(signals.next().await),
                        None => pending().await,
                    }
                },
            ),
            future::race(
                async {
                    match self.idle_inhibitor_changes.as_mut() {
                        Some(changes) => {
                            WorkerInput::IdleInhibitorsChanged(changes.next().await.is_some())
                        }
                        None => pending().await,
                    }
                },
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
        .await
    }

    pub(super) async fn handle(
        &mut self,
        input: WorkerInput,
        events: &EventSender<SessionEvent>,
    ) -> Result<ControlFlow<()>, SessionError> {
        match input {
            WorkerInput::Sleep(Ok(Some(true))) => Ok(self.prepare_for_sleep(events)),
            WorkerInput::Sleep(Ok(Some(false))) => {
                if self.features.delay_inhibitor {
                    self.inhibitor = Some(acquire_delay_inhibitor(&self.proxy).await?);
                }
                Ok(send_event(events, SessionEvent::Resumed))
            }
            WorkerInput::Sleep(Ok(None))
            | WorkerInput::Session(None)
            | WorkerInput::IdleInhibitorsChanged(false) => Err(SessionError::Disconnected),
            WorkerInput::Sleep(Err(error)) => Err(error),
            WorkerInput::Session(Some(signal)) => Ok(match signals::decode(&signal)? {
                Some(event) => send_event(events, event),
                None => ControlFlow::Continue(()),
            }),
            WorkerInput::IdleInhibitorsChanged(true) => self.refresh_idle_inhibitors(events).await,
            WorkerInput::Request(SessionRequest::SetIdleHint(idle)) => {
                set_idle_hint(self.session_proxy.as_ref(), idle).await?;
                Ok(ControlFlow::Continue(()))
            }
            WorkerInput::Shutdown => {
                if self.features.idle_hint {
                    set_idle_hint(self.session_proxy.as_ref(), false).await?;
                }
                Ok(ControlFlow::Break(()))
            }
        }
    }

    fn prepare_for_sleep(&mut self, events: &EventSender<SessionEvent>) -> ControlFlow<()> {
        send_event(events, SessionEvent::PrepareForSleep(self.inhibitor.take()))
    }

    async fn refresh_idle_inhibitors(
        &mut self,
        events: &EventSender<SessionEvent>,
    ) -> Result<ControlFlow<()>, SessionError> {
        let next = query_idle_inhibitors(&self.proxy).await?;
        if next != self.idle_inhibitors {
            self.idle_inhibitors = next;
            return Ok(send_event(
                events,
                SessionEvent::SystemdIdleInhibitorsChanged(self.idle_inhibitors.clone()),
            ));
        }
        Ok(ControlFlow::Continue(()))
    }
}

fn send_event(events: &EventSender<SessionEvent>, event: SessionEvent) -> ControlFlow<()> {
    match events.send(event) {
        Ok(()) => ControlFlow::Continue(()),
        Err(_) => ControlFlow::Break(()),
    }
}

pub(super) enum WorkerInput {
    Sleep(Result<Option<bool>, SessionError>),
    Session(Option<Message>),
    IdleInhibitorsChanged(bool),
    Request(SessionRequest),
    Shutdown,
}

#[cfg(test)]
mod tests;
