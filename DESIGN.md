# Orchard — design

**Status:** draft v0.1 · 2026-10-10

**Pitch:** Orchard is a permissionless Flatpak store. Developers publish apps directly to
their own OCI registries; Nostr carries discovery, identity, releases, and reviews; the
desktop client (Rust + GPUI) renders one unified catalog — including Flathub's — and makes
every app installable as a *native* Flatpak through a local index shim. **The store is a
client, not a gatekeeper.**

## Decisions (locked)

| Area | Decision |
|---|---|
| Artifacts | OCI images on developer-owned registries (ghcr, quay, self-hosted); `.flatpak` bundles as fallback |
| Metadata protocol | Nostr events (Zapstore-style); Orchard is a client/indexer, never the source of truth |
| Product surface | Desktop client only (no server-side store) |
| Flathub | Ingested client-side as a read-only "reviewed" channel |
| Language / UI | Rust end-to-end; GPUI `0.2` from crates.io |
| Bootstrap | Client-first MVP; distribute Orchard itself as a `.flatpak` bundle + `orchard://` links |

## What we take from each project

**From Flathub** — AppStream-quality metadata, polished catalog UX, one-click native
Flatpak installs, the "verified" badge idea (→ nip05), the shared runtime ecosystem.

**From Zapstore** — publisher-owned release channels, Nostr identity and events,
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
   flatpak / GNOME Software / Discover ────────────┘
```

**Key idea — the client is the remote.** `orchard-indexd` runs on `127.0.0.1` and speaks
Flatpak's OCI registry-index protocol (`oci+http://127.0.0.1:7780`), backed by whatever the
client learned from Nostr. Consequences:

- every Orchard app is a normal Flatpak-remote app → GNOME Software / Discover keep working
- `flatpak update` works; delta downloads come from the underlying registry
- distribution infrastructure = zero central servers

## Distribution

- Apps are OCI images on any registry the developer controls. Flatpak metadata (ref,
  permissions, AppStream) rides in OCI **annotations** — the client can read the manifest
  and config cheaply to render the permissions viewer before pulling layers.
- **Runtime convention (important):** apps default to runtimes hosted on the flathub
  remote (standard flatpakref behavior) so runtimes are shared, not duplicated per app.
  Self-contained images are an explicit opt-in in the publish manifest.
- Fallback transports: single-file `.flatpak` bundles; and Flatpak ≥ 1.17 can direct-install
  OCI images (`flatpak install oci://…`) — though updates then lack remote integration, so
  the shim remains the primary path.

## Nostr protocol (draft — reconcile with zapstore's `spec/` folder)

All payload events are addressable-replaceable (NIP-01 kinds 30000–39999), content is a
JSON document, tags stay minimal (`d`, `alt`).

| Event | Kind | `d` tag | Carries |
|---|---|---|---|
| Publisher profile | 0 | — | name, nip05, about (standard NIP-01) |
| App listing | 30063 | `<app-id>` | name, summary, description, icon/screenshots (`imeta`), categories |
| Release | 30064 | `<app-id>@<version>` | artifact ref (registry/repo/tag/**digest**), changelog, finish-args snapshot, runtime requirement |
| Review | 30065 | `<app-id>` | rating, text, zap-receipt refs |

Rules:

- relays are dumb mirrors; the client reads several and merges
- curation is published as event lists that users opt into (web of trust, no server ranking)
- the relay list is itself a published event, plus a hardcoded fallback in the client

## Trust without review

1. **Verify before install:** fetch the image manifest by tag; the digest must match the
   release event, otherwise refuse (Zapstore behavior).
2. **Key continuity:** pin the first-seen publisher pubkey per app id; a change is a loud
   warning, not a silent accept.
3. **Provenance badge (Orchard's own):** GitHub Actions OIDC → cosign keyless signature →
   "built by CI from `owner/repo`" — permissionless provenance.
4. **Honest permissions:** `finish-args` → human-readable grants with risk levels
   (`orchard-core::permissions`). The sandbox is the mitigation for permissionlessness —
   make "permissionless store, explicit permissions" the brand.
5. **Moderation reality:** relays and clients can filter; artifacts persist elsewhere.
   Nothing is ranked for sale.

## Flathub ingestion

Read AppStream locally from the user's existing flathub remote
(`~/.local/share/flatpak/appstream/flathub/…`) — zero mirroring, zero network for the
catalog. Rendered as the "reviewed" channel; Orchard-native apps as the
"publisher-verified" channel. One unified catalog.

## Crates

| Crate | Role |
|---|---|
| `orchard-core` | domain types, event model, digest + permission logic |
| `orchard-flatpak` | the *only* place the flatpak CLI is invoked (auto `flatpak-spawn --host` when sandboxed) |
| `orchard-indexd` | local shim: Flatpak OCI registry-index on `127.0.0.1` (+ standalone bin for M0) |
| `orchard` | GPUI app |
| `orchard-publish` (M5) | manifest → build → OCI push → sign → Nostr event |

## Milestones

**M0 — OCI spike (proves or kills the architecture). Do first.**

- [ ] Stand up a local OCI registry (`registry:2` on `:5000`)
- [ ] Convert a trivial Flatpak app to an OCI image (`flatpak-container-tools`,
      `flatpak-oci-mirror`-style tooling, or hand-rolled)
- [ ] `cargo run -p orchard-indexd` serves `/index/<arch>` from a static list
- [ ] `flatpak remote-add orchard oci+http://127.0.0.1:7780` →
      `flatpak install orchard com.example.Hello` → run it
- [ ] Also exercise the fallback: direct `flatpak install oci://…` (≥ 1.17)
- [ ] Confirm exact index endpoints and annotation keys (e.g. `org.flatpak.ref`) against
      flatpak source `common/flatpak-oci-registry.c`

*Exit criteria: a hello-world app installs and runs from a registry Orchard does not host.*

**M1 — client shell (GPUI):** window, Flathub catalog from local AppStream, search, detail
page, install with progress via the `orchard-flatpak` seam.

**M2 — shim to real catalog:** the client manages the `orchard` remote; Orchard channel
appears alongside Flathub in one catalog.

**M3 — Nostr read path:** relay fetch, publisher profiles, listings, releases; digest
verification before install.

**M4 — permissions viewer + `orchard://` scheme handler** (the signature feature).

**M5 — publish CLI:** `orchard-publish` = manifest → build → OCI push → cosign → signed
Nostr event. Then Orchard feeds itself.

**M6 — reviews + web of trust; self-distribution:** CI builds Orchard's own `.flatpak`
bundle; dogfood the update path by updating Orchard through Orchard.

## Risks / open questions

- Exact OCI index endpoints and annotation keys — confirm in M0. Ground truth:
  `common/flatpak-oci-registry.c` in the flatpak repo; see Harbor issue 22997 and the Pulp
  docs (`/index/dynamic`) for prior art.
- GPUI on Linux is its youngest backend; API churns. Pin `0.2.x`; watch signatures.
- Sandboxed-client networking: a port bound inside the flatpak sandbox should be reachable
  from the host (network namespace shared by default) — verify in M2.
- Event kinds must be finalized against zapstore's spec for possible compatibility.
- Review spam / sybil resistance in the web of trust.
- aarch64 day-one or later?
- License: workspace currently declares `MIT OR Apache-2.0` — confirm before first release.

## Build requirements (GPUI on Linux)

Expect the usual native stack for the `orchard` crate: `libxkbcommon`, `wayland`, freetype/
fontconfig, udev, and a Vulkan loader. Exact set confirmed during the first build; the
other three crates are pure-Rust.

## References

- Zapstore — https://github.com/zapstore/zapstore
- Flathub website monorepo — https://github.com/flathub-infra/website
- Flathub docs — https://github.com/flathub-infra/documentation
- `flatpak-remote(5)`, `oci+https://` extension — https://www.mankier.com/5/flatpak-remote
- Harbor: request for native Flatpak index support — https://github.com/goharbor/harbor/issues/22997
- Pulp: hosting Flatpak OCI images (`/index/dynamic`) — https://pulpproject.org/pulp_container/docs/user/guides/manage-flatpak/
- Flatpak 1.17: direct OCI install / sideloading — https://ostechnix.com/flatpak-1-17-0-oci-direct-install-sideloading/
- flatpak-container-tools (flatpak → OCI) — https://gitlab.com/flatpak-container/flatpak-container-tools
- gpui — https://crates.io/crates/gpui · https://gpui.rs
