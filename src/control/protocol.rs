use std::{fmt, sync::mpsc::SyncSender};

use serde::Serialize;

use crate::{
    listener::{InhibitPolicy, ListenerPhase, ListenerStatus},
    session::SystemdIdleInhibitor,
};

pub(crate) enum ControlEvent {
    Status {
        request: ControlRequest,
        format: StatusFormat,
    },
    Pause(ControlRequest),
    Resume(ControlRequest),
    Reload(ControlRequest),
    Failed(super::ControlError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StatusFormat {
    Text,
    Json,
}

pub(crate) struct ControlRequest {
    reply: SyncSender<String>,
}

impl ControlRequest {
    pub(crate) fn new(reply: SyncSender<String>) -> Self {
        Self { reply }
    }

    pub(crate) fn respond(self, response: String) {
        // the worker may have timed out and dropped its reply receiver
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

impl StatusSnapshot {
    pub(crate) fn render(&self, format: StatusFormat) -> Result<String, serde_json::Error> {
        match format {
            StatusFormat::Text => Ok(self.to_string()),
            StatusFormat::Json => self.render_json(),
        }
    }

    fn render_json(&self) -> Result<String, serde_json::Error> {
        let listeners = self
            .listeners
            .iter()
            .map(|listener| JsonListenerStatus {
                id: listener.listener_id.index(),
                state: listener_phase_label(listener.phase),
                timeout_seconds: listener.timeout.as_secs(),
                ignore_inhibit: listener.inhibit_policy == InhibitPolicy::Ignore,
            })
            .collect();
        let dbus = self
            .dbus_inhibitors
            .iter()
            .map(|inhibitor| JsonDbusInhibitor {
                cookie: inhibitor.cookie,
                application: &inhibitor.application,
                reason: &inhibitor.reason,
            })
            .collect();
        let systemd = self
            .systemd_inhibitors
            .iter()
            .map(|inhibitor| JsonSystemdInhibitor {
                pid: inhibitor.pid,
                uid: inhibitor.uid,
                who: &inhibitor.who,
                why: &inhibitor.why,
            })
            .collect();
        // pin the wire format to a version before serializing it
        let status = JsonStatus {
            schema_version: 1,
            paused: self.paused,
            idle_hint: self.idle_hint,
            listeners,
            inhibitors: JsonInhibitors {
                active: !self.dbus_inhibitors.is_empty() || !self.systemd_inhibitors.is_empty(),
                dbus,
                systemd,
            },
        };
        let mut response = serde_json::to_string(&status)?;
        response.push('\n');
        Ok(response)
    }
}

#[derive(Serialize)]
struct JsonStatus<'a> {
    schema_version: u8,
    paused: bool,
    idle_hint: Option<bool>,
    listeners: Vec<JsonListenerStatus>,
    inhibitors: JsonInhibitors<'a>,
}

#[derive(Serialize)]
struct JsonListenerStatus {
    id: usize,
    state: &'static str,
    timeout_seconds: u64,
    ignore_inhibit: bool,
}

#[derive(Serialize)]
struct JsonInhibitors<'a> {
    active: bool,
    dbus: Vec<JsonDbusInhibitor<'a>>,
    systemd: Vec<JsonSystemdInhibitor<'a>>,
}

#[derive(Serialize)]
struct JsonDbusInhibitor<'a> {
    cookie: u32,
    application: &'a str,
    reason: &'a str,
}

#[derive(Serialize)]
struct JsonSystemdInhibitor<'a> {
    pid: u32,
    uid: u32,
    who: &'a str,
    why: &'a str,
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
                let phase = listener_phase_label(listener.phase);
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

fn listener_phase_label(phase: ListenerPhase) -> &'static str {
    match phase {
        ListenerPhase::Armed => "armed",
        ListenerPhase::TimedOut => "timed-out",
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{DbusInhibitorStatus, StatusFormat, StatusSnapshot};
    use crate::{
        listener::{InhibitPolicy, ListenerId, ListenerPhase, ListenerStatus},
        session::SystemdIdleInhibitor,
    };

    #[test]
    fn formats_each_listener_phase_and_policy_in_both_status_formats() {
        for (phase, policy, state_label, policy_label, ignore_inhibit) in [
            (
                ListenerPhase::Armed,
                InhibitPolicy::Respect,
                "armed",
                "respected",
                false,
            ),
            (
                ListenerPhase::Armed,
                InhibitPolicy::Ignore,
                "armed",
                "ignored",
                true,
            ),
            (
                ListenerPhase::TimedOut,
                InhibitPolicy::Respect,
                "timed-out",
                "respected",
                false,
            ),
            (
                ListenerPhase::TimedOut,
                InhibitPolicy::Ignore,
                "timed-out",
                "ignored",
                true,
            ),
        ] {
            let status = StatusSnapshot {
                paused: false,
                listeners: vec![ListenerStatus {
                    listener_id: ListenerId::from_index(0),
                    timeout: Duration::from_secs(300),
                    inhibit_policy: policy,
                    phase,
                }],
                dbus_inhibitors: Vec::new(),
                systemd_inhibitors: Vec::new(),
                idle_hint: None,
            };

            assert_eq!(
                status.to_string(),
                format!(
                    "sweetsidle: running\ntracked inhibitors active: false\nidle hint: disabled\nlisteners:\n  - id=0 state={state_label} timeout=300s inhibitors={policy_label}\ntracked inhibitors: none\n"
                )
            );
            let rendered = status
                .render(StatusFormat::Json)
                .unwrap_or_else(|error| panic!("JSON status failed: {error}"));
            let value: serde_json::Value = serde_json::from_str(&rendered)
                .unwrap_or_else(|error| panic!("JSON status was invalid: {error}"));
            assert_eq!(
                value["listeners"],
                serde_json::json!([{
                    "id": 0,
                    "state": state_label,
                    "timeout_seconds": 300,
                    "ignore_inhibit": ignore_inhibit,
                }])
            );
        }
    }

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

    #[test]
    fn formats_machine_readable_status_as_json() {
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

        let rendered = status
            .render(StatusFormat::Json)
            .unwrap_or_else(|error| panic!("JSON status failed: {error}"));
        let value: serde_json::Value = serde_json::from_str(&rendered)
            .unwrap_or_else(|error| panic!("JSON status was invalid: {error}"));

        assert_eq!(
            value,
            serde_json::json!({
                "schema_version": 1,
                "paused": true,
                "idle_hint": false,
                "listeners": [{
                    "id": 0,
                    "state": "timed-out",
                    "timeout_seconds": 300,
                    "ignore_inhibit": true
                }],
                "inhibitors": {
                    "active": true,
                    "dbus": [{
                        "cookie": 7,
                        "application": "video player",
                        "reason": "playing\nmedia"
                    }],
                    "systemd": [{
                        "pid": 42,
                        "uid": 1000,
                        "who": "backup",
                        "why": "saving files"
                    }]
                }
            })
        );
        assert!(rendered.ends_with('\n'));
    }
}
