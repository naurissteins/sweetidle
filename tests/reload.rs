use std::{
    error::Error,
    fs::{self, File},
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        mpsc::{self, Receiver, Sender, TryRecvError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use rustix::process::{Pid, Signal, kill_process};
use tempfile::TempDir;
use wayland_protocols::ext::idle_notify::v1::server::{
    ext_idle_notification_v1::{self, ExtIdleNotificationV1},
    ext_idle_notifier_v1::{self, ExtIdleNotifierV1},
};
use wayland_server::{
    Client, DataInit, Dispatch, Display, DisplayHandle, GlobalDispatch, ListeningSocket, New,
    Resource, backend::ClientId, protocol::wl_seat,
};

const WAIT_LIMIT: Duration = Duration::from_secs(5);

#[test]
#[ignore = "runs the real daemon against a private Wayland compositor"]
fn sighup_applies_valid_config_without_restarting_the_daemon() -> Result<(), Box<dyn Error>> {
    let mut harness = ReloadHarness::start("initial")?;
    harness.wait_for_markers(1)?;

    harness.write_config("reloaded")?;
    harness.signal_reload()?;
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
    harness.compositor.pulse()?;
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
    harness.compositor.pulse()?;
    harness.wait_for_markers(2)?;

    assert_eq!(harness.markers()?, ["active", "active"]);
    harness.stop()?;
    Ok(())
}

#[test]
#[ignore = "runs the real daemon against a private Wayland compositor"]
fn status_reports_live_listener_state() -> Result<(), Box<dyn Error>> {
    let mut harness = ReloadHarness::start("status")?;
    harness.wait_for_markers(1)?;

    let status = harness.status()?;

    assert!(status.contains("sweetsidle: running"));
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

    assert_eq!(harness.control("pause")?, "sweetsidle: paused\n");
    harness.compositor.pulse()?;
    thread::sleep(Duration::from_millis(100));
    assert_eq!(harness.markers()?, ["controlled"]);
    assert!(harness.status()?.contains("sweetsidle: paused"));
    assert_eq!(harness.control("pause")?, "sweetsidle: already paused\n");

    assert_eq!(harness.control("resume")?, "sweetsidle: resumed\n");
    harness.wait_for_markers(2)?;
    assert!(harness.status()?.contains("sweetsidle: running"));
    assert_eq!(harness.control("resume")?, "sweetsidle: already running\n");
    harness.stop()?;
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
        let temp_dir = tempfile::tempdir()?;
        let config_path = temp_dir.path().join("config.toml");
        let marker_path = temp_dir.path().join("commands.log");
        let log_path = temp_dir.path().join("daemon.log");
        let socket_path = temp_dir.path().join("wayland-test");
        let compositor = MockCompositor::start(socket_path.clone())?;
        write_config(&config_path, &marker_path, initial_marker)?;

        let log = File::create(&log_path)?;
        let log_stdout = log.try_clone()?;
        let child = Command::new(env!("CARGO_BIN_EXE_sweetsidle"))
            .args(["--config", path_text(&config_path)?])
            .env("WAYLAND_DISPLAY", &socket_path)
            .env("XDG_RUNTIME_DIR", temp_dir.path())
            .stdout(Stdio::from(log_stdout))
            .stderr(Stdio::from(log))
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

    fn wait_for_markers(&mut self, expected_count: usize) -> Result<(), Box<dyn Error>> {
        wait_until(&mut self.daemon, &self.log_path, || {
            marker_lines(&self.marker_path)
                .map(|markers| markers.len() >= expected_count)
                .unwrap_or(false)
        })
    }

    fn wait_for_log(&mut self, expected: &str) -> Result<(), Box<dyn Error>> {
        wait_until(&mut self.daemon, &self.log_path, || {
            fs::read_to_string(&self.log_path)
                .map(|log| log.contains(expected))
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
        let output = Command::new(env!("CARGO_BIN_EXE_sweetsidle"))
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
    let marker_path = shell_quote(path_text(marker_path)?);
    let command = format!("printf '%s\\n' {marker} >> {marker_path}");
    let command = command.replace('\\', "\\\\").replace('"', "\\\"");
    let config = format!(
        "[general]\nignore_dbus_inhibit = true\nignore_systemd_inhibit = true\n\n[[listener]]\ntimeout = 3600\non_timeout = \"{command}\"\nignore_inhibit = true\n"
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
    let deadline = Instant::now() + WAIT_LIMIT;
    while Instant::now() < deadline {
        if ready() {
            return Ok(());
        }
        if let Some(status) = daemon.try_wait()? {
            let log = fs::read_to_string(log_path).unwrap_or_default();
            return Err(format!("sweetsidle exited early with {status}:\n{log}").into());
        }
        thread::sleep(Duration::from_millis(10));
    }

    let log = fs::read_to_string(log_path).unwrap_or_default();
    Err(format!("timed out waiting for sweetsidle:\n{log}").into())
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
            .ok_or("sweetsidle process is no longer running")?;
        kill_process(Pid::from_child(child), signal)?;
        Ok(())
    }

    fn try_wait(&mut self) -> Result<Option<std::process::ExitStatus>, Box<dyn Error>> {
        self.child
            .as_mut()
            .ok_or_else(|| "sweetsidle process is no longer running".into())
            .and_then(|child| child.try_wait().map_err(Into::into))
    }

    fn stop(&mut self) -> Result<(), Box<dyn Error>> {
        self.signal(Signal::INT)?;
        let child = self
            .child
            .take()
            .ok_or("sweetsidle process is no longer running")?;
        let status = child.wait_with_output()?.status;
        if !status.success() {
            return Err(format!("sweetsidle stopped with {status}").into());
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
    Pulse,
    Stop,
}

struct MockCompositor {
    commands: Sender<CompositorCommand>,
    thread: Option<JoinHandle<Result<(), String>>>,
}

impl MockCompositor {
    fn start(socket_path: std::path::PathBuf) -> Result<Self, Box<dyn Error>> {
        let socket = ListeningSocket::bind_absolute(socket_path)?;
        let mut display = Display::<MockState>::new()?;
        let handle = display.handle();
        handle.create_global::<MockState, wl_seat::WlSeat, _>(1, ());
        handle.create_global::<MockState, ExtIdleNotifierV1, _>(2, ());
        let (commands, receiver) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("sweetsidle-test-wayland".to_string())
            .spawn(move || run_compositor(&mut display, &socket, &receiver))?;

        Ok(Self {
            commands,
            thread: Some(thread),
        })
    }

    fn pulse(&self) -> Result<(), Box<dyn Error>> {
        self.commands.send(CompositorCommand::Pulse)?;
        Ok(())
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
    loop {
        while let Some(stream) = socket.accept().map_err(|error| error.to_string())? {
            display
                .handle()
                .insert_client(stream, Arc::new(()))
                .map_err(|error| error.to_string())?;
        }

        match commands.try_recv() {
            Ok(CompositorCommand::Pulse) => state.pulse(),
            Ok(CompositorCommand::Stop) => return Ok(()),
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => return Ok(()),
        }

        display
            .dispatch_clients(&mut state)
            .map_err(|error| error.to_string())?;
        display.flush_clients().map_err(|error| error.to_string())?;
        thread::sleep(Duration::from_millis(2));
    }
}

#[derive(Default)]
struct MockState {
    notifications: Vec<ExtIdleNotificationV1>,
}

impl MockState {
    fn pulse(&mut self) {
        self.notifications.retain(Resource::is_alive);
        for notification in &self.notifications {
            notification.resumed();
            notification.idled();
        }
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
