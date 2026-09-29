use std::{error::Error, fmt};

use serde::Deserialize;

use super::{Config, GeneralConfig, ListenerConfig};

// idle notify timeouts are encoded as milliseconds in a u32
const MAX_TIMEOUT_SECONDS: u64 = u32::MAX as u64 / 1000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    Parse(String),
    MissingField { table: String, field: String },
    InvalidValue { field: String, reason: String },
}

impl Config {
    pub fn parse(source: &str) -> Result<Self, ConfigError> {
        let document = toml::from_str::<ConfigDocument>(source)
            .map_err(|error| ConfigError::Parse(error.to_string()))?;

        let general = parse_general(document.general)?;
        let idle_hint_timeout_seconds = document
            .idle_hint
            .map(|idle_hint| required_timeout("idle_hint", idle_hint.timeout))
            .transpose()?;
        let listeners = document
            .listener
            .into_iter()
            .enumerate()
            .map(|(index, listener)| parse_listener(index, listener))
            .collect::<Result<Vec<_>, _>>()?;

        Ok(Self {
            general,
            idle_hint_timeout_seconds,
            listeners,
        })
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse(message) => write!(formatter, "failed to parse TOML: {message}"),
            Self::MissingField { table, field } => {
                write!(formatter, "`{table}` is missing required field `{field}`")
            }
            Self::InvalidValue { field, reason } => {
                write!(formatter, "`{field}` is invalid: {reason}")
            }
        }
    }
}

impl Error for ConfigError {}

// unknown tables fail before any runtime configuration is built
#[derive(Debug, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct ConfigDocument {
    general: GeneralDocument,
    idle_hint: Option<IdleHintDocument>,
    listener: Vec<ListenerDocument>,
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct GeneralDocument {
    lock_command: Option<String>,
    unlock_command: Option<String>,
    before_sleep_command: Option<String>,
    after_resume_command: Option<String>,
    wait_before_sleep: bool,
    ignore_dbus_inhibit: bool,
    ignore_systemd_inhibit: bool,
}

impl Default for GeneralDocument {
    fn default() -> Self {
        Self {
            lock_command: None,
            unlock_command: None,
            before_sleep_command: None,
            after_resume_command: None,
            wait_before_sleep: true,
            ignore_dbus_inhibit: false,
            ignore_systemd_inhibit: false,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct IdleHintDocument {
    timeout: Option<i64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ListenerDocument {
    timeout: Option<i64>,
    on_timeout: Option<String>,
    on_resume: Option<String>,
    #[serde(default)]
    ignore_inhibit: bool,
}

fn parse_general(document: GeneralDocument) -> Result<GeneralConfig, ConfigError> {
    Ok(GeneralConfig {
        lock_command: optional_command("general.lock_command", document.lock_command)?,
        unlock_command: optional_command("general.unlock_command", document.unlock_command)?,
        before_sleep_command: optional_command(
            "general.before_sleep_command",
            document.before_sleep_command,
        )?,
        after_resume_command: optional_command(
            "general.after_resume_command",
            document.after_resume_command,
        )?,
        wait_before_sleep: document.wait_before_sleep,
        ignore_dbus_inhibit: document.ignore_dbus_inhibit,
        ignore_systemd_inhibit: document.ignore_systemd_inhibit,
    })
}

fn parse_listener(index: usize, document: ListenerDocument) -> Result<ListenerConfig, ConfigError> {
    let table = format!("listener[{index}]");

    Ok(ListenerConfig {
        timeout_seconds: required_timeout(&table, document.timeout)?,
        on_timeout: optional_command(&format!("{table}.on_timeout"), document.on_timeout)?,
        on_resume: optional_command(&format!("{table}.on_resume"), document.on_resume)?,
        ignore_inhibit: document.ignore_inhibit,
    })
}

fn required_timeout(table: &str, value: Option<i64>) -> Result<u64, ConfigError> {
    let timeout = value.ok_or_else(|| ConfigError::MissingField {
        table: table.to_string(),
        field: "timeout".to_string(),
    })?;

    let timeout = u64::try_from(timeout)
        .ok()
        .filter(|timeout| *timeout > 0)
        .ok_or_else(|| ConfigError::InvalidValue {
            field: format!("{table}.timeout"),
            reason: "must be greater than zero".to_string(),
        })?;

    if timeout > MAX_TIMEOUT_SECONDS {
        return Err(ConfigError::InvalidValue {
            field: format!("{table}.timeout"),
            reason: format!("must be at most {MAX_TIMEOUT_SECONDS} seconds"),
        });
    }

    Ok(timeout)
}

fn optional_command(field: &str, value: Option<String>) -> Result<Option<String>, ConfigError> {
    value
        .map(|command| {
            let command = command.trim().to_string();
            if command.is_empty() {
                Err(ConfigError::InvalidValue {
                    field: field.to_string(),
                    reason: "command must not be empty".to_string(),
                })
            } else {
                Ok(command)
            }
        })
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::{Config, ConfigError, MAX_TIMEOUT_SECONDS};

    #[test]
    fn parses_general_idle_hint_and_listeners() {
        let source = r#"
[general]
lock_command = "sweetslock"
unlock_command = "sweetslockctl unlock"
before_sleep_command = "sweetslock --daemonize"
after_resume_command = "sweetsmsg output '*' power on"
wait_before_sleep = true
ignore_dbus_inhibit = false
ignore_systemd_inhibit = true

[idle_hint]
timeout = 300

[[listener]]
timeout = 300
on_timeout = "sweetslock"
ignore_inhibit = true

[[listener]]
timeout = 600
on_timeout = "sweetsmsg output '*' power off"
on_resume = "sweetsmsg output '*' power on"
"#;

        let config = Config::parse(source).unwrap_or_else(|error| panic!("{error}"));

        assert_eq!(config.general.lock_command.as_deref(), Some("sweetslock"));
        assert_eq!(
            config.general.unlock_command.as_deref(),
            Some("sweetslockctl unlock")
        );
        assert!(config.general.wait_before_sleep);
        assert!(config.general.ignore_systemd_inhibit);
        assert_eq!(config.idle_hint_timeout_seconds, Some(300));
        assert_eq!(config.listeners.len(), 2);
        assert_eq!(config.listeners[0].timeout_seconds, 300);
        assert!(config.listeners[0].ignore_inhibit);
        assert!(!config.listeners[1].ignore_inhibit);
        assert_eq!(
            config.listeners[1].on_resume.as_deref(),
            Some("sweetsmsg output '*' power on")
        );
    }

    #[test]
    fn omitted_general_table_uses_defaults() {
        let config = Config::parse("").unwrap_or_else(|error| panic!("{error}"));

        assert!(config.general.wait_before_sleep);
        assert!(!config.general.ignore_dbus_inhibit);
        assert!(!config.general.ignore_systemd_inhibit);
    }

    #[test]
    fn rejects_unknown_top_level_tables() {
        let error = parse_error("[banana]\nvalue = true\n");

        assert!(matches!(error, ConfigError::Parse(_)));
        assert!(error.to_string().contains("unknown field `banana`"));
    }

    #[test]
    fn rejects_unknown_general_fields() {
        let error = parse_error("[general]\nbanana = true\n");

        assert!(matches!(error, ConfigError::Parse(_)));
        assert!(error.to_string().contains("unknown field `banana`"));
    }

    #[test]
    fn rejects_unknown_listener_fields() {
        let error = parse_error("[[listener]]\ntimeout = 30\nbanana = true\n");

        assert!(matches!(error, ConfigError::Parse(_)));
        assert!(error.to_string().contains("unknown field `banana`"));
    }

    #[test]
    fn rejects_missing_listener_timeout() {
        let error = parse_error("[[listener]]\non_timeout = \"sweetslock\"\n");

        assert_eq!(
            error,
            ConfigError::MissingField {
                table: "listener[0]".to_string(),
                field: "timeout".to_string(),
            }
        );
    }

    #[test]
    fn rejects_non_positive_listener_timeout() {
        for timeout in [-1, 0] {
            let source = format!("[[listener]]\ntimeout = {timeout}\n");
            let error = parse_error(&source);

            assert_eq!(
                error,
                ConfigError::InvalidValue {
                    field: "listener[0].timeout".to_string(),
                    reason: "must be greater than zero".to_string(),
                }
            );
        }
    }

    #[test]
    fn accepts_largest_protocol_timeout_for_listener_and_idle_hint() {
        let source = format!(
            "[idle_hint]\ntimeout = {MAX_TIMEOUT_SECONDS}\n\n[[listener]]\ntimeout = {MAX_TIMEOUT_SECONDS}\n"
        );

        let config = Config::parse(&source).unwrap_or_else(|error| panic!("{error}"));

        assert_eq!(config.idle_hint_timeout_seconds, Some(MAX_TIMEOUT_SECONDS));
        assert_eq!(config.listeners[0].timeout_seconds, MAX_TIMEOUT_SECONDS);
    }

    #[test]
    fn rejects_listener_timeout_above_protocol_limit() {
        let source = format!("[[listener]]\ntimeout = {}\n", MAX_TIMEOUT_SECONDS + 1);

        assert_eq!(
            parse_error(&source),
            ConfigError::InvalidValue {
                field: "listener[0].timeout".to_string(),
                reason: format!("must be at most {MAX_TIMEOUT_SECONDS} seconds"),
            }
        );
    }

    #[test]
    fn rejects_idle_hint_timeout_above_protocol_limit() {
        let source = format!("[idle_hint]\ntimeout = {}\n", MAX_TIMEOUT_SECONDS + 1);

        assert_eq!(
            parse_error(&source),
            ConfigError::InvalidValue {
                field: "idle_hint.timeout".to_string(),
                reason: format!("must be at most {MAX_TIMEOUT_SECONDS} seconds"),
            }
        );
    }

    #[test]
    fn rejects_duplicate_idle_hint_tables() {
        let error = parse_error("[idle_hint]\ntimeout = 60\n[idle_hint]\ntimeout = 120\n");

        assert!(matches!(error, ConfigError::Parse(_)));
    }

    #[test]
    fn rejects_missing_idle_hint_timeout() {
        let error = parse_error("[idle_hint]\n");

        assert_eq!(
            error,
            ConfigError::MissingField {
                table: "idle_hint".to_string(),
                field: "timeout".to_string(),
            }
        );
    }

    #[test]
    fn rejects_zero_idle_hint_timeout() {
        let error = parse_error("[idle_hint]\ntimeout = 0\n");

        assert_eq!(
            error,
            ConfigError::InvalidValue {
                field: "idle_hint.timeout".to_string(),
                reason: "must be greater than zero".to_string(),
            }
        );
    }

    #[test]
    fn rejects_empty_commands() {
        let error = parse_error("[[listener]]\ntimeout = 1\non_timeout = \"  \"\n");

        assert_eq!(
            error,
            ConfigError::InvalidValue {
                field: "listener[0].on_timeout".to_string(),
                reason: "command must not be empty".to_string(),
            }
        );
    }

    #[test]
    fn rejects_empty_general_commands() {
        let error = parse_error("[general]\nunlock_command = \"  \"\n");

        assert_eq!(
            error,
            ConfigError::InvalidValue {
                field: "general.unlock_command".to_string(),
                reason: "command must not be empty".to_string(),
            }
        );
    }

    #[test]
    fn rejects_duplicate_fields() {
        let error = parse_error("[general]\nwait_before_sleep = true\nwait_before_sleep = false\n");

        assert!(matches!(error, ConfigError::Parse(_)));
    }

    #[test]
    fn rejects_invalid_boolean_types() {
        let error = parse_error("[general]\nwait_before_sleep = \"true\"\n");

        assert!(matches!(error, ConfigError::Parse(_)));
    }

    #[test]
    fn rejects_legacy_kdl_syntax() {
        let error = parse_error("general {\n  wait-before-sleep #true\n}\n");

        assert!(matches!(error, ConfigError::Parse(_)));
    }

    fn parse_error(source: &str) -> ConfigError {
        match Config::parse(source) {
            Ok(config) => panic!("unexpected config: {config:?}"),
            Err(error) => error,
        }
    }
}
