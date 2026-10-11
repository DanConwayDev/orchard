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
    /// Nostr pubkey (hex or npub; normalize later, M3 decision).
    pub pubkey: String,
    /// nip05 address (e.g. `name@example.com`), the analog of Flathub's
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
    /// `sha256:…`. MUST match the registry manifest, else refuse to install.
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

/// The MVP catalog transport: a JSON document with the same information the
/// Nostr events will carry, one entry per published app. When the Nostr read
/// path lands (M3) this becomes a projection of kind 30063/30064 events and
/// the schema below is the event payload contract, so the client code does
/// not change shape.
///
/// Served by `orchard-indexd` at `/v1/catalog` and read by the client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogFile {
    pub version: u32,
    pub apps: Vec<CatalogApp>,
}

/// One user review (kind 30065). Replaceable per (pubkey, app id): a
/// reviewer edits by republishing, so one identity holds at most one
/// review per app. Author name fields resolve from the author's kind 0
/// profile when the feed has seen one.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Review {
    pub app_id: String,
    pub pubkey: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub nip05: Option<String>,
    /// 0.0 to 5.0, one decimal. Absent: text-only review.
    #[serde(default)]
    pub rating: Option<f32>,
    pub content: String,
    pub created_at: i64,
}

/// The reviews transport: served by `orchard-indexd` at `/v1/reviews` and
/// read by the client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewsFile {
    pub version: u32,
    pub reviews: Vec<Review>,
    /// The earliest event indexd has on record per reviewer pubkey,
    /// unix seconds. Reviews from keys first seen under a week ago are
    /// marked in the client; absence means unknown age, never new.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub first_seen: std::collections::BTreeMap<String, u64>,
}

/// The reviewer contact-list graph: served by `orchard-indexd` at
/// `/v1/graph`, read by the client. Public data on purpose: the
/// client computes personal distance (who is two steps away) from it
/// locally, so the server never learns who is asking.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphFile {
    pub version: u32,
    /// Reviewer pubkey -> the pubkeys their newest kind 3 follows.
    pub graph: std::collections::BTreeMap<String, Vec<String>>,
}

/// The profiles transport: the raw kind 0 metadata each author last
/// signed, served by `orchard-indexd` at `/v1/profiles`. Raw on
/// purpose: kind 0 is replaceable, so a client that republishes a
/// subset of the fields would drop the rest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfilesFile {
    pub version: u32,
    /// pubkey -> the metadata JSON as signed.
    pub profiles: std::collections::BTreeMap<String, String>,
}

/// The client's private memory of which publisher keys it has seen
/// per app id: the continuity record behind the new-key disclosure.
/// Rotation is legitimate; the disclosure is a fact, not a verdict.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ContinuityFile {
    pub version: u32,
    /// app id -> the publisher keys seen for it.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub apps: std::collections::BTreeMap<String, ContinuityKeys>,
}

/// The keys seen for one app id, and the ones already disclosed as a
/// change (the fact sticks: a disclosure is not un-seen).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ContinuityKeys {
    /// Every publisher key ever seen for the app id, discovery order.
    pub keys: Vec<String>,
    /// Keys this client flagged as a change.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub flagged: Vec<String>,
}

/// One published app in the MVP catalog: a [Listing] plus its latest [Release].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogApp {
    pub app_id: String,
    pub name: String,
    pub summary: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub icon_url: Option<String>,
    #[serde(default)]
    pub categories: Vec<String>,
    pub publisher: Publisher,
    pub version: String,
    /// Full flatpak ref, e.g. `app/com.example.Hello/x86_64/stable`.
    pub flatpak_ref: String,
    /// Artifact location: `oci:<dir>` or `docker://<registry>/<repo>:<tag>`.
    pub location: String,
    /// Pinned manifest digest, `sha256:…`. Installs refuse to run on mismatch.
    #[serde(default)]
    pub digest: Option<String>,
    /// Flatpak metadata ini, for the permissions viewer.
    #[serde(default)]
    pub metadata: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
    #[serde(default)]
    pub published_at: i64,
    /// The previous publisher key that signed a migration statement
    /// naming this row's publisher, when one exists: the difference
    /// between an authorized handover and an unexplained new key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub successor_to: Option<String>,
}

/// Which half of the store an app came from, for badges and install routing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// Flathub: the reviewed, centralized channel.
    Reviewed,
    /// An Orchard catalog: permissionless, publisher-owned, digest-pinned.
    Publisher,
}

/// The merged, UI-facing app record. One of these per row in the client,
/// whether it came from AppStream (reviewed) or the Orchard catalog.
#[derive(Debug, Clone)]
pub struct StoreApp {
    pub app_id: String,
    pub name: String,
    pub summary: String,
    pub description: Option<String>,
    pub version: Option<String>,
    /// Latest release date, `YYYY-MM-DD` (reviewed channel).
    pub release_date: Option<String>,
    /// Project homepage URL (reviewed channel).
    pub homepage: Option<String>,
    pub license: Option<String>,
    pub source: Source,
    pub icon_path: Option<std::path::PathBuf>,
    pub icon_url: Option<String>,
    pub categories: Vec<String>,
    /// Screenshot image URLs, best first (reviewed channel).
    pub screenshots: Vec<String>,
    /// Publisher pubkey (publisher channel only).
    pub publisher: Option<String>,
    /// Flatpak metadata ini (publisher channel only).
    pub metadata: Option<String>,
    /// Artifact location string (publisher channel only).
    pub location: Option<String>,
    /// Pinned digest (publisher channel only).
    pub pinned_digest: Option<String>,
    /// The previous publisher key that designated this row's
    /// publisher (publisher channel only).
    pub successor_to: Option<String>,
}

impl StoreApp {
    pub fn from_appstream(app: &crate::appstream::AppstreamApp) -> Self {
        StoreApp {
            app_id: app.id.clone(),
            name: app.name.clone(),
            summary: app.summary.clone(),
            description: app.description.clone(),
            version: app.version.clone(),
            release_date: app.release_date.clone(),
            homepage: app.homepage.clone(),
            license: app.license.clone(),
            source: Source::Reviewed,
            icon_path: app.icon_path.clone(),
            icon_url: app.icon_url.clone(),
            categories: app.categories.clone(),
            screenshots: app.screenshots.clone(),
            publisher: None,
            metadata: None,
            location: None,
            pinned_digest: None,
            successor_to: None,
        }
    }

    pub fn from_catalog(app: &CatalogApp) -> Self {
        StoreApp {
            app_id: app.app_id.clone(),
            name: app.name.clone(),
            summary: app.summary.clone(),
            description: app.description.clone(),
            version: Some(app.version.clone()),
            release_date: None,
            homepage: None,
            license: None,
            source: Source::Publisher,
            icon_path: None,
            icon_url: app.icon_url.clone(),
            categories: app.categories.clone(),
            screenshots: Vec::new(),
            publisher: Some(app.publisher.pubkey.clone()),
            metadata: app.metadata.clone(),
            location: Some(app.location.clone()),
            pinned_digest: app.digest.clone(),
            successor_to: app.successor_to.clone(),
        }
    }
}
