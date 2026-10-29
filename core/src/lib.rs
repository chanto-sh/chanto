//! Chanto's core: the parts of the agent that don't touch the operating
//! system, so the CLI and the WebAssembly build share them.

pub mod limits;
pub mod message;
pub mod model;
