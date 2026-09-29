#[derive(Debug, Clone, PartialEq, Eq, Default)]
// one typed shape feeds listeners and runtime setup
pub struct Config {
    pub general: GeneralConfig,
    pub idle_hint_timeout_seconds: Option<u64>,
    pub listeners: Vec<ListenerConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneralConfig {
    pub lock_command: Option<String>,
    pub unlock_command: Option<String>,
    pub before_sleep_command: Option<String>,
    pub after_resume_command: Option<String>,
    pub wait_before_sleep: bool,
    pub ignore_dbus_inhibit: bool,
    pub ignore_systemd_inhibit: bool,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        // sleep hooks wait by default so locking can finish before suspend
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListenerConfig {
    pub timeout_seconds: u64,
    pub on_timeout: Option<String>,
    pub on_resume: Option<String>,
    pub ignore_inhibit: bool,
}
