//! Vercel Sandbox provider: the pure, substrate-free half (request bodies,
//! response parsing, spend policy, the ustar writer and config admission).
//! The HTTP client, exec bridge and lifecycle live in `thegn_svc::vercel`.

pub mod admit;
pub mod api;
pub mod policy;
pub mod ustar;
