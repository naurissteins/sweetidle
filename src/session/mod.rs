mod logind;

pub use logind::SessionError;

pub(crate) use logind::{DelayInhibitor, LogindFeatures, LogindMonitor, SessionEvent};
