//! Standalone runner: serve a catalog file over HTTP without the GPUI client.
//!
//!   orchard-indexd [path/to/catalog.json]
//!
//! Defaults to `./demo/catalog.json` when it exists, then
//! `$XDG_DATA_HOME/orchard/catalog.json`. The client embeds this library and
//! runs the same service in-process; the standalone binary exists for
//! debugging and for driving `flatpak` from scripts.

use std::sync::Arc;

use anyhow::Context as _;
use orchard_core::CatalogFile;
use orchard_indexd::{ServerState, StaticCatalog};

fn load_catalog() -> anyhow::Result<CatalogFile> {
    let mut candidates: Vec<std::path::PathBuf> = Vec::new();
    if let Some(arg) = std::env::args().nth(1) {
        candidates.push(arg.into());
    }
    candidates.push("demo/catalog.json".into());
    if let Some(data) = std::env::var_os("XDG_DATA_HOME") {
        candidates.push(std::path::PathBuf::from(data).join("orchard/catalog.json"));
    }
    if let Some(home) = std::env::var_os("HOME") {
        candidates.push(
            std::path::PathBuf::from(home)
                .join(".local/share/orchard/catalog.json"),
        );
    }

    for path in &candidates {
        match std::fs::read(path) {
            Ok(bytes) => {
                let catalog: CatalogFile = serde_json::from_slice(bytes.as_slice())
                    .with_context(|| format!("parsing {}", path.display()))?;
                eprintln!("loaded catalog from {}", path.display());
                return Ok(catalog);
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => return Err(err).with_context(|| path.display().to_string()),
        }
    }
    anyhow::bail!("no catalog found (tried {:?})", candidates)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let addr = std::env::var("ORCHARD_INDEXD_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:7780".to_string())
        .parse()?;

    let catalog = Arc::new(load_catalog()?);
    let entries = catalog
        .apps
        .iter()
        .map(|app| {
            orchard_indexd::IndexEntry::for_ref(
                &app.flatpak_ref,
                app.digest.clone().unwrap_or_else(|| "sha256:unknown".into()),
                0,
            )
        })
        .collect();

    let state = ServerState {
        index: Arc::new(StaticCatalog::new(entries)),
        catalog,
    };

    println!("orchard-indexd serving on http://{addr}");
    orchard_indexd::serve(addr, state).await
}
