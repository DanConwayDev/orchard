//! Sign and publish one publisher migration (kind 30066, Orchard
//! draft): the OLD key designates a successor key for one app id, or
//! with `--revoke` takes the designation back. The feed suppresses
//! the old key's listing once the successor lists the same app id.
//!
//!     migrate --nsec <hex> [--relay ws://...] [--revoke] <app-id> <successor pubkey hex or npub>
//!
//! Without --nsec an ephemeral key is generated (printed once, then
//! gone).

use anyhow::{bail, Result};
use nostr::prelude::*;
use orchard_nostr::relay;

fn main() {
    if let Err(err) = run() {
        eprintln!("migrate failed: {err:#}");
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
    let mut revoke = false;
    let mut positional: Vec<String> = Vec::new();
    while let Some(flag) = values.next() {
        let mut next =
            || values.next().ok_or_else(|| anyhow::anyhow!("missing value for {flag}"));
        match flag.as_str() {
            "--nsec" => nsec = Some(next()?),
            "--relay" => relays = vec![next()?],
            "--revoke" => revoke = true,
            other => positional.push(other.to_string()),
        }
    }
    let (app_id, successor) = match positional.len() {
        2 => (positional[0].clone(), Some(positional[1].clone())),
        1 if revoke => (positional[0].clone(), None),
        _ => {
            bail!(
                "usage: migrate --nsec <hex> [--revoke] <app-id> <successor pubkey hex or npub>"
            );
        }
    };
    let successor = successor
        .map(|raw| {
            if raw.starts_with("npub1") {
                // bech32 accepted too: decode to hex
                Ok(PublicKey::from_bech32(&raw)?.to_string())
            } else if PublicKey::from_hex(&raw).is_ok() {
                Ok(raw)
            } else {
                bail!("{raw} is not a hex pubkey or npub");
            }
        })
        .transpose()?;
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
    println!("migrating publisher {}", keys.get_public_key()?.to_string());
    let mut builder = EventBuilder::new(Kind::Custom(orchard_nostr::KIND_MIGRATION as u16), "")
        .tag(Tag::custom("d", [app_id.as_str()]));
    if let Some(successor) = &successor {
        builder = builder.tag(Tag::custom("p", [successor.as_str()]));
    }
    let event = builder.finalize(&keys)?;
    for relay_url in &relays {
        relay::send(relay_url, &[event.clone()])?;
    }
    match successor {
        Some(successor) => println!(
            "migration published: {app_id} passes to {}",
            &successor[..16.min(successor.len())],
        ),
        None => println!("migration revoked: {app_id} keeps its current key"),
    }
    Ok(())
}
