mod error;
mod protocol;
mod socket;

pub use error::ControlError;
pub use socket::{pause, query_status, query_status_json, resume};

pub(crate) use protocol::{
    ControlEvent, ControlRequest, DbusInhibitorStatus, StatusFormat, StatusSnapshot,
};
pub(crate) use socket::ControlMonitor;
