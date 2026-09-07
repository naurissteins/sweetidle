use std::{error::Error, fmt};

use kdl::{KdlDocument, KdlNode, KdlValue};

use super::{Config, GeneralConfig, ListenerConfig};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    Parse(String),
    UnknownTopLevel(String),
    DuplicateNode(String),
    UnexpectedEntries(String),
    MissingField { node: String, field: String },
    UnknownField { parent: String, field: String },
    DuplicateField { parent: String, field: String },
    InvalidType { field: String, expected: String },
    InvalidValue { field: String, reason: String },
}

impl Config {
    pub fn parse(source: &str) -> Result<Self, ConfigError> {
        let document = source
            .parse::<KdlDocument>()
            .map_err(|error| ConfigError::Parse(error.to_string()))?;

        Self::from_document(&document)
    }

    fn from_document(document: &KdlDocument) -> Result<Self, ConfigError> {
        let mut general = None;
        let mut idle_hint_timeout_seconds = None;
        let mut listeners = Vec::new();

        for node in document.nodes() {
            match node.name().value() {
                "general" => {
                    if general.is_some() {
                        return Err(ConfigError::DuplicateNode("general".to_string()));
                    }

                    general = Some(parse_general(node)?);
                }
                "idlehint" => {
                    if idle_hint_timeout_seconds.is_some() {
                        return Err(ConfigError::DuplicateNode("idlehint".to_string()));
                    }

                    idle_hint_timeout_seconds = Some(parse_idle_hint(node)?);
                }
                "listener" => listeners.push(parse_listener(node)?),
                name => return Err(ConfigError::UnknownTopLevel(name.to_string())),
            }
        }

        Ok(Self {
            general: general.unwrap_or_default(),
            idle_hint_timeout_seconds,
            listeners,
        })
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse(message) => write!(formatter, "failed to parse KDL: {message}"),
            Self::UnknownTopLevel(name) => write!(formatter, "unknown top-level node `{name}`"),
            Self::DuplicateNode(name) => write!(formatter, "duplicate `{name}` node"),
            Self::UnexpectedEntries(node) => {
                write!(formatter, "`{node}` must not have positional arguments")
            }
            Self::MissingField { node, field } => {
                write!(formatter, "`{node}` is missing required field `{field}`")
            }
            Self::UnknownField { parent, field } => {
                write!(formatter, "unknown field `{field}` in `{parent}`")
            }
            Self::DuplicateField { parent, field } => {
                write!(formatter, "duplicate field `{field}` in `{parent}`")
            }
            Self::InvalidType { field, expected } => {
                write!(formatter, "`{field}` must be {expected}")
            }
            Self::InvalidValue { field, reason } => {
                write!(formatter, "`{field}` is invalid: {reason}")
            }
        }
    }
}

impl Error for ConfigError {}

fn parse_general(node: &KdlNode) -> Result<GeneralConfig, ConfigError> {
    ensure_no_entries("general", node)?;

    let mut config = GeneralConfig::default();
    let mut seen = SeenFields::default();

    for child in node.iter_children() {
        let field = child.name().value();

        match field {
            "lock-command" => {
                seen.insert("general", field)?;
                config.lock_command = Some(required_command(field, child)?);
            }
            "unlock-command" => {
                seen.insert("general", field)?;
                config.unlock_command = Some(required_command(field, child)?);
            }
            "before-sleep-command" => {
                seen.insert("general", field)?;
                config.before_sleep_command = Some(required_command(field, child)?);
            }
            "after-resume-command" => {
                seen.insert("general", field)?;
                config.after_resume_command = Some(required_command(field, child)?);
            }
            "wait-before-sleep" => {
                seen.insert("general", field)?;
                config.wait_before_sleep = required_bool(field, child)?;
            }
            "ignore-dbus-inhibit" => {
                seen.insert("general", field)?;
                config.ignore_dbus_inhibit = required_bool(field, child)?;
            }
            "ignore-systemd-inhibit" => {
                seen.insert("general", field)?;
                config.ignore_systemd_inhibit = required_bool(field, child)?;
            }
            _ => {
                return Err(ConfigError::UnknownField {
                    parent: "general".to_string(),
                    field: field.to_string(),
                });
            }
        }
    }

    Ok(config)
}

fn parse_listener(node: &KdlNode) -> Result<ListenerConfig, ConfigError> {
    ensure_only_property("listener", "timeout", node)?;

    let timeout_seconds = required_timeout("listener", node)?;
    let mut on_timeout = None;
    let mut on_resume = None;
    let mut seen = SeenFields::default();

    for child in node.iter_children() {
        let field = child.name().value();

        match field {
            "on-timeout" => {
                seen.insert("listener", field)?;
                on_timeout = Some(required_command(field, child)?);
            }
            "on-resume" => {
                seen.insert("listener", field)?;
                on_resume = Some(required_command(field, child)?);
            }
            _ => {
                return Err(ConfigError::UnknownField {
                    parent: "listener".to_string(),
                    field: field.to_string(),
                });
            }
        }
    }

    Ok(ListenerConfig {
        timeout_seconds,
        on_timeout,
        on_resume,
    })
}

fn parse_idle_hint(node: &KdlNode) -> Result<u64, ConfigError> {
    ensure_only_property("idlehint", "timeout", node)?;
    if let Some(child) = node.iter_children().next() {
        return Err(ConfigError::UnknownField {
            parent: "idlehint".to_string(),
            field: child.name().value().to_string(),
        });
    }

    required_timeout("idlehint", node)
}

fn ensure_no_entries(name: &str, node: &KdlNode) -> Result<(), ConfigError> {
    if node.entries().is_empty() {
        Ok(())
    } else {
        Err(ConfigError::UnexpectedEntries(name.to_string()))
    }
}

fn ensure_only_property(name: &str, property: &str, node: &KdlNode) -> Result<(), ConfigError> {
    let mut found = false;
    for entry in node.entries() {
        match entry.name().map(|entry_name| entry_name.value()) {
            Some(field) if field == property && !found => found = true,
            Some(field) if field == property => {
                return Err(ConfigError::DuplicateField {
                    parent: name.to_string(),
                    field: property.to_string(),
                });
            }
            _ => return Err(ConfigError::UnexpectedEntries(name.to_string())),
        }
    }

    Ok(())
}

fn required_timeout(name: &str, node: &KdlNode) -> Result<u64, ConfigError> {
    let value = node
        .get("timeout")
        .ok_or_else(|| ConfigError::MissingField {
            node: name.to_string(),
            field: "timeout".to_string(),
        })?;

    let Some(timeout) = value.as_integer() else {
        return Err(ConfigError::InvalidType {
            field: format!("{name}.timeout"),
            expected: "a positive integer".to_string(),
        });
    };

    u64::try_from(timeout)
        .ok()
        .filter(|timeout| *timeout > 0)
        .ok_or_else(|| ConfigError::InvalidValue {
            field: format!("{name}.timeout"),
            reason: "must be greater than zero".to_string(),
        })
}

fn required_command(field: &str, node: &KdlNode) -> Result<String, ConfigError> {
    let command = single_argument(field, node)?
        .as_string()
        .ok_or_else(|| ConfigError::InvalidType {
            field: field.to_string(),
            expected: "a string".to_string(),
        })?
        .trim()
        .to_string();

    if command.is_empty() {
        return Err(ConfigError::InvalidValue {
            field: field.to_string(),
            reason: "command must not be empty".to_string(),
        });
    }

    Ok(command)
}

fn required_bool(field: &str, node: &KdlNode) -> Result<bool, ConfigError> {
    single_argument(field, node)?
        .as_bool()
        .ok_or_else(|| ConfigError::InvalidType {
            field: field.to_string(),
            expected: "a boolean".to_string(),
        })
}

fn single_argument<'a>(field: &str, node: &'a KdlNode) -> Result<&'a KdlValue, ConfigError> {
    if node.entries().len() != 1 || node.entries()[0].name().is_some() {
        return Err(ConfigError::InvalidType {
            field: field.to_string(),
            expected: "exactly one positional argument".to_string(),
        });
    }

    if node.iter_children().next().is_some() {
        return Err(ConfigError::InvalidType {
            field: field.to_string(),
            expected: "a leaf node".to_string(),
        });
    }

    Ok(node.entries()[0].value())
}

#[derive(Default)]
struct SeenFields<'a> {
    fields: Vec<&'a str>,
}

impl<'a> SeenFields<'a> {
    fn insert(&mut self, parent: &str, field: &'a str) -> Result<(), ConfigError> {
        if self.fields.contains(&field) {
            return Err(ConfigError::DuplicateField {
                parent: parent.to_string(),
                field: field.to_string(),
            });
        }

        self.fields.push(field);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{Config, ConfigError};

    #[test]
    fn parses_general_and_listeners() {
        let source = r#"
general {
  lock-command "sweetslock"
  unlock-command "sweetslockctl unlock"
  before-sleep-command "sweetslock --daemonize"
  after-resume-command "sweetsmsg output '*' power on"
  wait-before-sleep #true
  ignore-dbus-inhibit #false
  ignore-systemd-inhibit #true
}

idlehint timeout=300

listener timeout=300 {
  on-timeout "sweetslock"
}

listener timeout=600 {
  on-timeout "sweetsmsg output '*' power off"
  on-resume "sweetsmsg output '*' power on"
}
"#;

        let config = match Config::parse(source) {
            Ok(config) => config,
            Err(error) => panic!("{error}"),
        };

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
        assert_eq!(
            config.listeners[1].on_resume.as_deref(),
            Some("sweetsmsg output '*' power on")
        );
    }

    #[test]
    fn rejects_unknown_top_level_nodes() {
        let error = match Config::parse("banana {}\n") {
            Ok(config) => panic!("unexpected config: {config:?}"),
            Err(error) => error,
        };

        assert_eq!(error, ConfigError::UnknownTopLevel("banana".to_string()));
    }

    #[test]
    fn rejects_missing_listener_timeout() {
        let error = match Config::parse("listener { on-timeout \"sweetslock\" }\n") {
            Ok(config) => panic!("unexpected config: {config:?}"),
            Err(error) => error,
        };

        assert_eq!(
            error,
            ConfigError::MissingField {
                node: "listener".to_string(),
                field: "timeout".to_string()
            }
        );
    }

    #[test]
    fn rejects_zero_listener_timeout() {
        let error = match Config::parse("listener timeout=0\n") {
            Ok(config) => panic!("unexpected config: {config:?}"),
            Err(error) => error,
        };

        assert_eq!(
            error,
            ConfigError::InvalidValue {
                field: "listener.timeout".to_string(),
                reason: "must be greater than zero".to_string()
            }
        );
    }

    #[test]
    fn rejects_duplicate_idle_hint_nodes() {
        let error = match Config::parse("idlehint timeout=60\nidlehint timeout=120\n") {
            Ok(config) => panic!("unexpected config: {config:?}"),
            Err(error) => error,
        };

        assert_eq!(error, ConfigError::DuplicateNode("idlehint".to_string()));
    }

    #[test]
    fn rejects_duplicate_idle_hint_timeout_properties() {
        let error = match Config::parse("idlehint timeout=60 timeout=120\n") {
            Ok(config) => panic!("unexpected config: {config:?}"),
            Err(error) => error,
        };

        assert_eq!(
            error,
            ConfigError::DuplicateField {
                parent: "idlehint".to_string(),
                field: "timeout".to_string(),
            }
        );
    }

    #[test]
    fn rejects_zero_idle_hint_timeout() {
        let error = match Config::parse("idlehint timeout=0\n") {
            Ok(config) => panic!("unexpected config: {config:?}"),
            Err(error) => error,
        };

        assert_eq!(
            error,
            ConfigError::InvalidValue {
                field: "idlehint.timeout".to_string(),
                reason: "must be greater than zero".to_string()
            }
        );
    }

    #[test]
    fn rejects_empty_commands() {
        let error = match Config::parse("listener timeout=1 { on-timeout \"  \" }\n") {
            Ok(config) => panic!("unexpected config: {config:?}"),
            Err(error) => error,
        };

        assert_eq!(
            error,
            ConfigError::InvalidValue {
                field: "on-timeout".to_string(),
                reason: "command must not be empty".to_string()
            }
        );
    }

    #[test]
    fn rejects_empty_unlock_command() {
        let error = match Config::parse("general { unlock-command \"  \" }\n") {
            Ok(config) => panic!("unexpected config: {config:?}"),
            Err(error) => error,
        };

        assert_eq!(
            error,
            ConfigError::InvalidValue {
                field: "unlock-command".to_string(),
                reason: "command must not be empty".to_string()
            }
        );
    }
}
