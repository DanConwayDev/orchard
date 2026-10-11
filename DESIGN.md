# Orchard design

**Status:** draft v0.2 · 2026-10-11

**Pitch:** Orchard is a permissionless Flatpak store. Developers publish apps directly to
their own OCI registries; Nostr carries discovery, identity, releases, and reviews; the
desktop client (Rust + GPUI) renders one unified catalog, including Flathub's, and makes
every app installable as a *native* Flatpak through a local index shim. **The store is a
client, not a gatekeeper.**

## Decisions (locked)

| Area | Decision |
|---|---|
| Artifacts | OCI images on developer-owned registries (ghcr, quay, self-hosted); `.flatpak` bundles as fallback |
| Metadata protocol | Nostr events (Zapstore-style); Orchard is a client/indexer, never the source of truth |
| Product surface | Desktop client only (no server-side store) |
| Flathub | Ingested client-side as a read-only "reviewed" channel |
| Binaries | Developer-owned OCI registries only (ghcr, quay, self-hosted); Orchard never stores or proxies bytes |
| Media | Publisher-hosted URLs (their site, forge, or a blob server they pick); no Orchard-run media service |
| Language / UI | Rust end-to-end; GPUI `0.2` from crates.io |
| Bootstrap | Client-first MVP; distribute Orchard itself as a `.flatpak` bundle + `orchard://` links |
| Build | Podman container on the atomic host (see "Build environment") |

## What we take from each project

**From Flathub:** AppStream-quality metadata, polished catalog UX, one-click native
Flatpak installs, the "verified" badge idea (→ nip05), the shared runtime ecosystem.

**From Zapstore:** publisher-owned release channels, Nostr identity and events,
verify-before-install (digests + signatures), community curation via web of trust,
lightning/zaps (later).

## Architecture

```
┌─────────────────────────────────────────────────────────┐
│ Orchard client (GPUI, Rust)                             │
│  catalog · search · detail · permissions · install UX   │
└──────┬──────────────────┬──────────────────┬────────────┘
       │ flatpak CLI      │ Nostr relays     │ OCI registry
       │ (orchard-flatpak │ (nostr-sdk,      │ API (OCI
       │  seam, via       │  M3)             │ dist-spec)
       │  flatpak-spawn   │                  │
       │  --host if       │                  │
       │  sandboxed)      │                  │
       ▼                  ▼                  ▼
┌────────────────┐ ┌────────────────┐ ┌────────────────────┐
│ orchard-indexd │ │ publisher      │ │ ghcr / quay / any  │
│ 127.0.0.1:7780 │ │ profiles,      │ │ registry           │
│ Flatpak OCI    │ │ listings,      │ │ (developer-owned   │
│ registry index │ │ releases,      │ │  namespaces)       │
└───────▲────────┘ │ reviews        │ └────────────────────┘
        │          └────────────────┘              ▲
        │ registered as flatpak remote "orchard"   │ publish
        │ oci+http://127.0.0.1:7780                │ (orchard-publish, M5)
        ▼                                          │
   flatpak / GNOME Software / Discover ───────────┘
```

**Key idea: the client is the remote.** `orchard-indexd` runs on `127.0.0.1` and speaks
Flatpak's OCI registry-index protocol (`oci+http://127.0.0.1:7780`), backed by whatever the
client learned from Nostr. Consequences:

- every Orchard app is a normal Flatpak-remote app, so GNOME Software / Discover keep working
- `flatpak update` works; delta downloads come from the underlying registry
- distribution infrastructure = zero central servers

## Distribution

- Apps are OCI images on any registry the developer controls. Flatpak metadata (ref,
  permissions, AppStream) rides in OCI **config labels** (docker-style `config.Labels`,
  capital L, verified against a live Fedora registry image); the client can read the
  manifest and config cheaply to render the permissions viewer before pulling layers.
- **The flatpak OCI image format, validated end-to-end (M0):**
  - The layer tar is the app tree rooted at `files/`, `metadata`, `export/` (what
    `flatpak build-export` commits). Flatpak imports it with
    `ostree_repo_import_archive_to_mtree` and **reconstructs the ostree commit at
    install time** from config labels: `org.flatpak.ref` (required),
    `org.flatpak.metadata` (required), optional `org.flatpak.timestamp/subject/body/
    parent-commit/installed-size/download-size`, and `org.flatpak.commit-metadata.<key>`
    labels carrying base64 GVariant "v" values. No ostree repo or commit checksum is
    computed on the publish side.
  - The commit's `xa.metadata` must match the `org.flatpak.metadata` ini byte-for-byte
    (`validate_commit_metadata` in `flatpak-dir.c`), so the publisher adds the label
    `org.flatpak.commit-metadata.xa.metadata` = base64(GVariant v-wrapping the ini).
    GLib serialises a string variant as `value + b"\x00\x00" + b"s"` (verified with
    glib 2.43 ctypes round-trip; matches Fedora's live images).
  - Manifest digest is the pinned identity: `sha256(manifest bytes)`, the same value
    flatpak records as the commit's `Alt-id`.
- **MVP install transport:** `flatpak install --system -y oci:<layout-dir>` (or
  `oci-archive:`). Direct `docker://` installs also work (1.17+) but flatpak hard-wires
  https for them (`flatpak-docker-reference.c`), so a local registry needs TLS; local
  registry + CA install is the follow-up, and ghcr/quay are https anyway. The native
  `oci+http://` remote flow (client-is-the-remote, GNOME Software integration) needs
  the shim to serve an **ostree summary** whose ref checksums are manifest digests with
  `xa.oci-registry-uri` in the summary metadata; that is a later milestone, not MVP.
- **Runtime convention (important):** apps default to runtimes hosted on the flathub
  remote (standard flatpakref behavior) so runtimes are shared, not duplicated per app.
  Self-contained images are an explicit opt-in in the publish manifest.

## Nostr protocol — NIP-82 draft / Zapstore-compatible

All payload events follow the NIP-82 draft ("software applications"): kind **32267** app
listing, **30063** release, **3063** asset, plus NIP-01 kind **0** publisher profiles.
This replaces the earlier 30063/30064 draft — 30063 already means *release* to Zapstore;
matching the draft NIP keeps Zapstore publishers readable on Orchard day one.

| Event | Kind | `d` tag | Tags | Content |
|---|---|---|---|---|
| Publisher profile | 0 | (none) | standard NIP-01 | name, nip05, about |
| App listing | 32267 | `<app-id>` | `name`, `summary`, `icon`, `image` (screenshots), `t` (categories), `f` (platform), `license`, `repository`, `h` | full description |
| Release | 30063 | `<app-id>@<version>` | `i` (app id), `version`, `c` (channel: stable/beta), `e` (asset refs) | changelog |
| Asset | 3063 | — | `i`, `x` (sha256), `version`, `url`, `m`, `size`, `f`, **`oci`**, **`runtime`** | — |
| Review | 30065 | `<app-id>` | rating, text (Orchard-draft until NIP-82 grows one; M6) | review text |

Orchard's additive tags (invisible to Zapstore readers):

- `f` values are flatpak arches: `linux-x86_64`, `linux-aarch64` (Zapstore uses
  `android-*`).
- Asset `oci`: the artifact ref `registry/repo@sha256:<digest>`. The `x` tag keeps its
  NIP-82 role — sha256 verified before install — and for Orchard it is the OCI *manifest*
  digest, the same pin `orchard-core::oci` checks (M0 path).
- Asset `runtime`: the flatpak runtime requirement
  (`org.freedesktop.Platform/x86_64/25.08`), needed at install time.

Media (icons, screenshots) are plain URLs the publisher hosts themselves — their website,
a forge, or a blob server of their choosing; Orchard runs no media service and proxies no
bytes. The client already fetches remote images from arbitrary URLs into a disk cache.
Signing (`orchard-publish`) follows Zapstore's `SIGN_WITH` shape: nsec or hex key, NIP-46
bunker for CI, NIP-07 browser extension.

**Sandbox permissions are never published in events.** The publisher declares
`finish-args` in their build manifest; flatpak bakes them into the artifact as commit
metadata (`xa.metadata`, carried by OCI config labels — required byte-exact since M0).
The client reads the *real* sandbox from the artifact at page open, the same way it reads
Flathub's via `flatpak remote-info`. Because the release event pins the digest, the
permissions shown on the page are byte-identical to what install verifies — no
declared-vs-actual gap to trust anyone about. A risk grader (low/medium/high, already
implemented in `orchard-core::permissions`) can warn the publisher how their app will
read, as a nicety, not a gate.

Rules:

- relays are dumb mirrors; the client reads several and merges. Publishing to Zapstore's
  relay is not a plan: their relay whitelists writers (allow list, `zapstore.yaml` repo
  verification, Vertex reputation), and their catalog is Android APKs anyway. Orchard's
  default set is an Orchard-run event relay (events only — a few KB per app; binaries
  live on publisher registries and never touch relays) plus generic permissionless
  relays; Zapstore's relay stays an optional read source should it serve open reads.
- curation is published as event lists that users opt into (web of trust, no server ranking)
- the relay list is itself a published event, plus a hardcoded fallback in the client

## Trust without review

1. **Verify before install:** fetch the image manifest by tag; the digest must match the
   release event, otherwise refuse (Zapstore behavior).
2. **Key continuity:** pin the first-seen publisher pubkey per app id; a change is a loud
   warning, not a silent accept.
3. **Provenance badge (Orchard's own):** GitHub Actions OIDC → cosign keyless signature →
   "built by CI from `owner/repo`": permissionless provenance.
4. **Honest permissions:** read from the artifact itself (see above); human-readable
   grants with risk levels (`orchard-core::permissions`). The sandbox is the mitigation
   for permissionlessness; make "permissionless store, explicit permissions" the brand.
5. **Moderation reality:** relays and clients can filter; artifacts persist elsewhere.
   Nothing is ranked for sale.

## Flathub ingestion

Read AppStream locally from the user's existing flathub remote
(`~/.local/share/flatpak/appstream/flathub/…`): zero mirroring, zero network for the
catalog. Rendered as the "reviewed" channel; Orchard-native apps as the
"publisher-verified" channel. One unified catalog.

## Crates

| Crate | Role |
|---|---|
| `orchard-core` | domain types, event model, digest + permission logic |
| `orchard-flatpak` | the *only* place the flatpak CLI is invoked (auto `flatpak-spawn --host` when sandboxed) |
| `orchard-indexd` | local shim: Flatpak OCI registry-index on `127.0.0.1` (+ standalone bin for M0); hosts the Nostr read path (relay threads, verified feed, event cache) and serves the merged catalog |
| `orchard-nostr` | NIP-82 events both ways: the verified read feed (`Feed`), the sync relay client (one thread per relay, reconnect with backoff), shared event builders for the publish side |
| `orchard` | GPUI app |
| `orchard-publish` (M5) | manifest → build → OCI push → sign → Nostr event |

## Build environment

The host is an atomic Fedora image: no C compiler, no glibc dev files, and a host cargo
would poison shared target artifacts anyway. All builds run in podman, the same road kuma
and kumaui use:

```
./scripts/build.sh check                 # whole-workspace typecheck
./scripts/build.sh test --workspace
./scripts/build.sh build --release
```

The script builds `localhost/orchard-dev-rust` on first use from
`containers/orchard-dev-rust.containerfile` (based on kuma's `kuma-dev-gcc`, which tracks
the release's Fedora; the gpui native stack lives here), keeps rustup and the cargo
registry in named volumes for incremental builds, and pins the toolchain via
`rust-toolchain.toml`. Run the built binary on the host, not in the container: the GPUI
shell needs the host's Wayland session.

## Milestones

**M0: OCI spike (proves or kills the architecture). DONE 2026-10-10.**

- [x] Nail the exact image format against flatpak 1.18.4 sources and a live Fedora
      registry image (labels in `config.Labels`; layer = app tree; commit reconstructed
      at install from labels; `xa.metadata` commit-metadata required and byte-exact).
- [x] `scripts/make-demo-oci.py` hand-rolls a valid OCI layout for the demo app
      (deterministic: same manifest digest on every run).
- [x] `flatpak install --system -y --no-deps oci:/…/demo/oci-hello` inside the dev
      container: **Installation complete**, `flatpak list` shows
      `app/com.orchard.Hello/x86_64/stable`, commit reconstructed, alt-id = manifest
      digest. (No local registry or TLS needed for this path.)
- [x] Client-side digest verification in `orchard-core::oci` (manifest fetch + sha256
      + pin check + config-label read), unit-tested, plus an index endpoint on the shim.

*Exit criteria met: a hello-world app installs from an artifact Orchard does not host
(localhost registry tested indirectly through the format; direct oci: transport tested
for real). Registry-with-TLS and the native ostree-summary remote are follow-ups.*

**M1: client shell (GPUI).** Window, Flathub catalog from local AppStream, search, detail
page, install with progress via the `orchard-flatpak` seam. (Built: `crates/orchard`,
AppStream parser validated against the real flathub file: 4788 apps in ~190 ms.)

**M3: Nostr read path — vertical slice.** `orchard-nostr` crate: relay connect
(configurable list + fallback), fetch kind 0 / 32267 / 30063 / 3063, verify signatures,
merge into the publisher channel. Closed by a hand-rolled test publisher (a script
signing real events for the demo app) so the whole loop — event → catalog row → digest
verify → install — is proven with one app before any tooling investment.

**M4: permissions viewer + `orchard://` scheme handler** (the signature feature).

**M5: publish CLI (built).** `orchard-publish` = manifest → build → OCI push → sign →
Nostr event. The manifest (TOML) carries the listing and release fields; the app tree's
`metadata` ini is the sandbox source of truth and rides the image config labels
byte-exact; the layer tar and gzip are deterministic (mtime 0, sorted entries), so the
same tree builds to the same digest. Conventions proven live against a TLS registry:
- the asset's `oci` tag names the repository (`docker://host/repo`), the `x` tag pins
  `sha256:<manifest digest>`; the client verifies by fetching the manifest **by digest**
  (immutable) and flatpak installs the **digest ref** (`docker://host/repo@sha256:…`),
  so what was verified is exactly what gets installed, no tag races;
- a registry part may carry an explicit scheme (`docker://https://localhost:5000/hello`);
  otherwise localhost registries use http and everything else https (docker's
  insecure-registry convention); `install_arg` always prints the bare host form;
- the push is monolithic blob upload + manifest PUT, then a read-back by digest: the
  signed event only goes out if the registry serves the bytes we built.

Cosign is dropped from M5: the Nostr signature already binds publisher identity to the
pinned digest, and a second signature chain would add a trust source without adding
trust.

**M6, reviews slice (built).** Kind 30065 joins the draft: `d` = app id, content = the
review text, `rating` = optional 0.0 to 5.0 one-decimal tag. Replaceable per (pubkey,
app id), so editing is republishing and one identity holds one review per app; a
malformed rating is a hard error like any other protocol violation. indexd serves the
merged document at `/v1/reviews` and the raw kind 0 metadata per pubkey at
`/v1/profiles`; the client fetches on page open and while a page is open on the
refresh tick. Posting requires sign-in through NIP-46 Nostr Connect: the client shows
a modal with the `nostrconnect://` URI in a clickable box (an ephemeral client key,
the connect relay, and an anti-spoofing secret inside); clicking copies the URI and
opens it with the desktop's scheme handler, where a signer app takes the pairing.
The connect channel rides kind 24133 over a public NIP-46 relay
(`wss://relay.nip46.com`, override with `ORCHARD_CONNECT_RELAY`) so both ends can
reach it regardless of where the catalog mirrors live. The signer's connect response
must echo the secret, and from then on every review and profile publish is a
NIP-44-encrypted `sign_event` request the user approves on their device. The URI
states the whole request surface: name and repo url, and the permissions
`sign_event:30065,sign_event:0` (reviews and profile metadata, nothing else), so a
capable signer shows one consent screen and decides itself whether to re-confirm
inside the scope; signers that ignore the param just ask per signature. Pasting a
secret key into the client was an earlier draft and is gone: the client never sees
the identity's key, the remote signer's pubkey is the review identity. Publishing a
name is proper profile management: the client merges against the author's latest
kind 0 (`/v1/profiles`) and republishes the whole object through the signer, so
fields the author already published survive; when indexd cannot answer, it publishes
nothing rather than guess and says so in the status bar. The pairing persists at
`$XDG_DATA_HOME/orchard/identity.json` (mode 0600: client key, signer pubkey, relay,
optional name; `$HOME/.local/share` fallback) and restores the session on launch;
sign out ends the session but keeps the pairing on the device. The reviews section
is collapsed by default; its header (count and one-decimal average, aligned with the
other section headers) toggles it, and rows sit flush with the other page fields.
Compose is a mode (r, or click "Write a review"): keystrokes go to the draft instead
of the search box, digits 1 to 5 rate, Enter posts, Escape leaves compose before it
closes the page. Sybil resistance and the web of trust stay open.

**M6, review trust slice (built).** One identity one review per app is the floor
(replaceable semantics); on top of it, publisher self-reviews are flagged on their
rows and excluded from the aggregate average and count, disclosed in a details line
("1 publisher review not counted"). indexd tracks the earliest event it has seen per
author key (absorb-time bookkeeping, so the disk cache replays it across restarts)
and serves it in the reviews document as `first_seen`; reviews from keys first seen
under a week ago carry a "new key" row marker and a "n of m from new keys" line, and
are never excluded: a weak heuristic does not drop votes. The aggregate counts
everything else once per identity. Next: the personal web of trust, Zapstore's own
answer (reviews constrained to the reader's graph): distance 1 from the signed-in
user's own kind 3, fetched read-only from default social relays, follow markers on
review rows plus a circle-only aggregate; distance 2 later via indexd crawling
reviewer kind 3s.

**M6, header slice (built).** The top bar is the mark (a single-color orchard tree,
the wordmark dropped), the search box centered across the full window width (an
absolute layer spanning the bar), and two controls on the right: the channel
dropdown (All / Flathub / Orchard / Installed replace the chip row; it stays
visible on app pages) and the account control ("Sign in" when signed out; a person
icon when signed in, the profile picture once avatars are fetched; the name still
shows in the session menu). Dropdowns are window-level layers painted after
everything: a click-away
overlay with the panel as a later sibling, anchored at the click point. Sign-in is a
window-level modal (dimmed backdrop, centered panel): the panel swallows the mouse
down (`stop_propagation`) so clicks on it never reach the backdrop's close, a lesson
the dropdowns never exposed because closing was their click behavior anyway.
Sign-in from the header does not open the review composer; from the reviews flow it
drops into the composer after the pairing completes. Tooltips hide during window
prepaint, and gpui registers its repaint-on-hover-change listener only for elements
with a hover style: a tooltip target without any hover style never learns the pointer
left and the tooltip sticks. Every tooltip target carries a no-op hover style (the
review rows' sprout and circle glyphs).

**M6, circle slice (built).** The personal web of trust, Zapstore's own answer
(reviews constrained to the reader's graph), starts at distance 1: at sign-in and at
launch for a restored session, the client fetches the signed-in identity's own kind 3
read-only and anonymously from default social relays (`wss://relay.primal.net`,
`wss://nos.lol`; `ORCHARD_SOCIAL_RELAYS` overrides): one REQ whose only input
is the key, deadline-guarded, union across relays. Reviewers in that set carry a
person glyph with a hover tooltip, and the details line gains "n in your circle,
avg x.y"; the global aggregate is unchanged. A union that comes back empty keeps the
old set (a relay outage must not erase a known circle); kind 3 edits mid session land
on the next launch. Distance 2 comes later via indexd crawling reviewer kind 3s.

**M6: reviews + web of trust; self-distribution.** CI builds Orchard's own `.flatpak`
bundle; dogfood the update path by updating Orchard through Orchard.

## Risks / open questions

- GPUI on Linux is its youngest backend; API churns. Pin `0.2.x`; watch signatures.
- Sandboxed-client networking: a port bound inside the flatpak sandbox should be reachable
  from the host (network namespace shared by default), verified in M2.
- Review spam / sybil resistance in the web of trust.
- aarch64 day-one or later?

**Resolved 2026-10-11:** event kinds reconciled with Zapstore's `zsp` reference
implementation (NIP-82 draft: 32267/30063/3063 — see "Nostr protocol"); license settled
to MIT-only; OCI index endpoints and annotation keys confirmed in M0 (ground truth:
`common/flatpak-oci-registry.c` in the flatpak repo; Harbor issue 22997 and the Pulp
docs for prior art).

## References

- Zapstore: https://github.com/zapstore/zapstore
- Flathub website monorepo: https://github.com/flathub-infra/website
- Flathub docs: https://github.com/flathub-infra/documentation
- `flatpak-remote(5)`, `oci+https://` extension: https://www.mankier.com/5/flatpak-remote
- Harbor: request for native Flatpak index support: https://github.com/goharbor/harbor/issues/22997
- Pulp: hosting Flatpak OCI images (`/index/dynamic`): https://pulpproject.org/pulp_container/docs/user/guides/manage-flatpak/
- Flatpak 1.17: direct OCI install / sideloading: https://ostechnix.com/flatpak-1-17-0-oci-direct-install-sideloading/
- flatpak-container-tools (flatpak → OCI): https://gitlab.com/flatpak-container/flatpak-container-tools
- gpui: https://crates.io/crates/gpui · https://gpui.rs
