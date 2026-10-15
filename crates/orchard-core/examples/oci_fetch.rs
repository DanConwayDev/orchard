//! Diagnostic: replay the client's publisher install fetch, off the GUI.
//!
//!   cargo run --release -p orchard-core --example oci_fetch -- <location> <pinned-digest|-> <dest-dir>
//!
//! Runs Location::parse, oci::verify, and oci::download_layout in the same
//! order verify_then_install does, so a registry-side or layout-side
//! failure reproduces on the terminal.

use std::path::Path;

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let location_text = args
        .next()
        .expect("usage: oci_fetch <location> <pinned-digest|-> <dest-dir>");
    let pinned_text = args
        .next()
        .expect("pinned digest, or - for none");
    let dest = args.next().expect("dest dir");
    let pinned = if pinned_text == "-" {
        None
    } else {
        Some(pinned_text.as_str())
    };

    let location = orchard_core::oci::Location::parse(&location_text)?;
    println!("parsed location: {location:?}");
    let report = orchard_core::oci::verify(&location, pinned)?;
    println!("verify ok: manifest {}", report.digest);
    let digest =
        orchard_core::oci::download_layout(&location, Some(&report.digest), Path::new(&dest))?;
    println!("layout at {dest} (manifest {digest})");
    Ok(())
}
