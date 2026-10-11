# Orchard

A permissionless Flatpak store. Flathub's UX and artifact format, a
Nostr-based trust model in place of a review gate: publishers ship
directly, clients verify signatures and digests, and the store is a
client, not a gatekeeper.

## How it works

- **Publishers** push their app as an OCI image to any registry they
  control and sign small Nostr events (listing, release, asset, profile)
  that describe it. `PUBLISHING.md` is the guide, including a ready CI
  workflow.
- **The client** merges those events into a catalog beside Flathub's
  (read from your local AppStream), shows the sandbox permissions the
  artifact really carries, pins the release to its manifest digest, and
  installs through flatpak. Installs are refused when bytes do not match
  the pin.
- **Reviews are Nostr events too**, signed through your own signer over
  NIP-46 Nostr Connect: the client never holds your identity key.
  Publishers cannot review themselves into a rating, and a publisher key
  change is disclosed, with a signed handover shown as an authorized
  migration.
- **No central server.** Discovery is a rendezvous set of public relays;
  binaries live on publisher registries; the client embeds a local index
  shim so every Orchard app is a normal Flatpak-remote app, visible to
  GNOME Software and Discover.
- **Settings apply live.** Channel visibility, relay sets, update
  cadence, and privacy toggles live on a settings page (the account
  menu); edits persist to `~/.config/orchard/settings.json` and take
  effect immediately, no restart.

## Status

Early, pre-release MVP. The full loop works end to end against local
infrastructure: publish, discover, verify, install, update, review.
Multi-arch (x86_64 and aarch64) is built and the first public release
lands with the `v0.1.0` tag. `DESIGN.md` is the architecture record with
the milestone history.

## Try it (Fedora atomic host, x86_64)

The host has no toolchain; everything builds in a container via
`scripts/build.sh`.

    ./scripts/build.sh check
    ./scripts/build.sh build --release
    ./target/release/orchard

Out of the box the client browses and installs the real Flathub catalog.
The Orchard channel lights up when a relay and a publisher exist; see
`PUBLISHING.md` to become one.

## Layout

- `crates/orchard-core`: domain model, AppStream parser, OCI verifier,
  permissions translator.
- `crates/orchard-flatpak`: the single seam to the flatpak CLI.
- `crates/orchard-indexd`: the local catalog service (embedded in the
  client), relay sessions, event cache.
- `crates/orchard-nostr`: event model, builders, and the verified feed.
- `crates/orchard`: the GPUI client.
- `crates/orchard-publish`: the publisher CLI (`PUBLISHING.md`).
- `demo/`: a demo app tree and manifest for the local fixture.
- `self/`: Orchard publishing itself, and its CI workflow.

## Docs

- `PUBLISHING.md`: the publisher guide. The manifest, the CLI, multi-arch,
  the CI workflow, and key rotation.
- `DESIGN.md`: the architecture record. Locked decisions, the Nostr event
  draft, milestones.
- `AGENTS.md`: container build rules and the sharp edges of building on
  an atomic host.

## License

MIT, see `LICENSE`.
