//! # orchard-indexd
//!
//! The heart of Orchard's distribution model: a small HTTP service on
//! `127.0.0.1` that the client runs in-process. It is the future home of
//! every protocol surface Orchard needs between "the catalog" and "flatpak",
//! so that the user's flatpak can one day talk to it as an ordinary remote
//! (`oci+http://127.0.0.1:7780`) and GNOME Software, Discover, and
//! `flatpak update` all keep working.
//!
//! ## Endpoints
//!
//! - `GET /v1/catalog` - the full catalog document ([`CatalogFile`]). The
//!   client reads this over HTTP rather than opening catalog files directly,
//!   because indexd is the seam a remote catalog (Nostr, M3) plugs into
//!   without touching client code.
//! - `GET /index/<arch>` - an OCI image index enumerating the catalog's
//!   artifacts. Kept from the first spike: this is *not* what flatpak's
//!   `oci+` remotes read (validated in the M0 spike: those fetch an **ostree
//!   summary** from the URL base, ref entries mapping to manifest digests,
//!   with `xa.oci-registry-uri` pointing at the blob registry; see
//!   `crates/orchard-flatpak/src/lib.rs`). The native-remote summary
//!   endpoint lands in a later milestone; until then MVP installs go direct
//!   via `flatpak install oci:/…` / `docker://…` with client-side digest
//!   verification.
//! - `GET /` - service description.

use std::sync::Arc;

use axum::extract::{Path as AxumPath, State};
use axum::routing::get;
use axum::{Json, Router};

use orchard_core::CatalogFile;

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
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub annotations: std::collections::BTreeMap<String, String>,
}

impl IndexEntry {
    /// Build an index entry for a flatpak ref, e.g.
    /// `app/com.example.Hello/x86_64/stable`.
    pub fn for_ref(flatpak_ref: &str, digest: String, size: u64) -> Self {
        let mut annotations = std::collections::BTreeMap::new();
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

/// Where the shim gets its index entries. In the MVP this is the static
/// catalog file; later it is backed by the client's live Nostr state.
pub trait CatalogSource: Send + Sync + 'static {
    /// Index entries for the given architecture (e.g. `x86_64`, `aarch64`).
    fn entries(&self, arch: &str) -> Vec<IndexEntry>;
}

/// A fixed catalog, useful for tests.
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

/// Shared server state: the index source and the catalog document.
#[derive(Clone)]
pub struct ServerState {
    pub index: Arc<dyn CatalogSource>,
    pub catalog: Arc<CatalogFile>,
}

/// Build the axum router for a state.
pub fn router(state: ServerState) -> Router {
    Router::new()
        .route("/", get(root))
        .route("/index/{arch}", get(index))
        .route("/v1/catalog", get(catalog))
        .with_state(state)
}

async fn root() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "service": "orchard-indexd",
        "description": "Orchard's local catalog service",
        "endpoints": ["/v1/catalog", "/index/{arch}"]
    }))
}

async fn index(
    State(state): State<ServerState>,
    AxumPath(arch): AxumPath<String>,
) -> Json<OciIndex> {
    Json(OciIndex {
        schema_version: 2,
        media_type: INDEX_MEDIA_TYPE.to_string(),
        manifests: state.index.entries(&arch),
    })
}

async fn catalog(State(state): State<ServerState>) -> Json<CatalogFile> {
    Json((*state.catalog).clone())
}

/// Bind and serve. The port is reachable from the host even when Orchard runs
/// sandboxed, because flatpak sandboxes share the network namespace by default
/// (asserted in M2).
pub async fn serve(addr: std::net::SocketAddr, state: ServerState) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, router(state)).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use orchard_core::CatalogApp;

    fn sample_catalog() -> CatalogFile {
        CatalogFile {
            version: 1,
            apps: vec![CatalogApp {
                app_id: "com.orchard.Hello".into(),
                name: "Orchard Hello".into(),
                summary: "Demo app".into(),
                description: None,
                icon_url: None,
                categories: Vec::new(),
                publisher: orchard_core::Publisher {
                    pubkey: "demo-pubkey".into(),
                    nip05: None,
                },
                version: "1.0.0".into(),
                flatpak_ref: "app/com.orchard.Hello/x86_64/stable".into(),
                location: "oci:/tmp/demo".into(),
                digest: Some("sha256:00".into()),
                metadata: None,
                notes: None,
                published_at: 0,
            }],
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn catalog_endpoint_serves_document() {
        let catalog = sample_catalog();
        let entries = catalog
            .apps
            .iter()
            .map(|a| {
                IndexEntry::for_ref(
                    &a.flatpak_ref,
                    a.digest.clone().unwrap_or_default(),
                    0,
                )
            })
            .collect();
        let router = router(ServerState {
            index: Arc::new(StaticCatalog::new(entries)),
            catalog: Arc::new(catalog),
        });

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });

        // Blocking client on this thread; the multi-thread runtime keeps the
        // server responsive on its own threads.
        let agent = ureq::AgentBuilder::new().build();

        let body: CatalogFile = agent
            .get(&format!("http://{addr}/v1/catalog"))
            .call()
            .unwrap()
            .into_json()
            .unwrap();
        assert_eq!(body.apps[0].app_id, "com.orchard.Hello");

        let index: OciIndex = agent
            .get(&format!("http://{addr}/index/x86_64"))
            .call()
            .unwrap()
            .into_json()
            .unwrap();
        assert_eq!(index.manifests.len(), 1);
        assert_eq!(
            index.manifests[0].flatpak_ref(),
            Some("app/com.orchard.Hello/x86_64/stable")
        );

        server.abort();
    }
}
