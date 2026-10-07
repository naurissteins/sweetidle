use std::collections::HashMap;

use crate::session::{DbusInhibitEvent, InhibitorInfo, SystemdIdleInhibitor};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Transition {
    Started,
    Ended,
    Unchanged,
}

#[derive(Default)]
pub(super) struct InhibitionState {
    dbus_count: usize,
    systemd_count: usize,
    dbus_details: HashMap<u32, InhibitorInfo>,
    systemd_details: Vec<SystemdIdleInhibitor>,
}

impl InhibitionState {
    pub(super) fn apply_dbus(&mut self, event: &DbusInhibitEvent) -> Transition {
        let was_inhibited = self.inhibited();
        let active_count = match event {
            DbusInhibitEvent::Added {
                inhibitor,
                active_count,
            } => {
                self.dbus_details
                    .insert(inhibitor.cookie, inhibitor.clone());
                *active_count
            }
            DbusInhibitEvent::Removed {
                inhibitor,
                active_count,
            } => {
                self.dbus_details.remove(&inhibitor.cookie);
                *active_count
            }
            DbusInhibitEvent::OwnerDisconnected {
                owner,
                active_count,
                ..
            } => {
                self.dbus_details
                    .retain(|_, inhibitor| inhibitor.owner != *owner);
                *active_count
            }
            // runtime routing reports the error without changing accounting
            DbusInhibitEvent::Failed(_) => return Transition::Unchanged,
        };
        // worker counts are authoritative even if diagnostic metadata is incomplete
        self.dbus_count = active_count;
        self.transition(was_inhibited)
    }

    pub(super) fn apply_systemd(&mut self, inhibitors: Vec<SystemdIdleInhibitor>) -> Transition {
        let was_inhibited = self.inhibited();
        self.systemd_count = inhibitors.len();
        self.systemd_details = inhibitors;
        self.transition(was_inhibited)
    }

    pub(super) fn inhibited(&self) -> bool {
        self.dbus_count > 0 || self.systemd_count > 0
    }

    pub(super) fn dbus_count(&self) -> usize {
        self.dbus_count
    }

    pub(super) fn systemd_count(&self) -> usize {
        self.systemd_count
    }

    pub(super) fn dbus_details(&self) -> impl Iterator<Item = &InhibitorInfo> {
        self.dbus_details.values()
    }

    pub(super) fn systemd_details(&self) -> &[SystemdIdleInhibitor] {
        &self.systemd_details
    }

    fn transition(&self, was_inhibited: bool) -> Transition {
        match (was_inhibited, self.inhibited()) {
            (false, true) => Transition::Started,
            (true, false) => Transition::Ended,
            _ => Transition::Unchanged,
        }
    }
}

#[cfg(test)]
mod tests;
