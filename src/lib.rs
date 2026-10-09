// keep platform adapters separate from listener and configuration policy
mod command;
pub mod config;
pub mod control;
mod listener;
pub mod runtime;
mod session;
mod wayland;

#[cfg(test)]
mod test_support;

pub use command::ProcessError;
