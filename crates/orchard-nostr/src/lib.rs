//! NIP-82 software events for Orchard.
//!
//! Orchard consumes the NIP-82 draft event kinds: the same ones Zapstore's
//! `zsp` publishes, so publishers are cross-compatible:
//!
//! | kind  | role              | `d` tag              |
//! |-------|-------------------|----------------------|
//! | 0     | publisher profile | (none)               |
//! | 32267 | app listing       | `<app-id>`           |
//! | 30063 | release           | `<app-id>@<version>` |
//! | 3063  | asset             | (none)               |
//!
//! Orchard's additive tags: `f` carries flatpak arches (`linux-x86_64`,
//! `linux-aarch64`), and the asset carries `oci` (artifact location) and
//! `runtime` (flatpak runtime requirement). Sandbox permissions are never in
//! events: the artifact's commit metadata (`xa.metadata`) is the single
//! source of truth, pinned by the asset's sha256.
//!
//! Everything here is offline: events arrive as signed NIP-01 JSON, are
//! verified (id hash + Schnorr signature), accumulated, and projected into
//! Orchard's [`CatalogFile`]. The relay transport lives elsewhere.

use std::collections::BTreeMap;

use anyhow::{bail, Context as _, Result};
use nostr::prelude::*;
use orchard_core::{CatalogApp, CatalogFile, Publisher};

pub mod connect;
pub mod events;
pub mod relay;

pub const KIND_PROFILE: u32 = 0;
pub const KIND_APP: u32 = 32267;
pub const KIND_RELEASE: u32 = 30063;
pub const KIND_ASSET: u32 = 3063;
pub const KIND_REVIEW: u32 = 30065;

/// The event fields Orchard reads, parsed once from the canonical JSON so
/// tag handling does not depend on the `Tag` API surface.
struct Parts {
    id: String,
    pubkey: String,
    created_at: u64,
    kind: u32,
    content: String,
    /// Tag rows as-is: `["d", "com.example.App"]`, …
    tags: Vec<Vec<String>>,
}

impl Parts {
    fn parse(event: &Event) -> Result<Self> {
        let value: serde_json::Value = serde_json::from_str(&event.as_json())?;
        let id = value["id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("event without id"))?
            .to_string();
        let pubkey = value["pubkey"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("event without pubkey"))?
            .to_string();
        let created_at = value["created_at"].as_u64().unwrap_or(0);
        let kind = value["kind"].as_u64().unwrap_or(0) as u32;
        let content = value["content"].as_str().unwrap_or_default().to_string();
        let tags = value["tags"]
            .as_array()
            .map(|rows| {
                rows.iter()
                    .filter_map(|row| {
                        row.as_array().map(|items| {
                            items
                                .iter()
                                .filter_map(|item| item.as_str().map(str::to_string))
                                .collect::<Vec<_>>()
                        })
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        Ok(Self {
            id,
            pubkey,
            created_at,
            kind,
            content,
            tags,
        })
    }

    /// First value of the named tag across all rows.
    fn tag(&self, name: &str) -> Option<String> {
        self.tags
            .iter()
            .find(|row| row.first().is_some_and(|key| key == name))
            .and_then(|row| row.get(1).cloned())
            .filter(|value| !value.is_empty())
    }

    /// Every value of the named tag (row's second element onward).
    fn tag_values(&self, name: &str) -> Vec<String> {
        self.tags
            .iter()
            .filter(|row| row.first().is_some_and(|key| key == name))
            .flat_map(|row| row.iter().skip(1).cloned())
            .filter(|value| !value.is_empty())
            .collect()
    }
}

/// One app listing (kind 32267). `d` is the app id (also the map key).
struct Listing {
    event: Event,
    created_at: u64,
    name: String,
    summary: String,
    icon_url: Option<String>,
    categories: Vec<String>,
    platforms: Vec<String>,
}

/// One release (kind 30063). `d` is `<app-id>@<version>`; `e` refs asset
/// events. Newest first in the feed's per-app vector.
struct Release {
    event: Event,
    created_at: u64,
    version: String,
    channel: Option<String>,
    asset_refs: Vec<String>,
}

/// One asset (kind 3063): the artifact pin. `x` is the sha256 of the
/// artifact; `oci` carries the location; `runtime` the flatpak runtime
/// requirement. Assets are unique by event id (no `d` tag).
struct Asset {
    event: Event,
    created_at: u64,
    digest: Option<String>,
    location: Option<String>,
    // the runtime requirement and asset version surface on the app page
    #[allow(dead_code)]
    runtime: Option<String>,
    #[allow(dead_code)]
    version: Option<String>,
}

/// A publisher profile (kind 0): content is standard NIP-01 metadata.
struct Profile {
    event: Event,
    created_at: u64,
    nip05: Option<String>,
}

/// A user review (kind 30065): `d` is the app id, content the text,
/// `rating` an optional 0.0 to 5.0 tag. Replaceable per (pubkey, app id):
/// editing is republishing, so one identity holds one review per app.
struct Review {
    event: Event,
    created_at: u64,
    rating: Option<f32>,
}

/// The accumulated set of verified NIP-82 events, ready to project into a
/// catalog. Replaceable semantics: the newest event per `d` wins.
#[derive(Default)]
pub struct Feed {
    listings: BTreeMap<String, Listing>,
    releases: BTreeMap<String, Vec<Release>>,
    assets: BTreeMap<String, Asset>,
    profiles: BTreeMap<String, Profile>,
    /// Keyed (pubkey, app id): one review per identity per app.
    reviews: BTreeMap<(String, String), Review>,
}

impl Feed {
    pub fn new() -> Self {
        Self::default()
    }

    /// Verify and absorb one event. `Ok(false)` means the event is valid
    /// but not useful (stale replaceable, wrong kind, no linux platform,
    /// duplicate asset); `Err` means malformed or rejected.
    pub fn absorb(&mut self, event: Event) -> Result<bool> {
        // NIP-01 integrity: the id is the event's canonical hash and the
        // signature must verify against it. A bad signature is a hard
        // error, not a skip: relays that pass garbage are lying.
        event
            .verify()
            .map_err(|err| anyhow::anyhow!("signature verification failed: {err}"))?;

        let parts = Parts::parse(&event)?;
        match parts.kind {
            KIND_APP => self.absorb_app(event, parts),
            KIND_RELEASE => self.absorb_release(event, parts),
            KIND_ASSET => self.absorb_asset(event, parts),
            KIND_PROFILE => self.absorb_profile(event, parts),
            KIND_REVIEW => self.absorb_review(event, parts),
            _ => Ok(false),
        }
    }

    fn absorb_app(&mut self, event: Event, parts: Parts) -> Result<bool> {
        let Some(app_id) = parts.tag("d") else {
            bail!("app listing without d tag");
        };
        // Only flatpak platforms are catalogable: Zapstore android events
        // must not become rows in a flatpak store. A listing with no `f`
        // tag at all is treated as platform-agnostic.
        let platforms = parts.tag_values("f");
        if !platforms.is_empty() && !platforms.iter().any(|p| p.starts_with("linux-")) {
            return Ok(false);
        }
        let newer = self
            .listings
            .get(&app_id)
            .is_none_or(|old| old.created_at <= parts.created_at);
        if !newer {
            return Ok(false);
        }
        self.listings.insert(
            app_id,
            Listing {
                created_at: parts.created_at,
                name: parts
                    .tag("name")
                    .or_else(|| parts.tag("d"))
                    .unwrap_or_default(),
                summary: parts.tag("summary").unwrap_or_default(),
                icon_url: parts.tag("icon"),
                categories: parts.tag_values("t"),
                platforms,
                event,
            },
        );
        Ok(true)
    }

    fn absorb_release(&mut self, event: Event, parts: Parts) -> Result<bool> {
        let Some(d) = parts.tag("d") else {
            bail!("release without d tag");
        };
        let Some((app_id, version)) = d.rsplit_once('@') else {
            bail!("release d tag must be <app-id>@<version>, got {d:?}");
        };
        if app_id.is_empty() || version.is_empty() {
            bail!("release d tag must be <app-id>@<version>, got {d:?}");
        }
        // the i tag must agree with the d tag's app id when present
        if let Some(i) = parts.tag("i") {
            if i != app_id {
                bail!("release i tag {i:?} disagrees with d tag {app_id:?}");
            }
        }
        let releases = self.releases.entry(app_id.to_string()).or_default();
        if releases
            .iter()
            .any(|old| old.event.id.to_string() == parts.id)
        {
            return Ok(false);
        }
        // newest first; equal timestamps keep insertion order
        let position = releases
            .iter()
            .position(|old| old.created_at < parts.created_at)
            .unwrap_or(releases.len());
        releases.insert(
            position,
            Release {
                created_at: parts.created_at,
                version: version.to_string(),
                channel: parts.tag("c"),
                asset_refs: parts.tag_values("e"),
                event,
            },
        );
        Ok(true)
    }

    fn absorb_asset(&mut self, event: Event, parts: Parts) -> Result<bool> {
        let digest = parts.tag("x").map(normalize_digest);
        let location = parts.tag("oci");
        check_digest_agreement(digest.as_deref(), location.as_deref())?;
        if self.assets.contains_key(&parts.id) {
            return Ok(false);
        }
        self.assets.insert(
            parts.id.clone(),
            Asset {
                created_at: parts.created_at,
                digest,
                location,
                runtime: parts.tag("runtime"),
                version: parts.tag("version"),
                event,
            },
        );
        Ok(true)
    }

    fn absorb_profile(&mut self, event: Event, parts: Parts) -> Result<bool> {
        let newer = self
            .profiles
            .get(&parts.pubkey)
            .is_none_or(|old| old.created_at <= parts.created_at);
        if !newer {
            return Ok(false);
        }
        let metadata: serde_json::Value = serde_json::from_str(&parts.content).unwrap_or_default();
        self.profiles.insert(
            parts.pubkey.clone(),
            Profile {
                created_at: parts.created_at,
                nip05: metadata["nip05"].as_str().map(str::to_string),
                event,
            },
        );
        Ok(true)
    }

    fn absorb_review(&mut self, event: Event, parts: Parts) -> Result<bool> {
        let Some(app_id) = parts.tag("d") else {
            bail!("review without d tag");
        };
        // a rating tag, when present, must be a sane 0.0 to 5.0: a client
        // that publishes another format is broken, not lying
        let rating = match parts.tag("rating") {
            Some(raw) => {
                let value: f32 = raw
                    .parse()
                    .with_context(|| format!("review rating {raw:?} is not a number"))?;
                if !(0.0..=5.0).contains(&value) {
                    bail!("review rating {value} outside 0.0 to 5.0");
                }
                Some(value)
            }
            None => None,
        };
        let key = (parts.pubkey.clone(), app_id);
        let newer = self
            .reviews
            .get(&key)
            .is_none_or(|old| old.created_at <= parts.created_at);
        if !newer {
            return Ok(false);
        }
        self.reviews.insert(
            key,
            Review {
                created_at: parts.created_at,
                rating,
                event,
            },
        );
        Ok(true)
    }

    /// The feed's reviews, app id ascending, newest first within each
    /// app. Author name fields resolve from the author's kind 0 profile
    /// when one has been seen.
    pub fn reviews(&self) -> orchard_core::ReviewsFile {
        let mut reviews: Vec<orchard_core::Review> = self
            .reviews
            .iter()
            .map(|((pubkey, app_id), review)| orchard_core::Review {
                app_id: app_id.clone(),
                pubkey: pubkey.clone(),
                name: self
                    .profiles
                    .get(pubkey)
                    .and_then(|p| {
                        let event: serde_json::Value =
                            serde_json::from_str(&p.event.content).unwrap_or_default();
                        event["name"].as_str().map(str::to_string)
                    }),
                nip05: self.profiles.get(pubkey).and_then(|p| p.nip05.clone()),
                rating: review.rating,
                content: review.event.content.clone(),
                created_at: review.created_at as i64,
            })
            .collect();
        reviews.sort_by(|a, b| a.app_id.cmp(&b.app_id).then(b.created_at.cmp(&a.created_at)));
        orchard_core::ReviewsFile {
            version: 1,
            reviews,
        }
    }

    /// Project the feed into a catalog: only apps with a listing, a
    /// release, and an asset, all signed by the same publisher key,
    /// become rows. `metadata` is deliberately `None`: the client reads
    /// the real sandbox from the artifact itself.
    pub fn catalog(&self) -> CatalogFile {
        let mut apps: Vec<CatalogApp> = Vec::new();
        for (app_id, listing) in &self.listings {
            let author = listing.event.pubkey.to_string();
            // walk releases newest-first; first one with an installable
            // asset from the same publisher wins
            let Some(picked) = self.releases.get(app_id).and_then(|releases| {
                releases
                    .iter()
                    .filter(|release| release.event.pubkey.to_string() == author)
                    // flatpak installs from the stable branch; "main" is
                    // Zapstore's default channel name for the same thing
                    .filter(|release| {
                        matches!(release.channel.as_deref(), None | Some("stable") | Some("main"))
                    })
                    .find_map(|release| {
                        let asset = release.asset_refs.iter().find_map(|reference| {
                            let asset = self.assets.get(reference)?;
                            if asset.event.pubkey.to_string() != author {
                                return None;
                            }
                            asset.location.as_ref()?; // nothing installable
                            Some(asset)
                        })?;
                        Some((release, asset))
                    })
            }) else {
                continue;
            };
            let (release, asset) = picked;
            let arch = listing
                .platforms
                .iter()
                .find_map(|platform| platform.strip_prefix("linux-"))
                .unwrap_or("x86_64")
                .to_string();
            apps.push(CatalogApp {
                app_id: app_id.clone(),
                name: listing.name.clone(),
                summary: listing.summary.clone(),
                description: (!listing.event.content.is_empty())
                    .then(|| listing.event.content.clone()),
                icon_url: listing.icon_url.clone(),
                categories: listing.categories.clone(),
                publisher: Publisher {
                    pubkey: author.clone(),
                    nip05: self.profiles.get(&author).and_then(|p| p.nip05.clone()),
                },
                version: release.version.clone(),
                flatpak_ref: format!("app/{app_id}/{arch}/stable"),
                location: asset.location.clone().unwrap_or_default(),
                digest: asset.digest.clone(),
                metadata: None,
                notes: (!release.event.content.is_empty())
                    .then(|| release.event.content.clone()),
                published_at: release.created_at as i64,
            });
        }
        apps.sort_by(|a, b| a.app_id.cmp(&b.app_id));
        CatalogFile { version: 1, apps }
    }

    /// Canonical JSON for every absorbed event, in stable order: the
    /// on-disk cache. Loading re-verifies each event, so a corrupted or
    /// tampered cache is harmless.
    pub fn event_jsons(&self) -> Vec<String> {
        let mut events: Vec<(u64, String)> = Vec::new();
        events.extend(self.listings.values().map(|l| (l.created_at, l.event.as_json())));
        events.extend(
            self.releases
                .values()
                .flatten()
                .map(|r| (r.created_at, r.event.as_json())),
        );
        events.extend(self.assets.values().map(|a| (a.created_at, a.event.as_json())));
        events.extend(
            self.reviews
                .values()
                .map(|r| (r.created_at, r.event.as_json())),
        );
        events.extend(
            self.profiles
                .values()
                .map(|p| (p.created_at, p.event.as_json())),
        );
        events.sort_by(|a, b| a.0.cmp(&b.0));
        events.into_iter().map(|(_, json)| json).collect()
    }

    /// The latest kind 0 metadata per pubkey, as the raw JSON the
    /// author signed. Clients merge against this before republishing a
    /// profile: kind 0 is replaceable, a subset would drop fields.
    pub fn profile_raws(&self) -> std::collections::BTreeMap<String, String> {
        self.profiles
            .iter()
            .map(|(pubkey, profile)| (pubkey.clone(), profile.event.content.clone()))
            .collect()
    }
}

/// When the location embeds its own digest (`docker://…@sha256:…`), it must
/// agree with the `x` pin; path-style locations (`oci:/dir`) carry no
/// digest and are checked at install time instead.
fn check_digest_agreement(digest: Option<&str>, location: Option<&str>) -> Result<()> {
    let (Some(digest), Some(location)) = (digest, location) else {
        return Ok(());
    };
    let Some(at) = location.rfind('@') else {
        return Ok(());
    };
    let pinned = &location[at + 1..];
    if pinned.starts_with("sha256:") && !pinned.eq_ignore_ascii_case(digest) {
        bail!("asset oci ref {location:?} pins {pinned:?}, disagrees with digest {digest:?}");
    }
    Ok(())
}

/// Accept bare 64-hex or already-prefixed digests; the catalog's pin
/// format is `sha256:<hex>`.
fn normalize_digest(x: String) -> String {
    if x.starts_with("sha256:") {
        x
    } else {
        format!("sha256:{x}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sign one event; the builder + finalize path is the canonical
    /// construction (id + sig computed by the crate).
    fn signed(keys: &Keys, kind: u32, content: &str, tags: Vec<(&str, &str)>) -> Event {
        let builder = EventBuilder::new(Kind::Custom(kind as u16), content).tags(tags.into_iter().map(
            |(name, value)| Tag::custom(name, [value]),
        ));
        builder.finalize(keys).unwrap()
    }

    /// A realistic test digest (64 hex chars).
    const DIGEST: &str = "f127a079f4a50c3d960e6707aeb7373ee01403cc7b079b9b6fc3743c21a6e7a7";

    fn demo_keys() -> Keys {
        Keys::parse("5122b2537caa7e084e8aec9278e222b1a9b53a1cd2dbfb7d76f0b1bd41c8d6ea").unwrap()
    }

    fn absorb_all(feed: &mut Feed, events: &[Event]) {
        for event in events {
            feed.absorb(event.clone()).unwrap();
        }
    }

    #[test]
    fn projects_listing_release_asset_into_catalog() {
        let keys = demo_keys();
        let app = signed(
            &keys,
            KIND_APP,
            "Long description of the demo app.",
            vec![
                ("d", "com.example.Hello"),
                ("name", "Hello"),
                ("summary", "A demo app"),
                ("icon", "https://example.com/icon.png"),
                ("t", "Utilities"),
                ("f", "linux-x86_64"),
            ],
        );
        let asset = signed(
            &keys,
            KIND_ASSET,
            "",
            vec![
                ("i", "com.example.Hello"),
                ("x", DIGEST),
                ("version", "1.0.0"),
                ("oci", "oci:/srv/registry/hello"),
                ("runtime", "org.freedesktop.Platform/x86_64/25.08"),
            ],
        );
        let asset_id = asset.id.to_string();
        let release = signed(
            &keys,
            KIND_RELEASE,
            "First release.",
            vec![
                ("d", "com.example.Hello@1.0.0"),
                ("i", "com.example.Hello"),
                ("version", "1.0.0"),
                ("c", "stable"),
                ("e", &asset_id),
            ],
        );
        let mut feed = Feed::new();
        absorb_all(&mut feed, &[app, asset, release]);
        let catalog = feed.catalog();
        assert_eq!(catalog.apps.len(), 1);
        let entry = &catalog.apps[0];
        assert_eq!(entry.app_id, "com.example.Hello");
        assert_eq!(entry.name, "Hello");
        assert_eq!(entry.summary, "A demo app");
        assert_eq!(
            entry.description.as_deref(),
            Some("Long description of the demo app.")
        );
        assert_eq!(entry.version, "1.0.0");
        assert_eq!(entry.flatpak_ref, "app/com.example.Hello/x86_64/stable");
        assert_eq!(entry.location, "oci:/srv/registry/hello");
        assert_eq!(entry.digest.as_deref(), Some(format!("sha256:{DIGEST}").as_str()));
        assert_eq!(entry.metadata, None);
        assert_eq!(entry.notes.as_deref(), Some("First release."));
        assert_eq!(entry.publisher.pubkey, demo_keys().get_public_key().unwrap().to_string());
        assert_eq!(entry.publisher.nip05, None);
        assert!(entry.published_at > 0);
    }

    #[test]
    fn rejects_tampered_signatures() {
        let keys = demo_keys();
        let app = signed(
            &keys,
            KIND_APP,
            "Original.",
            vec![("d", "com.example.Hello"), ("f", "linux-x86_64")],
        );
        // flip one content byte: id and signature no longer match
        let raw = app.as_json().replace("Original.", "Tampered.");
        let tampered = Event::from_json(raw).unwrap();
        let mut feed = Feed::new();
        assert!(feed.absorb(tampered).is_err(), "tampered event must not absorb");
        assert!(feed.catalog().apps.is_empty());
    }

    #[test]
    fn cross_key_release_is_not_projected() {
        let publisher = demo_keys();
        let attacker = Keys::generate();
        let app = signed(
            &publisher,
            KIND_APP,
            "App.",
            vec![("d", "com.example.Hello"), ("f", "linux-x86_64")],
        );
        let asset = signed(
            &publisher,
            KIND_ASSET,
            "",
            vec![("x", DIGEST), ("oci", &format!("docker://ghcr.io/x@sha256:{DIGEST}"))],
        );
        let asset_id = asset.id.to_string();
        let release = signed(
            &attacker,
            KIND_RELEASE,
            "Hijacked release.",
            vec![
                ("d", "com.example.Hello@9.9.9"),
                ("i", "com.example.Hello"),
                ("version", "9.9.9"),
                ("e", &asset_id),
            ],
        );
        let mut feed = Feed::new();
        absorb_all(&mut feed, &[app, asset, release]);
        assert!(feed.catalog().apps.is_empty());
    }

    #[test]
    fn mismatched_pin_and_ref_is_rejected() {
        let keys = demo_keys();
        let other = "aa".repeat(32);
        let asset = signed(
            &keys,
            KIND_ASSET,
            "",
            vec![("x", DIGEST), ("oci", &format!("docker://ghcr.io/x@sha256:{other}"))],
        );
        let mut feed = Feed::new();
        assert!(feed.absorb(asset).is_err(), "pin/ref disagreement must reject");
    }

    #[test]
    fn android_listings_are_ignored() {
        let keys = demo_keys();
        let app = signed(
            &keys,
            KIND_APP,
            "An android app.",
            vec![("d", "com.example.Android"), ("f", "android-arm64-v8a")],
        );
        let mut feed = Feed::new();
        absorb_all(&mut feed, &[app]);
        assert!(feed.catalog().apps.is_empty());
    }

    #[test]
    fn replaceable_events_keep_the_newest() {
        let keys = demo_keys();
        let old = signed(
            &keys,
            KIND_APP,
            "Old description.",
            vec![("d", "com.example.Hello"), ("f", "linux-x86_64")],
        );
        // same d tag, newer created_at
        let newer = EventBuilder::new(Kind::Custom(KIND_APP as u16), "New description.")
            .tag(Tag::custom("d", ["com.example.Hello"]))
            .tag(Tag::custom("f", ["linux-x86_64"]))
            .custom_created_at(Timestamp::from(old.created_at + 60))
            .finalize(&keys)
            .unwrap();
        // a full chain beneath, so the projection has something to show
        let asset = signed(
            &keys,
            KIND_ASSET,
            "",
            vec![("x", DIGEST), ("oci", "oci:/srv/hello")],
        );
        let asset_id = asset.id.to_string();
        let release = signed(
            &keys,
            KIND_RELEASE,
            "Release.",
            vec![
                ("d", "com.example.Hello@1.0.0"),
                ("version", "1.0.0"),
                ("e", &asset_id),
            ],
        );
        let mut feed = Feed::new();
        absorb_all(&mut feed, &[old, newer, asset, release]);
        let catalog = feed.catalog();
        assert_eq!(catalog.apps.len(), 1);
        assert_eq!(
            catalog.apps[0].description.as_deref(),
            Some("New description.")
        );
    }

    #[test]
    fn newest_release_wins_and_beta_is_skipped() {
        let keys = demo_keys();
        let app = signed(
            &keys,
            KIND_APP,
            "App.",
            vec![("d", "com.example.Hello"), ("f", "linux-x86_64")],
        );
        let asset = signed(
            &keys,
            KIND_ASSET,
            "",
            vec![("x", DIGEST), ("oci", "oci:/srv/hello")],
        );
        let asset_id = asset.id.to_string();
        let stable = signed(
            &keys,
            KIND_RELEASE,
            "Stable.",
            vec![
                ("d", "com.example.Hello@1.0.0"),
                ("version", "1.0.0"),
                ("c", "stable"),
                ("e", &asset_id),
            ],
        );
        let beta = EventBuilder::new(Kind::Custom(KIND_RELEASE as u16), "Beta.")
            .tag(Tag::custom("d", ["com.example.Hello@2.0.0"]))
            .tag(Tag::custom("version", ["2.0.0"]))
            .tag(Tag::custom("c", ["beta"]))
            .tag(Tag::custom("e", [asset_id.as_str()]))
            .custom_created_at(Timestamp::from(stable.created_at + 60))
            .finalize(&keys)
            .unwrap();
        let mut feed = Feed::new();
        absorb_all(&mut feed, &[app, asset, stable, beta]);
        let catalog = feed.catalog();
        assert_eq!(catalog.apps.len(), 1);
        assert_eq!(catalog.apps[0].version, "1.0.0");
    }
}
