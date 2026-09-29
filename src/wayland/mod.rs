// keep protocol dispatch behind the idle connection interface
mod idle;

pub(crate) use idle::IdleWaylandState;
pub use idle::{IdleEvent, IdleWaylandConnection, IdleWaylandError};
