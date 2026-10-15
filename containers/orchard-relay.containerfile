# Orchard dev relay: nostr-rs-relay 0.10.0 with the quanta TSC
# calibration panic patched out. On hosts with a warped TSC core
# (metrics-rs/quanta#61, AMD ThinkPads), quanta 0.9.3 (pinned here via
# prometheus 0.13) panics in its calibration and takes down the tokio
# worker that answers EVENT writes: reads work, writes hang with no
# OK. Upstream's image (0.10.0, the newest) still pins quanta 0.9.3,
# so the dev relay builds the upstream 0.10.0 release with quanta's
# own fix (metrics-rs/quanta#113) applied to 0.9.3: skip the ratio
# adjustment on a zero delta instead of panicking.
#
# Build and run from the repo root:
#   podman build -t localhost/orchard-relay \
#     -f containers/orchard-relay.containerfile containers/
#   podman run -d --name orchard-relay -p 8008:8080 \
#     -v ./relay/config.toml:/usr/src/app/config.toml:z \
#     localhost/orchard-relay
# The config is bind-mounted, not copied: the repo file is the source
# of truth and a restart applies its edits. The database lives in the
# container layer (survives podman restart, not podman rm).

FROM docker.io/library/rust:1 AS build
RUN apt-get update \
    && apt-get install -y --no-install-recommends gawk protobuf-compiler \
    && rm -rf /var/lib/apt/lists
WORKDIR /build
ADD https://github.com/scsibug/nostr-rs-relay/archive/refs/tags/0.10.0.tar.gz src.tar.gz
RUN tar xzf src.tar.gz && mv nostr-rs-relay-0.10.0 relay
WORKDIR /build/relay
# quanta 0.9.3, patched: the calibration's zero-delta panic becomes a
# skip (the loop re-measures next round; until it converges the scale
# is 1:1, which only skews prometheus metric durations)
RUN mkdir quanta-fix \
    && curl -sL https://static.crates.io/crates/quanta/quanta-0.9.3.crate -o /tmp/quanta.crate \
    && tar xzf /tmp/quanta.crate -C /tmp \
    && mv /tmp/quanta-0.9.3/* quanta-fix/ \
    && sed -i 's|panic!("po2_denom was zero!");|return; // patched: quanta#113 (TSC warp makes the delta wrap)|' quanta-fix/src/lib.rs \
    && printf '\n[patch.crates-io]\nquanta = { path = "quanta-fix" }\n' >> Cargo.toml
RUN cargo build --release

FROM registry.fedoraproject.org/fedora:latest
RUN useradd --system --uid 1000 appuser
COPY --from=build /build/relay/target/release/nostr-rs-relay /usr/local/bin/
RUN mkdir -p /usr/src/app/db && chown -R appuser:appuser /usr/src/app
WORKDIR /usr/src/app
USER appuser
CMD ["nostr-rs-relay", "--db", "/usr/src/app/db"]
