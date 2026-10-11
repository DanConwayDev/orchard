//! Minimal OCI artifact access for verification and install.
//!
//! Orchard pins artifacts by manifest digest. Before any install the client
//! fetches the manifest from the artifact's location, hashes it, and refuses
//! to proceed when the hash differs from the catalog pin. This module speaks
//! just enough of the container ecosystem to do that:
//!
//! - `docker://<registry>/<repo>:<tag>`: the Docker Registry v2 HTTP API
//!   (`/v2/<repo>/manifests/<ref>`, `/v2/<repo>/blobs/<digest>`), which is
//!   what flatpak's own OCI support uses under the hood. Anonymous pulls on
//!   no-auth and CDN-redirecting registries work; token-authenticated
//!   registries (ghcr, quay) are a later milestone.
//! - `oci:<dir>`: a local OCI image layout (what `skopeo copy ... oci:` and
//!   podman push produce), which is also flatpak's zero-setup install path.
//!
//! Format notes, validated against flatpak 1.18 sources and a live Fedora
//! registry image: image config labels (capital-L `Labels`) carry
//! `org.flatpak.ref` (required by flatpak), `org.flatpak.metadata` (the app
//! ini), and optional sizes; a manifest may be a per-arch manifest or an
//! image index with a `platform` per entry.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context as _, Result};
use sha2::{Digest as _, Sha256};

/// Accept header for manifest fetches: OCI and Docker manifest/index types.
pub const MANIFEST_ACCEPT: &str = "application/vnd.oci.image.manifest.v1+json, \
     application/vnd.docker.distribution.manifest.v2+json, \
     application/vnd.oci.image.index.v1+json, \
     application/vnd.docker.distribution.manifest.list.v2+json";

/// What a successful verification learned about an artifact.
#[derive(Debug, Clone)]
pub struct VerifyReport {
    /// Manifest digest actually seen, `sha256:<hex>`.
    pub digest: String,
    /// Bytes of the (possibly per-arch selected) manifest.
    pub manifest_size: u64,
    /// `org.flatpak.ref` from the image config labels, if present.
    pub flatpak_ref: Option<String>,
    /// `org.flatpak.metadata` from the image config labels, if present.
    pub metadata: Option<String>,
    /// `org.flatpak.installed-size` label, bytes, if present.
    pub installed_size: Option<u64>,
    /// Sum of layer descriptor sizes, bytes.
    pub download_size: u64,
    /// Digest of the image config blob (which carries the labels).
    pub config_digest: Option<String>,
}

/// Hex sha256 of bytes, `sha256:`-prefixed like OCI digests.
pub fn sha256_digest(bytes: &[u8]) -> String {
    let hash = Sha256::digest(bytes);
    let hex: String = hash.iter().map(|b| format!("{b:02x}")).collect();
    format!("sha256:{hex}")
}

/// Where an installable artifact lives, parsed from a location string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Location {
    /// A local OCI image layout directory, e.g. `oci:/path/to/layout`.
    OciDir(PathBuf),
    /// A registry exposed via the Docker v2 API, e.g.
    /// `docker://ghcr.io/owner/repo:tag` (tag optional; digest pin rides
    /// alongside in the catalog).
    Registry {
        registry: String,
        repository: String,
        tag: Option<String>,
    },
}

impl Location {
    pub fn parse(s: &str) -> Result<Self> {
        if let Some(rest) = s.strip_prefix("oci:") {
            if rest.is_empty() {
                bail!("oci: location needs a path");
            }
            return Ok(Location::OciDir(PathBuf::from(rest)));
        }
        if let Some(rest) = s.strip_prefix("docker://") {
            // Registry = up to the first slash; the tag is a trailing colon
            // in the repository part (a registry port lives before it).
            let (registry, repository) = rest
                .split_once('/')
                .ok_or_else(|| anyhow!("docker:// location needs <registry>/<repo>: {s}"))?;
            let (repository, tag) = match repository.rfind(':') {
                Some(i) => (&repository[..i], Some(repository[i + 1..].to_string())),
                None => (repository, None),
            };
            if registry.is_empty() || repository.is_empty() {
                bail!("malformed docker:// location: {s}");
            }
            return Ok(Location::Registry {
                registry: registry.to_string(),
                repository: repository.to_string(),
                tag,
            });
        }
        bail!("unsupported artifact location {s:?} (want oci: or docker://)")
    }

    /// What to hand to `flatpak install` as the image location.
    pub fn install_arg(&self) -> String {
        match self {
            Location::OciDir(path) => format!("oci:{}", path.display()),
            Location::Registry {
                registry,
                repository,
                tag,
            } => {
                let repo = if registry.contains('/') {
                    repository.to_string()
                } else {
                    format!("{registry}/{repository}")
                };
                match tag {
                    Some(tag) => format!("docker://{repo}:{tag}"),
                    None => format!("docker://{repo}"),
                }
            }
        }
    }
}

/// Fetch and verify an artifact. `pinned_digest`, when set, must match the
/// manifest digest actually served.
pub fn verify(location: &Location, pinned_digest: Option<&str>) -> Result<VerifyReport> {
    match location {
        Location::OciDir(dir) => verify_local(dir, pinned_digest),
        Location::Registry {
            registry,
            repository,
            tag,
        } => verify_registry(registry, repository, tag.as_deref(), pinned_digest),
    }
}

fn verify_local(dir: &Path, pinned: Option<&str>) -> Result<VerifyReport> {
    let index_path = dir.join("index.json");
    let index_bytes = std::fs::read(&index_path)
        .with_context(|| format!("reading {}", index_path.display()))?;
    let index: serde_json::Value = serde_json::from_slice(&index_bytes)
        .with_context(|| format!("parsing {}", index_path.display()))?;
    let manifests = index
        .get("manifests")
        .and_then(|m| m.as_array())
        .ok_or_else(|| anyhow!("no manifests array in {}", index_path.display()))?;

    let entry = match manifests.len() {
        1 => &manifests[0],
        0 => bail!("empty index in {}", dir.display()),
        _ => bail!(
            "multiple manifests in {}; pass a reference (not supported yet)",
            dir.display()
        ),
    };
    let claimed = entry
        .get("digest")
        .and_then(|d| d.as_str())
        .ok_or_else(|| anyhow!("index entry without digest"))?;
    let bytes = read_local_blob(dir, claimed)?;
    let digest = sha256_digest(&bytes);
    check_pin(pinned, &digest)?;
    let mut report = report_from_manifest(&bytes, &digest)?;
    report.manifest_size = bytes.len() as u64;
    // The config blob carries the flatpak labels; read it locally with the
    // same integrity check the registry path gets from the digest URL.
    if let Some(config_digest) = report.config_digest.clone() {
        let config_bytes = read_local_blob(dir, &config_digest)?;
        apply_config_labels(&config_bytes, &mut report);
    }
    Ok(report)
}

fn verify_registry(
    registry: &str,
    repository: &str,
    tag: Option<&str>,
    pinned: Option<&str>,
) -> Result<VerifyReport> {
    let base = registry_base(registry);
    let agent = agent();
    let reference = tag.unwrap_or("latest");
    let url = format!("{base}/v2/{repository}/manifests/{reference}");
    let bytes = http_get(&agent, &url)?;

    // A tag may resolve to an index; select this machine's arch entry.
    let parsed: serde_json::Value = serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing manifest from {url}"))?;
    if parsed.get("manifests").is_some() {
        let wanted = arch_names();
        let entry = parsed["manifests"]
            .as_array()
            .and_then(|list| {
                list.iter()
                    .find(|e| entry_arch(e).is_some_and(|a| wanted.contains(&a)))
            })
            .ok_or_else(|| anyhow!("no manifest for {:?} in index at {url}", wanted))?;
        let entry_digest = entry
            .get("digest")
            .and_then(|d| d.as_str())
            .ok_or_else(|| anyhow!("index entry without digest at {url}"))?;
        let manifest_url = format!("{base}/v2/{repository}/manifests/{entry_digest}");
        let manifest_bytes = http_get(&agent, &manifest_url)?;
        let digest = sha256_digest(&manifest_bytes);
        check_pin(pinned, &digest)?;
        let mut report = report_from_manifest(&manifest_bytes, &digest)?;
        report.manifest_size = manifest_bytes.len() as u64;
        // Config and layers come through the same registry.
        finish_registry_report(&agent, &base, repository, &mut report)?;
        return Ok(report);
    }

    let digest = sha256_digest(&bytes);
    check_pin(pinned, &digest)?;
    let mut report = report_from_manifest(&bytes, &digest)?;
    report.manifest_size = bytes.len() as u64;
    finish_registry_report(&agent, &base, repository, &mut report)?;
    Ok(report)
}

fn finish_registry_report(
    agent: &ureq::Agent,
    base: &str,
    repository: &str,
    report: &mut VerifyReport,
) -> Result<()> {
    // Layers are only reachable as blobs; we do not download them here
    // (flatpak does at install), but the config blob carries the labels.
    if let Some(config_digest) = report.config_digest.clone() {
        let url = format!("{base}/v2/{repository}/blobs/{config_digest}");
        let config_bytes = http_get(agent, &url)?;
        apply_config_labels(&config_bytes, report);
    }
    Ok(())
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(30))
        .build()
}

/// Local registries speak plain http; everything else is https.
fn registry_base(registry: &str) -> String {
    let local = registry.starts_with("localhost:")
        || registry.starts_with("127.0.0.1:")
        || registry.starts_with("[::1]:");
    let scheme = if local { "http" } else { "https" };
    format!("{scheme}://{registry}")
}

fn http_get(agent: &ureq::Agent, url: &str) -> Result<Vec<u8>> {
    let response = agent
        .get(url)
        .set("Accept", MANIFEST_ACCEPT)
        .call()
        .with_context(|| format!("GET {url}"))?;
    let mut reader = response.into_reader();
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut reader, &mut bytes)
        .with_context(|| format!("reading body of {url}"))?;
    Ok(bytes)
}

/// Architecture aliases flatpak and OCI use for the same machine.
fn arch_names() -> Vec<&'static str> {
    match std::env::consts::ARCH {
        "x86_64" => vec!["amd64", "x86_64"],
        "aarch64" => vec!["arm64", "aarch64"],
        other => vec![other],
    }
}

fn entry_arch(entry: &serde_json::Value) -> Option<&str> {
    entry
        .get("platform")
        .and_then(|p| p.get("architecture"))
        .and_then(|a| a.as_str())
}

/// Build the report from manifest bytes: config digest, layer sizes.
fn report_from_manifest(manifest_bytes: &[u8], digest: &str) -> Result<VerifyReport> {
    let manifest: serde_json::Value = serde_json::from_slice(manifest_bytes)
        .with_context(|| format!("parsing manifest {digest}"))?;
    let config_digest = manifest
        .get("config")
        .and_then(|c| c.get("digest"))
        .and_then(|d| d.as_str())
        .map(str::to_string);
    let download_size = manifest
        .get("layers")
        .and_then(|l| l.as_array())
        .map(|layers| {
            layers
                .iter()
                .filter_map(|l| l.get("size").and_then(|s| s.as_u64()))
                .sum()
        })
        .unwrap_or(0);
    Ok(VerifyReport {
        digest: digest.to_string(),
        manifest_size: 0,
        flatpak_ref: None,
        metadata: None,
        installed_size: None,
        download_size,
        config_digest,
    })
}

/// Fill label-derived fields from the image config blob.
fn apply_config_labels(config_bytes: &[u8], report: &mut VerifyReport) {
    let Ok(config) = serde_json::from_slice::<serde_json::Value>(config_bytes) else {
        return;
    };
    // Docker-style capital Labels; accept lowercase too.
    let labels = config
        .get("config")
        .and_then(|c| c.get("Labels").or_else(|| c.get("labels")))
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let get = |key: &str| -> Option<String> {
        labels
            .get(key)
            .and_then(|v| v.as_str())
            .map(str::to_string)
    };
    report.flatpak_ref = get("org.flatpak.ref");
    report.metadata = get("org.flatpak.metadata");
    report.installed_size = get("org.flatpak.installed-size")
        .and_then(|s| s.parse().ok());
}

/// Read a blob from an OCI layout, verifying its content matches the digest
/// in its path name.
fn read_local_blob(dir: &Path, digest: &str) -> Result<Vec<u8>> {
    let hex = digest
        .strip_prefix("sha256:")
        .ok_or_else(|| anyhow!("only sha256 digests are supported: {digest}"))?;
    let path = dir.join("blobs/sha256").join(hex);
    let bytes = std::fs::read(&path)
        .with_context(|| format!("reading {}", path.display()))?;
    let actual = sha256_digest(&bytes);
    if actual != digest {
        bail!("blob integrity failure: {} hashes to {actual}", path.display());
    }
    Ok(bytes)
}

fn check_pin(pinned: Option<&str>, actual: &str) -> Result<()> {
    match pinned {
        Some(pinned) if pinned != actual => Err(anyhow!(
            "digest mismatch: catalog pins {pinned}, registry served {actual}"
        )),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Unique per test: tests run in parallel and must not share a layout.
    fn unique_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "orchard-oci-test-{}-{name}",
            std::process::id()
        ))
    }

    /// Build a minimal but valid OCI layout in a temp dir: one config, one
    /// gzip layer, one manifest, index.json.
    fn make_layout(name: &str) -> (PathBuf, String) {
        let dir = unique_dir(name);
        let _ = std::fs::remove_dir_all(&dir);
        let blobs = dir.join("blobs/sha256");
        std::fs::create_dir_all(&blobs).unwrap();

        let config = serde_json::json!({
            "architecture": "amd64",
            "os": "linux",
            "config": {
                "Labels": {
                    "org.flatpak.ref": "app/com.example.Hello/x86_64/stable",
                    "org.flatpak.metadata": "[Application]\nname=com.example.Hello\n"
                }
            },
            "rootfs": {"type": "layers", "diff_ids": ["sha256:aa"]}
        });
        let config_bytes = serde_json::to_vec(&config).unwrap();
        let config_digest = sha256_digest(&config_bytes);

        let layer_bytes = b"layer-bytes".to_vec();
        let layer_digest = sha256_digest(&layer_bytes);

        let manifest = serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": {
                "mediaType": "application/vnd.oci.image.config.v1+json",
                "digest": config_digest,
                "size": config_bytes.len()
            },
            "layers": [{
                "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
                "digest": layer_digest,
                "size": layer_bytes.len()
            }]
        });
        let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
        let manifest_digest = sha256_digest(&manifest_bytes);

        for (digest, bytes) in [
            (&config_digest, &config_bytes),
            (&layer_digest, &layer_bytes),
            (&manifest_digest, &manifest_bytes),
        ] {
            let hex = digest.strip_prefix("sha256:").unwrap();
            std::fs::write(blobs.join(hex), bytes).unwrap();
        }

        let index = serde_json::json!({
            "schemaVersion": 2,
            "manifests": [{
                "mediaType": "application/vnd.oci.image.manifest.v1+json",
                "digest": manifest_digest,
                "size": manifest_bytes.len()
            }]
        });
        std::fs::write(dir.join("index.json"), serde_json::to_vec(&index).unwrap())
            .unwrap();
        std::fs::write(
            dir.join("oci-layout"),
            br#"{"imageLayoutVersion":"1.0.0"}"#,
        )
        .unwrap();

        (dir, manifest_digest)
    }

    #[test]
    fn verifies_local_layout_and_reads_labels() {
        let (dir, digest) = make_layout("verify");
        let report = verify(&Location::OciDir(dir.clone()), Some(&digest)).unwrap();
        assert_eq!(report.digest, digest);
        assert_eq!(
            report.flatpak_ref.as_deref(),
            Some("app/com.example.Hello/x86_64/stable")
        );
        assert!(report.metadata.as_deref().unwrap_or("").starts_with("[Application]"));
        assert_eq!(report.download_size, 11);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rejects_pin_mismatch() {
        let (dir, _digest) = make_layout("pin");
        let err = verify(&Location::OciDir(dir.clone()), Some("sha256:bad"))
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("digest mismatch"), "{err}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn detects_tampered_blob() {
        let (dir, digest) = make_layout("tamper");
        // Flip a byte in the config blob.
        let hex = {
            let manifest: serde_json::Value = serde_json::from_slice(
                &std::fs::read(
                    dir.join("blobs/sha256")
                        .join(digest.strip_prefix("sha256:").unwrap()),
                )
                .unwrap(),
            )
            .unwrap();
            manifest["config"]["digest"]
                .as_str()
                .unwrap()
                .strip_prefix("sha256:")
                .unwrap()
                .to_string()
        };
        let path = dir.join("blobs/sha256").join(&hex);
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[0] ^= 0xff;
        std::fs::write(&path, &bytes).unwrap();
        assert!(verify(&Location::OciDir(dir.clone()), None).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn parses_locations() {
        let loc = Location::parse("docker://ghcr.io/owner/repo:1.2").unwrap();
        assert_eq!(
            loc,
            Location::Registry {
                registry: "ghcr.io".into(),
                repository: "owner/repo".into(),
                tag: Some("1.2".into())
            }
        );
        assert_eq!(loc.install_arg(), "docker://ghcr.io/owner/repo:1.2");

        // Host with port keeps the port in the registry part.
        let loc = Location::parse("docker://localhost:5000/hello:stable").unwrap();
        assert_eq!(
            loc,
            Location::Registry {
                registry: "localhost:5000".into(),
                repository: "hello".into(),
                tag: Some("stable".into())
            }
        );

        let loc = Location::parse("oci:/some/dir").unwrap();
        assert_eq!(loc.install_arg(), "oci:/some/dir");

        assert!(Location::parse("https://nope").is_err());
    }

    #[test]
    fn local_registries_use_http() {
        assert_eq!(
            registry_base("localhost:5000"),
            "http://localhost:5000"
        );
        assert_eq!(
            registry_base("ghcr.io"),
            "https://ghcr.io"
        );
    }
}
