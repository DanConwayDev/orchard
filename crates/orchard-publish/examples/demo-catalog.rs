//! Offline demo catalog: sign one app's NIP-82 events with a fresh
//! ephemeral key and project them through the real `Feed` into the
//! catalog file the client's file fallback reads. No relays, no
//! registry, nothing sent anywhere: the location is the local OCI
//! layout `orchard-publish` builds without `--registry`, so the whole
//! install/update loop runs offline.
//!
//!     cargo run -p orchard-publish --example demo-catalog -- \
//!         demo/hello.publish.toml \
//!         <artifact digest, printed by the publish run> \
//!         demo/catalog.json [<event cache to append to>]
//!
//! The optional fourth argument is the client's event cache
//! (`$XDG_DATA_HOME/orchard/nostr-events.json`, a JSON array of
//! serialized events): the signed events land there and the embedded
//! indexd absorbs them on the next launch, no relay needed. The relay
//! path stays the real one for production.

use anyhow::{bail, Result};
use nostr::prelude::*;
use orchard_nostr::{Feed, KIND_APP, KIND_ASSET, KIND_RELEASE};
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Deserialize)]
struct Manifest {
    app: App,
    release: Release,
}

#[derive(Deserialize)]
struct App {
    id: String,
    name: String,
    summary: String,
    description: Option<String>,
    categories: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct Release {
    version: String,
    arch: Option<String>,
    notes: Option<String>,
    runtime: Option<String>,
}

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let Some(manifest_path) = args.next() else {
        bail!("usage: demo-catalog <manifest> <artifact-digest> <out>");
    };
    let Some(digest) = args.next() else {
        bail!("missing the artifact digest (the publish run prints it)");
    };
    let Some(out) = args.next() else {
        bail!("missing the output catalog path");
    };
    let digest = if digest.starts_with("sha256:") {
        digest
    } else {
        format!("sha256:{digest}")
    };

    let manifest: Manifest =
        toml::from_str(&std::fs::read_to_string(&manifest_path)?)?;
    let app_id = manifest.app.id.clone();
    let arch = manifest
        .release
        .arch
        .clone()
        .unwrap_or_else(|| "linux-x86_64".to_string());
    let version = manifest.release.version.clone();

    // the location ships as an absolute path: the GUI may run from
    // anywhere
    let layout =
        std::fs::canonicalize(PathBuf::from("target/publish").join(&app_id))?;
    let location = format!("oci:{}", layout.display());

    let keys = Keys::generate();
    let pubkey = keys.public_key();

    let listing = EventBuilder::new(
        Kind::Custom(KIND_APP as u16),
        manifest.app.description.clone().unwrap_or_default(),
    )
    .tags([
        Tag::custom("d", [app_id.as_str()]),
        Tag::custom("name", [manifest.app.name.as_str()]),
        Tag::custom("summary", [manifest.app.summary.as_str()]),
        Tag::custom(
            "t",
            [manifest
                .app
                .categories
                .as_deref()
                .and_then(|c| c.first())
                .map(String::as_str)
                .unwrap_or("Utility")],
        ),
        Tag::custom("f", [arch.as_str()]),
    ])
    .finalize(&keys)?;

    let asset = EventBuilder::new(Kind::Custom(KIND_ASSET as u16), "")
        .tags([
            Tag::custom("i", [app_id.as_str()]),
            Tag::custom("x", [digest.as_str()]),
            Tag::custom("version", [version.as_str()]),
            Tag::custom("f", [arch.as_str()]),
            Tag::custom("oci", [location.as_str()]),
        ])
        .finalize(&keys)?;
    let asset_id = asset.id.to_string();

    let d = format!("{app_id}@{version}@{arch}");
    let mut release_tags = vec![
        Tag::custom("d", [d.as_str()]),
        Tag::custom("i", [app_id.as_str()]),
        Tag::custom("version", [version.as_str()]),
        Tag::custom("f", [arch.as_str()]),
        Tag::custom("c", ["stable"]),
        Tag::custom("e", [asset_id.as_str()]),
    ];
    if let Some(runtime) = &manifest.release.runtime {
        release_tags.push(Tag::custom("runtime", [runtime.as_str()]));
    }
    let release = EventBuilder::new(
        Kind::Custom(KIND_RELEASE as u16),
        manifest.release.notes.clone().unwrap_or_default(),
    )
    .tags(release_tags)
    .finalize(&keys)?;

    let events = [listing, asset, release];
    let mut feed = Feed::new();
    for event in events.clone() {
        feed.absorb(event)?;
    }
    let catalog = feed.catalog_for(&arch);
    if catalog.apps.is_empty() {
        bail!("the events did not project into a catalog row");
    }
    std::fs::write(&out, serde_json::to_string_pretty(&catalog)?)?;
    println!("wrote {out}: {} row(s)", catalog.apps.len());
    println!("  publisher {pubkey}");
    println!("  version   {}", catalog.apps[0].version);
    println!("  location  {}", catalog.apps[0].location);
    println!(
        "  digest    {}",
        catalog.apps[0].digest.as_deref().unwrap_or("-")
    );

    // optional: append the signed events to the client's event cache,
    // so the embedded indexd serves the row without any relay
    if let Some(cache) = args.next() {
        let mut events_cache: Vec<String> = std::fs::read(&cache)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        for event in events {
            events_cache.push(serde_json::to_string(&event)?);
        }
        std::fs::write(&cache, serde_json::to_string(&events_cache)?)?;
        println!("appended 3 events to {}", cache);
    }
    Ok(())
}
