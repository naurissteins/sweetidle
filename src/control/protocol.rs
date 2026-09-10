use std::{fmt, sync::mpsc::SyncSender};

use crate::{
    listener::{InhibitPolicy, ListenerPhase, ListenerStatus},
    session::SystemdIdleInhibitor,
};

pub(crate) enum ControlEvent {
    Status(ControlRequest),
    Pause(ControlRequest),
    Resume(ControlRequest),
    Failed(super::ControlError),
}

pub(crate) struct ControlRequest {
    reply: SyncSender<String>,
}

impl ControlRequest {
    pub(crate) fn new(reply: SyncSender<String>) -> Self {
        Self { reply }
    }

    pub(crate) fn respond(self, response: String) {
        let _ = self.reply.try_send(response);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DbusInhibitorStatus {
    pub(crate) cookie: u32,
    pub(crate) application: String,
    pub(crate) reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StatusSnapshot {
    pub(crate) paused: bool,
    pub(crate) listeners: Vec<ListenerStatus>,
    pub(crate) dbus_inhibitors: Vec<DbusInhibitorStatus>,
    pub(crate) systemd_inhibitors: Vec<SystemdIdleInhibitor>,
    pub(crate) idle_hint: Option<bool>,
}

impl fmt::Display for StatusSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            formatter,
            "sweetsidle: {}",
            if self.paused { "paused" } else { "running" }
        )?;
        writeln!(
            formatter,
            "tracked inhibitors active: {}",
            !self.dbus_inhibitors.is_empty() || !self.systemd_inhibitors.is_empty()
        )?;
        match self.idle_hint {
            Some(idle) => writeln!(
                formatter,
                "idle hint: {}",
                if idle { "idle" } else { "active" }
            )?,
            None => writeln!(formatter, "idle hint: disabled")?,
        }

        if self.listeners.is_empty() {
            writeln!(formatter, "listeners: none")?;
        } else {
            writeln!(formatter, "listeners:")?;
            for listener in &self.listeners {
                let phase = match listener.phase {
                    ListenerPhase::Armed => "armed",
                    ListenerPhase::TimedOut => "timed-out",
                };
                let inhibitors = match listener.inhibit_policy {
                    InhibitPolicy::Respect => "respected",
                    InhibitPolicy::Ignore => "ignored",
                };
                writeln!(
                    formatter,
                    "  - id={} state={phase} timeout={}s inhibitors={inhibitors}",
                    listener.listener_id.index(),
                    listener.timeout.as_secs()
                )?;
            }
        }

        if self.dbus_inhibitors.is_empty() && self.systemd_inhibitors.is_empty() {
            return writeln!(formatter, "tracked inhibitors: none");
        }

        writeln!(formatter, "tracked inhibitors:")?;
        for inhibitor in &self.dbus_inhibitors {
            writeln!(
                formatter,
                "  - source=dbus cookie={} application={:?} reason={:?}",
                inhibitor.cookie, inhibitor.application, inhibitor.reason
            )?;
        }
        for inhibitor in &self.systemd_inhibitors {
            writeln!(
                formatter,
                "  - source=systemd pid={} uid={} who={:?} why={:?}",
                inhibitor.pid, inhibitor.uid, inhibitor.who, inhibitor.why
            )?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{DbusInhibitorStatus, StatusSnapshot};
    use crate::{
        listener::{InhibitPolicy, ListenerId, ListenerPhase, ListenerStatus},
        session::SystemdIdleInhibitor,
    };

    #[test]
    fn formats_listener_and_inhibitor_details() {
        let status = StatusSnapshot {
            paused: true,
            listeners: vec![ListenerStatus {
                listener_id: ListenerId::from_index(0),
                timeout: Duration::from_secs(300),
                inhibit_policy: InhibitPolicy::Ignore,
                phase: ListenerPhase::TimedOut,
            }],
            dbus_inhibitors: vec![DbusInhibitorStatus {
                cookie: 7,
                application: "video player".to_string(),
                reason: "playing\nmedia".to_string(),
            }],
            systemd_inhibitors: vec![SystemdIdleInhibitor {
                who: "backup".to_string(),
                why: "saving files".to_string(),
                uid: 1000,
                pid: 42,
            }],
            idle_hint: Some(false),
        };

        assert_eq!(
            status.to_string(),
            concat!(
                "sweetsidle: paused\n",
                "tracked inhibitors active: true\n",
                "idle hint: active\n",
                "listeners:\n",
                "  - id=0 state=timed-out timeout=300s inhibitors=ignored\n",
                "tracked inhibitors:\n",
                "  - source=dbus cookie=7 application=\"video player\" reason=\"playing\\nmedia\"\n",
                "  - source=systemd pid=42 uid=1000 who=\"backup\" why=\"saving files\"\n",
            )
        );
    }
}
