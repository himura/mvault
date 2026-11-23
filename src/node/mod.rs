//! The node: a workspace whose models are declared in a manifest and placed
//! from a mirror.

pub mod config;
pub mod lock;
pub mod manifest;
pub mod plan;
pub mod remote;
pub mod resolve;
pub mod sync;
pub mod workspace;
