mod cli;
mod event_log;

use std::{
    ffi::OsString,
    io,
    path::{Path, PathBuf},
};

use sweetidle::{
    config::{Config, ConfigLoadError},
    control,
    runtime::{ConfigReloadOutcome, IdleRuntime},
};
use tracing::{info, warn};

use self::{
    cli::{Action, StatusOutput, VERSION, parse_args, print_help},
    event_log::{RuntimeControl, handle_runtime_event, init_logging},
};

fn main() {
    if let Err(error) = run(std::env::args_os().skip(1)) {
        eprintln!("error: {error}");
        std::process::exit(2);
    }
}

fn run(args: impl IntoIterator<Item = OsString>) -> Result<(), String> {
    match parse_args(args)? {
        Action::Help => print_help(),
        Action::Version => println!("sweetidle {VERSION}"),
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
        "sweetidle started"
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
            info!("sweetidle stopped");
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

struct LoadedConfig {
    path: PathBuf,
    config: Config,
}

fn load_config(path: Option<PathBuf>) -> Result<LoadedConfig, String> {
    let path = match path {
        Some(path) => path,
        None => default_config_path()?,
    };
    let config = Config::load(&path).map_err(|error| match &error {
        ConfigLoadError::Read { source, .. } if source.kind() == io::ErrorKind::NotFound => {
            format!("{error}\n{}", missing_config_hint(&path))
        }
        _ => error.to_string(),
    })?;

    Ok(LoadedConfig { path, config })
}

fn missing_config_hint(path: &Path) -> String {
    // packaged builds ship the example under share/doc next to bin
    let example = std::env::current_exe()
        .ok()
        .and_then(|exe| {
            Some(
                exe.parent()?
                    .parent()?
                    .join("share/doc/sweetidle/example.toml"),
            )
        })
        .filter(|example| example.is_file());
    let Some(example) = example else {
        return format!(
            "hint: copy config/example.toml from the sweetidle source to `{}`",
            path.display()
        );
    };
    // install makes the parent directory and drops the read-only nix store mode
    format!(
        "hint: start from the example config:\n  install -Dm644 '{}' '{}'",
        example.display(),
        path.display()
    )
}

fn default_config_path() -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME")
        .ok_or_else(|| "HOME is not set; pass --config <PATH>".to_string())?;
    let home = PathBuf::from(home);
    if !home.is_absolute() {
        return Err("HOME must be absolute; pass --config <PATH>".to_string());
    }

    Ok(home.join(".config/sweetidle/config.toml"))
}
