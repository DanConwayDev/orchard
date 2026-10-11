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

/// NIP-82 kinds Orchard reads: profiles, listings, releases, assets,
/// reviews.
const SUB_KINDS: [u16; 5] = [
    crate::KIND_PROFILE as u16,
    crate::KIND_APP as u16,
    crate::KIND_RELEASE as u16,
    crate::KIND_ASSET as u16,
    crate::KIND_REVIEW as u16,
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
pub fn send(relay_url: &str, events: &[Event]) -> Result<()> {    let (mut socket, _response) = tungstenite::connect(relay_url)?;
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

/// One read-only, anonymous query for a key's kind 3 contact list:
/// connect, REQ (kinds [3], authors [pubkey], limit 1), read until
/// EOSE or the deadline, return the newest event's `p` tag pubkeys.
/// Blocking with a hard deadline; run on a worker thread. The fetch
/// carries only the key: no read receipt, no profile exposure.
pub fn fetch_contacts(relay_url: &str, pubkey: &str, timeout: Duration) -> Result<Vec<String>> {
    let req = serde_json::json!([
        "REQ",
        "orchard-contacts",
        { "kinds": [Kind::ContactList], "authors": [pubkey], "limit": 1u16 },
    ])
    .to_string();
    let (mut socket, _response) = tungstenite::connect(relay_url)?;
    crate::connect::set_read_timeout(&mut socket, timeout);
    socket.send(Message::text(req))?;
    let mut contacts: Vec<String> = Vec::new();
    loop {
        let message = match socket.read() {
            Ok(message) => message,
            // the read deadline: serve whatever arrived
            Err(tungstenite::Error::Io(err))
                if err.kind() == std::io::ErrorKind::WouldBlock
                    || err.kind() == std::io::ErrorKind::TimedOut =>
            {
                break
            }
            Err(err) => return Err(err.into()),
        };
        match message {
            Message::Text(text) => {
                let frame: serde_json::Value = serde_json::from_str(text.as_str())?;
                let label = frame
                    .as_array()
                    .and_then(|rows| rows.first())
                    .and_then(|v| v.as_str());
                match label {
                    Some("EVENT") => {
                        let raw = frame
                            .get(2)
                            .ok_or_else(|| anyhow::anyhow!("EVENT frame without event"))?;
                        let event: Event = serde_json::from_value(raw.clone())?;
                        // guard against relays that ignore the authors
                        // filter; only the newest kind 3 wins
                        if event.kind == Kind::ContactList && event.pubkey.to_string() == pubkey {
                            contacts = contacts_from_event(&event);
                        }
                    }
                    Some("EOSE") => break,
                    _ => {}
                }
            }
            Message::Ping(payload) => socket.send(Message::Pong(payload))?,
            Message::Close(_) => break,
            _ => {}
        }
    }
    Ok(contacts)
}

/// The `p` tag pubkeys of a contact-list event, in tag order.
fn contacts_from_event(event: &Event) -> Vec<String> {
    event
        .tags
        .iter()
        .filter(|tag| tag.as_slice().first().is_some_and(|name| name.as_str() == "p"))
        .filter_map(|tag| tag.as_slice().get(1).map(|v| v.to_string()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The contact list's follows are its p tags, in tag order.
    #[test]
    fn contacts_from_event_reads_p_tags() {
        let keys = Keys::generate();
        let event = EventBuilder::new(Kind::ContactList, "")
            .tag(Tag::custom("p", ["aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "follows-something"]))
            .tag(Tag::custom("p", ["bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"]))
            .tag(Tag::custom("t", ["hashtag"]))
            .finalize(&keys)
            .unwrap();
        let contacts = contacts_from_event(&event);
        assert_eq!(contacts.len(), 2);
        assert!(contacts[0].starts_with("aaaaaaaa"));
        assert!(contacts[1].starts_with("bbbbbbbb"));
    }
}
