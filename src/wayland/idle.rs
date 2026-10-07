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

use crate::listener::{InhibitPolicy, ListenerId, TimeoutRequest};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleEvent {
    Idled(ListenerId),
    Resumed(ListenerId),
    IdleHintIdled,
    IdleHintResumed,
}

#[derive(Debug)]
pub(crate) struct IdleWaylandParts {
    pub(crate) connection: Connection,
    pub(crate) event_queue: EventQueue<IdleWaylandState>,
    pub(crate) state: IdleWaylandState,
}

impl IdleWaylandParts {
    pub(crate) fn connect(
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
        validate_requests(&requests, Some(notifier.version()))?;
        let notifications = create_notifications(&notifier, &seat, &queue_handle, &requests, 0, 0)?;

        connection.flush().map_err(IdleWaylandError::Flush)?;

        Ok(Self {
            connection,
            event_queue,
            state: IdleWaylandState::new(notifier, seat, queue_handle, requests, notifications),
        })
    }
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
    InputIdleUnsupported {
        listener_id: ListenerId,
        advertised_version: u32,
    },
    Flush(wayland_client::backend::WaylandError),
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
                "idle hint timeout of {} milliseconds exceeds the Wayland protocol limit",
                timeout.as_millis()
            ),
            Self::InputIdleUnsupported {
                listener_id,
                advertised_version,
            } => write!(
                formatter,
                "listener {} sets `ignore_inhibit = true`, but the compositor advertises ext_idle_notifier_v1 version {advertised_version}; version 2 is required",
                listener_id.index()
            ),
            Self::Flush(source) => {
                write!(
                    formatter,
                    "failed to send idle notification requests: {source}"
                )
            }
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
            Self::MissingGlobal(_)
            | Self::TimeoutTooLarge { .. }
            | Self::IdleHintTimeoutTooLarge(_)
            | Self::InputIdleUnsupported { .. } => None,
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct IdleWaylandState {
    events: Vec<IdleEvent>,
    configuration_generation: u64,
    inhibition_generation: u64,
    notifier: Option<ExtIdleNotifierV1>,
    seat: Option<wl_seat::WlSeat>,
    queue_handle: Option<QueueHandle<Self>>,
    requests: Vec<IdleNotificationRequest>,
    notifications: Vec<ActiveNotification>,
}

impl IdleWaylandState {
    fn new(
        notifier: ExtIdleNotifierV1,
        seat: wl_seat::WlSeat,
        queue_handle: QueueHandle<Self>,
        requests: Vec<IdleNotificationRequest>,
        notifications: Vec<ActiveNotification>,
    ) -> Self {
        Self {
            events: Vec::new(),
            configuration_generation: 0,
            inhibition_generation: 0,
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

    fn accepts_generation(&self, data: &IdleNotificationData) -> bool {
        // destroyed proxies can still leave queued events behind
        data.configuration_generation == self.configuration_generation
            && (data.inhibit_policy == InhibitPolicy::Ignore
                || data.inhibition_generation == self.inhibition_generation)
    }

    pub(crate) fn reset_notifications(&mut self) -> Result<(), IdleWaylandError> {
        self.configuration_generation = self.configuration_generation.wrapping_add(1);
        self.inhibition_generation = self.inhibition_generation.wrapping_add(1);
        self.events.clear();
        for notification in self.notifications.drain(..) {
            notification.proxy.destroy();
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
            self.configuration_generation,
            self.inhibition_generation,
        )?;

        Ok(())
    }

    pub(crate) fn pause_notifications(&mut self) {
        self.configuration_generation = self.configuration_generation.wrapping_add(1);
        self.inhibition_generation = self.inhibition_generation.wrapping_add(1);
        self.events.clear();
        for notification in self.notifications.drain(..) {
            notification.proxy.destroy();
        }
    }

    pub(crate) fn reset_inhibited_notifications(&mut self) -> Result<(), IdleWaylandError> {
        // listeners that ignore inhibitors keep their current notifications
        self.inhibition_generation = self.inhibition_generation.wrapping_add(1);
        self.events
            .retain(|event| event_ignores_inhibitors(&self.requests, event));

        let mut ignored = Vec::new();
        for notification in self.notifications.drain(..) {
            if notification.inhibit_policy == InhibitPolicy::Respect {
                notification.proxy.destroy();
            } else {
                ignored.push(notification);
            }
        }
        self.notifications = ignored;

        let (Some(notifier), Some(seat), Some(queue_handle)) = (
            self.notifier.as_ref(),
            self.seat.as_ref(),
            self.queue_handle.as_ref(),
        ) else {
            return Ok(());
        };
        let respecting = self
            .requests
            .iter()
            .filter(|request| request.inhibit_policy == InhibitPolicy::Respect);
        self.notifications.extend(create_notifications(
            notifier,
            seat,
            queue_handle,
            respecting,
            self.configuration_generation,
            self.inhibition_generation,
        )?);

        Ok(())
    }

    pub(crate) fn reconfigure_notifications(
        &mut self,
        listeners: &[TimeoutRequest],
        idle_hint_timeout: Option<Duration>,
    ) -> Result<(), IdleWaylandError> {
        let requests = notification_requests(listeners, idle_hint_timeout);
        validate_requests(&requests, self.notifier.as_ref().map(Proxy::version))?;

        self.requests = requests;
        self.reset_notifications()
    }

    pub(crate) fn validate_notifications(
        &self,
        listeners: &[TimeoutRequest],
        idle_hint_timeout: Option<Duration>,
    ) -> Result<(), IdleWaylandError> {
        let requests = notification_requests(listeners, idle_hint_timeout);
        validate_requests(&requests, self.notifier.as_ref().map(Proxy::version))
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

fn create_notifications<'a>(
    notifier: &ExtIdleNotifierV1,
    seat: &wl_seat::WlSeat,
    queue_handle: &QueueHandle<IdleWaylandState>,
    requests: impl IntoIterator<Item = &'a IdleNotificationRequest>,
    configuration_generation: u64,
    inhibition_generation: u64,
) -> Result<Vec<ActiveNotification>, IdleWaylandError> {
    requests
        .into_iter()
        .map(|request| {
            let timeout = timeout_millis(request)?;
            let data = IdleNotificationData {
                target: request.target,
                inhibit_policy: request.inhibit_policy,
                configuration_generation,
                inhibition_generation,
            };
            let proxy = match request.inhibit_policy {
                InhibitPolicy::Respect => {
                    notifier.get_idle_notification(timeout, seat, queue_handle, data)
                }
                InhibitPolicy::Ignore => {
                    ensure_input_idle_supported(request, notifier.version())?;
                    notifier.get_input_idle_notification(timeout, seat, queue_handle, data)
                }
            };
            Ok(ActiveNotification {
                proxy,
                inhibit_policy: request.inhibit_policy,
            })
        })
        .collect()
}

fn validate_requests(
    requests: &[IdleNotificationRequest],
    notifier_version: Option<u32>,
) -> Result<(), IdleWaylandError> {
    for request in requests {
        timeout_millis(request)?;
        if let Some(version) = notifier_version {
            ensure_input_idle_supported(request, version)?;
        }
    }
    Ok(())
}

fn ensure_input_idle_supported(
    request: &IdleNotificationRequest,
    advertised_version: u32,
) -> Result<(), IdleWaylandError> {
    if request.inhibit_policy != InhibitPolicy::Ignore || advertised_version >= 2 {
        return Ok(());
    }

    let IdleNotificationTarget::Listener(listener_id) = request.target else {
        return Ok(());
    };
    Err(IdleWaylandError::InputIdleUnsupported {
        listener_id,
        advertised_version,
    })
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
            inhibit_policy: request.inhibit_policy,
        })
        .collect::<Vec<_>>();

    if let Some(timeout) = idle_hint_timeout {
        requests.push(IdleNotificationRequest {
            target: IdleNotificationTarget::IdleHint,
            timeout,
            inhibit_policy: InhibitPolicy::Respect,
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
    inhibit_policy: InhibitPolicy,
}

#[derive(Debug)]
struct ActiveNotification {
    proxy: ExtIdleNotificationV1,
    inhibit_policy: InhibitPolicy,
}

fn event_ignores_inhibitors(requests: &[IdleNotificationRequest], event: &IdleEvent) -> bool {
    let listener_id = match event {
        IdleEvent::Idled(listener_id) | IdleEvent::Resumed(listener_id) => *listener_id,
        IdleEvent::IdleHintIdled | IdleEvent::IdleHintResumed => return false,
    };

    requests.iter().any(|request| {
        request.target == IdleNotificationTarget::Listener(listener_id)
            && request.inhibit_policy == InhibitPolicy::Ignore
    })
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
    inhibit_policy: InhibitPolicy,
    configuration_generation: u64,
    inhibition_generation: u64,
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
        if !state.accepts_generation(data) {
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
        IdleWaylandState, ensure_input_idle_supported, notification_requests, timeout_millis,
        validate_requests,
    };
    use crate::listener::{InhibitPolicy, ListenerId, TimeoutRequest};

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
            inhibit_policy: InhibitPolicy::Respect,
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
                    inhibit_policy: InhibitPolicy::Respect,
                },
            ]
        );
    }

    #[test]
    fn reports_idle_hint_timeout_overflow_separately() {
        let request = IdleNotificationRequest {
            target: IdleNotificationTarget::IdleHint,
            timeout: Duration::from_millis(u64::from(u32::MAX) + 1),
            inhibit_policy: InhibitPolicy::Respect,
        };

        assert!(matches!(
            timeout_millis(&request),
            Err(IdleWaylandError::IdleHintTimeoutTooLarge(timeout)) if timeout == request.timeout
        ));
    }

    #[test]
    fn failed_reconfiguration_preserves_existing_requests() {
        let existing = notification_request(0, 60);
        let mut state = IdleWaylandState {
            requests: vec![existing],
            ..IdleWaylandState::default()
        };
        let oversized = TimeoutRequest {
            listener_id: ListenerId::from_index(1),
            timeout: Duration::from_millis(u64::from(u32::MAX) + 1),
            inhibit_policy: InhibitPolicy::Respect,
        };

        assert!(matches!(
            state.reconfigure_notifications(&[oversized], None),
            Err(IdleWaylandError::TimeoutTooLarge { .. })
        ));
        assert_eq!(state.requests, vec![existing]);
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
            configuration_generation: 3,
            inhibition_generation: 5,
            ..IdleWaylandState::default()
        };

        assert!(!state.accepts_generation(&notification_data(2, 5, InhibitPolicy::Respect)));
        assert!(!state.accepts_generation(&notification_data(3, 4, InhibitPolicy::Respect)));
        assert!(state.accepts_generation(&notification_data(3, 5, InhibitPolicy::Respect)));
        assert!(state.accepts_generation(&notification_data(3, 4, InhibitPolicy::Ignore)));
    }

    #[test]
    fn input_idle_requires_protocol_version_two() {
        let request = IdleNotificationRequest {
            target: IdleNotificationTarget::Listener(ListenerId::from_index(2)),
            timeout: Duration::from_secs(60),
            inhibit_policy: InhibitPolicy::Ignore,
        };

        assert!(matches!(
            ensure_input_idle_supported(&request, 1),
            Err(IdleWaylandError::InputIdleUnsupported {
                listener_id,
                advertised_version: 1,
            }) if listener_id == ListenerId::from_index(2)
        ));
        assert!(ensure_input_idle_supported(&request, 2).is_ok());
    }

    #[test]
    fn validates_candidate_against_bound_notifier_version() {
        let requests = notification_requests(
            &[
                request(0, 60),
                request_with_policy(1, 120, InhibitPolicy::Ignore),
            ],
            None,
        );

        assert!(matches!(
            validate_requests(&requests, Some(1)),
            Err(IdleWaylandError::InputIdleUnsupported {
                listener_id,
                advertised_version: 1,
            }) if listener_id == ListenerId::from_index(1)
        ));
        assert!(validate_requests(&requests, Some(2)).is_ok());
    }

    #[test]
    fn inhibitor_resets_keep_only_opted_out_events() {
        let requests = notification_requests(
            &[
                request_with_policy(0, 60, InhibitPolicy::Respect),
                request_with_policy(1, 60, InhibitPolicy::Ignore),
            ],
            Some(Duration::from_secs(300)),
        );
        let mut state = IdleWaylandState {
            requests,
            events: vec![
                IdleEvent::Idled(ListenerId::from_index(0)),
                IdleEvent::Idled(ListenerId::from_index(1)),
                IdleEvent::IdleHintIdled,
            ],
            ..IdleWaylandState::default()
        };

        assert!(state.reset_inhibited_notifications().is_ok());
        assert_eq!(
            state.take_events(),
            vec![IdleEvent::Idled(ListenerId::from_index(1))]
        );
    }

    #[test]
    fn pausing_discards_pending_events_and_rejects_old_callbacks() {
        let listener_id = ListenerId::from_index(0);
        let mut state = IdleWaylandState {
            events: vec![IdleEvent::Idled(listener_id)],
            configuration_generation: 2,
            inhibition_generation: 4,
            ..IdleWaylandState::default()
        };

        state.pause_notifications();

        assert!(state.take_events().is_empty());
        assert!(!state.accepts_generation(&notification_data(2, 4, InhibitPolicy::Respect)));
    }

    fn request(listener_index: usize, timeout_seconds: u64) -> TimeoutRequest {
        TimeoutRequest {
            listener_id: ListenerId::from_index(listener_index),
            timeout: Duration::from_secs(timeout_seconds),
            inhibit_policy: InhibitPolicy::Respect,
        }
    }

    fn request_with_policy(
        listener_index: usize,
        timeout_seconds: u64,
        inhibit_policy: InhibitPolicy,
    ) -> TimeoutRequest {
        TimeoutRequest {
            listener_id: ListenerId::from_index(listener_index),
            timeout: Duration::from_secs(timeout_seconds),
            inhibit_policy,
        }
    }

    fn notification_request(
        listener_index: usize,
        timeout_seconds: u64,
    ) -> IdleNotificationRequest {
        IdleNotificationRequest {
            target: IdleNotificationTarget::Listener(ListenerId::from_index(listener_index)),
            timeout: Duration::from_secs(timeout_seconds),
            inhibit_policy: InhibitPolicy::Respect,
        }
    }

    fn notification_data(
        configuration_generation: u64,
        inhibition_generation: u64,
        inhibit_policy: InhibitPolicy,
    ) -> super::IdleNotificationData {
        super::IdleNotificationData {
            target: IdleNotificationTarget::Listener(ListenerId::from_index(0)),
            inhibit_policy,
            configuration_generation,
            inhibition_generation,
        }
    }
}
