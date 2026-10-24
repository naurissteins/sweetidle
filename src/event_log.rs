use sweetidle::{
    ProcessError,
    config::Config,
    runtime::{InhibitorChange, RuntimeEvent},
};
use tracing::{debug, error, info, warn};

pub(super) fn init_logging(verbose: bool) -> Result<(), String> {
    let level = if verbose {
        tracing::Level::DEBUG
    } else {
        tracing::Level::INFO
    };

    tracing_subscriber::fmt()
        .with_max_level(level)
        .with_target(false)
        .without_time()
        .try_init()
        .map_err(|error| format!("failed to initialize logging: {error}"))
}

pub(super) enum RuntimeControl {
    Continue,
    Shutdown,
    Reload(Config),
}

pub(super) fn handle_runtime_event(event: RuntimeEvent) -> RuntimeControl {
    match event {
        RuntimeEvent::ShutdownRequested(signal) => {
            info!(signal = signal.name(), "shutdown requested");
            RuntimeControl::Shutdown
        }
        RuntimeEvent::ConfigReloadRequested => {
            info!("configuration reload requested");
            RuntimeControl::Continue
        }
        RuntimeEvent::ConfigReloadReady(config) => RuntimeControl::Reload(config),
        RuntimeEvent::ConfigReloadRejected(error) => {
            warn!(error = %error, "configuration reload rejected; keeping current configuration");
            RuntimeControl::Continue
        }
        RuntimeEvent::IdlePaused => {
            info!("idle detection paused");
            RuntimeControl::Continue
        }
        RuntimeEvent::IdleResumed => {
            info!("idle detection resumed");
            RuntimeControl::Continue
        }
        RuntimeEvent::PreparingForSleep => {
            info!("preparing for system sleep");
            RuntimeControl::Continue
        }
        RuntimeEvent::ResumedFromSleep => {
            info!("system resumed from sleep");
            RuntimeControl::Continue
        }
        RuntimeEvent::LockRequested => {
            info!("session lock requested");
            RuntimeControl::Continue
        }
        RuntimeEvent::UnlockRequested => {
            info!("session unlock requested");
            RuntimeControl::Continue
        }
        RuntimeEvent::IdleHintChanged { idle } => {
            debug!(idle, "logind session idle hint changed");
            RuntimeControl::Continue
        }
        RuntimeEvent::DbusInhibitorChanged {
            change,
            cookie,
            application,
            reason,
            active_count,
        } => {
            let message = match change {
                InhibitorChange::Added => "D-Bus idle inhibitor added",
                InhibitorChange::Removed => "D-Bus idle inhibitor removed",
            };
            debug!(cookie, application, reason, active_count, "{message}");
            RuntimeControl::Continue
        }
        RuntimeEvent::DbusInhibitorOwnerDisconnected {
            owner,
            released_count,
            active_count,
        } => {
            debug!(
                owner,
                released_count, active_count, "released inhibitors for disconnected D-Bus client"
            );
            RuntimeControl::Continue
        }
        RuntimeEvent::SystemdIdleInhibitorsChanged {
            inhibitors,
            active_count,
        } => {
            debug!(active_count, "systemd idle inhibitors changed");
            for inhibitor in inhibitors {
                debug!(
                    who = inhibitor.who,
                    why = inhibitor.why,
                    uid = inhibitor.uid,
                    pid = inhibitor.pid,
                    "active systemd idle inhibitor"
                );
            }
            RuntimeControl::Continue
        }
        RuntimeEvent::IdleSuppressed {
            listener_id,
            active_dbus_inhibitors,
            active_systemd_inhibitors,
        } => {
            debug!(
                listener = listener_id.index(),
                active_dbus_inhibitors, active_systemd_inhibitors, "idle action suppressed"
            );
            RuntimeControl::Continue
        }
        RuntimeEvent::CommandStarted(start) => {
            info!(
                command_id = start.command_id.index(),
                context = ?start.context,
                process_id = start.process_id,
                "command started"
            );
            RuntimeControl::Continue
        }
        RuntimeEvent::CommandSkipped(decision) => {
            debug!(
                context = ?decision.context,
                reason = ?decision.reason,
                "command skipped"
            );
            RuntimeControl::Continue
        }
        RuntimeEvent::CommandFinished {
            command,
            completion,
        } => {
            if completion.status.success() {
                info!(
                    command_id = command.command_id.index(),
                    context = ?command.context,
                    exit_code = ?completion.status.code(),
                    "command finished"
                );
            } else {
                warn!(
                    command_id = command.command_id.index(),
                    context = ?command.context,
                    exit_code = ?completion.status.code(),
                    "command exited unsuccessfully"
                );
            }
            RuntimeControl::Continue
        }
        RuntimeEvent::CommandFailed { command, error } => {
            let source = match &error {
                ProcessError::Spawn { source, .. } | ProcessError::Wait { source, .. } => source,
            };
            error!(
                command_id = command.command_id.index(),
                context = ?command.context,
                error = %source,
                "command failed"
            );
            RuntimeControl::Continue
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::{self, Write},
        sync::{Arc, Mutex},
    };

    use sweetidle::{
        config::{Config, ConfigLoadError},
        runtime::ShutdownSignal,
    };

    use super::{InhibitorChange, RuntimeControl, RuntimeEvent, handle_runtime_event};

    #[test]
    fn shutdown_requests_stop_the_daemon() {
        for signal in [ShutdownSignal::Interrupt, ShutdownSignal::Terminate] {
            assert!(matches!(
                handle_runtime_event(RuntimeEvent::ShutdownRequested(signal)),
                RuntimeControl::Shutdown
            ));
        }
    }

    #[test]
    fn ready_reload_returns_the_candidate_intact() {
        let candidate = Config::parse("[idle_hint]\ntimeout = 91\n")
            .unwrap_or_else(|error| panic!("invalid test config: {error}"));

        let RuntimeControl::Reload(config) =
            handle_runtime_event(RuntimeEvent::ConfigReloadReady(candidate.clone()))
        else {
            panic!("reload candidate did not reach the daemon");
        };

        assert_eq!(config, candidate);
    }

    #[test]
    fn rejected_reload_keeps_the_daemon_running() {
        let error = ConfigLoadError::Read {
            path: "missing.toml".into(),
            source: io::Error::from(io::ErrorKind::NotFound),
        };

        assert!(matches!(
            handle_runtime_event(RuntimeEvent::ConfigReloadRejected(error)),
            RuntimeControl::Continue
        ));
    }

    struct LogBuffer(Arc<Mutex<Vec<u8>>>);

    impl Write for LogBuffer {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(|error| panic!("log buffer poisoned: {error}"))
                .write(bytes)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn dbus_inhibitor_logs_preserve_messages_and_fields() {
        let output = Arc::new(Mutex::new(Vec::new()));
        let writer = Arc::clone(&output);
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_ansi(false)
            .with_target(false)
            .without_time()
            .with_writer(move || LogBuffer(Arc::clone(&writer)))
            .finish();

        tracing::subscriber::with_default(subscriber, || {
            for event in [
                RuntimeEvent::DbusInhibitorChanged {
                    change: InhibitorChange::Added,
                    cookie: 7,
                    application: "test application".to_string(),
                    reason: "test reason".to_string(),
                    active_count: 1,
                },
                RuntimeEvent::DbusInhibitorChanged {
                    change: InhibitorChange::Removed,
                    cookie: 7,
                    application: "test application".to_string(),
                    reason: "test reason".to_string(),
                    active_count: 0,
                },
            ] {
                assert!(matches!(
                    handle_runtime_event(event),
                    RuntimeControl::Continue
                ));
            }
        });

        let output = output
            .lock()
            .unwrap_or_else(|error| panic!("log buffer poisoned: {error}"));
        assert_eq!(
            String::from_utf8_lossy(&output),
            concat!(
                "DEBUG D-Bus idle inhibitor added cookie=7 application=\"test application\" reason=\"test reason\" active_count=1\n",
                "DEBUG D-Bus idle inhibitor removed cookie=7 application=\"test application\" reason=\"test reason\" active_count=0\n",
            )
        );
    }
}
