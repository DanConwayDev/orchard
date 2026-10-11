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

use std::sync::{Arc, Mutex};

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

/// Where the served catalog document comes from. In the MVP this is the
/// static catalog file; the Nostr feed wraps it in [`NostrCatalog`].
pub trait CatalogDocument: Send + Sync + 'static {
    /// The full catalog document to serve.
    fn document(&self) -> CatalogFile;
}

impl CatalogDocument for CatalogFile {
    fn document(&self) -> CatalogFile {
        self.clone()
    }
}

/// The base document plus a live Nostr feed. Feed rows win on app-id
/// collisions: an app a publisher actually signed is the authoritative row,
/// the file entry is the bootstrap fallback.
pub struct NostrCatalog {
    pub base: CatalogFile,
    pub feed: Arc<Mutex<orchard_nostr::Feed>>,
}

impl CatalogDocument for NostrCatalog {
    fn document(&self) -> CatalogFile {
        let mut apps = self.feed.lock().expect("feed lock poisoned").catalog().apps;
        for base_app in &self.base.apps {
            if !apps.iter().any(|app| app.app_id == base_app.app_id) {
                apps.push(base_app.clone());
            }
        }
        apps.sort_by(|a, b| a.app_id.cmp(&b.app_id));
        CatalogFile { version: 1, apps }
    }
}

/// Shared server state: the catalog document (and the OCI index derived
/// from it per request).
#[derive(Clone)]
pub struct ServerState {
    pub document: Arc<dyn CatalogDocument>,
}

/// Relay and cache configuration for [`serve`].
#[derive(Debug, Clone, Default)]
pub struct RelayConfig {
    /// NIP-01 relay URLs to subscribe to, e.g. `ws://127.0.0.1:8008`.
    pub urls: Vec<String>,
    /// Where to persist verified events across restarts. Absent: no cache.
    pub cache: Option<std::path::PathBuf>,
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
    // derive the entries from the live document so relay-published apps
    // show up without restarting
    let needle = format!("/{arch}/");
    let manifests = state
        .document
        .document()
        .apps
        .iter()
        .filter(|app| app.flatpak_ref.contains(&needle))
        .map(|app| {
            IndexEntry::for_ref(
                &app.flatpak_ref,
                app.digest.clone().unwrap_or_else(|| "sha256:unknown".into()),
                0,
            )
        })
        .collect();
    Json(OciIndex {
        schema_version: 2,
        media_type: INDEX_MEDIA_TYPE.to_string(),
        manifests,
    })
}

async fn catalog(State(state): State<ServerState>) -> Json<CatalogFile> {
    Json(state.document.document())
}

/// Bind and serve. The port is reachable from the host even when Orchard runs
/// sandboxed, because flatpak sandboxes share the network namespace by default
/// (asserted in M2).
///
/// With [`RelayConfig`] set, verified NIP-82 events from the given relays are
/// merged over the state's base document and cached to disk.
pub async fn serve(
    addr: std::net::SocketAddr,
    state: ServerState,
    relays: RelayConfig,
) -> anyhow::Result<()> {
    let state = if relays.urls.is_empty() {
        state
    } else {
        let feed = Arc::new(Mutex::new(orchard_nostr::Feed::new()));
        load_cache(&relays.cache, &feed);
        spawn_relay_threads(&relays.urls, Arc::clone(&feed));
        spawn_cache_writer(relays.cache, Arc::clone(&feed));
        ServerState {
            document: Arc::new(NostrCatalog {
                base: state.document.document(),
                feed,
            }),
        }
    };
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, router(state)).await?;
    Ok(())
}

/// Load the event cache and absorb it into the feed. Every event is
/// re-verified on load; corrupt or tampered entries are skipped with a note.
fn load_cache(path: &Option<std::path::PathBuf>, feed: &Arc<Mutex<orchard_nostr::Feed>>) {
    let Some(path) = path else { return };
    let Ok(bytes) = std::fs::read(path) else {
        return;
    };
    let Ok(jsons) = serde_json::from_slice::<Vec<String>>(&bytes) else {
        eprintln!("nostr cache {}: unparseable, starting empty", path.display());
        return;
    };
    let mut feed = feed.lock().expect("feed lock poisoned");
    let mut accepted = 0usize;
    for json in jsons {
        match nostr::event::Event::from_json(json)
            .map_err(|err| anyhow::anyhow!("{err}"))
            .and_then(|event| feed.absorb(event))
        {
            Ok(true) => accepted += 1,
            Ok(false) => {}
            Err(err) => eprintln!("nostr cache: dropping event: {err:#}"),
        }
    }
    eprintln!("nostr cache: {accepted} events loaded");
}

/// One blocking thread per relay; the threads live as long as the process.
fn spawn_relay_threads(urls: &[String], feed: Arc<Mutex<orchard_nostr::Feed>>) {
    for url in urls {
        let relay_url = url.clone();
        let feed = Arc::clone(&feed);
        let spawned = std::thread::Builder::new()
            .name(format!("relay-{url}"))
            .spawn(move || {
                orchard_nostr::relay::run_forever(&relay_url, feed, Arc::new(|| {}))
            });
        if spawned.is_err() {
            eprintln!("failed to spawn relay thread for {url}");
        }
    }
}

/// Periodically write the feed's events to the cache when they changed.
/// Coalesces bursts: relay event storms cost at most one write per tick.
fn spawn_cache_writer(path: Option<std::path::PathBuf>, feed: Arc<Mutex<orchard_nostr::Feed>>) {
    let Some(path) = path else { return };
    if std::thread::Builder::new()
        .name("nostr-cache".into())
        .spawn(move || {
            let mut signatures = Vec::new();
            loop {
                std::thread::sleep(std::time::Duration::from_secs(3));
                let jsons = feed.lock().expect("feed lock poisoned").event_jsons();
                if jsons != signatures {
                    if let Some(parent) = path.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    match serde_json::to_vec(&jsons) {
                        Ok(bytes) => {
                            if std::fs::write(&path, bytes).is_ok() {
                                signatures = jsons;
                            }
                        }
                        Err(_) => {}
                    }
                }
            }
        })
        .is_err()
    {
        eprintln!("failed to spawn nostr cache writer");
    }
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
        let router = router(ServerState {
            document: Arc::new(catalog.clone()),
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
