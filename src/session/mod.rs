// keep D-Bus service state separate from logind session monitoring
mod inhibit;
mod logind;
mod screensaver;
mod systemd_inhibit;

pub use logind::SessionError;
pub use screensaver::DbusInhibitError;
pub use systemd_inhibit::SystemdIdleInhibitor;

pub(crate) use inhibit::InhibitorInfo;
pub(crate) use logind::{DelayInhibitor, LogindFeatures, LogindMonitor, SessionEvent};
pub(crate) use screensaver::{DbusInhibitEvent, DbusInhibitMonitor};
pub(crate) use systemd_inhibit::{LogindInhibitor, idle_block_inhibitors};
