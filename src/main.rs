use std::path::PathBuf;

use sweetsidle::{
    command::ProcessError,
    config::Config,
    runtime::{IdleRuntime, RuntimeEvent},
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
        Action::CheckConfig(path) => check_config_file(path)?,
        Action::Run { path, verbose } => run_daemon(path, verbose)?,
    }

    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
enum Action {
    Help,
    Version,
    CheckConfig(Option<PathBuf>),
    Run {
        path: Option<PathBuf>,
        verbose: bool,
    },
}

fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Action, String> {
    let mut check_config = false;
    let mut config_path = None;
    let mut verbose = false;
    let mut args = args.into_iter();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(Action::Help),
            "-V" | "--version" => return Ok(Action::Version),
            "-v" | "--verbose" => verbose = true,
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

    if check_config {
        if verbose {
            return Err("`--verbose` cannot be used with `--check-config`".to_string());
        }
        Ok(Action::CheckConfig(config_path))
    } else {
        Ok(Action::Run {
            path: config_path,
            verbose,
        })
    }
}

fn print_help() {
    println!(
        "\
sweetsidle {VERSION}

Usage: sweetsidle [OPTIONS]

Options:
  -c, --config <PATH>  Use a specific config file
      --check-config   Parse and validate the config file, then exit
  -v, --verbose        Enable debug logging while running
  -h, --help           Print help
  -V, --version        Print version
"
    );
}

fn check_config_file(path: Option<PathBuf>) -> Result<(), String> {
    let loaded = load_config(path)?;
    match loaded.config.idle_hint_timeout_seconds {
        Some(timeout) => println!(
            "configuration ok: {} listener(s), idlehint timeout: {timeout}s",
            loaded.config.listeners.len()
        ),
        None => println!(
            "configuration ok: {} listener(s), idlehint disabled",
            loaded.config.listeners.len()
        ),
    }
    Ok(())
}

fn run_daemon(path: Option<PathBuf>, verbose: bool) -> Result<(), String> {
    init_logging(verbose)?;
    let loaded = load_config(path)?;
    let mut runtime = IdleRuntime::connect(&loaded.config).map_err(|error| error.to_string())?;

    info!(
        config = %loaded.path.display(),
        listeners = loaded.config.listeners.len(),
        idle_hint_timeout_seconds = ?loaded.config.idle_hint_timeout_seconds,
        "sweetsidle started"
    );

    loop {
        let events = runtime.dispatch(None).map_err(|error| error.to_string())?;
        let mut shutdown = false;

        for event in events {
            shutdown |= log_runtime_event(event);
        }

        if shutdown {
            info!("sweetsidle stopped");
            return Ok(());
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

fn log_runtime_event(event: RuntimeEvent) -> bool {
    match event {
        RuntimeEvent::ShutdownRequested(signal) => {
            info!(signal = signal.name(), "shutdown requested");
            true
        }
        RuntimeEvent::PreparingForSleep => {
            info!("preparing for system sleep");
            false
        }
        RuntimeEvent::ResumedFromSleep => {
            info!("system resumed from sleep");
            false
        }
        RuntimeEvent::LockRequested => {
            info!("session lock requested");
            false
        }
        RuntimeEvent::UnlockRequested => {
            info!("session unlock requested");
            false
        }
        RuntimeEvent::IdleHintChanged { idle } => {
            debug!(idle, "logind session idle hint changed");
            false
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
            false
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
            false
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
            false
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
            false
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
            false
        }
        RuntimeEvent::CommandStarted(start) => {
            info!(
                command_id = start.command_id.index(),
                context = ?start.context,
                process_id = start.process_id,
                "command started"
            );
            false
        }
        RuntimeEvent::CommandSkipped(decision) => {
            debug!(
                context = ?decision.context,
                reason = ?decision.reason,
                "command skipped"
            );
            false
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
            false
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
            false
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
    let source = std::fs::read_to_string(&path)
        .map_err(|error| format!("failed to read `{}`: {error}", path.display()))?;
    let config = Config::parse(&source)
        .map_err(|error| format!("invalid config `{}`: {error}", path.display()))?;

    Ok(LoadedConfig { path, config })
}

fn default_config_path() -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME")
        .ok_or_else(|| "HOME is not set; pass --config <PATH>".to_string())?;

    Ok(PathBuf::from(home).join(".config/sweetsidle/config.kdl"))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{Action, parse_args};

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
            parse_args(["--config".to_string(), "idle.kdl".to_string()]),
            Ok(Action::Run {
                path: Some(PathBuf::from("idle.kdl")),
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
                "idle.kdl".to_string(),
            ]),
            Ok(Action::CheckConfig(Some(PathBuf::from("idle.kdl"))))
        );
    }
}
