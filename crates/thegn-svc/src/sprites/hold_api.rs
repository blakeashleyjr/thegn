//! The keep-awake hold's Tasks-API policy, shared with the host bridge via
//! `thegn_core::sprite_hold` so both sides agree on expiry, cadence, ceiling
//! and the task name. The hold itself runs inside the sprite (the
//! [`HOLD_SCRIPT`] heartbeat); nothing in svc polls the Tasks API.

pub use thegn_core::sprite_hold::{
    HOLD_DEFAULT_CEILING_SECS, HOLD_EXPIRE_SECS, HOLD_MAX_CEILING_SECS, HOLD_REFRESH_SECS,
    HOLD_SCRIPT, ceiling_secs, hold_name, wrap_argv,
};
