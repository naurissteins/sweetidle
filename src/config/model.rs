#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Config {
    pub general: GeneralConfig,
    pub listeners: Vec<ListenerConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GeneralConfig {
    pub lock_command: Option<String>,
    pub before_sleep_command: Option<String>,
    pub after_resume_command: Option<String>,
    pub ignore_dbus_inhibit: bool,
    pub ignore_systemd_inhibit: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListenerConfig {
    pub timeout_seconds: u64,
    pub on_timeout: Option<String>,
    pub on_resume: Option<String>,
}
