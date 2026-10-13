//! Orchard: the permissionless Flatpak store client.
//!
//! MVP: browse and search the Flathub catalog (from the local AppStream),
//! plus the Orchard channel (publisher apps from the catalog served by the
//! in-process indexd), with digest-verified direct OCI installs and a
//! finish-args permissions viewer.

mod assets;
mod cache;
mod qr;
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
fn start_indexd(catalog: orchard_core::CatalogFile, settings: orchard_core::SettingsHandle) {
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
                document: std::sync::Arc::new(catalog),
                relays_reset: None,
            };
            let cache = crate::cache::events_cache_path();
            // the settings handle stays live: relay edits in the
            // client's settings page reach the relay session and the
            // crawlers within a second, no restart
            if let Err(err) = runtime.block_on(orchard_indexd::serve(addr, state, settings, cache)) {
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
    let settings = orchard_core::SettingsHandle::load_default();
    if let Some(catalog) = seed_catalog {
        start_indexd(catalog, settings.clone());
        // the UI's first fetches (profiles, graph) fire within
        // milliseconds; give the in-process server's bind a moment,
        // or the client degrades for an hour over a lost race
        for _ in 0..40 {
            if std::net::TcpStream::connect(indexd_addr()).is_ok() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
    }
    let settings_for_root = settings.clone();

    Application::new()
        .with_assets(assets::Assets)
        .run(move |cx: &mut App| {
            // Establish the palette seam: defaults today, wallpaper-derived
            // palettes later swap this wholesale.
            crate::ui::apply_palette(&crate::ui::Palette::default());
            let bounds = Bounds::centered(None, size(px(1100.), px(720.)), cx);
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(gpui::TitlebarOptions {
                        title: Some("Orchard".into()),
                        ..Default::default()
                    }),
                    app_id: Some("rocks.orchard.Orchard".into()),
                    ..Default::default()
                },
                |window, cx| {
                    let settings = settings_for_root.clone();
                    let root = cx.new(|cx| StoreRoot::new(cx, settings));
                    // Keyboard lives on the root from the start (type to search).
                    window.focus(&root.read(cx).focus_handle);
                    root
                },
            )
            .expect("failed to open Orchard window");
            cx.activate(true);
        });
}
