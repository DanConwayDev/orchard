//! Minimal NIP-01 relay client: subscribe to NIP-82 kinds, verify, absorb.
//!
//! One blocking thread per relay URL. The Nostr wire protocol for reads is
//! tiny: send a `REQ`, then process `EVENT` frames until the relay closes
//! (the subscription stays open, so new events arrive live). Reconnects
//! with a fixed backoff; relays are dumb mirrors, so idempotent re-REQs are
//! safe (the feed deduplicates).
//!
//! Deliberately no async runtime here: indexd owns the threading model and
//! hands us a shared [`Feed`] behind a mutex.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use nostr::prelude::*;
use tungstenite::Message;

use crate::Feed;

/// NIP-82 kinds Orchard reads: profiles, listings, releases, assets.
const SUB_KINDS: [u16; 4] = [
    crate::KIND_PROFILE as u16,
    crate::KIND_APP as u16,
    crate::KIND_RELEASE as u16,
    crate::KIND_ASSET as u16,
];

const BACKOFF: Duration = Duration::from_secs(5);

/// Run one relay until the process exits, reconnecting on errors. Calls
/// `on_change` whenever the feed may have grown. Never returns normally.
pub fn run_forever(
    url: &str,
    feed: Arc<Mutex<Feed>>,
    on_change: Arc<dyn Fn() + Send + Sync>,
) {
    let sub = serde_json::json!([
        "REQ",
        "orchard",
        { "kinds": SUB_KINDS },
    ])
    .to_string();
    loop {
        match session(url, &sub, &feed, &on_change) {
            Ok(()) => {
                // clean close: still a stop; retry in case the relay restarts
                std::thread::sleep(BACKOFF);
            }
            Err(err) => {
                eprintln!("relay {url}: {err:#}; retrying in {}s", BACKOFF.as_secs());
                std::thread::sleep(BACKOFF);
            }
        }
    }
}

fn session(
    url: &str,
    sub: &str,
    feed: &Arc<Mutex<Feed>>,
    on_change: &Arc<dyn Fn() + Send + Sync>,
) -> Result<()> {
    let (mut socket, _response) = tungstenite::connect(url)?;
    socket.send(Message::text(sub))?;
    loop {
        let message = socket.read()?;
        match message {
            Message::Text(text) => {
                if handle_frame(text.as_str(), feed)? {
                    on_change();
                }
            }
            Message::Ping(payload) => socket.send(Message::Pong(payload))?,
            Message::Close(_) => return Ok(()),
            _ => {}
        }
    }
}

/// Process one text frame. Returns true if an event was absorbed.
fn handle_frame(text: &str, feed: &Arc<Mutex<Feed>>) -> Result<bool> {
    let frame: serde_json::Value = serde_json::from_str(text)?;
    match frame.as_array().and_then(|rows| rows.first()).and_then(|v| v.as_str()) {
        // ["EVENT", <sub-id>, <event>]
        Some("EVENT") => {
            let raw = frame.get(2).ok_or_else(|| anyhow::anyhow!("EVENT frame without event"))?;
            let event: Event = serde_json::from_value(raw.clone())?;
            let mut feed = feed.lock().expect("feed lock poisoned");
            feed.absorb(event)
        }
        // ["EOSE", <sub-id>]: stored subscription stays open, keep reading
        Some("EOSE") => Ok(false),
        // NOTICE/OK/CLOSED: nothing to act on for a read-only sub
        _ => Ok(false),
    }
}

/// Publish signed events to one relay and wait for every OK. A rejection
/// or notice is a hard error: a relay that silently drops events would
/// strand the release.
pub fn send(relay_url: &str, events: &[Event]) -> Result<()> {
    let (mut socket, _response) = tungstenite::connect(relay_url)?;
    for event in events {
        socket.send(Message::text(
            serde_json::json!(["EVENT", event]).to_string(),
        ))?;
    }
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
                    println!("  {relay_url}: ok {id}");
                } else {
                    anyhow::bail!("{relay_url}: rejected {id}: {message}");
                }
            }
            Some("NOTICE") => anyhow::bail!(
                "{relay_url}: notice {}",
                frame.get(1).and_then(|v| v.as_str()).unwrap_or("")
            ),
            _ => {}
        }
    }
    Ok(())
}
