//! Sign and publish NIP-82 events for one app release: the hand-rolled
//! test publisher for the M3 vertical slice. It is the reference for what
//! `orchard-publish` (M5) will do properly.
//!
//! Usage (all keys/options long-form, no dependencies):
//!
//!     publish \
//!       --nsec 5122b2...            # or ORCHARD_NSEC env; omitted: generate + print
//!       --relay ws://127.0.0.1:8008 # repeatable
//!       --app-id com.example.App
//!       --name "Example App" --summary "Does a thing"
//!       --description "Longer markdown description."
//!       --icon https://example.com/icon.png --category Utilities
//!       --version 1.2.0
//!       --location oci:/srv/registry/app   # or docker://host/repo@sha256:...
//!       --digest sha256:abcd...            # 64 hex chars
//!       --runtime org.freedesktop.Platform/x86_64/25.08
//!       --channel stable --notes "What changed."
//!       --author-name "Example Dev" --nip05 dev@example.com
//!
//! Publishes: profile (optional), listing, asset, release. Waits for the
//! relay's OK for each event; exits nonzero on any rejection.

use anyhow::{bail, Result};
use nostr::prelude::*;
use orchard_nostr::{KIND_APP, KIND_ASSET, KIND_RELEASE};
use tungstenite::Message;

fn main() {
    if let Err(err) = run() {
        eprintln!("publish failed: {err:#}");
        std::process::exit(1);
    }
}

struct Args {
    nsec: Option<String>,
    relays: Vec<String>,
    app_id: String,
    name: String,
    summary: String,
    description: Option<String>,
    icon: Option<String>,
    categories: Vec<String>,
    version: String,
    location: String,
    digest: String,
    runtime: Option<String>,
    channel: String,
    notes: Option<String>,
    author_name: Option<String>,
    nip05: Option<String>,
    arch: String,
}

fn parse_args() -> Result<Args> {
    let mut values = std::env::args().skip(1);
    let mut nsec = std::env::var("ORCHARD_NSEC").ok();
    let mut relays = vec!["ws://127.0.0.1:8008".to_string()];
    let mut app_id = None;
    let mut name = None;
    let mut summary = None;
    let mut description = None;
    let mut icon = None;
    let mut categories = Vec::new();
    let mut version = None;
    let mut location = None;
    let mut digest = None;
    let mut runtime = None;
    let mut channel = "stable".to_string();
    let mut notes = None;
    let mut author_name = None;
    let mut nip05 = None;
    let mut arch = "linux-x86_64".to_string();
    while let Some(flag) = values.next() {
        let mut next = || {
            values.next().ok_or_else(|| anyhow::anyhow!("missing value for {flag}"))
        };
        match flag.as_str() {
            "--nsec" => nsec = Some(next()?),
            "--relay" => relays = vec![next()?],
            "--app-id" => app_id = Some(next()?),
            "--name" => name = Some(next()?),
            "--summary" => summary = Some(next()?),
            "--description" => description = Some(next()?),
            "--icon" => icon = Some(next()?),
            "--category" => categories.push(next()?),
            "--version" => version = Some(next()?),
            "--location" => location = Some(next()?),
            "--digest" => digest = Some(next()?),
            "--runtime" => runtime = Some(next()?),
            "--channel" => channel = next()?,
            "--notes" => notes = Some(next()?),
            "--author-name" => author_name = Some(next()?),
            "--nip05" => nip05 = Some(next()?),
            "--arch" => arch = next()?,
            other => bail!("unknown argument {other:?}"),
        }
    }
    Ok(Args {
        nsec,
        relays,
        app_id: app_id.ok_or_else(|| anyhow::anyhow!("--app-id required"))?,
        name: name.ok_or_else(|| anyhow::anyhow!("--name required"))?,
        summary: summary.ok_or_else(|| anyhow::anyhow!("--summary required"))?,
        description,
        icon,
        categories,
        version: version.ok_or_else(|| anyhow::anyhow!("--version required"))?,
        location: location.ok_or_else(|| anyhow::anyhow!("--location required"))?,
        digest: digest.ok_or_else(|| anyhow::anyhow!("--digest required"))?,
        runtime,
        channel,
        notes,
        author_name,
        nip05,
        arch,
    })
}

fn run() -> Result<()> {
    let args = parse_args()?;

    // keys: explicit nsec beats env; nothing given means generate once and
    // print so the operator can pin it in ORCHARD_NSEC for next time
    let keys = match args.nsec.as_deref() {
        Some(raw) => Keys::parse(raw.trim())?,
        None => {
            let generated = Keys::generate();
            eprintln!(
                "no key given: generated ephemeral key, USE IT AGAIN OR LOSE THE IDENTITY:\n  nsec {}",
                generated.secret_key().to_secret_hex()
            );
            generated
        }
    };
    let pubkey = keys.get_public_key()?.to_string();
    println!("publisher {pubkey}");

    // asset first: the release references its event id
    let mut asset_tags = vec![
        ("i", args.app_id.as_str()),
        ("x", args.digest.as_str()),
        ("version", args.version.as_str()),
        ("oci", args.location.as_str()),
    ];
    let asset_runtime: Option<(&str, &str)> = args
        .runtime
        .as_deref()
        .map(|runtime| ("runtime", runtime));
    if let Some((name, value)) = asset_runtime {
        asset_tags.push((name, value));
    }
    let asset = signed(&keys, KIND_ASSET, "", asset_tags)?;
    let asset_id = asset.id.to_string();

    let release = signed(
        &keys,
        KIND_RELEASE,
        args.notes.as_deref().unwrap_or(""),
        vec![
            ("d", &format!("{}@{}", args.app_id, args.version)[..]),
            ("i", args.app_id.as_str()),
            ("version", args.version.as_str()),
            ("c", args.channel.as_str()),
            ("e", asset_id.as_str()),
        ],
    )?;

    let mut listing_tags = vec![
        ("d", args.app_id.as_str()),
        ("name", args.name.as_str()),
        ("summary", args.summary.as_str()),
        ("f", args.arch.as_str()),
    ];
    if let Some(icon) = args.icon.as_deref() {
        listing_tags.push(("icon", icon));
    }
    for category in &args.categories {
        listing_tags.push(("t", category.as_str()));
    }
    let listing = signed(&keys, KIND_APP, args.description.as_deref().unwrap_or(""), listing_tags)?;

    let mut events = vec![asset, release, listing];
    // profile last so a fresh relay session has the listing already
    if args.author_name.is_some() || args.nip05.is_some() {
        let profile = serde_json::json!({
            "name": args.author_name.clone().unwrap_or_else(|| args.name.clone()),
            "nip05": args.nip05,
        });
        // kind 0 content is the metadata document itself
        let profile = EventBuilder::new(Kind::Metadata, profile.to_string())
            .finalize(&keys)?;
        events.push(profile);
    }

    for relay in &args.relays {
        publish_to(relay, &events)?;
    }
    println!("published {} events", events.len());
    Ok(())
}

fn signed(
    keys: &Keys,
    kind: u32,
    content: &str,
    tags: Vec<(&str, &str)>,
) -> Result<Event> {
    let builder = EventBuilder::new(Kind::Custom(kind as u16), content).tags(tags.into_iter().map(
        |(name, value)| Tag::custom(name, [value]),
    ));
    Ok(builder.finalize(keys)?)
}

fn publish_to(relay: &str, events: &[Event]) -> Result<()> {
    let (mut socket, _response) = tungstenite::connect(relay)?;
    for event in events {
        socket.send(Message::text(
            serde_json::json!(["EVENT", event]).to_string(),
        ))?;
    }
    // read OKs until every event is acknowledged or the relay errs
    let mut confirmed = 0;
    while confirmed < events.len() {
        let reply = socket.read()?;
        let Message::Text(text) = reply else {
            continue;
        };
        let frame: serde_json::Value = serde_json::from_str(text.as_str())?;
        match frame.as_array().and_then(|rows| rows.first()).and_then(|v| v.as_str()) {
            Some("OK") => {
                let (id, accepted, message) = (
                    frame.get(1).and_then(|v| v.as_str()).unwrap_or("?"),
                    frame.get(2).and_then(|v| v.as_bool()).unwrap_or(false),
                    frame.get(3).and_then(|v| v.as_str()).unwrap_or(""),
                );
                if accepted {
                    confirmed += 1;
                    println!("  {relay}: ok {id}");
                } else {
                    bail!("{relay}: rejected {id}: {message}");
                }
            }
            Some("NOTICE") => {
                bail!("{relay}: notice {}", frame.get(1).and_then(|v| v.as_str()).unwrap_or(""))
            }
            _ => {}
        }
    }
    Ok(())
}

