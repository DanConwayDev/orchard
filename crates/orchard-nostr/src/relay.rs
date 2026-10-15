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

/// The relay the project operates, and the one every publish must
/// reach: first in the rendezvous set. The third-party relays are
/// redundancy; they can refuse or drop Orchard events at any time, so
/// the catalog's canonical copy always lands on infrastructure the
/// project controls. `orchard-publish` refuses a manifest-driven
/// publish whose relay list omits this relay; `--relays` bypasses the
/// check for local tests.
pub const REQUIRED_RELAY: &str = "wss://relay.orchard.rocks";

/// The rendezvous relay set: where publishers announce catalog events
/// and where clients read them from. Small and public on purpose, led
/// by [`REQUIRED_RELAY`]. The client's default list and every
/// publisher's manifest overlap here; discovery is the overlap, and
/// the relays themselves are mailboxes, not trust roots.
/// Pointer-following (NIP-65) for self-hosting publishers is a future
/// edge, not part of this floor.
pub const RENDEZVOUS_RELAYS: &[&str] = &[
    REQUIRED_RELAY,
    "wss://relay.primal.net",
    "wss://nos.lol",
];

/// The list plus the required relay: a custom list that omits it gets
/// it prepended, so a reader (and the settings page that edits it) can
/// trim the third-party relays but never the project's own.
pub fn with_required_relay(mut urls: Vec<String>) -> Vec<String> {
    if !urls.iter().any(|url| url == REQUIRED_RELAY) {
        urls.insert(0, REQUIRED_RELAY.to_string());
    }
    urls
}

/// Parse an `ORCHARD_RELAYS`-style comma list into URLs, trimming and
/// dropping empties. No fallback: the caller decides the default.
pub fn parse_urls(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .map(str::to_string)
        .collect()
}

/// Parse an `ORCHARD_RELAYS`-style comma list, falling back to the
/// rendezvous set when unset or blank.
pub fn relay_urls(env_value: Option<&str>) -> Vec<String> {
    match env_value {
        Some(value) if !value.trim().is_empty() => parse_urls(value),
        _ => RENDEZVOUS_RELAYS
            .iter()
            .map(|url| url.to_string())
            .collect(),
    }
}

/// The social relay set: where kind 0 profiles and kind 3 contact
/// lists live. The rendezvous set by default; ORCHARD_SOCIAL_RELAYS
/// overrides, so a dev loop can point the social graph at a local
/// relay. Same parsing rules as [`relay_urls`].
pub fn social_relay_urls(env_value: Option<&str>) -> Vec<String> {
    relay_urls(env_value)
}

/// NIP-82 kinds Orchard reads: profiles, listings, releases, assets,
/// reviews, migrations.
const SUB_KINDS: [u16; 6] = [
    crate::KIND_PROFILE as u16,
    crate::KIND_APP as u16,
    crate::KIND_RELEASE as u16,
    crate::KIND_ASSET as u16,
    crate::KIND_REVIEW as u16,
    crate::KIND_MIGRATION as u16,
];

const BACKOFF: Duration = Duration::from_secs(5);

/// How far behind the feed's newest event a reconnect asks: enough
/// overlap to survive clock skew, small enough that a busy relay's
/// re-delivery stays cheap. Longer outages heal through the cache
/// clear's full refill, not this filter.
const SINCE_OVERLAP: u64 = 60 * 60;

/// The subscription text for one session. Before the first full
/// backfill it asks for everything; after, only for events newer than
/// the feed's newest (minus overlap), so a reconnect costs a delta
/// instead of re-downloading and re-verifying the relay's whole
/// history.
fn subscription(feed: &Feed, backfilled: bool) -> String {
    let mut filter = serde_json::json!({ "kinds": SUB_KINDS });
    if backfilled
        && let Some(newest) = feed.newest_created_at()
    {
        filter["since"] = serde_json::json!(newest.saturating_sub(SINCE_OVERLAP));
    }
    serde_json::json!(["REQ", "orchard", filter]).to_string()
}

/// How long one relay read may block before the loop wakes to check
/// the stop flag. Long enough that it is invisible; short enough that
/// a removed relay's thread is gone in under a minute.
const READ_WAKE: Duration = Duration::from_secs(30);

/// Run one relay until the process exits or `stop` flips, reconnecting
/// on errors. Calls `on_change` whenever the feed may have grown.
/// Returns when asked to stop; the caller may then drop the thread.
pub fn run_forever(
    url: &str,
    feed: Arc<Mutex<Feed>>,
    on_change: Arc<dyn Fn() + Send + Sync>,
    stop: Arc<std::sync::atomic::AtomicBool>,
) {
    // one clean look at the relay's stored history (a session that
    // saw its EOSE) is what makes the since-filter safe: until then
    // a reconnect re-pulls everything, so an interrupted first sync
    // heals instead of freezing a partial catalog in
    let mut backfilled = false;
    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
        // rebuilt per session: the anchor moves as the feed grows
        let sub = subscription(
            &feed
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
            backfilled,
        );
        // one panicked session must not take the thread down: a
        // background thread's panic is invisible in the UI, so it
        // costs a log line and the loop continues instead
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            session(url, &sub, &feed, &on_change, &stop)
        }));
        match outcome {
            Ok(Ok(saw_eose)) => {
                backfilled |= saw_eose;
                // clean close: still a stop; retry in case the relay restarts
                std::thread::sleep(BACKOFF);
            }
            Ok(Err(err)) => {
                eprintln!("relay {url}: {err:#}; retrying in {}s", BACKOFF.as_secs());
                std::thread::sleep(BACKOFF);
            }
            Err(panic) => {
                let message = panic_message(&panic);
                eprintln!("relay {url}: session panicked: {message}; retrying in {}s", BACKOFF.as_secs());
                std::thread::sleep(BACKOFF);
            }
        }
    }
}

/// The human part of a caught panic payload, for the log line.
fn panic_message(panic: &Box<dyn std::any::Any + Send>) -> String {
    panic
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".to_string())
}

fn session(
    url: &str,
    sub: &str,
    feed: &Arc<Mutex<Feed>>,
    on_change: &Arc<dyn Fn() + Send + Sync>,
    stop: &Arc<std::sync::atomic::AtomicBool>,
) -> Result<bool> {
    let (mut socket, _response) = tungstenite::connect(url)?;
    crate::connect::set_read_timeout(&mut socket, READ_WAKE);
    socket.send(Message::text(sub))?;
    // whether this session saw the relay's end-of-stored-events: the
    // caller's next subscription filters on since only once this has
    // happened once
    let mut saw_eose = false;
    loop {
        let message = match socket.read() {
            Ok(message) => message,
            // the wake-up: the subscription stays open server-side, so
            // this is only a checkpoint for the stop flag
            Err(tungstenite::Error::Io(err))
                if err.kind() == std::io::ErrorKind::WouldBlock
                    || err.kind() == std::io::ErrorKind::TimedOut =>
            {
                if stop.load(std::sync::atomic::Ordering::Relaxed) {
                    return Ok(saw_eose);
                }
                continue;
            }
            Err(err) => return Err(err.into()),
        };
        if stop.load(std::sync::atomic::Ordering::Relaxed) {
            return Ok(saw_eose);
        }
        match message {
            Message::Text(text) => {
                let (absorbed, eose) = handle_frame(text.as_str(), feed)?;
                saw_eose |= eose;
                if absorbed {
                    on_change();
                }
            }
            Message::Ping(payload) => socket.send(Message::Pong(payload))?,
            Message::Close(_) => return Ok(saw_eose),
            _ => {}
        }
    }
}

/// Process one text frame. Returns whether an event was absorbed and
/// whether the frame was the relay's end-of-stored-events marker.
pub(crate) fn handle_frame(text: &str, feed: &Arc<Mutex<Feed>>) -> Result<(bool, bool)> {
    let frame: serde_json::Value = serde_json::from_str(text)?;
    match frame.as_array().and_then(|rows| rows.first()).and_then(|v| v.as_str()) {
        // ["EVENT", <sub-id>, <event>]
        Some("EVENT") => {
            let raw = frame.get(2).ok_or_else(|| anyhow::anyhow!("EVENT frame without event"))?;
            let event: Event = match serde_json::from_value(raw.clone()) {
                Ok(event) => event,
                // an unparseable event is relay garbage: skip it and
                // keep the session, never wedge the sync on content
                Err(err) => {
                    eprintln!("relay event unparseable, skipping: {err}");
                    return Ok((false, false));
                }
            };
            // locks recover on poison: the feed is a dedup cache, so
            // the worst case of continuing is a duplicate the feed
            // absorbs anyway, while a poisoned lock would cascade a
            // single panic into every thread that shares it
            let mut feed = feed
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            match feed.absorb(event) {
                Ok(absorbed) => Ok((absorbed, false)),
                // a rejected event (a tampered signature, a relay
                // lying) skips and the session lives: one hostile
                // event must not freeze the catalog with a retry loop
                Err(err) => {
                    eprintln!("relay event rejected, skipping: {err:#}");
                    Ok((false, false))
                }
            }
        }
        // ["EOSE", <sub-id>]: the stored history is fully delivered;
        // the subscription stays open, keep reading
        Some("EOSE") => Ok((false, true)),
        // NOTICE/OK/CLOSED: nothing to act on for a read-only sub
        _ => Ok((false, false)),
    }
}

/// Publish signed events to one relay and wait for every OK. A rejection
/// or notice is a hard error: a relay that silently drops events would
/// strand the release.
/// How long `send` waits for the relay's OK per event before giving
/// up: a sick relay does not error, it hangs (the quanta panic of
/// 2026-10-11 left this dev relay silently swallowing every verified
/// event for hours), and a publish that waits forever is a publish
/// that never reports why.
const SEND_TIMEOUT: Duration = Duration::from_secs(30);

pub fn send(relay_url: &str, events: &[Event]) -> Result<()> {
    let (mut socket, _response) = tungstenite::connect(relay_url)?;
    crate::connect::set_read_timeout(&mut socket, SEND_TIMEOUT);
    for event in events {
        socket.send(Message::text(
            serde_json::json!(["EVENT", event]).to_string(),
        ))?;
    }
    let mut confirmed = 0;
    while confirmed < events.len() {
        let reply = socket.read().map_err(|err| {
            // a read timeout is the relay going quiet mid-verification:
            // name the failure instead of hanging the publish
            anyhow::anyhow!("{relay_url}: no OK within {}s ({err}); relay may be stalled", SEND_TIMEOUT.as_secs())
        })?;
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

/// One read-only, anonymous query for many keys' events of one kind
/// in a single REQ: connect, REQ (kinds [kind], authors [...]), read
/// until EOSE or the deadline, keep the newest event per author.
/// Blocking with a hard deadline; run on a worker thread. The fetch
/// carries only the keys: no read receipt, no profile exposure.
fn fetch_newest(
    relay_url: &str,
    kind: Kind,
    pubkeys: &[String],
    timeout: Duration,
) -> Result<Vec<Event>> {
    let req = serde_json::json!([
        "REQ",
        "orchard-contacts",
        { "kinds": [kind], "authors": pubkeys },
    ])
    .to_string();
    let (mut socket, _response) = tungstenite::connect(relay_url)?;
    crate::connect::set_read_timeout(&mut socket, timeout);
    socket.send(Message::text(req))?;
    let mut newest: std::collections::HashMap<String, Event> = Default::default();
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
                        absorb_newest(&mut newest, &event, kind, pubkeys);
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
    Ok(newest.into_values().collect())
}

/// Newest event per requested author wins; anything else (other
/// kinds, authors the relay threw in against the filter) is ignored.
fn absorb_newest(
    newest: &mut std::collections::HashMap<String, Event>,
    event: &Event,
    kind: Kind,
    pubkeys: &[String],
) {
    let pubkey = event.pubkey.to_string();
    if event.kind != kind || !pubkeys.contains(&pubkey) {
        return;
    }
    let created = event.created_at.as_secs();
    let known = newest
        .get(&pubkey)
        .map(|event| event.created_at.as_secs())
        .unwrap_or(0);
    if known <= created {
        newest.insert(pubkey, event.clone());
    }
}

/// One read-only, anonymous query for many keys' kind 3 contact
/// lists: the newest contact event per author, as `(pubkey, followed
/// pubkeys)` pairs. Authors with no event come back absent.
pub fn fetch_contact_lists(
    relay_url: &str,
    pubkeys: &[String],
    timeout: Duration,
) -> Result<Vec<(String, Vec<String>)>> {
    Ok(fetch_newest(relay_url, Kind::ContactList, pubkeys, timeout)?
        .into_iter()
        .map(|event| (event.pubkey.to_string(), contacts_from_event(&event)))
        .collect())
}

/// One read-only, anonymous query for a key's kind 0 profile: the
/// newest event's metadata JSON, or None when the relay has none.
/// Blocking with a hard deadline; run on a worker thread.
pub fn fetch_profile_raw(relay_url: &str, pubkey: &str, timeout: Duration) -> Result<Option<String>> {
    Ok(fetch_newest(
        relay_url,
        Kind::Metadata,
        &[pubkey.to_string()],
        timeout,
    )?
    .pop()
    .map(|event| event.content))
}

/// One read-only, anonymous query for many keys' kind 0 profile
/// events in a single REQ, newest per author, full events (the
/// profile crawler absorbs these through the feed's verify path).
/// Blocking with a hard deadline; run on a worker thread. The fetch
/// carries only the keys: no read receipt, no profile exposure.
pub fn fetch_profiles(
    relay_url: &str,
    pubkeys: &[String],
    timeout: Duration,
) -> Result<Vec<Event>> {
    fetch_newest(relay_url, Kind::Metadata, pubkeys, timeout)
}

/// One read-only, anonymous query for a key's kind 3 contact list:
/// the batched query narrowed to one author. Blocking with a hard
/// deadline; run on a worker thread. The fetch carries only the key:
/// no read receipt, no profile exposure.
pub fn fetch_contacts(relay_url: &str, pubkey: &str, timeout: Duration) -> Result<Vec<String>> {
    let keys = [pubkey.to_string()];
    Ok(fetch_contact_lists(relay_url, &keys, timeout)?
        .into_iter()
        .next()
        .map(|(_, follows)| follows)
        .unwrap_or_default())
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

    /// The relay list parser: env wins, blank falls back to the
    /// rendezvous set.
    #[test]
    fn relay_urls_falls_back_to_rendezvous() {
        assert_eq!(
            relay_urls(Some("ws://a:1, ws://b:2")),
            vec!["ws://a:1", "ws://b:2"]
        );
        assert_eq!(
            relay_urls(None),
            RENDEZVOUS_RELAYS.iter().map(|url| url.to_string()).collect::<Vec<_>>()
        );
        assert_eq!(relay_urls(Some("  ")).len(), RENDEZVOUS_RELAYS.len());
    }

    /// The required relay leads the set: the invariant the publish
    /// check and the docs both lean on.
    #[test]
    fn required_relay_leads_the_rendezvous_set() {
        assert_eq!(RENDEZVOUS_RELAYS.first(), Some(&REQUIRED_RELAY));
    }

    /// The pin: a custom list gains the required relay at the front;
    /// one already carrying it is untouched, order included.
    #[test]
    fn with_required_relay_pins_and_preserves() {
        assert_eq!(
            with_required_relay(vec!["wss://a".into()]),
            vec![REQUIRED_RELAY.to_string(), "wss://a".into()]
        );
        let carried = vec!["wss://a".into(), REQUIRED_RELAY.to_string()];
        assert_eq!(with_required_relay(carried.clone()), carried);
    }

    /// A signed stand-in event with full tag control.
    fn signed(keys: &Keys, kind: u32, content: &str, tags: Vec<(&str, &str)>) -> Event {
        EventBuilder::new(Kind::Custom(kind as u16), content)
            .tags(tags.into_iter().map(|(name, value)| Tag::custom(name, [value])))
            .finalize(keys)
            .unwrap()
    }

    /// Before a full backfill the subscription asks for everything.
    #[test]
    fn subscription_asks_for_everything_before_backfill() {
        let feed = Feed::new();
        assert!(!subscription(&feed, false).contains("since"));
    }

    /// An empty feed asks for everything even once backfilled: there
    /// is no anchor to filter from.
    #[test]
    fn subscription_backfilled_empty_feed_omits_since() {
        let feed = Feed::new();
        assert!(!subscription(&feed, true).contains("since"));
    }

    /// After a backfill the anchor is the feed's newest event minus
    /// the overlap, so a reconnect re-delivers recent history only.
    #[test]
    fn subscription_since_tracks_the_feed_newest() {
        let keys = Keys::generate();
        let listing = signed(
            &keys,
            crate::KIND_APP,
            "A listing.",
            vec![("d", "com.example.Hello"), ("f", "linux-x86_64")],
        );
        let mut feed = Feed::new();
        feed.absorb(listing).unwrap();
        let newest = feed.newest_created_at().unwrap();

        let frame: serde_json::Value =
            serde_json::from_str(&subscription(&feed, true)).unwrap();
        assert_eq!(
            frame[2]["since"].as_u64(),
            Some(newest.saturating_sub(SINCE_OVERLAP))
        );
        // the same feed, pre-backfill, still carries no filter
        assert!(!subscription(&feed, false).contains("since"));
    }

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

    /// The graph crawl's absorb rule: newest kind 3 per requested
    /// author wins; other kinds and unrequested authors never land.
    #[test]
    fn absorb_newest_keeps_newest_per_author() {
        let keys = Keys::generate();
        let other = Keys::generate();
        let requested = [keys.get_public_key().unwrap().to_string()];
        let pk = requested[0].clone();

        let old = EventBuilder::new(Kind::ContactList, "")
            .custom_created_at(Timestamp::from_secs(100))
            .tag(Tag::custom("p", ["aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"]))
            .finalize(&keys)
            .unwrap();
        let new = EventBuilder::new(Kind::ContactList, "")
            .custom_created_at(Timestamp::from_secs(200))
            .tag(Tag::custom("p", ["bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"]))
            .tag(Tag::custom("p", ["cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"]))
            .finalize(&keys)
            .unwrap();
        // someone else's contact list rides along against the filter
        let stranger = EventBuilder::new(Kind::ContactList, "")
            .custom_created_at(Timestamp::from_secs(300))
            .finalize(&other)
            .unwrap();
        // same author, wrong kind
        let profile = EventBuilder::new(Kind::Metadata, "")
            .custom_created_at(Timestamp::from_secs(400))
            .finalize(&keys)
            .unwrap();

        let mut newest = Default::default();
        absorb_newest(&mut newest, &old, Kind::ContactList, &requested);
        absorb_newest(&mut newest, &new, Kind::ContactList, &requested);
        absorb_newest(&mut newest, &stranger, Kind::ContactList, &requested);
        absorb_newest(&mut newest, &profile, Kind::ContactList, &requested);

        assert_eq!(newest.len(), 1);
        let stored = &newest[&pk];
        assert_eq!(stored.created_at.as_secs(), 200);
        assert_eq!(contacts_from_event(stored).len(), 2);

        // an older event than the stored one cannot win
        let stale = EventBuilder::new(Kind::ContactList, "")
            .custom_created_at(Timestamp::from_secs(150))
            .finalize(&keys)
            .unwrap();
        absorb_newest(&mut newest, &stale, Kind::ContactList, &requested);
        assert_eq!(newest[&pk].created_at.as_secs(), 200);
        assert_eq!(requested.len(), 1);
    }

    /// A relay that accepts the connection and never answers must
    /// fail the send within the timeout, not hang the publish. Bound
    /// by a socket connect to a closed port, which errors at once;
    /// the 30s ceiling only binds against a black-hole address, so
    /// this test asserts the error path exists and is named.
    #[test]
    fn send_to_a_dead_relay_errors_instead_of_hanging() {
        use crate::events::Listing;
        use nostr::prelude::Keys;
        let keys = Keys::generate();
        let listing = signed(
            &keys,
            crate::KIND_APP,
            "A listing.",
            vec![("d", "com.example.Hello"), ("f", "linux-x86_64")],
        );
        // port 1 on the loopback: nothing listens, connect refuses
        let started = std::time::Instant::now();
        let result = send("ws://127.0.0.1:1", std::slice::from_ref(&listing));
        assert!(result.is_err(), "a dead relay must error");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "the failure must be prompt, not a timeout-length hang"
        );
    }
}
