//! NIP-46 Nostr Connect, client side: the user's secret key never
//! touches the store. A pairing URI (`nostrconnect://…`) is shown as a
//! QR code, the user scans it with a signer app, and from then on this
//! process asks the signer to sign events (`sign_event`) over the same
//! relay. Everything here is blocking tungstenite, matching
//! [`crate::relay`]: short-lived sessions, no runtime.
//!
//! The connect channel rides kind 24133 with a `p` tag addressing the
//! client's ephemeral key. Payloads are NIP-44 encrypted to the peer.
//! The signer must echo the URI's anti-spoofing secret in its connect
//! response; an answer that does not repeat it is ignored.

use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};
use nostr::prelude::*;
use nostr::nips::nip44;
use nostr::nips::nip46::{
    NostrConnectMessage, NostrConnectMethod, NostrConnectRequest, NostrConnectResponse,
    NostrConnectUri, ResponseResult,
};
use tungstenite::{Message, WebSocket, stream::MaybeTlsStream};

/// How long a `sign_event` request may take. Human-in-the-loop: the
/// user has to pick up their device and approve.
const SIGN_TIMEOUT: Duration = Duration::from_secs(30);

/// The `nostrconnect://` pairing URI: the ephemeral client key, the
/// relay both ends must reach, the app name, and the anti-spoofing
/// secret the signer has to echo back.
pub fn connect_uri(
    client_pubkey: &PublicKey,
    relay: &str,
    app_name: &str,
    secret: &str,
) -> Result<String> {
    let uri = NostrConnectUri::client_with_secret(
        *client_pubkey,
        [RelayUrl::parse(relay)?],
        app_name,
        secret,
    );
    Ok(uri.to_string())
}

/// Listen (blocking, up to `window`) for the signer's connect response.
/// Returns the signer's pubkey and the echoed secret, or `Ok(None)`
/// when the window closed without an answer (the caller polls again).
pub fn poll_connect(
    relay: &str,
    keys: &Keys,
    expected_secret: &str,
    window: Duration,
) -> Result<Option<(String, String)>> {
    let client_pubkey = keys.get_public_key()?;
    let (mut socket, _response) = tungstenite::connect(relay)?;
    set_read_timeout(&mut socket, Duration::from_millis(250));
    let sub = serde_json::json!([
        "REQ",
        "orchard-connect",
        { "kinds": [Kind::NostrConnect.as_u16()], "#p": [client_pubkey.to_string()] },
    ])
    .to_string();
    socket.send(Message::text(sub))?;
    let deadline = Instant::now() + window;
    while Instant::now() < deadline {
        let message = match socket.read() {
            Ok(message) => message,
            Err(tungstenite::Error::Io(err))
                if err.kind() == std::io::ErrorKind::WouldBlock
                    || err.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue;
            }
            Err(err) => return Err(err.into()),
        };
        let Message::Text(text) = message else {
            continue;
        };
        if let Some(pair) = decrypt_message(&text, keys, |message, author| match message {
            // the connect response's result is the echoed secret: a
            // pairing answer without it is spoofing, not a mismatch
            NostrConnectMessage::Response { result, error, .. } => {
                if error.is_some() {
                    return Ok(None);
                }
                let Some(secret) = result.as_deref() else {
                    return Ok(None);
                };
                Ok((secret == expected_secret)
                    .then(|| (author.to_string(), secret.to_string())))
            }
            _ => Ok(None),
        })? {
            return Ok(Some(pair));
        }
    }
    Ok(None)
}

/// Ask the signer to sign one event: subscribe for responses, send the
/// encrypted request, and block until the answer with the matching id.
pub fn sign_event(
    relay: &str,
    keys: &Keys,
    signer_pubkey: &PublicKey,
    unsigned_json: &str,
) -> Result<Event> {
    let client_pubkey = keys.get_public_key()?;
    let unsigned = UnsignedEvent::from_json(unsigned_json)?;
    let message = NostrConnectMessage::request(&NostrConnectRequest::SignEvent(unsigned));
    let request_id = message.id().to_string();

    let (mut socket, _response) = tungstenite::connect(relay)?;
    set_read_timeout(&mut socket, Duration::from_millis(250));
    let sub = serde_json::json!([
        "REQ",
        "orchard-connect",
        { "kinds": [Kind::NostrConnect], "#p": [client_pubkey.to_string()] },
    ])
    .to_string();
    socket.send(Message::text(sub))?;

    let content = nip44::encrypt(
        keys.secret_key(),
        signer_pubkey,
        serde_json::to_string(&message)?,
        nip44::Version::default(),
    )?;
    let request = EventBuilder::new(Kind::NostrConnect, content)
        .tag(Tag::public_key(*signer_pubkey))
        .finalize(keys)?;
    socket.send(Message::text(
        serde_json::json!(["EVENT", request]).to_string(),
    ))?;

    let deadline = Instant::now() + SIGN_TIMEOUT;
    while Instant::now() < deadline {
        let frame = match socket.read() {
            Ok(message) => message,
            Err(tungstenite::Error::Io(err))
                if err.kind() == std::io::ErrorKind::WouldBlock
                    || err.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue;
            }
            Err(err) => return Err(err.into()),
        };
        let Message::Text(text) = frame else {
            continue;
        };
        if let Some(event) = decrypt_message(&text, keys, |message, _author| match message {
            // only the response to THIS request id counts
            NostrConnectMessage::Response { id, result, error } if id == &request_id => {
                if let Some(reason) = error {
                    return Err(anyhow!("signer refused: {reason}"));
                }
                let response = NostrConnectResponse::parse(
                    NostrConnectMethod::SignEvent,
                    result.clone(),
                    None,
                )
                .map_err(|err| anyhow!("bad sign_event response: {err}"))?;
                match response.result {
                    Some(ResponseResult::SignEvent(event)) => Ok(Some(*event)),
                    _ => Ok(None),
                }
            }
            _ => Ok(None),
        })? {
            return Ok(event);
        }
    }
    bail!("signer did not answer within {}s", SIGN_TIMEOUT.as_secs());
}

/// Read one relay frame, decrypt its NIP-44 payload to us, parse the
/// NIP-46 message, and let `keep` decide what counts as an answer.
/// The callback gets the message and the sender's pubkey; it returns
/// the value to surface (the echoed secret, the signed event), an
/// error to stop with, or nothing to keep reading.
fn decrypt_message<T>(
    frame: &str,
    keys: &Keys,
    keep: impl Fn(&NostrConnectMessage, &PublicKey) -> Result<Option<T>>,
) -> Result<Option<T>> {
    let json: serde_json::Value = serde_json::from_str(frame)?;
    if json.get(0).and_then(|v| v.as_str()) != Some("EVENT") {
        return Ok(None);
    }
    let Some(raw) = json.get(2) else {
        return Ok(None);
    };
    let event: Event = match serde_json::from_value(raw.clone()) {
        Ok(event) => event,
        Err(_) => return Ok(None),
    };
    // only messages addressed to us from one signer: wrong key or
    // broken crypto is noise, not an error
    let plain = match nip44::decrypt(keys.secret_key(), &event.pubkey, event.content.as_str()) {
        Ok(plain) => plain,
        Err(_) => return Ok(None),
    };
    let message: NostrConnectMessage = match serde_json::from_str(&plain) {
        Ok(message) => message,
        Err(_) => return Ok(None),
    };
    keep(&message, &event.pubkey)
}

/// The websocket needs short read timeouts so a poll can give up in
/// time; the underlying stream differs between ws:// and wss://.
fn set_read_timeout(socket: &mut WebSocket<MaybeTlsStream<std::net::TcpStream>>, timeout: Duration) {
    let _ = match socket.get_ref() {
        MaybeTlsStream::Plain(stream) => stream.set_read_timeout(Some(timeout)),
        MaybeTlsStream::Rustls(stream) => stream.get_ref().set_read_timeout(Some(timeout)),
        _ => Ok(()),
    };
}
