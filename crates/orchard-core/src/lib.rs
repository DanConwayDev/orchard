//! # orchard-core
//!
//! Domain types and verification logic shared by every Orchard component:
//! the catalog model ([`model`]), the AppStream ingest ([`appstream`]), the
//! OCI artifact verifier ([`oci`]), and the permission translator
//! ([`permissions`]).

pub mod appstream;
pub mod model;
pub mod oci;
pub mod permissions;

pub use appstream::*;
pub use model::*;
pub use oci::*;
pub use permissions::*;
