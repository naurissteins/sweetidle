use std::{
    error::Error,
    fs::{self, File},
    os::unix::process::CommandExt,
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        mpsc::{self, Receiver, Sender, TryRecvError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use rustix::process::{Pid, Signal, kill_process, kill_process_group};
use tempfile::TempDir;
use wayland_protocols::ext::idle_notify::v1::server::{
    ext_idle_notification_v1::{self, ExtIdleNotificationV1},
    ext_idle_notifier_v1::{self, ExtIdleNotifierV1},
};
use wayland_server::{
    Client, DataInit, Dispatch, Display, DisplayHandle, GlobalDispatch, ListeningSocket, New,
    Resource, backend::ClientId, protocol::wl_seat,
};

// each case runs the daemon against a private compositor and runtime dir
const WAIT_LIMIT: Duration = Duration::from_secs(5);
// some launchers leave SIGCHLD ignored, which survives exec
const IGNORE_SIGCHLD: &str = "trap '' CHLD; exec \"$0\" \"$@\"";
const SIGCHLD_MASK: u64 = 1 << (17 - 1);

#[test]
#[ignore = "runs the real daemon against a private Wayland compositor"]
fn reload_retains_timed_out_notification_and_runs_resume_once() -> Result<(), Box<dyn Error>> {
    let mut harness = ReloadHarness::start_with_actions("timeout", Some("resume"), 1)?;
    harness.wait_for_markers(1)?;
    let source = fs::read_to_string(&harness.config_path)?;
    fs::write(
        &harness.config_path,
        source.replace("[general]", "[general]\nwait_before_sleep = false"),
    )?;
    harness.signal_reload()?;
    harness.wait_for_log("configuration reloaded")?;

    assert!(harness.status()?.contains("state=timed-out"));
    assert_eq!(harness.compositor.created()?, 1);
    assert_eq!(harness.compositor.emit(1, MockEvent::Idle)?, 1);
    assert!(harness.status()?.contains("state=timed-out"));
    assert_eq!(harness.markers()?, ["timeout"]);
    harness.resume_listener_cycle()?;
    harness.wait_for_markers(2)?;
    assert_eq!(harness.compositor.emit(1, MockEvent::Resume)?, 1);
    assert!(harness.status()?.contains("state=armed"));
    assert_eq!(harness.markers()?, ["timeout", "resume"]);
    harness.stop()?;
    Ok(())
}

#[test]
#[ignore = "runs the real daemon against a private Wayland compositor"]
fn reordered_retained_proxy_resumes_its_new_listener_id() -> Result<(), Box<dyn Error>> {
    let mut harness = ReloadHarness::start_with_actions("timeout", Some("resume"), 2)?;
    harness.wait_for_markers(1)?;
    let original = fs::read_to_string(&harness.config_path)?;
    let second = "[[listener]]\ntimeout = 7200\nignore_inhibit = true\n";
    fs::write(&harness.config_path, format!("{original}\n{second}"))?;
    harness.signal_reload()?;
    harness.wait_for_log("configuration reloaded")?;
    let deadline = Instant::now() + WAIT_LIMIT;
    loop {
        let status: serde_json::Value = serde_json::from_str(&harness.status_json()?)?;
        if status["listeners"].as_array().is_some_and(|listeners| {
            listeners.len() == 2
                && listeners
                    .iter()
                    .all(|listener| listener["state"] == "timed-out")
        }) {
            break;
        }
        if Instant::now() >= deadline {
            return Err("second listener did not time out".into());
        }
        thread::sleep(Duration::from_millis(10));
    }
    let (general, first) = original
        .split_once("[[listener]]")
        .ok_or("missing listener")?;
    fs::write(
        &harness.config_path,
        format!("{general}{second}\n[[listener]]{first}"),
    )?;
    harness.signal_reload()?;
    harness.wait_for_log_count("configuration reloaded", 2)?;

    assert_eq!(harness.compositor.created()?, 2);
    assert_eq!(harness.compositor.emit(2, MockEvent::ResumeOne(0))?, 1);
    harness.wait_for_markers(2)?;
    assert_eq!(harness.markers()?, ["timeout", "resume"]);
    let status: serde_json::Value = serde_json::from_str(&harness.status_json()?)?;
    assert_eq!(status["listeners"][0]["state"], "timed-out");
    assert_eq!(status["listeners"][1]["state"], "armed");
    harness.stop()?;
    Ok(())
}

#[test]
#[ignore = "runs the real daemon against a private Wayland compositor"]
fn timed_out_listener_change_is_rejected_and_can_be_retried_after_resume()
-> Result<(), Box<dyn Error>> {
    let mut harness = ReloadHarness::start_with_actions("timeout", Some("resume"), 2)?;
    harness.wait_for_markers(1)?;
    let before = harness.status_json()?;
    harness.write_config("changed")?;
    harness.signal_reload()?;
    harness.wait_for_log("cannot change or remove timed-out listener 0")?;

    assert_eq!(harness.status_json()?, before);
    assert_eq!(harness.compositor.created()?, 1);
    harness.resume_listener_cycle()?;
    harness.wait_for_markers(2)?;
    harness.signal_reload()?;
    harness.wait_for_markers(3)?;
    assert_eq!(harness.markers()?, ["timeout", "resume", "changed"]);
    harness.stop()?;
    Ok(())
}

#[test]
#[ignore = "runs the real daemon against a private Wayland compositor"]
fn timed_out_listener_removal_is_rejected_until_resume() -> Result<(), Box<dyn Error>> {
    let mut harness = ReloadHarness::start_with_actions("timeout", Some("resume"), 2)?;
    harness.wait_for_markers(1)?;
    fs::write(
        &harness.config_path,
        "[general]\nignore_dbus_inhibit = true\nignore_systemd_inhibit = true\n",
    )?;
    harness.signal_reload()?;
    harness.wait_for_log("cannot change or remove timed-out listener 0")?;

    assert!(harness.status()?.contains("state=timed-out"));
    assert_eq!(harness.compositor.created()?, 1);
    harness.resume_listener_cycle()?;
    harness.wait_for_markers(2)?;
    harness.signal_reload()?;
    harness.wait_for_log("configuration reloaded")?;
    assert!(harness.status()?.contains("listeners: none"));
    assert_eq!(harness.markers()?, ["timeout", "resume"]);
    harness.stop()?;
    Ok(())
}

#[test]
#[ignore = "runs the real daemon against a private Wayland compositor"]
fn sighup_applies_valid_config_without_restarting_the_daemon() -> Result<(), Box<dyn Error>> {
    let mut harness = ReloadHarness::start("initial")?;
    harness.wait_for_markers(1)?;

    harness.resume_listener_cycle()?;
    harness.write_config("reloaded")?;
    harness.signal_reload()?;
    harness.wait_for_markers(2)?;

    assert_eq!(harness.markers()?, ["initial", "reloaded"]);
    harness.stop()?;
    Ok(())
}

#[test]
#[ignore = "runs the real daemon against a private Wayland compositor"]
fn control_reload_applies_valid_config_without_restarting_the_daemon() -> Result<(), Box<dyn Error>>
{
    let mut harness = ReloadHarness::start("initial")?;
    harness.wait_for_markers(1)?;

    harness.resume_listener_cycle()?;
    harness.write_config("reloaded")?;
    assert_eq!(harness.control("reload")?, "sweetidle: reload requested\n");
    harness.wait_for_markers(2)?;

    assert_eq!(harness.markers()?, ["initial", "reloaded"]);
    harness.stop()?;
    Ok(())
}

#[test]
#[ignore = "runs the real daemon against a private Wayland compositor"]
fn invalid_sighup_keeps_the_last_valid_config_active() -> Result<(), Box<dyn Error>> {
    let mut harness = ReloadHarness::start("active")?;
    harness.wait_for_markers(1)?;

    fs::write(&harness.config_path, "[[listener]]\ntimeout = 0\n")?;
    harness.signal_reload()?;
    harness.wait_for_log("configuration reload rejected")?;
    assert_eq!(harness.compositor.pulse(1)?, 1);
    harness.wait_for_markers(2)?;

    assert_eq!(harness.markers()?, ["active", "active"]);
    harness.stop()?;
    Ok(())
}

#[test]
#[ignore = "runs the real daemon against a private Wayland compositor"]
fn oversized_timeout_sighup_keeps_the_last_valid_config_active() -> Result<(), Box<dyn Error>> {
    let mut harness = ReloadHarness::start("active")?;
    harness.wait_for_markers(1)?;

    fs::write(&harness.config_path, "[[listener]]\ntimeout = 5000000\n")?;
    harness.signal_reload()?;
    harness.wait_for_log("configuration reload rejected")?;
    assert_eq!(harness.compositor.pulse(1)?, 1);
    harness.wait_for_markers(2)?;

    assert_eq!(harness.markers()?, ["active", "active"]);
    harness.stop()?;
    Ok(())
}

#[test]
#[ignore = "runs the real daemon against a private Wayland compositor"]
fn oversized_config_sighup_keeps_the_last_valid_config_active() -> Result<(), Box<dyn Error>> {
    let mut harness = ReloadHarness::start("active")?;
    harness.wait_for_markers(1)?;

    fs::OpenOptions::new()
        .write(true)
        .open(&harness.config_path)?
        .set_len(1024 * 1024 + 1)?;
    harness.signal_reload()?;
    harness.wait_for_log("1048576-byte limit (1 MiB)")?;
    assert_eq!(harness.compositor.pulse(1)?, 1);
    harness.wait_for_markers(2)?;

    assert_eq!(harness.markers()?, ["active", "active"]);
    harness.resume_listener_cycle()?;
    harness.write_config("recovered")?;
    harness.signal_reload()?;
    harness.wait_for_markers(3)?;
    assert_eq!(harness.markers()?, ["active", "active", "recovered"]);
    harness.stop()?;
    Ok(())
}

#[test]
#[ignore = "runs the real daemon against a private Wayland compositor"]
fn unsupported_input_idle_sighup_keeps_the_last_valid_config_active() -> Result<(), Box<dyn Error>>
{
    let mut harness = ReloadHarness::start_with_notifier_version("active", 1)?;
    harness.wait_for_markers(1)?;

    harness.write_config("rejected")?;
    harness.signal_reload()?;
    harness.wait_for_log("configuration reload rejected")?;
    assert_eq!(harness.compositor.pulse(1)?, 1);
    harness.wait_for_markers(2)?;

    assert_eq!(harness.markers()?, ["active", "active"]);
    let status: serde_json::Value = serde_json::from_str(&harness.status_json()?)?;
    assert_eq!(status["listeners"][0]["ignore_inhibit"], false);
    harness.stop()?;
    Ok(())
}

#[test]
#[ignore = "runs the real daemon against a private Wayland compositor"]
fn status_reports_live_listener_state() -> Result<(), Box<dyn Error>> {
    let mut harness = ReloadHarness::start("status")?;
    harness.wait_for_markers(1)?;

    let status = harness.status()?;

    assert!(status.contains("sweetidle: running"));
    assert!(status.contains("id=0 state=timed-out timeout=3600s inhibitors=ignored"));
    assert!(status.contains("tracked inhibitors: none"));
    harness.stop()?;
    Ok(())
}

#[test]
#[ignore = "runs the real daemon against a private Wayland compositor"]
fn json_status_reports_live_listener_state() -> Result<(), Box<dyn Error>> {
    let mut harness = ReloadHarness::start("status-json")?;
    harness.wait_for_markers(1)?;

    let status: serde_json::Value = serde_json::from_str(&harness.status_json()?)?;

    assert_eq!(status["schema_version"], 1);
    assert_eq!(status["paused"], false);
    assert_eq!(status["idle_hint"], serde_json::Value::Null);
    assert_eq!(status["listeners"][0]["id"], 0);
    assert_eq!(status["listeners"][0]["state"], "timed-out");
    assert_eq!(status["listeners"][0]["timeout_seconds"], 3600);
    assert_eq!(status["listeners"][0]["ignore_inhibit"], true);
    assert_eq!(status["inhibitors"]["active"], false);
    assert_eq!(status["inhibitors"]["dbus"], serde_json::json!([]));
    assert_eq!(status["inhibitors"]["systemd"], serde_json::json!([]));
    harness.stop()?;
    Ok(())
}

#[test]
#[ignore = "runs the real daemon against a private Wayland compositor"]
fn pause_suppresses_idle_actions_until_resume() -> Result<(), Box<dyn Error>> {
    let mut harness = ReloadHarness::start("controlled")?;
    harness.wait_for_markers(1)?;

    assert_eq!(harness.control("pause")?, "sweetidle: paused\n");
    assert_eq!(harness.compositor.pulse(0)?, 0);
    assert_eq!(harness.markers()?, ["controlled"]);
    assert!(harness.status()?.contains("sweetidle: paused"));
    assert_eq!(harness.control("pause")?, "sweetidle: already paused\n");

    assert_eq!(harness.control("resume")?, "sweetidle: resumed\n");
    harness.wait_for_markers(2)?;
    assert!(harness.status()?.contains("sweetidle: running"));
    assert_eq!(harness.control("resume")?, "sweetidle: already running\n");
    harness.stop()?;
    Ok(())
}

#[test]
#[ignore = "runs the real daemon against a private Wayland compositor"]
fn foreground_group_interrupt_leaves_running_commands_alive() -> Result<(), Box<dyn Error>> {
    let mut harness = ReloadHarness::start("initial")?;
    harness.wait_for_markers(1)?;
    harness.resume_listener_cycle()?;
    let marker_path = shell_quote(path_text(&harness.marker_path)?);
    let command = format!(
        "printf 'started\\n' >> {marker_path}; sleep 1; printf 'survived\\n' >> {marker_path}"
    );
    let command = command.replace('\\', "\\\\").replace('"', "\\\"");
    fs::write(
        &harness.config_path,
        format!(
            "[general]\nignore_dbus_inhibit = true\nignore_systemd_inhibit = true\n\n[[listener]]\ntimeout = 3600\non_timeout = \"{command}\"\nignore_inhibit = true\n"
        ),
    )?;
    harness.signal_reload()?;
    harness.wait_for_markers(2)?;

    let child = harness.daemon.child.as_ref().ok_or("daemon is missing")?;
    // signal only the isolated daemon group, as terminal ctrl-c would
    kill_process_group(Pid::from_child(child), Signal::INT)?;
    harness.daemon.wait_for_exit()?;

    let deadline = Instant::now() + WAIT_LIMIT;
    while Instant::now() < deadline && harness.markers()?.len() < 3 {
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(harness.markers()?, ["initial", "started", "survived"]);
    harness.compositor.stop()?;
    Ok(())
}

#[test]
#[ignore = "runs the real daemon against a private Wayland compositor"]
fn ignored_sigchld_from_the_launcher_does_not_stop_command_reaping() -> Result<(), Box<dyn Error>> {
    let mut harness = ReloadHarness::start_with_launcher("initial", None, 2, IGNORE_SIGCHLD)?;
    harness.wait_for_markers(1)?;
    harness.wait_for_log("command finished")?;
    harness.resume_listener_cycle()?;
    let marker_path = shell_quote(path_text(&harness.marker_path)?);
    let command = format!("grep SigIgn /proc/self/status >> {marker_path}");
    fs::write(
        &harness.config_path,
        format!(
            "[general]\nignore_dbus_inhibit = true\nignore_systemd_inhibit = true\n\n[[listener]]\ntimeout = 3600\non_timeout = \"{command}\"\nignore_inhibit = true\n"
        ),
    )?;
    harness.signal_reload()?;
    harness.wait_for_markers(2)?;
    harness.wait_for_log_count("command finished", 2)?;

    // hooks must start with default SIGCHLD, whatever the daemon inherited
    let markers = harness.markers()?;
    let ignored = markers[1]
        .strip_prefix("SigIgn:")
        .ok_or("hook did not report its ignored signals")?;
    let ignored = u64::from_str_radix(ignored.trim(), 16)?;
    assert_eq!(ignored & SIGCHLD_MASK, 0, "hook inherited ignored SIGCHLD");
    assert!(harness.status()?.contains("sweetidle: running"));
    harness.stop()?;
    Ok(())
}

#[test]
#[ignore = "runs the real daemon against a private Wayland compositor"]
fn second_instance_fails_before_connecting_to_the_compositor() -> Result<(), Box<dyn Error>> {
    let mut harness = ReloadHarness::start("initial")?;
    harness.wait_for_markers(1)?;
    let created = harness.compositor.created()?;

    let second = Command::new(env!("CARGO_BIN_EXE_sweetidle"))
        .args(["--config", path_text(&harness.config_path)?])
        .env(
            "WAYLAND_DISPLAY",
            harness.temp_dir.path().join("wayland-test"),
        )
        .env("XDG_RUNTIME_DIR", harness.temp_dir.path())
        .output()?;

    assert_eq!(second.status.code(), Some(2));
    let stderr = String::from_utf8(second.stderr)?;
    assert!(
        stderr.contains("another sweetidle instance is listening"),
        "unexpected second-instance error: {stderr}"
    );
    assert_eq!(harness.compositor.created()?, created);
    assert!(harness.status()?.contains("sweetidle: running"));
    harness.stop()?;
    Ok(())
}

#[test]
#[ignore = "runs the real daemon against a private Wayland compositor"]
fn hook_stderr_reaches_the_daemon_log_but_stdout_does_not() -> Result<(), Box<dyn Error>> {
    let mut harness = ReloadHarness::start("initial")?;
    harness.wait_for_markers(1)?;
    harness.resume_listener_cycle()?;
    let marker_path = shell_quote(path_text(&harness.marker_path)?);
    let command = format!(
        "printf 'hook-stdout\\n'; printf 'hook-stderr\\n' >&2; printf 'done\\n' >> {marker_path}"
    );
    fs::write(
        &harness.config_path,
        format!(
            "[general]\nignore_dbus_inhibit = true\nignore_systemd_inhibit = true\n\n[[listener]]\ntimeout = 3600\non_timeout = \"{command}\"\nignore_inhibit = true\n"
        ),
    )?;
    harness.signal_reload()?;
    harness.wait_for_markers(2)?;
    harness.wait_for_log_count("command finished", 2)?;

    let log = fs::read_to_string(&harness.log_path)?;
    assert!(log.contains("hook-stderr\n"), "{log}");
    assert!(!log.contains("hook-stdout"), "{log}");
    harness.stop()?;
    Ok(())
}

#[test]
#[ignore = "runs the real daemon against a private Wayland compositor"]
fn compositor_hangup_stops_the_daemon_cleanly() -> Result<(), Box<dyn Error>> {
    let mut harness = ReloadHarness::start("initial")?;
    harness.wait_for_markers(1)?;

    harness.compositor.stop()?;

    let status = harness.daemon.wait_for_status()?;
    let log = fs::read_to_string(&harness.log_path)?;
    assert!(status.success(), "{status}: {log}");
    assert!(
        log.contains("compositor closed the Wayland connection"),
        "{log}"
    );
    assert!(!harness.temp_dir.path().join("sweetidle.sock").exists());
    Ok(())
}

#[test]
#[ignore = "runs the real daemon against a private Wayland compositor"]
fn compositor_protocol_error_still_fails_the_daemon() -> Result<(), Box<dyn Error>> {
    let mut harness = ReloadHarness::start("initial")?;
    harness.wait_for_markers(1)?;

    assert_eq!(harness.compositor.emit(1, MockEvent::ProtocolError)?, 1);
    // real compositors disconnect a client after posting a protocol error
    harness.compositor.stop()?;

    let status = harness.daemon.wait_for_status()?;
    let log = fs::read_to_string(&harness.log_path)?;
    assert_eq!(status.code(), Some(2), "{log}");
    assert!(
        !log.contains("compositor closed the Wayland connection"),
        "{log}"
    );
    Ok(())
}

struct ReloadHarness {
    config_path: std::path::PathBuf,
    marker_path: std::path::PathBuf,
    log_path: std::path::PathBuf,
    daemon: TestDaemon,
    compositor: MockCompositor,
    temp_dir: TempDir,
}

impl ReloadHarness {
    fn start(initial_marker: &str) -> Result<Self, Box<dyn Error>> {
        Self::start_with_notifier_version(initial_marker, 2)
    }

    fn start_with_notifier_version(
        initial_marker: &str,
        notifier_version: u32,
    ) -> Result<Self, Box<dyn Error>> {
        Self::start_with_actions(initial_marker, None, notifier_version)
    }

    fn start_with_actions(
        initial_marker: &str,
        resume_marker: Option<&str>,
        notifier_version: u32,
    ) -> Result<Self, Box<dyn Error>> {
        Self::start_with_launcher(
            initial_marker,
            resume_marker,
            notifier_version,
            "exec \"$0\" \"$@\"",
        )
    }

    fn start_with_launcher(
        initial_marker: &str,
        resume_marker: Option<&str>,
        notifier_version: u32,
        launcher: &str,
    ) -> Result<Self, Box<dyn Error>> {
        let temp_dir = tempfile::tempdir()?;
        let config_path = temp_dir.path().join("config.toml");
        let marker_path = temp_dir.path().join("commands.log");
        let log_path = temp_dir.path().join("daemon.log");
        let socket_path = temp_dir.path().join("wayland-test");
        let compositor = MockCompositor::start(socket_path.clone(), notifier_version)?;
        write_config_with_policy(
            &config_path,
            &marker_path,
            initial_marker,
            notifier_version >= 2,
            resume_marker,
        )?;

        let log = File::create(&log_path)?;
        let log_stdout = log.try_clone()?;
        // the launcher shell execs the daemon, so the child PID stays the daemon's
        let child = Command::new("sh")
            .args(["-c", launcher, env!("CARGO_BIN_EXE_sweetidle")])
            .args(["--config", path_text(&config_path)?])
            .env("WAYLAND_DISPLAY", &socket_path)
            .env("XDG_RUNTIME_DIR", temp_dir.path())
            .stdout(Stdio::from(log_stdout))
            .stderr(Stdio::from(log))
            .process_group(0)
            .spawn()?;

        Ok(Self {
            config_path,
            marker_path,
            log_path,
            daemon: TestDaemon::new(child),
            compositor,
            temp_dir,
        })
    }

    fn write_config(&self, marker: &str) -> Result<(), Box<dyn Error>> {
        write_config(&self.config_path, &self.marker_path, marker)
    }

    fn signal_reload(&self) -> Result<(), Box<dyn Error>> {
        self.daemon.signal(Signal::HUP)
    }

    fn resume_listener_cycle(&mut self) -> Result<(), Box<dyn Error>> {
        assert_eq!(self.compositor.emit(1, MockEvent::Resume)?, 1);
        let deadline = Instant::now() + WAIT_LIMIT;
        loop {
            if self.status()?.contains("state=armed") {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err("listener did not resume".into());
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn wait_for_markers(&mut self, expected_count: usize) -> Result<(), Box<dyn Error>> {
        wait_until(&mut self.daemon, &self.log_path, || {
            marker_lines(&self.marker_path)
                .map(|markers| markers.len() >= expected_count)
                .unwrap_or(false)
        })
    }

    fn wait_for_log(&mut self, expected: &str) -> Result<(), Box<dyn Error>> {
        self.wait_for_log_count(expected, 1)
    }

    fn wait_for_log_count(&mut self, expected: &str, count: usize) -> Result<(), Box<dyn Error>> {
        wait_until(&mut self.daemon, &self.log_path, || {
            fs::read_to_string(&self.log_path)
                .map(|log| log.matches(expected).count() >= count)
                .unwrap_or(false)
        })
    }

    fn markers(&self) -> Result<Vec<String>, Box<dyn Error>> {
        marker_lines(&self.marker_path)
    }

    fn status(&mut self) -> Result<String, Box<dyn Error>> {
        self.control("status")
    }

    fn status_json(&mut self) -> Result<String, Box<dyn Error>> {
        self.control_args(&["status", "--json"])
    }

    fn control(&mut self, command: &str) -> Result<String, Box<dyn Error>> {
        self.control_args(&[command])
    }

    fn control_args(&mut self, args: &[&str]) -> Result<String, Box<dyn Error>> {
        let output = Command::new(env!("CARGO_BIN_EXE_sweetidle"))
            .args(args)
            .env("XDG_RUNTIME_DIR", self.temp_dir.path())
            .output()?;
        if !output.status.success() {
            return Err(format!(
                "{} command failed with {}: {}",
                args.join(" "),
                output.status,
                String::from_utf8_lossy(&output.stderr)
            )
            .into());
        }
        Ok(String::from_utf8(output.stdout)?)
    }

    fn stop(mut self) -> Result<(), Box<dyn Error>> {
        self.daemon.stop()?;
        self.compositor.stop()?;
        Ok(())
    }
}

fn write_config(
    config_path: &std::path::Path,
    marker_path: &std::path::Path,
    marker: &str,
) -> Result<(), Box<dyn Error>> {
    write_config_with_policy(config_path, marker_path, marker, true, None)
}

fn write_config_with_policy(
    config_path: &std::path::Path,
    marker_path: &std::path::Path,
    marker: &str,
    ignore_inhibit: bool,
    resume_marker: Option<&str>,
) -> Result<(), Box<dyn Error>> {
    let marker_path = shell_quote(path_text(marker_path)?);
    let command = |marker: &str| {
        let command = format!("printf '%s\\n' {marker} >> {marker_path}");
        command.replace('\\', "\\\\").replace('"', "\\\"")
    };
    let on_resume = resume_marker
        .map(|marker| format!("on_resume = \"{}\"\n", command(marker)))
        .unwrap_or_default();
    let command = command(marker);
    let config = format!(
        "[general]\nignore_dbus_inhibit = true\nignore_systemd_inhibit = true\n\n[[listener]]\ntimeout = 3600\non_timeout = \"{command}\"\n{on_resume}ignore_inhibit = {ignore_inhibit}\n"
    );
    fs::write(config_path, config)?;
    Ok(())
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn path_text(path: &std::path::Path) -> Result<&str, Box<dyn Error>> {
    path.to_str()
        .ok_or_else(|| format!("path is not valid UTF-8: {}", path.display()).into())
}

fn marker_lines(path: &std::path::Path) -> Result<Vec<String>, Box<dyn Error>> {
    match fs::read_to_string(path) {
        Ok(contents) => Ok(contents.lines().map(str::to_string).collect()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error.into()),
    }
}

fn wait_until(
    daemon: &mut TestDaemon,
    log_path: &std::path::Path,
    mut ready: impl FnMut() -> bool,
) -> Result<(), Box<dyn Error>> {
    // include daemon logs when a readiness check fails or times out
    let deadline = Instant::now() + WAIT_LIMIT;
    while Instant::now() < deadline {
        if ready() {
            return Ok(());
        }
        if let Some(status) = daemon.try_wait()? {
            let log = fs::read_to_string(log_path).unwrap_or_default();
            return Err(format!("sweetidle exited early with {status}:\n{log}").into());
        }
        thread::sleep(Duration::from_millis(10));
    }

    let log = fs::read_to_string(log_path).unwrap_or_default();
    Err(format!("timed out waiting for sweetidle:\n{log}").into())
}

struct TestDaemon {
    child: Option<Child>,
}

impl TestDaemon {
    fn new(child: Child) -> Self {
        Self { child: Some(child) }
    }

    fn signal(&self, signal: Signal) -> Result<(), Box<dyn Error>> {
        let child = self
            .child
            .as_ref()
            .ok_or("sweetidle process is no longer running")?;
        kill_process(Pid::from_child(child), signal)?;
        Ok(())
    }

    fn try_wait(&mut self) -> Result<Option<std::process::ExitStatus>, Box<dyn Error>> {
        self.child
            .as_mut()
            .ok_or_else(|| "sweetidle process is no longer running".into())
            .and_then(|child| child.try_wait().map_err(Into::into))
    }

    fn stop(&mut self) -> Result<(), Box<dyn Error>> {
        self.signal(Signal::INT)?;
        self.wait_for_exit()
    }

    fn wait_for_status(&mut self) -> Result<std::process::ExitStatus, Box<dyn Error>> {
        let deadline = Instant::now() + WAIT_LIMIT;
        while Instant::now() < deadline {
            if let Some(status) = self.try_wait()? {
                self.child = None;
                return Ok(status);
            }
            thread::sleep(Duration::from_millis(10));
        }
        Err("sweetidle did not exit".into())
    }

    fn wait_for_exit(&mut self) -> Result<(), Box<dyn Error>> {
        let child = self
            .child
            .take()
            .ok_or("sweetidle process is no longer running")?;
        let status = child.wait_with_output()?.status;
        if !status.success() {
            return Err(format!("sweetidle stopped with {status}").into());
        }
        Ok(())
    }
}

impl Drop for TestDaemon {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = kill_process(Pid::from_child(&child), Signal::INT);
            let _ = child.wait();
        }
    }
}

enum CompositorCommand {
    Pulse {
        expected_notifications: usize,
        event: MockEvent,
        reply: Sender<usize>,
    },
    Created(Sender<usize>),
    Stop,
}

#[derive(Clone, Copy)]
enum MockEvent {
    Pulse,
    Resume,
    Idle,
    ResumeOne(usize),
    ProtocolError,
}

struct MockCompositor {
    commands: Sender<CompositorCommand>,
    thread: Option<JoinHandle<Result<(), String>>>,
}

impl MockCompositor {
    fn start(
        socket_path: std::path::PathBuf,
        notifier_version: u32,
    ) -> Result<Self, Box<dyn Error>> {
        let socket = ListeningSocket::bind_absolute(socket_path)?;
        let mut display = Display::<MockState>::new()?;
        let handle = display.handle();
        handle.create_global::<MockState, wl_seat::WlSeat, _>(1, ());
        handle.create_global::<MockState, ExtIdleNotifierV1, _>(notifier_version, ());
        let (commands, receiver) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("sweetidle-test-wayland".to_string())
            .spawn(move || run_compositor(&mut display, &socket, &receiver))?;

        Ok(Self {
            commands,
            thread: Some(thread),
        })
    }

    fn pulse(&self, expected_notifications: usize) -> Result<usize, Box<dyn Error>> {
        self.emit(expected_notifications, MockEvent::Pulse)
    }

    fn emit(
        &self,
        expected_notifications: usize,
        event: MockEvent,
    ) -> Result<usize, Box<dyn Error>> {
        let (reply, received) = mpsc::channel();
        self.commands.send(CompositorCommand::Pulse {
            expected_notifications,
            event,
            reply,
        })?;
        received.recv_timeout(WAIT_LIMIT).map_err(Into::into)
    }

    fn created(&self) -> Result<usize, Box<dyn Error>> {
        let (reply, received) = mpsc::channel();
        self.commands.send(CompositorCommand::Created(reply))?;
        received.recv_timeout(WAIT_LIMIT).map_err(Into::into)
    }

    fn stop(&mut self) -> Result<(), Box<dyn Error>> {
        let _ = self.commands.send(CompositorCommand::Stop);
        let thread = self
            .thread
            .take()
            .ok_or("mock compositor already stopped")?;
        match thread.join() {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(error.into()),
            Err(_) => Err("mock compositor thread panicked".into()),
        }
    }
}

impl Drop for MockCompositor {
    fn drop(&mut self) {
        let _ = self.commands.send(CompositorCommand::Stop);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn run_compositor(
    display: &mut Display<MockState>,
    socket: &ListeningSocket,
    commands: &Receiver<CompositorCommand>,
) -> Result<(), String> {
    let mut state = MockState::default();
    let mut pending_pulse = None;
    loop {
        let mut created_reply = None;
        while let Some(stream) = socket.accept().map_err(|error| error.to_string())? {
            display
                .handle()
                .insert_client(stream, Arc::new(()))
                .map_err(|error| error.to_string())?;
        }

        match commands.try_recv() {
            Ok(CompositorCommand::Pulse {
                expected_notifications,
                event,
                reply,
            }) => pending_pulse = Some((expected_notifications, event, reply)),
            Ok(CompositorCommand::Created(reply)) => created_reply = Some(reply),
            Ok(CompositorCommand::Stop) => return Ok(()),
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => return Ok(()),
        }

        display
            .dispatch_clients(&mut state)
            .map_err(|error| error.to_string())?;
        // wait for create/destroy requests so pause assertions observe the actual notification lifecycle
        let pulse_reply = match pending_pulse.take() {
            Some((expected, event, reply)) if state.notifications.len() == expected => {
                Some((reply, state.emit(event)))
            }
            pending => {
                pending_pulse = pending;
                None
            }
        };
        display.flush_clients().map_err(|error| error.to_string())?;
        if let Some((reply, emitted)) = pulse_reply {
            let _ = reply.send(emitted);
        }
        if let Some(reply) = created_reply {
            let _ = reply.send(state.created);
        }
        thread::sleep(Duration::from_millis(2));
    }
}

#[derive(Default)]
struct MockState {
    notifications: Vec<ExtIdleNotificationV1>,
    created: usize,
}

impl MockState {
    fn emit(&mut self, event: MockEvent) -> usize {
        self.notifications.retain(Resource::is_alive);
        let mut emitted = 0;
        for (index, notification) in self.notifications.iter().enumerate() {
            match event {
                MockEvent::Pulse => {
                    notification.resumed();
                    notification.idled();
                }
                MockEvent::Resume => notification.resumed(),
                MockEvent::Idle => notification.idled(),
                MockEvent::ResumeOne(selected) if index == selected => notification.resumed(),
                MockEvent::ResumeOne(_) => continue,
                MockEvent::ProtocolError => notification.post_error(0_u32, "mock protocol error"),
            }
            emitted += 1;
        }
        emitted
    }
}

impl GlobalDispatch<wl_seat::WlSeat, ()> for MockState {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<wl_seat::WlSeat>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for MockState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &wl_seat::WlSeat,
        _request: wl_seat::Request,
        _data: &(),
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
    }
}

impl GlobalDispatch<ExtIdleNotifierV1, ()> for MockState {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<ExtIdleNotifierV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<ExtIdleNotifierV1, ()> for MockState {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &ExtIdleNotifierV1,
        request: ext_idle_notifier_v1::Request,
        _data: &(),
        _handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        let id = match request {
            ext_idle_notifier_v1::Request::GetIdleNotification { id, .. }
            | ext_idle_notifier_v1::Request::GetInputIdleNotification { id, .. } => id,
            _ => return,
        };
        let notification = data_init.init(id, ());
        notification.idled();
        state.created += 1;
        state.notifications.push(notification);
    }
}

impl Dispatch<ExtIdleNotificationV1, ()> for MockState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &ExtIdleNotificationV1,
        _request: ext_idle_notification_v1::Request,
        _data: &(),
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
    }

    fn destroyed(
        state: &mut Self,
        _client: ClientId,
        resource: &ExtIdleNotificationV1,
        _data: &(),
    ) {
        state.notifications.retain(|item| item != resource);
    }
}
