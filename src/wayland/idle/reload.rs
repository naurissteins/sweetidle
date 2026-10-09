use std::time::Duration;

use wayland_client::Proxy;

use super::{
    IdleEvent, IdleNotificationTarget, IdleWaylandError, IdleWaylandState, create_notifications,
    notification_requests, validate_requests,
};
use crate::listener::{ListenerId, TimeoutRequest};

impl IdleWaylandState {
    pub(crate) fn reconfigure_notifications(
        &mut self,
        listeners: &[TimeoutRequest],
        idle_hint_timeout: Option<Duration>,
        retained_ids: &[(ListenerId, ListenerId)],
    ) -> Result<(), IdleWaylandError> {
        let requests = notification_requests(listeners, idle_hint_timeout);
        validate_requests(&requests, self.notifier.as_ref().map(Proxy::version))?;

        self.configuration_generation = self.configuration_generation.wrapping_add(1);
        self.inhibition_generation = self.inhibition_generation.wrapping_add(1);
        self.events = self
            .events
            .drain(..)
            .filter_map(|event| remap_event(event, retained_ids))
            .collect();
        let mut retained = Vec::new();
        for mut notification in self.notifications.drain(..) {
            if let Some(target) = remap_target(notification.target, retained_ids) {
                notification.target = target;
                retained.push(notification);
            } else {
                notification.proxy.destroy();
            }
        }
        self.notifications = retained;
        self.requests = requests;
        self.create_reloaded_notifications(retained_ids)
    }

    fn create_reloaded_notifications(
        &mut self,
        retained_ids: &[(ListenerId, ListenerId)],
    ) -> Result<(), IdleWaylandError> {
        let (Some(notifier), Some(seat), Some(queue_handle)) = (
            self.notifier.as_ref(),
            self.seat.as_ref(),
            self.queue_handle.as_ref(),
        ) else {
            return Ok(());
        };
        let fresh = self.requests.iter().filter(|request| {
            !retained_ids
                .iter()
                .any(|(_, next)| request.target == IdleNotificationTarget::Listener(*next))
        });
        self.notifications.extend(create_notifications(
            notifier,
            seat,
            queue_handle,
            fresh,
            self.configuration_generation,
            self.inhibition_generation,
        )?);
        Ok(())
    }
}

fn remap_target(
    target: IdleNotificationTarget,
    retained_ids: &[(ListenerId, ListenerId)],
) -> Option<IdleNotificationTarget> {
    let IdleNotificationTarget::Listener(old) = target else {
        return None;
    };
    retained_ids.iter().find_map(|(previous, next)| {
        (*previous == old).then_some(IdleNotificationTarget::Listener(*next))
    })
}

fn remap_event(event: IdleEvent, retained_ids: &[(ListenerId, ListenerId)]) -> Option<IdleEvent> {
    let (IdleEvent::Idled(old) | IdleEvent::Resumed(old)) = event else {
        return None;
    };
    let IdleNotificationTarget::Listener(next) =
        remap_target(IdleNotificationTarget::Listener(old), retained_ids)?
    else {
        return None;
    };
    Some(match event {
        IdleEvent::Idled(_) => IdleEvent::Idled(next),
        _ => IdleEvent::Resumed(next),
    })
}
