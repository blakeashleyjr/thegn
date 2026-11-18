//! The keep-awake hold's Tasks-API constants, shared with the host bridge via
//! `thegn_core::sprite_hold` so both sides agree on expiry and cadence.

pub use thegn_core::sprite_hold::{HOLD_EXPIRE_SECS, HOLD_REFRESH_SECS};
