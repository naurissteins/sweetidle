use std::path::PathBuf;

use sweetsidle::{
    ProcessError,
    config::Config,
    control,
    runtime::{ConfigReloadOutcome, IdleRuntime, RuntimeEvent},
};
use tracing::{debug, error, info, warn};

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() {
    if let Err(error) = run(std::env::args().skip(1)) {
        eprintln!("error: {error}");
        std::process::exit(2);
    }
}

fn run(args: impl IntoIterator<Item = String>) -> Result<(), String> {
    match parse_args(args)? {
        Action::Help => print_help(),
        Action::Version => println!("sweetsidle {VERSION}"),
        Action::Status(StatusOutput::Text) => print_control(control::query_status())?,
        Action::Status(StatusOutput::Json) => print_control(control::query_status_json())?,
        Action::Pause => print_control(control::pause())?,
        Action::Resume => print_control(control::resume())?,
        Action::Reload => print_control(control::reload())?,
        Action::CheckConfig(path) => check_config_file(path)?,
        Action::Run { path, verbose } => run_daemon(path, verbose)?,
    }

    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
enum Action {
    Help,
    Version,
    Status(StatusOutput),
    Pause,
    Resume,
    Reload,
    CheckConfig(Option<PathBuf>),
    Run {
        path: Option<PathBuf>,
        verbose: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatusOutput {
    Text,
    Json,
}

fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Action, String> {
    let mut check_config = false;
    let mut config_path = None;
    let mut verbose = false;
    let mut status_json = false;
    let mut control_action = None;
    let mut args = args.into_iter();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(Action::Help),
            "-V" | "--version" => return Ok(Action::Version),
            "-v" | "--verbose" => verbose = true,
            "status" => {
                set_control_action(&mut control_action, Action::Status(StatusOutput::Text))?
            }
            "pause" => set_control_action(&mut control_action, Action::Pause)?,
            "resume" => set_control_action(&mut control_action, Action::Resume)?,
            "reload" => set_control_action(&mut control_action, Action::Reload)?,
            "--json" => status_json = true,
            "--check-config" => check_config = true,
            "-c" | "--config" => {
                let path = args
                    .next()
                    .ok_or_else(|| format!("`{arg}` requires a path argument"))?;
                config_path = Some(PathBuf::from(path));
            }
            _ => return Err(format!("unknown argument `{arg}`\ntry `sweetsidle --help`")),
        }
    }

    if let Some(action) = control_action {
        if check_config || verbose || config_path.is_some() {
            return Err(
                "control commands cannot be combined with daemon or config options".to_string(),
            );
        }
        match (action, status_json) {
            (Action::Status(_), true) => Ok(Action::Status(StatusOutput::Json)),
            (Action::Status(_), false) => Ok(Action::Status(StatusOutput::Text)),
            (_, true) => Err("`--json` can only be used with `status`".to_string()),
            (action, false) => Ok(action),
        }
    } else if check_config {
        if status_json {
            return Err("`--json` can only be used with `status`".to_string());
        }
        if verbose {
            return Err("`--verbose` cannot be used with `--check-config`".to_string());
        }
        Ok(Action::CheckConfig(config_path))
    } else {
        if status_json {
            return Err("`--json` can only be used with `status`".to_string());
        }
        Ok(Action::Run {
            path: config_path,
            verbose,
        })
    }
}

fn set_control_action(current: &mut Option<Action>, action: Action) -> Result<(), String> {
    if current.replace(action).is_some() {
        return Err("only one control command may be specified".to_string());
    }
    Ok(())
}

fn print_help() {
    println!(
        "\
sweetsidle {VERSION}

Usage: sweetsidle [OPTIONS]
       sweetsidle <COMMAND>

Commands:
  status [--json]     Show listener and inhibitor state
  pause               Pause idle listeners and the idle hint
  resume              Resume idle detection with fresh timers
  reload              Queue a reload of the active config

Options:
      --json           Print status as JSON (status only)
  -c, --config <PATH>  Use a specific config file
      --check-config   Parse and validate the config file, then exit
  -v, --verbose        Enable debug logging while running
  -h, --help           Print help
  -V, --version        Print version
"
    );
}

fn print_control(response: Result<String, control::ControlError>) -> Result<(), String> {
    print!("{}", response.map_err(|error| error.to_string())?);
    Ok(())
}

fn check_config_file(path: Option<PathBuf>) -> Result<(), String> {
    let loaded = load_config(path)?;
    match loaded.config.idle_hint_timeout_seconds {
        Some(timeout) => println!(
            "configuration ok: {} listener(s), idle hint timeout: {timeout}s",
            loaded.config.listeners.len()
        ),
        None => println!(
            "configuration ok: {} listener(s), idle hint disabled",
            loaded.config.listeners.len()
        ),
    }
    Ok(())
}

fn run_daemon(path: Option<PathBuf>, verbose: bool) -> Result<(), String> {
    init_logging(verbose)?;
    let loaded = load_config(path)?;
    let mut active_config = loaded.config;
    let mut runtime = IdleRuntime::connect(&active_config, loaded.path.clone())
        .map_err(|error| error.to_string())?;

    info!(
        config = %loaded.path.display(),
        listeners = active_config.listeners.len(),
        idle_hint_timeout_seconds = ?active_config.idle_hint_timeout_seconds,
        "sweetsidle started"
    );

    // the runtime blocks until an event wakes this loop
    loop {
        let events = runtime.dispatch(None).map_err(|error| error.to_string())?;
        let mut shutdown = false;
        let mut reload = None;

        for event in events {
            match handle_runtime_event(event) {
                RuntimeControl::Continue => {}
                RuntimeControl::Shutdown => shutdown = true,
                RuntimeControl::Reload(config) => reload = Some(config),
            }
        }

        if shutdown {
            info!("sweetsidle stopped");
            return Ok(());
        }

        let Some(candidate) = reload else {
            continue;
        };
        if candidate == active_config {
            info!(config = %loaded.path.display(), "configuration is unchanged");
            continue;
        }

        // keep the active config until the runtime accepts the candidate
        match runtime
            .reload_config(&candidate)
            .map_err(|error| error.to_string())?
        {
            ConfigReloadOutcome::Applied => {
                active_config = candidate;
                info!(
                    config = %loaded.path.display(),
                    listeners = active_config.listeners.len(),
                    idle_hint_timeout_seconds = ?active_config.idle_hint_timeout_seconds,
                    "configuration reloaded"
                );
            }
            ConfigReloadOutcome::Rejected(error) => {
                warn!(
                    config = %loaded.path.display(),
                    error = %error,
                    "configuration reload rejected; keeping current configuration"
                );
            }
            ConfigReloadOutcome::RestartRequired(reason) => {
                warn!(
                    config = %loaded.path.display(),
                    reason,
                    "configuration reload requires a restart; keeping current configuration"
                );
            }
        }
    }
}

fn init_logging(verbose: bool) -> Result<(), String> {
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

enum RuntimeControl {
    Continue,
    Shutdown,
    Reload(Config),
}

fn handle_runtime_event(event: RuntimeEvent) -> RuntimeControl {
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
        RuntimeEvent::DbusInhibitorAdded {
            cookie,
            application,
            reason,
            active_count,
        } => {
            debug!(
                cookie,
                application, reason, active_count, "D-Bus idle inhibitor added"
            );
            RuntimeControl::Continue
        }
        RuntimeEvent::DbusInhibitorRemoved {
            cookie,
            application,
            reason,
            active_count,
        } => {
            debug!(
                cookie,
                application, reason, active_count, "D-Bus idle inhibitor removed"
            );
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

struct LoadedConfig {
    path: PathBuf,
    config: Config,
}

fn load_config(path: Option<PathBuf>) -> Result<LoadedConfig, String> {
    let path = match path {
        Some(path) => path,
        None => default_config_path()?,
    };
    let config = Config::load(&path).map_err(|error| error.to_string())?;

    Ok(LoadedConfig { path, config })
}

fn default_config_path() -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME")
        .ok_or_else(|| "HOME is not set; pass --config <PATH>".to_string())?;

    Ok(PathBuf::from(home).join(".config/sweetsidle/config.toml"))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{Action, StatusOutput, parse_args};

    #[test]
    fn no_arguments_runs_with_the_default_config() {
        assert_eq!(
            parse_args(Vec::new()),
            Ok(Action::Run {
                path: None,
                verbose: false,
            })
        );
    }

    #[test]
    fn config_path_selects_daemon_startup() {
        assert_eq!(
            parse_args(["--config".to_string(), "idle.toml".to_string()]),
            Ok(Action::Run {
                path: Some(PathBuf::from("idle.toml")),
                verbose: false,
            })
        );
    }

    #[test]
    fn verbose_enables_debug_logging_for_daemon_startup() {
        assert_eq!(
            parse_args(["--verbose".to_string()]),
            Ok(Action::Run {
                path: None,
                verbose: true,
            })
        );
    }

    #[test]
    fn check_config_rejects_verbose_logging() {
        assert_eq!(
            parse_args(["--check-config".to_string(), "--verbose".to_string()]),
            Err("`--verbose` cannot be used with `--check-config`".to_string())
        );
    }

    #[test]
    fn check_config_keeps_the_explicit_path() {
        assert_eq!(
            parse_args([
                "--check-config".to_string(),
                "--config".to_string(),
                "idle.toml".to_string(),
            ]),
            Ok(Action::CheckConfig(Some(PathBuf::from("idle.toml"))))
        );
    }

    #[test]
    fn status_selects_the_control_client() {
        assert_eq!(
            parse_args(["status".to_string()]),
            Ok(Action::Status(StatusOutput::Text))
        );
    }

    #[test]
    fn reload_selects_the_control_client() {
        assert_eq!(parse_args(["reload".to_string()]), Ok(Action::Reload));
    }

    #[test]
    fn status_json_selects_machine_readable_output() {
        assert_eq!(
            parse_args(["status".to_string(), "--json".to_string()]),
            Ok(Action::Status(StatusOutput::Json))
        );
        assert_eq!(
            parse_args(["--json".to_string(), "status".to_string()]),
            Ok(Action::Status(StatusOutput::Json))
        );
    }

    #[test]
    fn status_rejects_daemon_options() {
        assert_eq!(
            parse_args(["status".to_string(), "--verbose".to_string()]),
            Err("control commands cannot be combined with daemon or config options".to_string())
        );
    }

    #[test]
    fn pause_and_resume_select_control_actions() {
        assert_eq!(parse_args(["pause".to_string()]), Ok(Action::Pause));
        assert_eq!(parse_args(["resume".to_string()]), Ok(Action::Resume));
    }

    #[test]
    fn json_is_rejected_without_status() {
        assert_eq!(
            parse_args(["pause".to_string(), "--json".to_string()]),
            Err("`--json` can only be used with `status`".to_string())
        );
        assert_eq!(
            parse_args(["--json".to_string()]),
            Err("`--json` can only be used with `status`".to_string())
        );
    }

    #[test]
    fn rejects_multiple_control_commands() {
        assert_eq!(
            parse_args(["pause".to_string(), "resume".to_string()]),
            Err("only one control command may be specified".to_string())
        );
    }
}
