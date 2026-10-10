//! # orchard-indexd
//!
//! The heart of Orchard's distribution model: a small HTTP service on
//! `127.0.0.1` that exposes Flatpak's OCI "registry index" protocol, backed by
//! Orchard's catalog instead of by a central server. The user's flatpak talks
//! to it as an ordinary remote (`oci+http://127.0.0.1:7780`), so every Orchard
//! app is a native Flatpak remote app: GNOME Software, Discover, and
//! `flatpak update` all keep working.
//!
//! ## Protocol status
//!
//! The index endpoints and annotation keys below are the current best
//! understanding and **MUST be validated in M0** against flatpak's
//! `common/flatpak-oci-registry.c`. Ground truth beats this doc comment.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::extract::{Path as AxumPath, State};
use axum::routing::get;
use axum::{Json, Router};

/// Annotation key Flatpak uses to map an OCI manifest to a flatpak ref.
pub const ANNOTATION_FLATPAK_REF: &str = "org.flatpak.ref";

const INDEX_MEDIA_TYPE: &str = "application/vnd.oci.image.index.v1+json";
const MANIFEST_MEDIA_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";

/// The document served at `GET /index/<arch>`: an OCI image index whose
/// entries are the Flatpak apps this catalog serves.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct OciIndex {
    #[serde(rename = "schemaVersion")]
    pub schema_version: u32,
    #[serde(rename = "mediaType")]
    pub media_type: String,
    pub manifests: Vec<IndexEntry>,
}

/// One entry in the index: a pointer to an image manifest plus Flatpak annotations.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct IndexEntry {
    #[serde(rename = "mediaType")]
    pub media_type: String,
    pub digest: String,
    pub size: u64,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub annotations: BTreeMap<String, String>,
}

impl IndexEntry {
    /// Build an index entry for a flatpak ref, e.g.
    /// `app/com.example.Hello/x86_64/stable`.
    pub fn for_ref(flatpak_ref: &str, digest: String, size: u64) -> Self {
        let mut annotations = BTreeMap::new();
        annotations.insert(
            ANNOTATION_FLATPAK_REF.to_string(),
            flatpak_ref.to_string(),
        );
        Self {
            media_type: MANIFEST_MEDIA_TYPE.to_string(),
            digest,
            size,
            annotations,
        }
    }

    pub fn flatpak_ref(&self) -> Option<&str> {
        self.annotations.get(ANNOTATION_FLATPAK_REF).map(String::as_str)
    }
}

/// Where the shim gets its catalog. In M0 this is a static list; from M2 it is
/// backed by the client's merged Flathub + Nostr state.
pub trait CatalogSource: Send + Sync + 'static {
    /// Index entries for the given architecture (e.g. `x86_64`, `aarch64`).
    fn entries(&self, arch: &str) -> Vec<IndexEntry>;
}

/// A fixed catalog, useful for the M0 spike and tests.
#[derive(Debug, Clone)]
pub struct StaticCatalog {
    entries: Vec<IndexEntry>,
}

impl StaticCatalog {
    pub fn new(entries: Vec<IndexEntry>) -> Self {
        Self { entries }
    }
}

impl CatalogSource for StaticCatalog {
    fn entries(&self, arch: &str) -> Vec<IndexEntry> {
        let needle = format!("/{arch}/");
        self.entries
            .iter()
            .filter(|e| match e.flatpak_ref() {
                Some(r) => r.contains(&needle),
                None => true, // entries without a ref pass through untouched
            })
            .cloned()
            .collect()
    }
}

/// Build the axum router for a catalog.
pub fn router(catalog: Arc<dyn CatalogSource>) -> Router {
    Router::new()
        .route("/", get(root))
        .route("/index/{arch}", get(index))
        .with_state(catalog)
}

async fn root() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "service": "orchard-indexd",
        "description": "Flatpak OCI registry-index shim backed by Orchard",
        "endpoints": ["/index/{arch}"],
        "note": "protocol under validation in M0 (see DESIGN.md)"
    }))
}

async fn index(
    State(catalog): State<Arc<dyn CatalogSource>>,
    AxumPath(arch): AxumPath<String>,
) -> Json<OciIndex> {
    Json(OciIndex {
        schema_version: 2,
        media_type: INDEX_MEDIA_TYPE.to_string(),
        manifests: catalog.entries(&arch),
    })
}

/// Bind and serve. The port is reachable from the host even when Orchard runs
/// sandboxed, because flatpak sandboxes share the network namespace by default
/// (asserted in M2).
pub async fn serve(addr: std::net::SocketAddr, catalog: Arc<dyn CatalogSource>) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, router(catalog)).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hello_entry() -> IndexEntry {
        IndexEntry::for_ref(
            "app/com.example.Hello/x86_64/stable",
            "sha256:0000000000000000000000000000000000000000000000000000000000000000".into(),
            1234,
        )
    }

    #[test]
    fn index_json_shape() {
        let catalog = StaticCatalog::new(vec![hello_entry()]);
        let index = OciIndex {
            schema_version: 2,
            media_type: INDEX_MEDIA_TYPE.to_string(),
            manifests: catalog.entries("x86_64"),
        };
        let json = serde_json::to_value(&index).unwrap();
        assert_eq!(json["schemaVersion"], 2);
        assert_eq!(
            json["manifests"][0]["annotations"]["org.flatpak.ref"],
            "app/com.example.Hello/x86_64/stable"
        );
    }

    #[test]
    fn static_catalog_filters_by_arch() {
        let mut other = IndexEntry::for_ref(
            "app/com.example.Other/aarch64/stable",
            "sha256:1111".into(),
            1,
        );
        other
            .annotations
            .insert("unrelated".into(), "x".into());
        let catalog = StaticCatalog::new(vec![hello_entry(), other]);
        let x86: Vec<_> = catalog.entries("x86_64");
        assert_eq!(x86.len(), 1);
        assert_eq!(x86[0].flatpak_ref(), Some("app/com.example.Hello/x86_64/stable"));
    }
}
