//! Diagnostic: parse an appstream.xml.gz and print a summary.
//!
//!   cargo run --release -p orchard-core --example appstream_dump -- <path>
//!
//! The path is usually `<install>/appstream/flathub/x86_64/active/appstream.xml.gz`.

use std::time::Instant;

fn main() -> anyhow::Result<()> {
    let path = std::env::args()
        .nth(1)
        .expect("usage: appstream_dump <appstream.xml.gz>");
    let path = std::path::Path::new(&path);
    let icons_root = path
        .parent()
        .map(|active| active.join("icons"))
        .expect("path needs a parent");

    let start = Instant::now();
    let apps = orchard_core::appstream::load(path, &icons_root)?;
    let elapsed = start.elapsed();
    let with_icons = apps.iter().filter(|app| app.icon_path.is_some()).count();
    let with_version = apps.iter().filter(|app| app.version.is_some()).count();
    println!(
        "parsed {} apps in {elapsed:.1?} ({with_icons} with cached icons, {with_version} with a version)",
        apps.len()
    );
    for app in apps.iter().take(5) {
        println!("  {}: {} - {} ({:?})", app.id, app.name, app.summary, app.version);
    }
    Ok(())
}
