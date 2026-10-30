//! Chanto's core: the parts of the agent that don't touch the operating
//! system, so the CLI and the WebAssembly build share them.

pub mod edit;
pub mod fs;
pub mod limits;
pub mod message;
pub mod model;
pub mod read;
pub mod search;
pub mod text;
