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

    /// The reviews document to serve. Static files carry none.
    fn reviews(&self) -> orchard_core::ReviewsFile {
        orchard_core::ReviewsFile {
            version: 1,
            reviews: Vec::new(),
            first_seen: Default::default(),
        }
    }

    /// The raw kind 0 metadata per pubkey. Static files carry none.
    fn profiles(&self) -> orchard_core::ProfilesFile {
        orchard_core::ProfilesFile {
            version: 1,
            profiles: std::collections::BTreeMap::new(),
        }
    }

    /// The reviewer contact-list graph. Static files carry none.
    fn graph(&self) -> orchard_core::GraphFile {
        orchard_core::GraphFile {
            version: 1,
            graph: Default::default(),
        }
    }
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
    /// The crawled reviewer contact-list graph, refreshed by the
    /// crawler thread.
    pub graph: Arc<Mutex<GraphStore>>,
}

impl CatalogDocument for NostrCatalog {
    fn document(&self) -> CatalogFile {
        let mut apps = self.feed.lock().expect("feed lock poisoned").catalog().apps;
        for base_app in &self.base.apps {
            if !apps.iter().any(|app| app.app_id == base_app.app_id) {
                apps.push(base_app.clone());
            }
        }
        apps.sort_by(|a, b| {
            a.app_id
                .cmp(&b.app_id)
                .then_with(|| a.publisher.pubkey.cmp(&b.publisher.pubkey))
        });
        CatalogFile { version: 1, apps }
    }

    fn reviews(&self) -> orchard_core::ReviewsFile {
        self.feed.lock().expect("feed lock poisoned").reviews()
    }

    fn profiles(&self) -> orchard_core::ProfilesFile {
        orchard_core::ProfilesFile {
            version: 1,
            profiles: self
                .feed
                .lock()
                .expect("feed lock poisoned")
                .profile_raws(),
        }
    }

    fn graph(&self) -> orchard_core::GraphFile {
        self.graph.lock().expect("graph lock poisoned").file()
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
    /// Social relay URLs for the profile crawler: kind 0 lives on the
    /// social relays, not here. Empty: no profile crawling.
    pub social: Vec<String>,
    /// Where to persist verified events across restarts. Absent: no cache.
    pub cache: Option<std::path::PathBuf>,
}

/// The crawled reviewer graph plus per-reviewer crawl times. The
/// crawler thread refreshes entries on a fixed cadence; the endpoint
/// serves whatever is here.
#[derive(Default)]
pub struct GraphStore {
    graph: std::collections::BTreeMap<String, Vec<String>>,
    crawled: std::collections::BTreeMap<String, std::time::Instant>,
}

/// How often the crawler wakes to look for new reviewers.
const GRAPH_TICK: std::time::Duration = std::time::Duration::from_secs(60);
/// How stale a crawled contact list may get before it is refetched.
const GRAPH_REFRESH: std::time::Duration = std::time::Duration::from_secs(600);
/// The one-shot REQ deadline per relay.
const GRAPH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// How often the profile crawler wakes to look for new keys.
const PROFILE_TICK: std::time::Duration = std::time::Duration::from_secs(60);
/// How stale a crawled profile may get before it is refetched.
const PROFILE_REFRESH: std::time::Duration = std::time::Duration::from_secs(600);
/// The one-shot REQ deadline per relay.
const PROFILE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

impl GraphStore {
    /// Reviewers to crawl now: in the reviewer set and either never
    /// crawled or last crawled longer ago than the refresh cadence.
    fn pending(&self, reviewers: &[String], cadence: std::time::Duration) -> Vec<String> {
        reviewers
            .iter()
            .filter(|pubkey| {
                self.crawled
                    .get(*pubkey)
                    .is_none_or(|at| at.elapsed() >= cadence)
            })
            .cloned()
            .collect()
    }

    /// Absorb one relay's answer for a batch. Requested authors with
    /// no event on the relay stamp anyway: absence is an answer (no
    /// contact list there), so the crawl does not re-ask every tick.
    fn absorb(&mut self, found: Vec<(String, Vec<String>)>, requested: &[String]) {
        let now = std::time::Instant::now();
        for (pubkey, follows) in found {
            self.graph.insert(pubkey.clone(), follows);
            self.crawled.insert(pubkey, now);
        }
        for pubkey in requested {
            self.crawled.entry(pubkey.clone()).or_insert(now);
        }
    }

    /// The served document: a copy of the graph.
    fn file(&self) -> orchard_core::GraphFile {
        orchard_core::GraphFile {
            version: 1,
            graph: self.graph.clone(),
        }
    }
}

/// Crawl the contact lists of known reviewers: the reviewers come
/// from the feed's review authors, the kind 3s from one batched
/// read-only REQ per relay. Blocking loop; one thread.
fn spawn_graph_crawler(
    urls: Vec<String>,
    feed: Arc<Mutex<orchard_nostr::Feed>>,
    graph: Arc<Mutex<GraphStore>>,
) {
    std::thread::Builder::new()
        .name("orchard-graph".into())
        .spawn(move || loop {
            std::thread::sleep(GRAPH_TICK);
            let reviewers: Vec<String> = {
                let feed = feed.lock().expect("feed lock poisoned");
                let mut seen: Vec<String> = Vec::new();
                for review in feed.reviews().reviews {
                    if !seen.contains(&review.pubkey) {
                        seen.push(review.pubkey);
                    }
                }
                seen
            };
            if reviewers.is_empty() {
                continue;
            }
            let pending = graph.lock().expect("graph lock poisoned").pending(&reviewers, GRAPH_REFRESH);
            if pending.is_empty() {
                continue;
            }
            for url in &urls {
                match orchard_nostr::relay::fetch_contact_lists(url, &pending, GRAPH_TIMEOUT) {
                    Ok(found) => {
                        graph.lock().expect("graph lock poisoned").absorb(found, &pending)
                    }
                    Err(err) => eprintln!("graph: {url}: {err:#}"),
                }
            }
        })
        .expect("spawning the graph crawler");
}

/// The profile crawler: kind 0 profiles ride the social relays, not
/// the app relays, so a store fed only from its app relay (the dev
/// loop, or a private relay set) never sees reviewers' names without
/// this. Every tick the crawler asks each social relay, one batched
/// REQ per relay, for the keys the store cares about and absorbs the
/// events through the feed's verify path; the feed keeps the newest
/// per key. Keys are marked before the fetch, so a dead relay backs
/// off to the refresh window instead of retrying every tick.
/// Anonymous and read-only, like every fetch here.
fn spawn_profile_crawler(urls: Vec<String>, feed: Arc<Mutex<orchard_nostr::Feed>>) {
    std::thread::Builder::new()
        .name("orchard-profiles".into())
        .spawn(move || {
            let mut crawled: std::collections::BTreeMap<String, std::time::Instant> =
                std::collections::BTreeMap::new();
            loop {
                let now = std::time::Instant::now();
                let pending: Vec<String> = {
                    let feed = feed.lock().expect("feed lock poisoned");
                    feed.crawl_keys()
                }
                .into_iter()
                .filter(|key| {
                    crawled
                        .get(key)
                        .is_none_or(|at| now.duration_since(*at) >= PROFILE_REFRESH)
                })
                .collect();
                if !pending.is_empty() {
                    for key in &pending {
                        crawled.insert(key.clone(), now);
                    }
                    for url in &urls {
                        match orchard_nostr::relay::fetch_profiles(url, &pending, PROFILE_TIMEOUT) {
                            Ok(events) => {
                                let mut feed = feed.lock().expect("feed lock poisoned");
                                for event in events {
                                    let _ = feed.absorb(event);
                                }
                            }
                            Err(err) => eprintln!("profiles: {url}: {err:#}"),
                        }
                    }
                }
                std::thread::sleep(PROFILE_TICK);
            }
        })
        .expect("spawning the profile crawler");
}

/// Build the axum router for a state.
pub fn router(state: ServerState) -> Router {
    Router::new()
        .route("/", get(root))
        .route("/index/{arch}", get(index))
        .route("/v1/catalog", get(catalog))
        .route("/v1/reviews", get(reviews))
        .route("/v1/profiles", get(profiles))
        .route("/v1/graph", get(graph))
        .with_state(state)
}

async fn root() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "service": "orchard-indexd",
        "description": "Orchard's local catalog service",
        "endpoints": ["/v1/catalog", "/v1/reviews", "/v1/profiles", "/v1/graph", "/index/{arch}"]
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

/// The reviews document, straight from the live feed.
async fn reviews(State(state): State<ServerState>) -> Json<orchard_core::ReviewsFile> {
    Json(state.document.reviews())
}

/// The raw kind 0 metadata per pubkey, straight from the live feed.
async fn profiles(State(state): State<ServerState>) -> Json<orchard_core::ProfilesFile> {
    Json(state.document.profiles())
}

/// The reviewer contact-list graph, straight from the crawler.
async fn graph(State(state): State<ServerState>) -> Json<orchard_core::GraphFile> {
    Json(state.document.graph())
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
        let graph = Arc::new(Mutex::new(GraphStore::default()));
        spawn_graph_crawler(relays.urls.clone(), Arc::clone(&feed), Arc::clone(&graph));
        if !relays.social.is_empty() {
            spawn_profile_crawler(relays.social.clone(), Arc::clone(&feed));
        }
        ServerState {
            document: Arc::new(NostrCatalog {
                base: state.document.document(),
                feed,
                graph,
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

    /// The crawl selection: never-crawled reviewers go first, freshly
    /// crawled ones wait for the cadence, and absence stamps too (a
    /// reviewer with no contact list is not re-asked every tick).
    #[test]
    fn graph_store_pends_and_absorbs() {
        let mut store = GraphStore::default();
        let reviewers = vec!["aaaa".to_string(), "bbbb".to_string()];
        assert_eq!(store.pending(&reviewers, GRAPH_REFRESH).len(), 2);

        // one reviewer answered, the other absent: both stamp
        store.absorb(
            vec![("aaaa".into(), vec!["cccc".into()])],
            &reviewers,
        );
        assert_eq!(store.pending(&reviewers, GRAPH_REFRESH).len(), 0);
        assert_eq!(store.graph["aaaa"], vec!["cccc".to_string()]);
        assert!(!store.graph.contains_key("bbbb"));

        // after the cadence only the backdated entry is pending again;
        // the freshly crawled one waits out its cadence
        store.crawled.insert(
            "aaaa".into(),
            std::time::Instant::now() - std::time::Duration::from_secs(601),
        );
        assert_eq!(store.pending(&reviewers, GRAPH_REFRESH), vec!["aaaa".to_string()]);
        assert_eq!(store.file().graph.len(), 1);
    }

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
                platforms: Vec::new(),
                flatpak_ref: "app/com.orchard.Hello/x86_64/stable".into(),
                location: "oci:/tmp/demo".into(),
                digest: Some("sha256:00".into()),
                metadata: None,
                notes: None,
                published_at: 0,
                successor_to: None,
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

        // a static catalog carries no profiles: the endpoint exists and
        // serves an empty document rather than a 404
        let profiles: orchard_core::ProfilesFile = agent
            .get(&format!("http://{addr}/v1/profiles"))
            .call()
            .unwrap()
            .into_json()
            .unwrap();
        assert!(profiles.profiles.is_empty());

        server.abort();
    }
}
