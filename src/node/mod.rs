//! The node: a workspace whose models are declared in a manifest and placed
//! from a mirror.

pub mod add;
pub mod config;
pub mod gc;
pub mod inspect;
pub mod lock;
pub mod manifest;
pub mod placed;
pub mod plan;
pub mod remote;
pub mod resolve;
pub mod sync;
pub mod update;
pub mod workspace;
