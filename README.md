# Orchard

A permissionless Flatpak store. Flathub's UX and artifact format, zapstore's
Nostr-based trust model: no curated review gate, publishers ship directly, and
the client verifies digests and shows permissions before install.

Status: early MVP. The Flathub channel browses and installs for real; the
Orchard channel is a local demo of the OCI + digest-verification flow, with
the Nostr read path still to come (see `DESIGN.md` for the milestones).

## Try the MVP (x86_64, Fedora atomic host)

The host has no toolchain; everything builds in a container via
`scripts/build.sh`.

    ./scripts/build.sh check
    ./scripts/build.sh build --release

Generate the demo channel artifact and catalog (host python, no deps):

    python3 scripts/make-demo-oci.py

Run the client (needs the host Wayland session, so not in the container):

    ./target/release/orchard

What to expect:

- The list fills from two channels: **Flathub** (parsed from your system
  AppStream, ~4.8k apps) and **Orchard** (one demo app served over HTTP by the
  in-process `orchard-indexd`).
- Browse by category chips (Games, Utilities, Audio & Video, ...), filter by
  channel, type to search. The list is virtualized; a full catalog scrolls
  like a screenful.
- Rows carry inline **Install** / **Open** actions; clicking a row opens the
  full app page: screenshots, provenance, permissions.
- Keyboard-first: type to search, arrow keys walk the filtered rows, Enter
  opens the page, Escape backs out one level.
- The Orchard entry's page shows the pinned manifest digest and a permissions
  view translated from the app's flatpak metadata (home access = high risk).
- **Verify & install** checks the registry manifest digest against the catalog
  pin, then runs `flatpak install --system -y oci:…/demo/oci-hello`. System
  installs may ask for your password (polkit), same as GNOME Software.
- After install: **Open** runs the app (it appends to `~/orchard-hello.txt`),
  **Remove** uninstalls.

## Trust model, in short

There is no review gate. The trust surface is the client: who published the
app (Nostr key, shown on the page), what the artifact claims to be, and
whether the bytes match that claim (digest verification before install).
Read the screenshots and permissions before installing; a publisher being
cryptographically consistent is not the same as a publisher being safe.

## Layout

- `crates/orchard-core` - domain model, AppStream parser, OCI verifier,
  permissions translator.
- `crates/orchard-flatpak` - the single seam to the flatpak CLI.
- `crates/orchard-indexd` - the local catalog service (embedded in the client;
  `cargo run -p orchard-indexd` runs it standalone).
- `crates/orchard` - the GPUI client.
- `demo/hello` - the demo app tree; `scripts/make-demo-oci.py` turns it into
  `demo/oci-hello/` (an OCI layout) plus `demo/catalog.json`.

`DESIGN.md` is the architecture record (locked decisions, the Nostr protocol
draft, milestones). `AGENTS.md` documents the container build rules and the
sharp edges learned building on an atomic host.

## License

MIT, see `LICENSE`.
