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
/// Publisher migration (Orchard-draft, same caveat as the review
/// kind): signed by the OLD key, `d` the app id, `p` the successor
/// key. No `p` revokes a previous designation.
pub const KIND_MIGRATION: u32 = 30066;

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
    repository: Option<String>,
    homepage: Option<String>,
}

/// A publisher migration (kind 30066, Orchard-draft): signed by the
/// OLD publisher key, `d` the app id, optional `p` the successor key.
/// Replaceable per (app id, old key): the newest statement from a key
/// wins, and an empty successor revokes the previous designation.
struct Migration {
    event: Event,
    created_at: u64,
    new_key: Option<String>,
}

/// One release (kind 30063). `d` is `<app-id>@<version>`; `e` refs asset
/// events. Newest first in the feed's per-app vector.
struct Release {
    event: Event,
    created_at: u64,
    version: String,
    /// The platform this release was built for, from the `f` tag.
    /// None: a legacy release from before the tag existed; it stays
    /// installable on any host.
    platform: Option<String>,
    channel: Option<String>,
    asset_refs: Vec<String>,
}

/// One asset (kind 3063): the artifact pin. `x` is the sha256 of the
/// artifact; `oci` carries the location; `runtime` the flatpak runtime
/// requirement. Assets are unique by event id (no `d` tag).
struct Asset {
    event: Event,
    created_at: u64,
    /// The platform this artifact was built for, from the `f` tag.
    /// None: a legacy asset from before the tag existed.
    platform: Option<String>,
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
/// Listings are keyed (app id, publisher): the permissionless case is
/// two publishers of one app id, and both rows survive.
#[derive(Default)]
pub struct Feed {
    /// Set on every retained event (and on clear) and taken by the
    /// cache writer: without a change, a writer tick costs one flag
    /// check, not a full-feed serialization under the lock.
    dirty: std::sync::atomic::AtomicBool,
    listings: BTreeMap<(String, String), Listing>,
    /// (app id, old key) -> the newest migration statement that key
    /// signed for the id.
    migrations: BTreeMap<(String, String), Migration>,
    releases: BTreeMap<String, Vec<Release>>,
    assets: BTreeMap<String, Asset>,
    profiles: BTreeMap<String, Profile>,
    /// Keyed (pubkey, app id): one review per identity per app.
    reviews: BTreeMap<(String, String), Review>,
    /// The earliest event per author key, unix seconds: the client's
    /// new-key signal for review trust. Deliberately naive: a relay
    /// that only recently saw a key makes the key look new.
    first_seen: BTreeMap<String, u64>,
}

impl Feed {
    pub fn new() -> Self {
        Self::default()
    }

    /// Drop every absorbed event: the "clear cached data" action.
    /// Relays refill the feed on their next wake; the disk cache
    /// follows on the writer's next tick.
    pub fn clear(&mut self) {
        *self = Self::default();
        // the writer must rewrite the (now empty) file, or the disk
        // cache the next load reads restores everything just dropped
        self.dirty.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Swap out the change flag: true when an event was retained (or
    /// the feed was cleared) since the last take.
    pub fn take_dirty(&self) -> bool {
        self.dirty.swap(false, std::sync::atomic::Ordering::Relaxed)
    }

    /// Re-arm the change flag after a failed persistence tick, so the
    /// writer retries instead of assuming the loss was written.
    pub fn mark_dirty(&self) {
        self.dirty.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// The newest event time the feed holds, across every kind: the
    /// reconnect `since` anchor.
    pub fn newest_created_at(&self) -> Option<u64> {
        [
            self.listings.values().map(|l| l.created_at).max(),
            self.migrations.values().map(|m| m.created_at).max(),
            self.releases
                .values()
                .flatten()
                .map(|r| r.created_at)
                .max(),
            self.assets.values().map(|a| a.created_at).max(),
            self.profiles.values().map(|p| p.created_at).max(),
            self.reviews.values().map(|r| r.created_at).max(),
        ]
        .into_iter()
        .flatten()
        .max()
    }

    /// Bound the feed to what the catalog can show. Relays refill on
    /// reconnect, so anything the projection ignores is pure cost:
    /// foreign releases no catalog can pick, versions past the newest
    /// few, assets nothing references, reviews of apps nobody lists,
    /// profiles nobody reads. Without this, memory and the disk cache
    /// grow with everything a relay ever saw, not with Orchard.
    pub fn trim(&mut self) {
        // a release is projectable only under a listing signed by the
        // same author; a short tail of each app's versions keeps the
        // newest-release-has-no-asset-yet walk working across a
        // publisher's recent history
        const VERSION_TAIL: usize = 10;
        let listed: std::collections::HashSet<(String, String)> =
            self.listings.keys().cloned().collect();
        for (app_id, releases) in self.releases.iter_mut() {
            releases.retain(|release| {
                listed.contains(&(app_id.clone(), release.event.pubkey.to_string()))
            });
            // stored newest-first; the tail is the newest
            releases.truncate(VERSION_TAIL);
        }
        self.releases.retain(|_, releases| !releases.is_empty());
        // assets survive only while a kept release points at them,
        // which also drops any asset whose author never listed
        let referenced: std::collections::HashSet<String> = self
            .releases
            .values()
            .flat_map(|releases| {
                releases
                    .iter()
                    .flat_map(|release| release.asset_refs.iter().cloned())
            })
            .collect();
        self.assets.retain(|id, _| referenced.contains(id));
        // reviews of unlisted apps are never rendered
        let listed_ids: std::collections::HashSet<String> =
            self.listings.keys().map(|(id, _)| id.clone()).collect();
        self.reviews
            .retain(|(_, app_id), _| listed_ids.contains(app_id));
        // profiles and first-seen live for the keys the survivors
        // name: publishers, reviewers, and a hand-over's two ends
        let mut wanted: std::collections::HashSet<String> = self
            .listings
            .keys()
            .map(|(_, key)| key.clone())
            .chain(self.reviews.keys().map(|(key, _)| key.clone()))
            .collect();
        for (app_id, migration) in self.migrations.iter() {
            if listed_ids.contains(&app_id.0) {
                wanted.insert(app_id.1.clone());
                if let Some(new_key) = &migration.new_key {
                    wanted.insert(new_key.clone());
                }
            }
        }
        self.profiles.retain(|key, _| wanted.contains(key));
        self.first_seen.retain(|key, _| wanted.contains(key));
        // a migration for an unlisted app id is inert: no old row can
        // step aside for a successor when the id is not listed at all
        self.migrations
            .retain(|(app_id, _), _| listed_ids.contains(app_id));
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
        // first-seen bookkeeping: keep the earliest event per author
        // key across every kind the feed retains
        let author = event.pubkey.to_string();
        match self.first_seen.get(&author) {
            Some(seen) if *seen <= parts.created_at => {}
            _ => {
                self.first_seen.insert(author, parts.created_at);
            }
        }
        let retained = match parts.kind {
            KIND_APP => self.absorb_app(event, parts),
            KIND_RELEASE => self.absorb_release(event, parts),
            KIND_ASSET => self.absorb_asset(event, parts),
            KIND_PROFILE => self.absorb_profile(event, parts),
            KIND_REVIEW => self.absorb_review(event, parts),
            KIND_MIGRATION => self.absorb_migration(event, parts),
            _ => Ok(false),
        };
        if matches!(retained, Ok(true)) {
            self.dirty.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        retained
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
        // a publisher replaces their own listing; another publisher's
        // listing of the same id is a second row, not a challenge
        let key = (app_id.clone(), parts.pubkey.clone());
        let newer = self
            .listings
            .get(&key)
            .is_none_or(|old| old.created_at <= parts.created_at);
        if !newer {
            return Ok(false);
        }
        self.listings.insert(
            key,
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
                repository: parts.tag("repository"),
                homepage: parts.tag("h"),
                event,
            },
        );
        Ok(true)
    }

    fn absorb_migration(&mut self, event: Event, parts: Parts) -> Result<bool> {
        let Some(app_id) = parts.tag("d") else {
            bail!("migration without d tag");
        };
        let key = (app_id, parts.pubkey.clone());
        let newer = self
            .migrations
            .get(&key)
            .is_none_or(|old| old.created_at <= parts.created_at);
        if !newer {
            return Ok(false);
        }
        self.migrations.insert(
            key,
            Migration {
                event,
                created_at: parts.created_at,
                new_key: parts.tag("p"),
            },
        );
        Ok(true)
    }

    fn absorb_release(&mut self, event: Event, parts: Parts) -> Result<bool> {
        let Some(d) = parts.tag("d") else {
            bail!("release without d tag");
        };
        // d names the release: <app-id>@<version> for legacy events,
        // <app-id>@<version>@<platform> once releases grew a platform.
        // Per-arch identity keeps one arch's republish from replacing
        // the other's on a relay, which keys replaceability on d.
        let pieces: Vec<&str> = d.split('@').collect();
        let (app_id, version, platform) = match pieces.as_slice() {
            [app_id, version] => (app_id.to_string(), version.to_string(), None),
            [app_id, version, platform] => (
                app_id.to_string(),
                version.to_string(),
                Some(platform.to_string()),
            ),
            _ => bail!("release d tag must be <app-id>@<version>[@<platform>], got {d:?}"),
        };
        if app_id.is_empty() || version.is_empty() || platform.as_deref() == Some("") {
            bail!("release d tag must be <app-id>@<version>[@<platform>], got {d:?}");
        }
        // the i tag must agree with the d tag's app id when present
        if let Some(i) = parts.tag("i")
            && i != app_id
        {
            bail!("release i tag {i:?} disagrees with d tag {app_id:?}");
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
                version,
                platform,
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
                platform: parts.tag("f"),
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
        // first_seen rides along, filtered to the keys that actually
        // review: the catalog's authors are not the client's business
        let mut first_seen: BTreeMap<String, u64> = BTreeMap::new();
        for (pubkey, _) in self.reviews.keys() {
            if let Some(seen) = self.first_seen.get(pubkey) {
                first_seen.insert(pubkey.clone(), *seen);
            }
        }
        orchard_core::ReviewsFile {
            version: 1,
            reviews,
            first_seen,
        }
    }

    /// Project the feed into a catalog: only apps with a listing, a
    /// release, and an asset, all signed by the same publisher key,
    /// become rows. `metadata` is deliberately `None`: the client reads
    /// the real sandbox from the artifact itself.
    pub fn catalog(&self) -> CatalogFile {
        self.catalog_for(&orchard_core::host_platform())
    }

    /// The catalog projected for one platform: releases built for
    /// other machines do not win, and a listing whose chain exists
    /// only for other platforms still projects (empty version, the
    /// claimed platforms attached) so the client can say so instead
    /// of offering an install that cannot work.
    pub fn catalog_for(&self, platform: &str) -> CatalogFile {
        let mut apps: Vec<CatalogApp> = Vec::new();
        // one row per (app id, publisher): the same app id signed by
        // two keys projects twice, each against its own release chain
        for ((app_id, author), listing) in &self.listings {
            // a migration statement signed by the listed key hands the
            // app id to a successor: once the successor's own listing
            // exists, the old row steps aside. A revoked or successor-
            // less designation keeps the row.
            let superseded = self
                .migrations
                .get(&(app_id.clone(), author.clone()))
                .and_then(|migration| migration.new_key.as_ref())
                .is_some_and(|successor| {
                    successor != author
                        && self
                            .listings
                            .contains_key(&(app_id.clone(), successor.clone()))
                });
            if superseded {
                continue;
            }
            // an installable asset of one release: same publisher,
            // pinned somewhere, built for this platform. An asset from
            // before the platform tag existed stays installable
            // anywhere; it is the fallback after an exact match.
            let same_publisher = |reference: &String| {
                self.assets
                    .get(reference)
                    .filter(|asset| asset.event.pubkey.to_string() == *author)
                    .filter(|asset| asset.location.is_some())
            };
            let installable = |release: &Release| {
                release
                    .asset_refs
                    .iter()
                    .filter_map(same_publisher)
                    .find(|asset| asset.platform.as_deref() == Some(platform))
                    .or_else(|| {
                        release
                            .asset_refs
                            .iter()
                            .filter_map(same_publisher)
                            .find(|asset| asset.platform.is_none())
                    })
            };
            // walk releases newest-first; first one with an
            // installable asset for this machine wins
            fn stable<'a>(
                releases: &'a [Release],
                author: &'a str,
            ) -> impl Iterator<Item = &'a Release> + 'a {
                releases
                    .iter()
                    .filter(move |release| release.event.pubkey.to_string() == author)
                    // flatpak installs from the stable branch; "main" is
                    // Zapstore's default channel name for the same thing
                    .filter(|release| {
                        matches!(release.channel.as_deref(), None | Some("stable") | Some("main"))
                    })
            }
            let picked = self.releases.get(app_id).and_then(|releases| {
                stable(releases, author)
                    .filter(|release| {
                        release.platform.as_deref().is_none_or(|built| built == platform)
                    })
                    .find_map(|release| Some((release, Some(installable(release)?))))
            });
            // the chain exists but builds nothing for this machine:
            // project the row with no install, so the page can say
            // what was built. A chain with no releases at all still
            // stays invisible, as before.
            let picked = picked.or_else(|| {
                self.releases.get(app_id).and_then(|releases| {
                    stable(releases, author).find(|release| {
                            release
                                .asset_refs
                                .iter()
                                .filter_map(same_publisher)
                                .next()
                                .is_some()
                        })
                        .map(|release| (release, None))
                })
            });
            let Some((release, asset)) = picked else {
                continue;
            };
            let (version, digest, location) = match asset {
                Some(asset) => (
                    release.version.clone(),
                    asset.digest.clone(),
                    asset.location.clone().unwrap_or_default(),
                ),
                // nothing for this machine: no version, no pin, no
                // install. The claimed platforms ride the row.
                None => (String::new(), None, String::new()),
            };
            let ref_arch = platform.strip_prefix("linux-").unwrap_or(platform);
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
                    nip05: self.profiles.get(author).and_then(|p| p.nip05.clone()),
                },
                version,
                platforms: listing.platforms.clone(),
                flatpak_ref: format!("app/{app_id}/{ref_arch}/stable"),
                location,
                digest,
                metadata: None,
                repository: listing.repository.clone(),
                homepage: listing.homepage.clone(),
                notes: (!release.event.content.is_empty())
                    .then(|| release.event.content.clone()),
                published_at: release.created_at as i64,
                // the newest statement naming this publisher as
                // successor, if any: the row's handover is authorized
                successor_to: self
                    .migrations
                    .iter()
                    .filter(|((_, _), migration)| {
                        migration.new_key.as_deref() == Some(author.as_str())
                    })
                    .max_by_key(|((_, _), migration)| migration.created_at)
                    .map(|((_, old_key), _)| old_key.clone()),
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
            self.migrations
                .values()
                .map(|m| (m.created_at, m.event.as_json())),
        );
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
        events.sort_by_key(|a| a.0);
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

    /// The keys whose profiles matter to this store: every listing
    /// author and every review author. Profiles ride the social
    /// relays rather than the app relays, so the profile crawler
    /// asks the social set for these keys' kind 0 events.
    pub fn crawl_keys(&self) -> Vec<String> {
        let mut keys: std::collections::BTreeSet<String> = self
            .listings
            .keys()
            .map(|(_, author)| author.clone())
            .collect();
        for (pubkey, _) in self.reviews.keys() {
            keys.insert(pubkey.clone());
        }
        keys.into_iter().collect()
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
                ("repository", "https://github.com/example/hello"),
                ("h", "https://hello.example.com"),
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
            entry.repository.as_deref(),
            Some("https://github.com/example/hello")
        );
        assert_eq!(entry.homepage.as_deref(), Some("https://hello.example.com"));
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

    /// Two arches of one version are two releases (per-arch `d`, so a
    /// relay keeps both) and each host picks its own chain.
    #[test]
    fn catalog_picks_the_release_built_for_the_host() {
        let keys = demo_keys();
        let chain = |platform: &str, version: &str| {
            let asset = signed(
                &keys,
                KIND_ASSET,
                "",
                vec![
                    ("i", "com.example.Hello"),
                    ("x", DIGEST),
                    ("version", version),
                    ("f", platform),
                    ("oci", "oci:/srv/registry/hello"),
                ],
            );
            let asset_id = asset.id.to_string();
            let d = format!("com.example.Hello@{version}@{platform}");
            let release = signed(
                &keys,
                KIND_RELEASE,
                "",
                vec![
                    ("d", &d),
                    ("i", "com.example.Hello"),
                    ("version", version),
                    ("f", platform),
                    ("c", "stable"),
                    ("e", &asset_id),
                ],
            );
            vec![asset, release]
        };
        let listing = signed(
            &keys,
            KIND_APP,
            "",
            vec![
                ("d", "com.example.Hello"),
                ("name", "Hello"),
                ("summary", "A demo app"),
                ("f", "linux-x86_64"),
                ("f", "linux-aarch64"),
            ],
        );
        let mut events = vec![listing];
        events.extend(chain("linux-x86_64", "1.0.0"));
        events.extend(chain("linux-aarch64", "1.0.0"));
        let mut feed = Feed::new();
        absorb_all(&mut feed, &events);
        let x86 = feed.catalog_for("linux-x86_64");
        assert_eq!(x86.apps.len(), 1);
        assert_eq!(x86.apps[0].version, "1.0.0");
        assert_eq!(x86.apps[0].flatpak_ref, "app/com.example.Hello/x86_64/stable");
        let arm = feed.catalog_for("linux-aarch64");
        assert_eq!(arm.apps.len(), 1);
        assert_eq!(arm.apps[0].version, "1.0.0");
        assert_eq!(arm.apps[0].flatpak_ref, "app/com.example.Hello/aarch64/stable");
        // the listing's coverage rides both rows
        assert_eq!(arm.apps[0].platforms, vec!["linux-x86_64", "linux-aarch64"]);
    }

    /// A chain that exists only for another platform still projects:
    /// no version, no pin, the claimed platforms attached, so the
    /// page can say what was built instead of offering an install
    /// that cannot work.
    #[test]
    fn foreign_arch_projects_without_an_install() {
        let keys = demo_keys();
        let listing = signed(
            &keys,
            KIND_APP,
            "",
            vec![
                ("d", "com.example.Hello"),
                ("name", "Hello"),
                ("summary", "A demo app"),
                ("f", "linux-aarch64"),
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
                ("f", "linux-aarch64"),
                ("oci", "oci:/srv/registry/hello"),
            ],
        );
        let asset_id = asset.id.to_string();
        let release = signed(
            &keys,
            KIND_RELEASE,
            "",
            vec![
                ("d", "com.example.Hello@1.0.0@linux-aarch64"),
                ("i", "com.example.Hello"),
                ("version", "1.0.0"),
                ("f", "linux-aarch64"),
                ("c", "stable"),
                ("e", &asset_id),
            ],
        );
        let mut feed = Feed::new();
        absorb_all(&mut feed, &[listing, asset, release]);
        let x86 = feed.catalog_for("linux-x86_64");
        assert_eq!(x86.apps.len(), 1);
        assert_eq!(x86.apps[0].version, "");
        assert_eq!(x86.apps[0].location, "");
        assert_eq!(x86.apps[0].digest, None);
        assert_eq!(x86.apps[0].platforms, vec!["linux-aarch64"]);
        // the machine it was built for installs normally
        let arm = feed.catalog_for("linux-aarch64");
        assert_eq!(arm.apps[0].version, "1.0.0");
        assert_eq!(
            arm.apps[0].digest.as_deref(),
            Some(format!("sha256:{DIGEST}").as_str())
        );
    }

    /// The permissionless case: a second publisher signs the same app
    /// id and both listings project, each against its own release
    /// chain. Neither replaces the other.
    #[test]
    fn two_publishers_of_one_app_id_project_twice() {
        let a = demo_keys();
        let b = Keys::parse(
            "a462984d90b5badd1ddef88f19ee6ad74bfcb06f431afc9b7b6ce2f6dbf6af70",
        )
        .unwrap();
        let listing = |keys: &Keys, summary: &str| {
            signed(
                keys,
                KIND_APP,
                "",
                vec![
                    ("d", "com.example.Hello"),
                    ("name", "Hello"),
                    ("summary", summary),
                    ("f", "linux-x86_64"),
                ],
            )
        };
        let asset = |keys: &Keys| {
            let asset = signed(
                keys,
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
            let release = signed(
                keys,
                KIND_RELEASE,
                "",
                vec![
                    ("d", "com.example.Hello@1.0.0"),
                    ("i", "com.example.Hello"),
                    ("version", "1.0.0"),
                    ("c", "stable"),
                    ("e", asset.id.to_string().as_str()),
                ],
            );
            (asset, release)
        };
        let (asset_a, release_a) = asset(&a);
        let (asset_b, release_b) = asset(&b);
        let mut feed = Feed::new();
        absorb_all(
            &mut feed,
            &[
                listing(&a, "first publisher"),
                asset_a,
                release_a,
                // the second publisher's listing arrives LAST, and must
                // not win the merge: it is a second row
                listing(&b, "second publisher"),
                asset_b,
                release_b,
            ],
        );
        let catalog = feed.catalog();
        assert_eq!(catalog.apps.len(), 2);
        let keys: Vec<&str> = catalog
            .apps
            .iter()
            .map(|app| app.publisher.pubkey.as_str())
            .collect();
        assert!(keys.contains(&a.get_public_key().unwrap().to_string().as_str()));
        assert!(keys.contains(&b.get_public_key().unwrap().to_string().as_str()));
        // each publisher's own summary rode along on their row
        assert!(catalog.apps.iter().any(|app| app.summary == "first publisher"));
        assert!(catalog
            .apps
            .iter()
            .any(|app| app.summary == "second publisher"));
    }

    /// The migration case: the old key's designation hands the app id
    /// to the successor, whose listing then stands alone instead of
    /// doubling the row.

    /// The live migration event from the dev relay, absorbed cold:
    /// signature, tags, and map placement all hold for real data.
    #[test]
    fn live_migration_event_absorbs() {
        let raw = r#"{"id":"aae5473ea4a30738e0595cfb23bdd741a6faae540e2372e445a8edc9a60128b5","pubkey":"ec2c39e7fb46ddea4e916c240661c4e0ba5128fe6cf8ccf10fa074b95b63379d","created_at":1791706308,"kind":30066,"tags":[["d","com.orchard.Orchard"],["p","14cad7029d2b32e46eb47172d47988d180b17ac97de07a57fdb16a5a97d50365"]],"content":"","sig":"a787e0aefa18be9547faa5d6fb4dd250111b62643d999f0afb641b2b3af0ab85615371544c1824fb8731d34db9f2154cff2dce09f57d5973b4f8b080d32c3306"}"#;
        let event = Event::from_json(raw).unwrap();
        let mut feed = Feed::new();
        let absorbed = feed.absorb(event).unwrap();
        assert!(absorbed, "the migration must absorb");
        assert_eq!(feed.event_jsons().len(), 1);
    }

    #[test]
    fn migrated_listing_steps_aside() {
        let a = demo_keys();
        let b = Keys::parse(
            "a462984d90b5badd1ddef88f19ee6ad74bfcb06f431afc9b7b6ce2f6dbf6af70",
        )
        .unwrap();
        let b_pubkey = b.get_public_key().unwrap().to_string();
        // a complete chain per key: listing, asset, release
        let chain = |keys: &Keys, summary: &str| {
            let listing = signed(
                keys,
                KIND_APP,
                "",
                vec![
                    ("d", "com.example.Hello"),
                    ("name", "Hello"),
                    ("summary", summary),
                    ("f", "linux-x86_64"),
                ],
            );
            let asset = signed(
                keys,
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
            let release = signed(
                keys,
                KIND_RELEASE,
                "",
                vec![
                    ("d", "com.example.Hello@1.0.0"),
                    ("i", "com.example.Hello"),
                    ("version", "1.0.0"),
                    ("c", "stable"),
                    ("e", asset.id.to_string().as_str()),
                ],
            );
            [listing, asset, release]
        };
        let mut feed = Feed::new();
        // both keys publish the app: two rows
        absorb_all(&mut feed, &chain(&a, "old key"));
        absorb_all(&mut feed, &chain(&b, "new key"));
        assert_eq!(feed.catalog().apps.len(), 2);
        // the old key designates the successor: its own row steps
        // aside and the successor's listing stands alone
        let migration = signed(
            &a,
            KIND_MIGRATION,
            "rotated to a new device",
            vec![("d", "com.example.Hello"), ("p", &b_pubkey)],
        );
        absorb_all(&mut feed, &[migration]);
        let catalog = feed.catalog();
        assert_eq!(catalog.apps.len(), 1);
        assert_eq!(catalog.apps[0].publisher.pubkey, b_pubkey);
        assert_eq!(catalog.apps[0].summary, "new key");
        // the surviving row carries the handover: the client can tell
        // an authorized rotation from an unexplained new key
        assert_eq!(
            catalog.apps[0].successor_to.as_deref(),
            Some(a.get_public_key().unwrap().to_string().as_str())
        );

        // revocation: a newer statement from the old key with no `p`
        // restores its row
        let revoke = EventBuilder::new(Kind::Custom(KIND_MIGRATION as u16), "")
            .custom_created_at(Timestamp::now())
            .tag(Tag::custom("d", ["com.example.Hello"]))
            .finalize(&a)
            .unwrap();
        absorb_all(&mut feed, &[revoke]);
        assert_eq!(feed.catalog().apps.len(), 2);
        // and the handover annotation is gone with the designation
        assert!(feed
            .catalog()
            .apps
            .iter()
            .all(|app| app.successor_to.is_none()));
    }

    /// A designation whose successor never lists suppresses nothing:
    /// the app must not lose its only row over an undelivered handover.
    #[test]
    fn successor_less_migration_keeps_the_row() {
        let a = demo_keys();
        let b = Keys::parse(
            "a462984d90b5badd1ddef88f19ee6ad74bfcb06f431afc9b7b6ce2f6dbf6af70",
        )
        .unwrap();
        let stranger = b.get_public_key().unwrap().to_string();
        let listing = signed(
            &a,
            KIND_APP,
            "",
            vec![
                ("d", "com.example.Hello"),
                ("name", "Hello"),
                ("f", "linux-x86_64"),
            ],
        );
        let asset = signed(
            &a,
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
        let release = signed(
            &a,
            KIND_RELEASE,
            "",
            vec![
                ("d", "com.example.Hello@1.0.0"),
                ("i", "com.example.Hello"),
                ("version", "1.0.0"),
                ("c", "stable"),
                ("e", asset.id.to_string().as_str()),
            ],
        );
        let migration = signed(
            &a,
            KIND_MIGRATION,
            "",
            vec![("d", "com.example.Hello"), ("p", &stranger)],
        );
        let mut feed = Feed::new();
        absorb_all(&mut feed, &[listing, asset, release, migration]);
        let catalog = feed.catalog();
        assert_eq!(catalog.apps.len(), 1);
        assert_eq!(
            catalog.apps[0].publisher.pubkey,
            a.get_public_key().unwrap().to_string()
        );
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

    /// first_seen keeps the earliest event per key no matter the order
    /// the feed learned about them, and rides on the reviews document
    /// filtered to reviewing keys.
    #[test]
    fn first_seen_tracks_earliest_event() {
        let keys = demo_keys();
        let pubkey = keys.get_public_key().unwrap().to_string();
        let older_secs: u64 = 1_700_000_000;
        let newer_secs: u64 = older_secs + 100;
        let later = EventBuilder::new(Kind::Custom(KIND_REVIEW as u16), "A review.")
            .tag(Tag::custom("d", ["com.example.Hello"]))
            .tag(Tag::custom("rating", ["4.0"]))
            .custom_created_at(Timestamp::from_secs(newer_secs))
            .finalize(&keys)
            .unwrap();
        let earlier = EventBuilder::new(Kind::Custom(KIND_APP as u16), "Older listing.")
            .tag(Tag::custom("d", ["com.example.Old"]))
            .custom_created_at(Timestamp::from_secs(older_secs))
            .finalize(&keys)
            .unwrap();
        let mut feed = Feed::new();
        // the feed learns the review first, the older listing second:
        // first_seen must still be the older event
        absorb_all(&mut feed, &[later, earlier]);
        assert_eq!(feed.first_seen.get(&pubkey).copied(), Some(older_secs));

        let file = feed.reviews();
        assert_eq!(file.reviews.len(), 1);
        assert_eq!(file.first_seen.get(&pubkey).copied(), Some(older_secs));

        // a key that never reviewed is not the client's business
        let quiet = Keys::generate();
        let listing = signed(
            &quiet,
            KIND_APP,
            "Just a listing.",
            vec![("d", "com.example.Quiet"), ("f", "linux-x86_64")],
        );
        let mut feed = Feed::new();
        absorb_all(&mut feed, &[listing]);
        assert!(feed.reviews().first_seen.is_empty());
    }

    /// The trim bounds the feed to what the catalog can show: foreign
    /// releases and their assets drop, reviews of unlisted apps drop,
    /// and profiles nobody reads drop. Without it, memory and the
    /// disk cache grow with everything a relay ever saw.
    #[test]
    fn trim_keeps_only_what_the_catalog_can_show() {
        let publisher = demo_keys();
        let outsider = Keys::generate();
        let reviewer = Keys::generate();

        let listing = signed(
            &publisher,
            KIND_APP,
            "A listing.",
            vec![("d", "com.example.Hello"), ("f", "linux-x86_64")],
        );
        let asset = signed(
            &publisher,
            KIND_ASSET,
            "",
            vec![
                ("i", "com.example.Hello"),
                ("x", DIGEST),
                ("oci", "oci:/srv/registry/hello"),
            ],
        );
        let asset_id = asset.id.to_string();
        let release = signed(
            &publisher,
            KIND_RELEASE,
            "",
            vec![
                ("d", "com.example.Hello@1.0.0"),
                ("c", "stable"),
                ("e", asset_id.as_str()),
            ],
        );
        // an outsider's release for the same app id: no catalog can
        // project it (the publisher check fails), so it is pure cost
        let foreign_asset = signed(
            &outsider,
            KIND_ASSET,
            "",
            vec![
                ("i", "com.example.Hello"),
                ("x", DIGEST),
                ("oci", "oci:/srv/registry/evil"),
            ],
        );
        let foreign_id = foreign_asset.id.to_string();
        let foreign_release = signed(
            &outsider,
            KIND_RELEASE,
            "",
            vec![
                ("d", "com.example.Hello@9.9.9"),
                ("c", "stable"),
                ("e", foreign_id.as_str()),
            ],
        );
        // a review of the listed app, and one of an app nobody lists
        let kept_review = signed(
            &reviewer,
            KIND_REVIEW,
            "Good app.",
            vec![("d", "com.example.Hello"), ("rating", "4.0")],
        );
        let stray_review = signed(
            &reviewer,
            KIND_REVIEW,
            "For an unlisted app.",
            vec![("d", "com.example.Ghost"), ("rating", "1.0")],
        );
        // the reviewer's profile is read (review names); the
        // outsider's is not referenced by anything kept
        let reviewer_profile = signed(&reviewer, KIND_PROFILE, r#"{"name":"Reviewer"}"#, vec![]);
        let stranger_profile = signed(&outsider, KIND_PROFILE, r#"{"name":"Stranger"}"#, vec![]);

        let mut feed = Feed::new();
        absorb_all(
            &mut feed,
            &[
                listing, asset, release, foreign_asset, foreign_release,
                kept_review, stray_review, reviewer_profile, stranger_profile,
            ],
        );
        feed.trim();

        assert_eq!(feed.catalog().apps.len(), 1);
        let reviews = feed.reviews();
        assert_eq!(reviews.reviews.len(), 1);
        assert_eq!(reviews.reviews[0].content, "Good app.");
        assert!(feed
            .profiles
            .contains_key(&reviewer.get_public_key().unwrap().to_string()));
        assert!(!feed
            .profiles
            .contains_key(&outsider.get_public_key().unwrap().to_string()));
    }

    /// The change flag: a retained event arms it, taking disarms it,
    /// and clear arms it (the writer must rewrite the emptied file,
    /// or the next load restores what the user just dropped).
    #[test]
    fn dirty_flag_tracks_retained_events_and_clear() {
        let keys = demo_keys();
        let mut feed = Feed::new();
        assert!(!feed.take_dirty());
        let listing = signed(
            &keys,
            KIND_APP,
            "A listing.",
            vec![("d", "com.example.Hello"), ("f", "linux-x86_64")],
        );
        absorb_all(&mut feed, &[listing]);
        assert!(feed.take_dirty());
        assert!(!feed.take_dirty());
        feed.clear();
        assert!(feed.take_dirty());
    }
}
