//! Diagnostic: parse an appstream.xml.gz and print a summary.
//!
//!   cargo run --release -p orchard-core --example appstream_dump -- <path>
//!
//! The path is usually `<install>/appstream/flathub/x86_64/active/appstream.xml.gz`.

use std::time::Instant;

fn main() -> anyhow::Result<()> {
    if std::env::args().nth(3).is_some() {
        return probe(&std::env::args().nth(1).unwrap(), &std::env::args().nth(3).unwrap().trim_start_matches("--"));
    }
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

// quick probe: print one app's description lines verbatim
fn probe(path: &str, id: &str) -> anyhow::Result<()> {
    use std::path::Path;
    let path = Path::new(path);
    let icons = path.parent().unwrap().join("icons");
    let apps = orchard_core::appstream::load(path, &icons)?;
    let app = apps.iter().find(|a| a.id == id).ok_or_else(|| anyhow::anyhow!("no {id}"))?;
    println!("version={:?} date={:?} home={:?}", app.version, app.release_date, app.homepage);
    for line in app.description.as_deref().unwrap_or("").lines() {
        println!("LINE| {line}");
    }
    Ok(())
}
