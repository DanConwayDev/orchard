//! Orchard: the permissionless Flatpak store client.
//!
//! MVP: browse and search the Flathub catalog (from the local AppStream),
//! plus the Orchard channel (publisher apps from the catalog served by the
//! in-process indexd), with digest-verified direct OCI installs and a
//! finish-args permissions viewer.

mod store;
mod ui;

use gpui::{App, AppContext as _, Application, Bounds, WindowBounds, WindowOptions, px, size};

use crate::store::{INDEXD_PORT, StoreRoot};

/// Where indexd listens inside the client process. The client is the remote.
pub fn indexd_addr() -> std::net::SocketAddr {
    std::net::SocketAddr::from(([127, 0, 0, 1], INDEXD_PORT))
}

/// Start indexd on its own thread with its own tokio runtime. Errors are
/// non-fatal for the MVP: the client falls back to reading the catalog file
/// directly (and indexd has no other MVP duty).
fn start_indexd(catalog: orchard_core::CatalogFile) {
    let addr = indexd_addr();
    std::thread::Builder::new()
        .name("orchard-indexd".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(err) => {
                    eprintln!("indexd runtime failed to start: {err}");
                    return;
                }
            };
            let state = orchard_indexd::ServerState {
                index: std::sync::Arc::new(orchard_indexd::StaticCatalog::new(
                    catalog
                        .apps
                        .iter()
                        .map(|app| {
                            orchard_indexd::IndexEntry::for_ref(
                                &app.flatpak_ref,
                                app.digest
                                    .clone()
                                    .unwrap_or_else(|| "sha256:unknown".into()),
                                0,
                            )
                        })
                        .collect(),
                )),
                catalog: std::sync::Arc::new(catalog),
            };
            if let Err(err) = runtime.block_on(orchard_indexd::serve(addr, state)) {
                eprintln!("indexd failed: {err:#}");
            }
        })
        .expect("failed to spawn indexd thread");
}

/// Catalog file candidates, mirroring indexd's standalone runner.
pub fn catalog_file_paths() -> Vec<std::path::PathBuf> {
    let mut paths = Vec::new();
    if let Some(exe_dir) = std::env::current_exe().ok().and_then(|p| p.ancestors().nth(2).map(|a| a.to_path_buf())) {
        paths.push(exe_dir.join("demo/catalog.json"));
    }
    paths.push("demo/catalog.json".into());
    if let Some(home) = std::env::var_os("HOME") {
        paths.push(
            std::path::PathBuf::from(home).join("Documents/orchard/demo/catalog.json"),
        );
    }
    if let Some(data) = std::env::var_os("XDG_DATA_HOME") {
        paths.push(std::path::PathBuf::from(data).join("orchard/catalog.json"));
    }
    if let Some(home) = std::env::var_os("HOME") {
        paths.push(
            std::path::PathBuf::from(home).join(".local/share/orchard/catalog.json"),
        );
    }
    paths
}

fn main() {
    // The catalog file feeds indexd at startup; the client then reads it
    // back over HTTP to prove the loop.
    let seed_catalog = catalog_file_paths()
        .iter()
        .find_map(|path| {
            let bytes = std::fs::read(path).ok()?;
            serde_json::from_slice::<orchard_core::CatalogFile>(&bytes).ok()
        });
    if let Some(catalog) = seed_catalog {
        start_indexd(catalog);
    }

    Application::new().run(|cx: &mut App| {
        // Establish the palette seam: defaults today, wallpaper-derived
        // palettes later swap this wholesale.
        crate::ui::apply_palette(&crate::ui::Palette::default());
        let bounds = Bounds::centered(None, size(px(1100.), px(720.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |window, cx| {
                let root = cx.new(|cx| StoreRoot::new(cx));
                // Keyboard lives on the root from the start (type to search).
                window.focus(&root.read(cx).focus_handle);
                root
            },
        )
        .expect("failed to open Orchard window");
        cx.activate(true);
    });
}
