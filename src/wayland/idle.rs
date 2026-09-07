use std::{error::Error, fmt, time::Duration};

use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle, delegate_noop,
    globals::{BindError, GlobalError, GlobalList, GlobalListContents, registry_queue_init},
    protocol::{wl_registry, wl_seat},
};
use wayland_protocols::ext::idle_notify::v1::client::{
    ext_idle_notification_v1::{self, ExtIdleNotificationV1},
    ext_idle_notifier_v1::{self, ExtIdleNotifierV1},
};

use crate::listener::{ListenerId, TimeoutRequest};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleEvent {
    Idled(ListenerId),
    Resumed(ListenerId),
    IdleHintIdled,
    IdleHintResumed,
}

#[derive(Debug)]
pub struct IdleWaylandConnection {
    connection: Connection,
    event_queue: EventQueue<IdleWaylandState>,
    state: IdleWaylandState,
}

impl IdleWaylandConnection {
    pub fn connect(
        requests: &[TimeoutRequest],
        idle_hint_timeout: Option<Duration>,
    ) -> Result<Self, IdleWaylandError> {
        let connection = Connection::connect_to_env().map_err(IdleWaylandError::Connect)?;
        let (globals, event_queue) =
            registry_queue_init(&connection).map_err(IdleWaylandError::Globals)?;
        let queue_handle = event_queue.handle();
        let notifier = bind_idle_notifier(&globals, &queue_handle)?;
        let seat = bind_first_seat(&globals, &queue_handle)?;
        let requests = notification_requests(requests, idle_hint_timeout);
        let notifications = create_notifications(&notifier, &seat, &queue_handle, &requests, 0)?;

        connection.flush().map_err(IdleWaylandError::Flush)?;

        Ok(Self {
            connection,
            event_queue,
            state: IdleWaylandState::new(notifier, seat, queue_handle, requests, notifications),
        })
    }

    pub fn dispatch_once(&mut self) -> Result<Vec<IdleEvent>, IdleWaylandError> {
        self.event_queue
            .blocking_dispatch(&mut self.state)
            .map_err(IdleWaylandError::Dispatch)?;

        Ok(self.state.take_events())
    }

    pub(crate) fn into_parts(self) -> IdleWaylandParts {
        IdleWaylandParts {
            connection: self.connection,
            event_queue: self.event_queue,
            state: self.state,
        }
    }
}

pub(crate) struct IdleWaylandParts {
    pub(crate) connection: Connection,
    pub(crate) event_queue: EventQueue<IdleWaylandState>,
    pub(crate) state: IdleWaylandState,
}

#[derive(Debug)]
pub enum IdleWaylandError {
    Connect(wayland_client::ConnectError),
    Globals(GlobalError),
    MissingGlobal(&'static str),
    Bind {
        interface: &'static str,
        source: BindError,
    },
    TimeoutTooLarge {
        listener_id: ListenerId,
        timeout: Duration,
    },
    IdleHintTimeoutTooLarge(Duration),
    Flush(wayland_client::backend::WaylandError),
    Dispatch(wayland_client::DispatchError),
}

impl fmt::Display for IdleWaylandError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Connect(source) => write!(formatter, "failed to connect to Wayland: {source}"),
            Self::Globals(source) => {
                write!(formatter, "failed to enumerate Wayland globals: {source}")
            }
            Self::MissingGlobal(interface) => {
                write!(formatter, "compositor does not advertise `{interface}`")
            }
            Self::Bind { interface, source } => {
                write!(formatter, "failed to bind `{interface}`: {source}")
            }
            Self::TimeoutTooLarge {
                listener_id,
                timeout,
            } => write!(
                formatter,
                "listener {} timeout of {} milliseconds exceeds the Wayland protocol limit",
                listener_id.index(),
                timeout.as_millis()
            ),
            Self::IdleHintTimeoutTooLarge(timeout) => write!(
                formatter,
                "idlehint timeout of {} milliseconds exceeds the Wayland protocol limit",
                timeout.as_millis()
            ),
            Self::Flush(source) => {
                write!(
                    formatter,
                    "failed to send idle notification requests: {source}"
                )
            }
            Self::Dispatch(source) => write!(formatter, "Wayland event dispatch failed: {source}"),
        }
    }
}

impl Error for IdleWaylandError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Connect(source) => Some(source),
            Self::Globals(source) => Some(source),
            Self::Bind { source, .. } => Some(source),
            Self::Flush(source) => Some(source),
            Self::Dispatch(source) => Some(source),
            Self::MissingGlobal(_)
            | Self::TimeoutTooLarge { .. }
            | Self::IdleHintTimeoutTooLarge(_) => None,
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct IdleWaylandState {
    events: Vec<IdleEvent>,
    notification_generation: u64,
    notifier: Option<ExtIdleNotifierV1>,
    seat: Option<wl_seat::WlSeat>,
    queue_handle: Option<QueueHandle<Self>>,
    requests: Vec<IdleNotificationRequest>,
    notifications: Vec<ExtIdleNotificationV1>,
}

impl IdleWaylandState {
    fn new(
        notifier: ExtIdleNotifierV1,
        seat: wl_seat::WlSeat,
        queue_handle: QueueHandle<Self>,
        requests: Vec<IdleNotificationRequest>,
        notifications: Vec<ExtIdleNotificationV1>,
    ) -> Self {
        Self {
            events: Vec::new(),
            notification_generation: 0,
            notifier: Some(notifier),
            seat: Some(seat),
            queue_handle: Some(queue_handle),
            requests,
            notifications,
        }
    }

    pub(crate) fn record(&mut self, event: IdleEvent) {
        self.events.push(event);
    }

    pub(crate) fn take_events(&mut self) -> Vec<IdleEvent> {
        std::mem::take(&mut self.events)
    }

    fn accepts_generation(&self, generation: u64) -> bool {
        generation == self.notification_generation
    }

    pub(crate) fn reset_notifications(&mut self) -> Result<(), IdleWaylandError> {
        self.notification_generation = self.notification_generation.wrapping_add(1);
        self.events.clear();
        for notification in self.notifications.drain(..) {
            notification.destroy();
        }

        let (Some(notifier), Some(seat), Some(queue_handle)) = (
            self.notifier.as_ref(),
            self.seat.as_ref(),
            self.queue_handle.as_ref(),
        ) else {
            return Ok(());
        };
        self.notifications = create_notifications(
            notifier,
            seat,
            queue_handle,
            &self.requests,
            self.notification_generation,
        )?;

        Ok(())
    }
}

fn bind_idle_notifier(
    globals: &GlobalList,
    queue_handle: &QueueHandle<IdleWaylandState>,
) -> Result<ExtIdleNotifierV1, IdleWaylandError> {
    ensure_global::<ExtIdleNotifierV1>(globals)?;
    globals
        .bind(queue_handle, 1..=2, ())
        .map_err(|source| IdleWaylandError::Bind {
            interface: ExtIdleNotifierV1::interface().name,
            source,
        })
}

fn bind_first_seat(
    globals: &GlobalList,
    queue_handle: &QueueHandle<IdleWaylandState>,
) -> Result<wl_seat::WlSeat, IdleWaylandError> {
    ensure_global::<wl_seat::WlSeat>(globals)?;
    globals
        .bind(queue_handle, 1..=9, ())
        .map_err(|source| IdleWaylandError::Bind {
            interface: wl_seat::WlSeat::interface().name,
            source,
        })
}

fn ensure_global<I: Proxy>(globals: &GlobalList) -> Result<(), IdleWaylandError> {
    let interface = I::interface().name;
    let advertised = globals
        .contents()
        .with_list(|globals| globals.iter().any(|global| global.interface == interface));

    if advertised {
        Ok(())
    } else {
        Err(IdleWaylandError::MissingGlobal(interface))
    }
}

fn create_notifications(
    notifier: &ExtIdleNotifierV1,
    seat: &wl_seat::WlSeat,
    queue_handle: &QueueHandle<IdleWaylandState>,
    requests: &[IdleNotificationRequest],
    generation: u64,
) -> Result<Vec<ExtIdleNotificationV1>, IdleWaylandError> {
    requests
        .iter()
        .map(|request| {
            let timeout = timeout_millis(request)?;
            Ok(notifier.get_idle_notification(
                timeout,
                seat,
                queue_handle,
                IdleNotificationData {
                    target: request.target,
                    generation,
                },
            ))
        })
        .collect()
}

fn notification_requests(
    listeners: &[TimeoutRequest],
    idle_hint_timeout: Option<Duration>,
) -> Vec<IdleNotificationRequest> {
    let mut requests = listeners
        .iter()
        .map(|request| IdleNotificationRequest {
            target: IdleNotificationTarget::Listener(request.listener_id),
            timeout: request.timeout,
        })
        .collect::<Vec<_>>();

    if let Some(timeout) = idle_hint_timeout {
        requests.push(IdleNotificationRequest {
            target: IdleNotificationTarget::IdleHint,
            timeout,
        });
    }

    requests
}

fn timeout_millis(request: &IdleNotificationRequest) -> Result<u32, IdleWaylandError> {
    u32::try_from(request.timeout.as_millis()).map_err(|_| match request.target {
        IdleNotificationTarget::Listener(listener_id) => IdleWaylandError::TimeoutTooLarge {
            listener_id,
            timeout: request.timeout,
        },
        IdleNotificationTarget::IdleHint => {
            IdleWaylandError::IdleHintTimeoutTooLarge(request.timeout)
        }
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IdleNotificationTarget {
    Listener(ListenerId),
    IdleHint,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct IdleNotificationRequest {
    target: IdleNotificationTarget,
    timeout: Duration,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for IdleWaylandState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_registry::WlRegistry,
        _event: wl_registry::Event,
        _data: &GlobalListContents,
        _connection: &Connection,
        _queue_handle: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtIdleNotifierV1, ()> for IdleWaylandState {
    fn event(
        _state: &mut Self,
        _proxy: &ExtIdleNotifierV1,
        _event: ext_idle_notifier_v1::Event,
        _data: &(),
        _connection: &Connection,
        _queue_handle: &QueueHandle<Self>,
    ) {
    }
}

#[derive(Debug, Clone, Copy)]
struct IdleNotificationData {
    target: IdleNotificationTarget,
    generation: u64,
}

impl Dispatch<ExtIdleNotificationV1, IdleNotificationData> for IdleWaylandState {
    fn event(
        state: &mut Self,
        _proxy: &ExtIdleNotificationV1,
        event: ext_idle_notification_v1::Event,
        data: &IdleNotificationData,
        _connection: &Connection,
        _queue_handle: &QueueHandle<Self>,
    ) {
        if !state.accepts_generation(data.generation) {
            return;
        }

        match event {
            ext_idle_notification_v1::Event::Idled => state.record(match data.target {
                IdleNotificationTarget::Listener(listener_id) => IdleEvent::Idled(listener_id),
                IdleNotificationTarget::IdleHint => IdleEvent::IdleHintIdled,
            }),
            ext_idle_notification_v1::Event::Resumed => state.record(match data.target {
                IdleNotificationTarget::Listener(listener_id) => IdleEvent::Resumed(listener_id),
                IdleNotificationTarget::IdleHint => IdleEvent::IdleHintResumed,
            }),
            _ => {}
        }
    }
}

delegate_noop!(IdleWaylandState: ignore wl_seat::WlSeat);

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{
        IdleEvent, IdleNotificationRequest, IdleNotificationTarget, IdleWaylandError,
        IdleWaylandState, notification_requests, timeout_millis,
    };
    use crate::listener::{ListenerId, TimeoutRequest};

    #[test]
    fn converts_listener_timeout_to_protocol_milliseconds() {
        assert!(matches!(
            timeout_millis(&notification_request(0, 300)),
            Ok(300_000)
        ));
    }

    #[test]
    fn rejects_listener_timeout_above_protocol_limit() {
        let request = IdleNotificationRequest {
            target: IdleNotificationTarget::Listener(ListenerId::from_index(3)),
            timeout: Duration::from_millis(u64::from(u32::MAX) + 1),
        };

        assert!(matches!(
            timeout_millis(&request),
            Err(IdleWaylandError::TimeoutTooLarge {
                listener_id,
                timeout
            }) if listener_id == ListenerId::from_index(3) && timeout == request.timeout
        ));
    }

    #[test]
    fn appends_idle_hint_after_listener_requests() {
        let requests = notification_requests(&[request(0, 60)], Some(Duration::from_secs(300)));

        assert_eq!(
            requests,
            vec![
                notification_request(0, 60),
                IdleNotificationRequest {
                    target: IdleNotificationTarget::IdleHint,
                    timeout: Duration::from_secs(300),
                },
            ]
        );
    }

    #[test]
    fn reports_idle_hint_timeout_overflow_separately() {
        let request = IdleNotificationRequest {
            target: IdleNotificationTarget::IdleHint,
            timeout: Duration::from_millis(u64::from(u32::MAX) + 1),
        };

        assert!(matches!(
            timeout_millis(&request),
            Err(IdleWaylandError::IdleHintTimeoutTooLarge(timeout)) if timeout == request.timeout
        ));
    }

    #[test]
    fn preserves_protocol_event_order_when_draining() {
        let listener_id = ListenerId::from_index(1);
        let mut state = IdleWaylandState::default();
        state.record(IdleEvent::Idled(listener_id));
        state.record(IdleEvent::Resumed(listener_id));

        assert_eq!(
            state.take_events(),
            vec![
                IdleEvent::Idled(listener_id),
                IdleEvent::Resumed(listener_id)
            ]
        );
        assert!(state.take_events().is_empty());
    }

    #[test]
    fn rejects_callbacks_from_replaced_notification_generations() {
        let state = IdleWaylandState {
            notification_generation: 3,
            ..IdleWaylandState::default()
        };

        assert!(!state.accepts_generation(2));
        assert!(state.accepts_generation(3));
    }

    fn request(listener_index: usize, timeout_seconds: u64) -> TimeoutRequest {
        TimeoutRequest {
            listener_id: ListenerId::from_index(listener_index),
            timeout: Duration::from_secs(timeout_seconds),
        }
    }

    fn notification_request(
        listener_index: usize,
        timeout_seconds: u64,
    ) -> IdleNotificationRequest {
        IdleNotificationRequest {
            target: IdleNotificationTarget::Listener(ListenerId::from_index(listener_index)),
            timeout: Duration::from_secs(timeout_seconds),
        }
    }
}
