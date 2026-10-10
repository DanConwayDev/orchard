//! # orchard-core
//!
//! Domain types and verification logic shared by every Orchard component:
//! the catalog model ([`model`]) and the permission translator ([`permissions`]).
//!
//! Deliberately dependency-light: Nostr transport, HTTP, and UI live elsewhere.

pub mod model;
pub mod permissions;

pub use model::*;
pub use permissions::*;
