// keep protocol bindings and dispatch behind the idle adapter
mod idle;

pub use idle::IdleWaylandError;
pub(crate) use idle::{IdleEvent, IdleWaylandParts, IdleWaylandState};
