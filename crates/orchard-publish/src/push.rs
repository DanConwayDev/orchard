//! Registry push, Docker v2 API: blob upload (single PUT, monolithic),
//! then the manifest, then a read-back check. Plain http for localhost
//! registries, https otherwise, matching the client's verify path.

use std::path::Path;

use anyhow::{bail, Context as _, Result};

use orchard_core::oci::sha256_digest;

const MEDIA_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";

/// Push every blob the layout's index references, then the manifest
/// itself. Returns the digest the registry will serve, which must equal
/// the locally computed one or something is lying.
///
/// `<registry>/<repo>` with an optional scheme on the registry part
/// (`https://localhost:5000/hello`): otherwise localhost registries use
/// http and everything else https.
pub fn push_layout(registry_repo: &str, tag: &str, layout: &Path) -> Result<String> {
    let mut scheme = None;
    let mut rest = registry_repo;
    for prefix in ["http://", "https://"] {
        if let Some(stripped) = rest.strip_prefix(prefix) {
            scheme = Some(prefix.trim_end_matches("://"));
            rest = stripped;
        }
    }
    let (registry, repository) = rest
        .split_once('/')
        .context("registry must be <registry>/<repo>, e.g. ghcr.io/owner/app")?;
    let base = registry_base(registry, scheme);
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(120))
        .build();

    // which blobs the index references: config, layers, and the manifest
    let index: serde_json::Value = serde_json::from_slice(&std::fs::read(layout.join("index.json"))?)
        .context("reading layout index.json")?;
    let manifest_digest = index["manifests"][0]["digest"]
        .as_str()
        .context("layout index without manifest digest")?
        .to_string();
    let manifest_bytes = read_blob(layout, &manifest_digest)?;
    let manifest: serde_json::Value = serde_json::from_slice(&manifest_bytes)?;
    let mut referenced = Vec::new();
    if let Some(config) = manifest["config"]["digest"].as_str() {
        referenced.push(config.to_string());
    }
    if let Some(layers) = manifest["layers"].as_array() {
        for layer in layers {
            if let Some(digest) = layer["digest"].as_str() {
                referenced.push(digest.to_string());
            }
        }
    }

    for digest in &referenced {
        if blob_present(&agent, &base, repository, digest)? {
            println!("  {registry}: blob {} already present", &digest[..19.min(digest.len())]);
            continue;
        }
        upload_blob(&agent, &base, repository, &read_blob(layout, digest)?)?;
        println!("  {registry}: pushed blob {}", &digest[..19.min(digest.len())]);
    }

    // 201 with an empty body on success; ureq already turned 4xx/5xx
    // into errors
    agent
        .put(&format!("{base}/v2/{repository}/manifests/{tag}"))
        .set("Content-Type", MEDIA_MANIFEST)
        .send_bytes(&manifest_bytes)
        .map_err(|err| anyhow::anyhow!("pushing manifest: {err}"))?;
    println!("  {registry}: pushed manifest {tag}");

    // read back by digest: the pin must be what the registry serves
    let served = agent
        .get(&format!("{base}/v2/{repository}/manifests/{manifest_digest}"))
        .set("Accept", MEDIA_MANIFEST)
        .call()
        .context("reading back the pushed manifest")?;
    let mut served_bytes = Vec::new();
    std::io::Read::read_to_end(&mut served.into_reader(), &mut served_bytes)?;
    let served_digest = sha256_digest(&served_bytes);
    if served_digest != manifest_digest {
        bail!(
            "registry serves {served_digest} where we pushed {manifest_digest}: refusing to sign"
        );
    }
    Ok(manifest_digest)
}

fn read_blob(layout: &Path, digest: &str) -> Result<Vec<u8>> {
    let path = layout
        .join("blobs/sha256")
        .join(digest.trim_start_matches("sha256:"));
    Ok(std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?)
}

fn blob_present(agent: &ureq::Agent, base: &str, repository: &str, digest: &str) -> Result<bool> {
    match agent
        .head(&format!("{base}/v2/{repository}/blobs/{digest}"))
        .call()
    {
        Ok(_) => Ok(true),
        Err(ureq::Error::Status(404, _)) => Ok(false),
        Err(err) => Err(err).context("checking blob"),
    }
}

fn upload_blob(agent: &ureq::Agent, base: &str, repository: &str, blob: &[u8]) -> Result<()> {
    let digest = sha256_digest(blob);
    let upload = agent
        .post(&format!("{base}/v2/{repository}/blobs/uploads/"))
        .call()
        .context("starting blob upload")?;
    let location = upload
        .header("Location")
        .context("upload response without Location")?
        .to_string();
    // the registry may hand back a relative URL
    let url = if location.starts_with('/') {
        format!("{base}{location}")
    } else {
        location
    };
    let separator = if url.contains('?') { '&' } else { '?' };
    agent
        .put(&format!("{url}{separator}digest={digest}"))
        .send_bytes(blob)
        .map_err(|err| anyhow::anyhow!("uploading blob {digest}: {err}"))?;
    Ok(())
}

fn registry_base(registry: &str, scheme: Option<&str>) -> String {
    let scheme = scheme.map(str::to_string).unwrap_or_else(|| {
        let local = registry.starts_with("localhost:")
            || registry.starts_with("127.0.0.1:")
            || registry.starts_with("[::1]:");
        if local { "http" } else { "https" }.to_string()
    });
    format!("{scheme}://{registry}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scheme_wins_over_localhost_default() {
        assert_eq!(registry_base("localhost:5000", None), "http://localhost:5000");
        assert_eq!(registry_base("ghcr.io", None), "https://ghcr.io");
        assert_eq!(
            registry_base("localhost:5000", Some("https")),
            "https://localhost:5000"
        );
    }
}
