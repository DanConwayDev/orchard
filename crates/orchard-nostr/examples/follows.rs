//! Sign and publish one contact list (kind 3): the demo seeder for
//! the web-of-truth graph. Follows are real social data; this tool
//! exists so a dev relay can stage scenarios the client renders.
//!
//!     follows --nsec <hex> [--relay ws://...] <pubkey hex>...
//!
//! Each positional argument is a 64-character hex pubkey to follow.
//! Without --nsec an ephemeral key is generated (printed once, then
//! gone).

use anyhow::{bail, Result};
use nostr::prelude::*;
use orchard_nostr::relay;

fn main() {
    if let Err(err) = run() {
        eprintln!("follows failed: {err:#}");
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
    let mut follows = Vec::new();
    while let Some(flag) = values.next() {
        let mut next = || values.next().ok_or_else(|| anyhow::anyhow!("missing value for {flag}"));
        match flag.as_str() {
            "--nsec" => nsec = Some(next()?),
            "--relay" => relays = vec![next()?],
            other if other.starts_with("npub1") => {
                // bech32 accepted too: decode to hex
                let key = PublicKey::from_bech32(other)?;
                follows.push(key.to_string());
            }
            other => follows.push(other.to_string()),
        }
    }
    if follows.is_empty() {
        bail!("usage: follows --nsec <hex> [--relay url] <pubkey hex or npub>...");
    }
    for pk in &follows {
        if PublicKey::from_hex(pk).is_err() {
            bail!("{pk} is not a hex pubkey or npub");
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
    println!("publisher {}", keys.get_public_key()?.to_string());
    let mut builder = EventBuilder::new(Kind::ContactList, "");
    for pk in &follows {
        builder = builder.tag(Tag::custom("p", [pk.as_str()]));
    }
    let event = builder.finalize(&keys)?;
    for relay_url in &relays {
        relay::send(relay_url, &[event.clone()])?;
    }
    println!("contact list published: follows {} key{}", follows.len(), if follows.len() == 1 { "" } else { "s" });
    Ok(())
}
