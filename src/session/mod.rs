mod inhibit;
mod logind;
mod screensaver;

pub use logind::SessionError;
pub use screensaver::DbusInhibitError;

#[cfg(test)]
pub(crate) use inhibit::InhibitorInfo;
pub(crate) use logind::{DelayInhibitor, LogindFeatures, LogindMonitor, SessionEvent};
pub(crate) use screensaver::{DbusInhibitEvent, DbusInhibitMonitor};
