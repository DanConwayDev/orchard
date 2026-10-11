//! Sign and publish one review (kind 30065): the reference for the
//! client's post path, useful as a test driver for the review pipeline.
//!
//!     review --nsec <hex> --app-id com.orchard.Hello --rating 4.5 \
//!            --text "Works on my machine." [--relay ws://...]

use anyhow::{bail, Result};
use nostr::prelude::*;
use orchard_nostr::events::{self, Review};
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
        Ok(value) if !value.trim().is_empty() => {
            value.split(',').next().unwrap_or("ws://127.0.0.1:8008").to_string()
        }
        _ => "ws://127.0.0.1:8008".to_string(),
    }];
    let mut app_id = None;
    let mut rating = None;
    let mut text = String::new();
    while let Some(flag) = values.next() {
        let mut next = || values.next().ok_or_else(|| anyhow::anyhow!("missing value for {flag}"));
        match flag.as_str() {
            "--nsec" => nsec = Some(next()?),
            "--relay" => relays = vec![next()?],
            "--app-id" => app_id = Some(next()?),
            "--rating" => rating = Some(next()?.parse::<f32>()?),
            "--text" => text = next()?,
            other => bail!("unknown argument {other:?}"),
        }
    }
    let app_id = app_id.ok_or_else(|| anyhow::anyhow!("--app-id required"))?;
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
    println!("reviewer {}", keys.get_public_key()?.to_string());
    let event = events::sign_review(
        &keys,
        &Review {
            app_id,
            rating,
            content: text,
        },
    )?;
    for relay_url in &relays {
        relay::send(relay_url, &[event.clone()])?;
    }
    println!("review published");
    Ok(())
}
