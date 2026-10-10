//! Catalog domain model.
//!
//! These types are the shared vocabulary between the Nostr event layer (M3),
//! the local index shim ([`orchard_indexd`]), and the GPUI client. They are
//! intentionally transport-agnostic: a `Listing` can be built from a Nostr
//! event, from Flathub AppStream, or from a test fixture.

use serde::{Deserialize, Serialize};

/// Flatpak application id, e.g. `org.gnome.Polari`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AppId(pub String);

impl AppId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A publisher, identified by their Nostr public key.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Publisher {
    /// Nostr pubkey (hex or npub; normalize later — M3 decision).
    pub pubkey: String,
    /// nip05 address (e.g. `name@example.com`) — the analog of Flathub's
    /// "verified" badge. `Some` only after the client validated it.
    pub nip05: Option<String>,
}

/// Which channel an app belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Channel {
    /// Ingested read-only from the user's flathub remote.
    Reviewed,
    /// Published through Orchard's permissionless Nostr protocol.
    PublisherVerified,
}

/// A pointer to an OCI artifact holding the app. The digest is the thing the
/// client verifies before install; the tag is just a convenience.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactRef {
    /// e.g. `ghcr.io`
    pub registry: String,
    /// e.g. `owner/repo`
    pub repository: String,
    pub tag: Option<String>,
    /// `sha256:…` — MUST match the registry manifest, else refuse to install.
    pub digest: Option<String>,
}

/// A published release of an app.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Release {
    pub version: String,
    pub artifact: ArtifactRef,
    pub notes: Option<String>,
    pub published_at: i64,
}

/// A complete app listing as shown in the catalog.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Listing {
    pub app_id: AppId,
    pub name: String,
    pub summary: String,
    pub description: Option<String>,
    pub icon_url: Option<String>,
    pub publisher: Publisher,
    pub channel: Channel,
    /// Latest first, empty until the first release event is seen.
    pub releases: Vec<Release>,
}
