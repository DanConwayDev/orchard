//! Sign and publish one review (kind 30065): the demo seeder for the
//! comment rows. Reviews in the real client are signed through the
//! user's NIP-46 remote signer; this tool exists so a dev relay can
//! stage scenarios the client renders.
//!
//!     review --nsec <hex> [--relay ws://...] <app-id> <rating> <text...>
//!
//! Rating is 0.0 to 5.0 (omit the tag with --no-rating). Without
//! --nsec an ephemeral key is generated (printed once, then gone).

use anyhow::{bail, Result};
use nostr::prelude::*;
use orchard_nostr::relay;

fn main() {
    if let Err(err) = run() {
        eprintln!("review failed: {err:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let mut values = std::env::args().skip(1);
    let mut nsec = std::env::var("ORCHARD_NSEC").ok();
    let mut relays = vec![match std::env::var("ORCHARD_RELAYS") {
        Ok(value) if !value.trim().is_empty() => value
            .split(',')
            .next()
            .unwrap_or("ws://127.0.0.1:8008")
            .to_string(),
        _ => "ws://127.0.0.1:8008".to_string(),
    }];
    let mut rating: Option<String> = None;
    let mut positional: Vec<String> = Vec::new();
    while let Some(flag) = values.next() {
        let mut next =
            || values.next().ok_or_else(|| anyhow::anyhow!("missing value for {flag}"));
        match flag.as_str() {
            "--nsec" => nsec = Some(next()?),
            "--relay" => relays = vec![next()?],
            "--rating" => rating = Some(next()?),
            other => positional.push(other.to_string()),
        }
    }
    if positional.len() < 2 {
        bail!("usage: review --nsec <hex> [--rating 0-5] <app-id> <text...>");
    }
    let app_id = positional.remove(0);
    let content = positional.join(" ");
    if let Some(value) = &rating {
        let value: f32 = value.parse()?;
        if !(0.0..=5.0).contains(&value) {
            bail!("rating must be 0.0 to 5.0");
        }
    }
    let keys = match nsec.as_deref() {
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
    println!("reviewing as {}", keys.get_public_key()?.to_string());
    let mut builder = EventBuilder::new(Kind::Custom(orchard_nostr::KIND_REVIEW as u16), &content)
        .tag(Tag::custom("d", [app_id.as_str()]));
    if let Some(rating) = &rating {
        builder = builder.tag(Tag::custom("rating", [rating.as_str()]));
    }
    let event = builder.finalize(&keys)?;
    for relay_url in &relays {
        relay::send(relay_url, &[event.clone()])?;
    }
    println!("review published: {app_id}");
    Ok(())
}
