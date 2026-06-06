//! Result sources: each turns a mode + query into rows for the engine. Some are
//! synchronous (small static/db-backed lists), others stream from a worker
//! thread (file walk, ripgrep) into a cloned injector.

pub mod command;
pub mod content;
pub mod file;
pub mod git;
pub mod nav;
