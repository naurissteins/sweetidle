use std::path::PathBuf;

pub(super) const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Action {
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
pub(super) enum StatusOutput {
    Text,
    Json,
}

pub(super) fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Action, String> {
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

pub(super) fn print_help() {
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
