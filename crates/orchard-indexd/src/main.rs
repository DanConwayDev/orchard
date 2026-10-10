//! Standalone runner for the M0 spike: serve a static catalog so a real
//! `flatpak remote-add orchard oci+http://127.0.0.1:<port>` can install a
//! hello-world app from a registry Orchard does not host.

use std::sync::Arc;

use orchard_indexd::{IndexEntry, StaticCatalog};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let addr = std::env::var("ORCHARD_INDEXD_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:7780".to_string())
        .parse()?;

    // Placeholder until M2 wires the real catalog in. Swap `com.example.Hello`
    // for whatever the spike's OCI image actually is; digest is intentionally
    // fake for now; the point is to validate the index protocol end-to-end.
    let catalog = StaticCatalog::new(vec![IndexEntry::for_ref(
        "app/com.example.Hello/x86_64/stable",
        "sha256:0000000000000000000000000000000000000000000000000000000000000000".into(),
        0,
    )]);

    println!("orchard-indexd serving on http://{addr}");
    orchard_indexd::serve(addr, Arc::new(catalog)).await
}
