//! Diagnostic: replicate the client's Installed filter pipeline.
//!
//!   cargo run --release -p orchard-core --example installed_dump

use std::collections::HashMap;

fn main() {
    // What orchard_flatpak::list_installed produces.
    let output = std::process::Command::new("flatpak")
        .args([
            "list",
            "--app",
            "--columns=application,version,origin,installation",
        ])
        .output()
        .expect("flatpak list");
    let text = String::from_utf8_lossy(&output.stdout);
    let installed: HashMap<String, ()> = text
        .lines()
        .filter_map(|line| {
            let app_id = line.split('\t').next()?.trim().to_string();
            if app_id.is_empty() {
                None
            } else {
                eprintln!("installed row: {line:?}");
                Some((app_id, ()))
            }
        })
        .collect();
    eprintln!("installed map: {} entries", installed.len());

    // What the AppStream load produces.
    for path in orchard_core::appstream::appstream_paths("flathub", "x86_64") {
        if !path.exists() {
            eprintln!("missing: {}", path.display());
            continue;
        }
        let icons_root = path.parent().unwrap().join("icons");
        let apps = orchard_core::appstream::load(&path, &icons_root).expect("parse");
        eprintln!("appstream: {} apps from {}", apps.len(), path.display());
        for id in ["app.zen_browser.zen", "com.spotify.Client", "io.github.celluloid_player.Celluloid", "net.lutris.Lutris"] {
            let present = apps.iter().any(|app| app.id == id);
            eprintln!("  {id}: in catalog = {present}, installed = {}", installed.contains_key(id));
        }
        break;
    }
}
