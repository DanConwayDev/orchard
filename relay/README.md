# Running an Orchard relay

Orchard needs no special relay software: it runs on stock
[nostr-rs-relay](https://github.com/scsibug/nostr-rs-relay) with an
operator configuration. This directory is the kit: `config.toml` is the
operator config (written for and verified against nostr-rs-relay
0.10.0), and `allowlist-probe.py` verifies a deployment from the
outside, the way a client would meet it.

## Why kind-restricted

An Orchard client reads exactly six event kinds (the NIP-82 draft kinds
plus profiles). Everything else a general relay accepts is storage and
bandwidth an Orchard client can never use, and admission surface for
junk: anyone can sign garbage asset or review events that pass
signature verification, and a general relay keeps them forever.
Restricting admissions to the Orchard set closes that at the door.

The relay stays permissionless: the allowlist is about *kinds*, not
*keys*. Any key can publish listings, releases, assets, reviews, and
migrations to it; that is the trust model. Events are signed, the relay
is a mailbox, and which relay an event came from affects availability,
never integrity.

## The kind set

| Kind  | Meaning |
|-------|---------|
| 0     | profile (publisher and reviewer names) |
| 3063  | asset (the artifact descriptor: location, digest, size) |
| 30063 | release |
| 30065 | review |
| 30066 | migration (key succession for one app id) |
| 32267 | listing |

## Deploying

1. Run nostr-rs-relay 0.10.0: bare binary, systemd, or a container
   (upstream's README shows the podman form with the config and data
   directory bind-mounted).
2. Copy `config.toml` beside it and edit the `[info]` section: the
   real `wss://` URL, name, description, and the operator contact.
3. The relay reads its config once at startup, from `config.toml` in
   its working directory or a `--config <path>` flag. A malformed
   config exits loudly at startup, which is the safe direction.
   Restarting applies a changed config; there is no hot reload.
4. Terminate TLS in front: nostr-rs-relay speaks plain WebSocket, and
   public relays must be `wss://`. nginx, Caddy, or Cloudflare all
   work; upstream's `docs/reverse-proxy.md` has examples.
5. Mind the database location. `--db` (or the `[database]` section,
   which `--db` overrides) decides where the SQLite files live. The
   default is the relay's working directory; adopting this config onto
   a relay whose database is elsewhere without pinning the location
   makes the restarted relay come up empty.

### The one load-bearing line

`limit_scrapers = false` must stay false. It rejects subscriptions
without author filters, and Orchard clients subscribe by kind. A relay
operator who enables it (upstream's config template recommends it for
the outbox model) refuses every Orchard client REQ. Watch that line
across upstream config-template updates.

### Two behaviors worth knowing before relying on it

- **The allowlist gates admissions, not storage.** Events of other
  kinds already on the relay stay and stay queryable after the config
  change. For a fresh relay that is moot; for an existing general
  relay, old history remains.
- **Pruning exists but is unverified.** The 0.10.0 binary parses a
  `[retention]` section (`max_events`, `max_bytes`, `persist_days`,
  `whitelist_addresses`) that its config template does not document.
  Rehearse it on a dev relay before leaning on it in production.

## Verifying a deployment

From any machine with Python 3 (standard library only):

    python3 relay/allowlist-probe.py wss://your-relay.example.com

Three checks, expected output in order:

1. a validly signed kind 1 is rejected with
   `blocked: event kind is blocked by relay`;
2. a validly signed Orchard migration event is admitted (`OK true`),
   which also proves the probe's signatures are good, so check 1 is
   genuinely the allowlist;
3. a kind-based subscription returns events (on a used relay) or EOSE,
   which proves `limit_scrapers` is false.

The probe exits 0 only when all three pass. Check 2 leaves one inert
event on the relay (a migration for a throwaway app id); clients trim
migrations that reference nothing.

Also confirm the identity document:

    curl -s -H 'Accept: application/nostr+json' https://your-relay/ | jq .name,.description

## Abuse posture

Solved at admission: foreign-kind junk never lands. Not solved: junk in
the Orchard's own kinds, reviews (kind 30065) chief among them. Any key
can sign reviews, and a sybil can flood them; permissionless is the
design, ranking and disclosure are the client's defenses, and the
relay's first-line friction is `messages_per_sec`. If a specific relay
ever needs per-key policy, nostr-rs-relay has two escalation paths,
both documented in its config template: `[authorization]`
(`pubkey_whitelist`, which ends permissionlessness and is a last
resort) and the `[grpc]` `event_admission_server` (per-event decisions
delegated to an external service over the nauthz protocol), which is
the hook for rate limits or reputation without forking.

## Relaying for the rendezvous set

Nostr relays do not gossip to each other, so an Orchard relay only has
the events published to it. A relay operated for Orchard belongs in the
rendezvous set **in addition to** the third-party relays, not instead
of them: if it is down, new clients still bootstrap through the others.
Publishers' manifests and clients' defaults list all of them, and
discovery is the overlap.

## Operating notes

- **A sick nostr-rs-relay fails silent.** The known failure mode (seen
  once on the dev relay: a `quanta` panic on startup inside a
  container) is a relay that accepts connections, answers cheap
  queries, and hangs on writes. `orchard-publish`'s relay send has a
  30 second timeout, so publishers get an error instead of a hang, and
  the diagnosis is `podman logs orchard-relay | grep panicked` followed
  by a restart. A periodic health check that round-trips a small event
  through the public endpoint is the cheap watchtower.
- **Backup the SQLite files** (`nostr.db` plus WAL) like any other
  stateful service; the events are the discovery layer.
- **The dev relay** (`orchard-relay` podman container,
  `ws://127.0.0.1:8008`) runs this same config with dev values
  (identities localhost, no rate limits so test loops run free),
  applied with `podman cp` into the container layer plus a restart.
  Its database also lives in the container layer: both survive
  `podman restart`, neither survives `podman rm`.
