//! Sprites (`sprites.dev`) lifecycle pieces that sit beside the
//! [`SpritesProvider`](crate::provider::SpritesProvider) REST client:
//!
//! - [`ledger`]: ownership. A `creating` record is written to the file ledger
//!   (`vps::registry`, provider `sprites`) **before** `POST /v1/sprites`, and
//!   `destroy` refuses a sprite this host cannot prove it created.
//! - [`marker`]: the creation marker on the sprite that lets another instance
//!   on the same account adopt it on reattach without `thegn env adopt`.
//! - [`resources`]: the memory policy (`min_ram_gb` × 1024 MiB).
//! - [`usage`]: CPU and reserved GB-hours from the Sprites usage API.
//! - [`hold_api`]: the keep-awake Tasks-API hold constants.
//! - [`inspect`]: a single sprite's status over the org API (never wakes it).
//! - [`job`]: selected build/test attempts on a project-bound sprite — reserved
//!   in a per-project ledger before dispatch, materialized and digest-verified,
//!   fenced against silent reruns, and reported with a bounded receipt.

pub mod hold_api;
pub mod inspect;
pub mod job;
pub mod ledger;
pub mod marker;
pub mod resources;
pub mod usage;

/// The borrowed HTTP context one sprite's API calls need: built by
/// [`SpritesProvider::sprites_http`](crate::provider::SpritesProvider) after the
/// API base has been validated, so the submodules never see an unchecked base.
// The resources/usage lanes read every field; the spine stubs read only some.
#[allow(dead_code)]
pub(crate) struct SpritesHttp<'a> {
    pub client: &'a reqwest::Client,
    pub base: &'a str,
    pub token: &'a str,
    pub name: &'a str,
}
