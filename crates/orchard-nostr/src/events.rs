//! Event builders for the publish side: the exact tags each NIP-82 event
//! carries, shared by the test publisher example and `orchard-publish`.
//! The read side ([`crate::Feed`]) accepts these; keeping both ends in one
//! crate means a tag change breaks the build instead of the network.

use anyhow::Result;
use nostr::prelude::*;

/// One app listing (kind 32267). `d` is the app id.
pub struct Listing {
    pub app_id: String,
    pub name: String,
    pub summary: String,
    pub description: Option<String>,
    pub icon: Option<String>,
    pub categories: Vec<String>,
    pub arch: String,
    pub license: Option<String>,
    pub repository: Option<String>,
}

/// One asset (kind 3063): the artifact pin.
pub struct Asset {
    pub app_id: String,
    pub version: String,
    /// `sha256:<hex>` of the artifact manifest.
    pub digest: String,
    /// `oci:/path` or `docker://registry/repo@sha256:<hex>`.
    pub location: String,
    pub runtime: Option<String>,
}

/// One release (kind 30063). `d` is `<app-id>@<version>`; `e` refs the
/// signed asset event.
pub struct Release {
    pub app_id: String,
    pub version: String,
    pub channel: String,
    pub notes: Option<String>,
    /// Hex id of the signed asset event.
    pub asset_id: String,
}

/// One publisher profile (kind 0).
pub struct Profile {
    pub name: Option<String>,
    pub nip05: Option<String>,
}

fn tag(name: &str, value: &str) -> Tag {
    Tag::custom(name, [value])
}

fn optional_tag(name: &str, value: &Option<String>) -> Option<Tag> {
    value.as_deref().filter(|v| !v.is_empty()).map(|v| tag(name, v))
}

pub fn sign_listing(keys: &Keys, listing: &Listing) -> Result<Event> {
    let builder = EventBuilder::new(Kind::Custom(crate::KIND_APP as u16), listing.description.as_deref().unwrap_or(""))
        .tag(tag("d", &listing.app_id))
        .tag(tag("name", &listing.name))
        .tag(tag("summary", &listing.summary))
        .tag_maybe(optional_tag("icon", &listing.icon))
        .tag_maybe(optional_tag("license", &listing.license))
        .tag_maybe(optional_tag("repository", &listing.repository))
        .tag(tag("f", &listing.arch));
    let builder = listing
        .categories
        .iter()
        .fold(builder, |builder, category| builder.tag(tag("t", category)));
    Ok(builder.finalize(keys)?)
}

pub fn sign_asset(keys: &Keys, asset: &Asset) -> Result<Event> {
    let builder = EventBuilder::new(Kind::Custom(crate::KIND_ASSET as u16), "")
        .tag(tag("i", &asset.app_id))
        .tag(tag("x", &asset.digest))
        .tag(tag("version", &asset.version))
        .tag(tag("oci", &asset.location))
        .tag_maybe(optional_tag("runtime", &asset.runtime));
    Ok(builder.finalize(keys)?)
}

pub fn sign_release(keys: &Keys, release: &Release) -> Result<Event> {
    let builder = EventBuilder::new(
        Kind::Custom(crate::KIND_RELEASE as u16),
        release.notes.as_deref().unwrap_or(""),
    )
    .tag(tag("d", &format!("{}@{}", release.app_id, release.version)))
    .tag(tag("i", &release.app_id))
    .tag(tag("version", &release.version))
    .tag(tag("c", &release.channel))
    .tag(tag("e", &release.asset_id));
    Ok(builder.finalize(keys)?)
}

pub fn sign_profile(keys: &Keys, profile: &Profile) -> Result<Event> {
    let metadata = serde_json::json!({
        "name": profile.name,
        "nip05": profile.nip05,
    });
    Ok(EventBuilder::new(Kind::Metadata, metadata.to_string()).finalize(keys)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The round-trip rule: a builder's event must absorb on the read
    /// side and project the same fields back.
    #[test]
    fn built_events_absorb_and_project() {
        let keys = Keys::generate();
        let listing = sign_listing(
            &keys,
            &Listing {
                app_id: "com.example.Hello".into(),
                name: "Hello".into(),
                summary: "A demo app".into(),
                description: Some("Long description.".into()),
                icon: Some("https://example.com/icon.png".into()),
                categories: vec!["Utilities".into()],
                arch: "linux-x86_64".into(),
                license: Some("MIT".into()),
                repository: None,
            },
        )
        .unwrap();
        let asset = sign_asset(
            &keys,
            &Asset {
                app_id: "com.example.Hello".into(),
                version: "1.0.0".into(),
                digest: format!("sha256:{}", "ab".repeat(32)),
                location: "oci:/srv/hello".into(),
                runtime: Some("org.freedesktop.Platform/x86_64/25.08".into()),
            },
        )
        .unwrap();
        let release = sign_release(
            &keys,
            &Release {
                app_id: "com.example.Hello".into(),
                version: "1.0.0".into(),
                channel: "stable".into(),
                notes: Some("First.".into()),
                asset_id: asset.id.to_string(),
            },
        )
        .unwrap();
        let mut feed = crate::Feed::new();
        for event in [&listing, &asset, &release] {
            feed.absorb(event.clone()).unwrap();
        }
        let catalog = feed.catalog();
        assert_eq!(catalog.apps.len(), 1);
        let app = &catalog.apps[0];
        assert_eq!(app.version, "1.0.0");
        assert_eq!(app.name, "Hello");
        assert_eq!(app.categories, ["Utilities"]);
        assert_eq!(app.description.as_deref(), Some("Long description."));
    }
}
