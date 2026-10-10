# Orchard

## Build

The host is atomic: there is no host toolchain. Never run cargo on the
host and never shim one; a host build cannot link here and poisons the
shared target/ tree besides. Build only through the container:

    ./scripts/build.sh check              # whole-workspace typecheck
    ./scripts/build.sh test --workspace
    ./scripts/build.sh build --release

The script creates `localhost/orchard-dev-rust` on first use (derived
from kuma's `kuma-dev-gcc`, per `containers/`), with rustup and the
cargo registry in named volumes so builds are incremental. The pinned
toolchain lives in `rust-toolchain.toml`.

Run the built binary on the host, not in the container: the GPUI shell
needs the host's Wayland session, and the container has no seat.

## Rules

- Commit before build: the tree that produced a binary is a named commit.
- Commits are the agent's to make; pushes wait for an explicit go.
- No em dashes in documentation or code comments; use commas, colons,
  or parentheses instead. Deliberate owner preference.
- Orchard is independent of kuma and kumaui: never write to or commit
  in those trees from this project, even for a fix that looks like it
  belongs there.

## Docs

`DESIGN.md` is the architecture record: locked decisions, the Nostr
protocol draft, milestones M0 through M6. Update it when a decision
changes.
