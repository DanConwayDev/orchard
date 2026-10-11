//! The store view: catalog list, search, full-page app details, install
//! actions.
//!
//! MVP interaction model: type to search, arrow through the list, Enter
//! or click opens the app page. All flatpak work runs on background
//! threads through the single CLI seam (`orchard-flatpak`); the UI polls
//! nothing, results land via `cx.spawn` updates.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::PathBuf;
use std::time::Duration;

use gpui::{
    div, prelude::*, px, uniform_list, AnyElement, AnimationExt, ClickEvent, ClipboardItem,
    Context, FocusHandle, Hsla, KeyDownEvent, MouseButton, MouseDownEvent, ParentElement, Point,
    Render, ScrollHandle, ScrollStrategy, ScrollWheelEvent, SharedString, Styled, StyledText,
    TextLayout, TextRun, UniformListScrollHandle, Window,
};

use orchard_core::{oci, permissions, CatalogFile, Risk, Source, StoreApp};

use crate::ui::{self, app_icon, badge, button};

/// Port the in-process indexd listens on. The client is the remote.
pub const INDEXD_PORT: u16 = 7780;

const FLATHUB_REMOTE: &str = "flathub";

/// Lifecycle of a per-app operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobState {
    Verifying,
    Installing,
    Updating,
    Removing,
}

impl JobState {
    fn label(&self) -> &'static str {
        match self {
            JobState::Verifying => "Verifying digest…",
            JobState::Installing => "Installing…",
            JobState::Updating => "Updating…",
            JobState::Removing => "Removing…",
        }
    }
}

/// Which section of the catalog list is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Filter {
    All,
    Reviewed,
    Publisher,
    Installed,
}

impl Filter {
    fn label(&self) -> &'static str {
        match self {
            Filter::All => "All",
            Filter::Reviewed => "Flathub",
            Filter::Publisher => "Orchard",
            Filter::Installed => "Installed",
        }
    }
}

/// What `flatpak list` reported for an installed app.
#[derive(Debug, Clone)]
/// An installed app, as `flatpak list` reports it.
pub struct InstalledInfo {
    /// Deploy version from `flatpak list`, e.g. `1.2.95.453`.
    pub version: Option<String>,
    /// Human size from `flatpak info`, e.g. `1.0 GB`.
    pub size: Option<String>,
    /// Sandbox metadata ini read from the deploy, for the permissions view.
    pub metadata: Option<String>,
}

pub struct StoreRoot {
    pub(crate) focus_handle: FocusHandle,
    apps: Vec<StoreApp>,
    installed: HashMap<String, InstalledInfo>,
    search: String,
    filter: Filter,
    selected: Option<String>,
    /// The publisher of the row the selection came from, when it came
    /// from a row click: two publishers of one app id are two rows,
    /// and only the clicked twin is the page's subject. Keyboard
    /// selection carries None (the first twin).
    selected_publisher: Option<String>,
    jobs: HashMap<String, JobState>,
    /// The app awaiting an install confirmation: its button reads
    /// "Sure?" until the second press starts the job, or Escape, page
    /// close, or another install dismisses it.
    confirm_install: Option<String>,
    error: Option<String>,
    flathub_loading: bool,
    /// Remote images resolved through the disk cache: url -> path, or
    /// None when the fetch failed.
    fetched: HashMap<String, Option<PathBuf>>,
    /// Fetches in flight, keyed by url.
    fetching: HashSet<String>,
    /// Sandbox metadata read from the remote for apps that are not
    /// installed (reviewed channel), fetched on page open and cached
    /// for the session.
    remote_metadata: HashMap<String, String>,
    /// Sizes of the remote commit for apps that are not installed:
    /// app id -> (download, installed), humanized by flatpak.
    remote_sizes: HashMap<String, (Option<String>, Option<String>)>,
    /// Remote fetches in flight (metadata + sizes), keyed by app id.
    remote_fetching: HashSet<String>,
    /// Publisher-channel page-open artifact checks in flight.
    publisher_fetching: HashSet<String>,
    /// Failed pre-install digest checks: app key -> why. Security
    /// relevant, so it shows on the page, not just in the log.
    verify_errors: HashMap<String, String>,
    /// app id -> verified manifest digest (publisher channel,
    /// post-verify).
    verified: HashMap<String, String>,
    /// Which publisher keys this client has seen per app id: the
    /// continuity record behind the new-key disclosure.
    continuity: orchard_core::ContinuityFile,
    /// Reviews from indexd (kind 30065): app id -> newest first.
    reviews: HashMap<String, Vec<orchard_core::Review>>,
    /// The earliest event per reviewer pubkey, from the reviews
    /// document: the new-key signal for the reviews aggregate.
    first_seen: std::collections::BTreeMap<String, u64>,
    /// A reviews fetch in flight (whole document; small at this scale).
    reviews_fetching: bool,
    /// The signed-in identity's own follows (kind 3 `p` tags): the
    /// personal web of trust. Review rows from these keys carry the
    /// circle glyph; empty when signed out or not yet fetched.
    follows: std::collections::HashSet<String>,
    /// A follows fetch in flight.
    follows_fetching: bool,
    /// The reviewer contact-list graph from indexd: reviewer pubkey
    /// -> the pubkeys their kind 3 follows. Public data; the
    /// distance-2 set is computed from it locally.
    graph: std::collections::BTreeMap<String, Vec<String>>,
    /// A graph fetch in flight.
    graph_fetching: bool,
    /// Every profile indexd holds, parsed once: display names and
    /// picture urls for the account trigger, review rows, and the
    /// publisher line.
    profiles: std::collections::BTreeMap<String, ProfileBits>,
    /// A profiles fetch in flight.
    profiles_fetching: bool,
    /// NIP-05 verdicts per publisher pubkey: the claim that was
    /// checked and whether the domain backed it. Absent means
    /// unchecked, not unverified.
    nip05: std::collections::BTreeMap<String, Nip05Verdict>,
    /// Publisher keys with a NIP-05 check in flight.
    nip05_checking: Vec<String>,
    /// The signed-in identity's own social-relay profile fetch in
    /// flight (one per session; stops once the profile lands).
    own_profile_fetching: bool,
    /// The reviews list is collapsed by default; the header toggles it.
    reviews_expanded: bool,
    /// The signed-in reviewer: a NIP-46 session (the remote signer's
    /// key is the identity). None until the user pairs a signer; the
    /// pairing file survives restarts and restores the session.
    reviewer: Option<Reviewer>,
    /// The sign-in modal is open (a QR pairing, then an optional name).
    signin_modal: bool,
    /// The live pairing session while the QR shows: None once paired
    /// (the session becomes a pending reviewer) or on close.
    connect: Option<ConnectSession>,
    /// The reviewer built from a successful pairing, waiting through
    /// the optional name question.
    pending_reviewer: Option<Reviewer>,
    /// The modal opened from the reviews flow: drop into the composer
    /// after sign-in. The header entry point does not.
    signin_for_review: bool,
    /// The channel dropdown is open, anchored at the click point.
    channel_menu: Option<gpui::Point<gpui::Pixels>>,
    /// The account dropdown is open, anchored at the click point.
    account_menu: Option<gpui::Point<gpui::Pixels>>,
    /// Compose mode: keystrokes go to the review draft, not the search
    /// box or the page's type-to-close. Entered by clicking the compose
    /// box or pressing r on a page; left by Escape or a successful post.
    composing: bool,
    /// The draft review (or, while the sign-in form is open, the name):
    /// (rating 1.0 to 5.0, text). The two forms never show at once, so
    /// one input serves both. Kept across accidental Escapes, cleared on
    /// post and on page switch.
    review_draft: (Option<f32>, String),
    /// Draft text cursor (byte index) and selection anchor, mirroring
    /// the search box fields.
    review_cursor: usize,
    review_anchor: Option<usize>,
    /// Layout handle for the draft text (caret mapping). None until the
    /// first frame with text has been laid out.
    review_layout: Option<TextLayout>,
    /// A review post in flight.
    review_posting: bool,
    /// Review compose/post error, shown next to the compose box.
    review_error: Option<String>,
    /// Detail page open? Selection survives closing, so keyboard
    /// navigation resumes where it left off.
    page_open: bool,
    /// Active browse category (None = all).
    category: Option<String>,
    /// Text cursor position, byte index into `search`.
    cursor: usize,
    /// Selection anchor (byte index); None = plain caret, no selection.
    anchor: Option<usize>,
    /// Layout handle for the search text: maps byte indices to pixels
    /// for the caret and mouse positioning. None until the first
    /// frame's text has been laid out.
    search_layout: Option<TextLayout>,
    /// Scroll handle for the screenshot strip (wheel capture).
    shot_handle: ScrollHandle,
    /// Scroll handle for the detail page (reset on app switch).
    page_scroll: ScrollHandle,
    /// The enlarged screenshot (its url), over a dimmed page.
    lightbox: Option<String>,
    /// Scroll handle for the virtualized list (keyboard reveal).
    list_handle: UniformListScrollHandle,
}

impl StoreRoot {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let mut this = StoreRoot {
            focus_handle: cx.focus_handle(),
            apps: Vec::new(),
            installed: HashMap::new(),
            search: String::new(),
            filter: Filter::All,
            selected: None,
            selected_publisher: None,
            jobs: HashMap::new(),
            confirm_install: None,
            verified: HashMap::new(),
            continuity: load_continuity(),
            error: None,
            flathub_loading: true,
            fetched: HashMap::new(),
            fetching: HashSet::new(),
            remote_metadata: HashMap::new(),
            remote_sizes: HashMap::new(),
            remote_fetching: HashSet::new(),
            publisher_fetching: HashSet::new(),
            verify_errors: HashMap::new(),
            reviews: HashMap::new(),
            first_seen: Default::default(),
            reviews_fetching: false,
            follows: Default::default(),
            follows_fetching: false,
            graph: Default::default(),
            graph_fetching: false,
            profiles: Default::default(),
            profiles_fetching: false,
            nip05: Default::default(),
            nip05_checking: Vec::new(),
            own_profile_fetching: false,
            reviews_expanded: false,
            reviewer: load_reviewer(),
            signin_modal: false,
            connect: None,
            pending_reviewer: None,
            signin_for_review: false,
            channel_menu: None,
            account_menu: None,
            composing: false,
            review_draft: (None, String::new()),
            review_cursor: 0,
            review_anchor: None,
            review_layout: None,
            review_posting: false,
            review_error: None,
            page_open: false,
            category: None,
            cursor: 0,
            anchor: None,
            search_layout: None,
            shot_handle: ScrollHandle::new(),
            page_scroll: ScrollHandle::new(),
            lightbox: None,
            list_handle: UniformListScrollHandle::new(),
        };

        // a saved reviewer signs back in automatically; their profile
        // name resolves in the background when the local record is
        // missing one
        if this.reviewer.is_some() {
            this.resolve_reviewer_name(cx);
        }

        // Orchard channel: read the catalog over HTTP from the in-process
        // indexd (the client is the remote), fall back to the file. Blocking
        // calls on the background pool keep startup snappy.
        cx.spawn(async move |this, cx| {
            let orchard_apps = cx
                .background_executor()
                .spawn(async move { load_orchard_catalog() })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.apply_orchard_apps(orchard_apps, cx);
                // the reviewer graph and the profiles are public data,
                // no session needed
                this.ensure_graph(cx);
                this.ensure_profiles(cx);
                // a session restored from identity.json learns its
                // circle right away, not on the first tick
                if this.reviewer.is_some() {
                    this.ensure_follows(cx);
                    this.ensure_own_profile(cx);
                }
            });
        })
        .detach();

        // Orchard channel refresh: indexd merges relay events as they
        // arrive, so a publisher's new release appears here without a
        // restart. Cheap no-op when the document has not changed.
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(30))
                    .await;
                let orchard_apps = cx
                    .background_executor()
                    .spawn(async move { load_orchard_catalog() })
                    .await;
                let _ = this.update(cx, |this, cx| {
                    this.apply_orchard_apps(orchard_apps, cx);
                    // reviews only move while somebody is looking
                    if this.page_open {
                        this.ensure_reviews(cx);
                        // the graph feeds the same pages' two-hop marks
                        this.ensure_graph(cx);
                    }
                    // the account trigger wears the profile picture
                    this.ensure_profiles(cx);
                    // a restored session picks its circle up here
                    if this.reviewer.is_some() && this.follows.is_empty() {
                        this.ensure_follows(cx);
                    }
                    if this.reviewer.is_some() {
                        this.ensure_own_profile(cx);
                    }
                });
            }
        })
        .detach();

        // Reviewed channel: parse flathub's AppStream in the background.
        cx.spawn(async move |this, cx| {
            let apps = cx
                .background_executor()
                .spawn(async move { load_flathub() })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.apps.extend(apps);
                this.flathub_loading = false;
                cx.notify();
            });
        })
        .detach();

        // Installed state.
        this.refresh_installed(cx);
        this
    }

    // ----- catalog helpers -----

    /// Indices into `apps` matching the search, source filter, and
    /// category, in display order. Returns indices (not clones) so
    /// renders stay cheap.
    fn filtered(&self) -> Vec<usize> {
        let needle = self.search.to_lowercase();
        let mut out = self
            .apps
            .iter()
            .enumerate()
            .filter(|(_, app)| match self.filter {
                Filter::All => true,
                Filter::Reviewed => app.source == Source::Reviewed,
                Filter::Publisher => app.source == Source::Publisher,
                Filter::Installed => self.installed.contains_key(&app.app_id),
            })
            .filter(|(_, app)| match &self.category {
                Some(want) => app.categories.iter().any(|c| c == want),
                None => true,
            })
            .filter(|(_, app)| {
                needle.is_empty()
                    || app.name.to_lowercase().contains(&needle)
                    || app.summary.to_lowercase().contains(&needle)
                    || app.app_id.to_lowercase().contains(&needle)
            })
            .map(|(ix, _)| ix)
            .collect::<Vec<_>>();
        // most recently updated first; the file order is alphabetical
        // by app id, which reads as noise. Undated apps sink, name
        // breaks ties.
        out.sort_by(|&a, &b| {
            let (a, b) = (&self.apps[a], &self.apps[b]);
            let name = |app: &StoreApp| app.name.to_lowercase();
            match (&b.release_date, &a.release_date) {
                (Some(x), Some(y)) => x
                    .cmp(y)
                    .then_with(|| name(a).cmp(&name(b))),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => name(a).cmp(&name(b)),
            }
        });
        out
    }

    fn selected_app(&self) -> Option<&StoreApp> {
        let id = self.selected.as_ref()?;
        self.find_app(id)
    }

    /// The app a selection points at: the id, and when the selection
    /// came from a specific publisher's row, that row's twin only.
    /// None publisher matches either (keyboard selection, reviewed
    /// apps).
    fn find_app(&self, app_id: &str) -> Option<&StoreApp> {
        self.apps.iter().find(|app| {
            app.app_id == app_id
                && self.selected_publisher.as_deref() == app.publisher.as_deref()
        })
    }

    fn refresh_installed(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let installed = cx
                .background_executor()
                .spawn(async move { list_installed() })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.installed = installed;
                cx.notify();
            });
        })
        .detach();
    }

    /// Select an app (keyboard) without opening the page. Images are
    /// fetched when the page opens, not per arrow press.
    fn set_selected(&mut self, app_id: &str, cx: &mut Context<Self>) {
        if self.selected.as_deref() == Some(app_id) {
            return;
        }
        self.selected = Some(app_id.to_string());
        // keyboard selection aims at the first twin; only a row click
        // names its publisher
        self.selected_publisher = None;
        cx.notify();
    }

    /// Open the detail page for an app and queue its remote images
    /// (icon fallback and screenshots) for the disk cache.
    fn open_page(&mut self, app_id: &str, cx: &mut Context<Self>) {
        let is_new = self.selected.as_deref() != Some(app_id);
        self.selected = Some(app_id.to_string());
        if is_new {
            // the strip's horizontal scroll and the page's vertical
            // scroll must not leak between apps, nor a lightbox
            self.shot_handle.set_offset(Point::new(px(0.), px(0.)));
            self.page_scroll.set_offset(Point::new(px(0.), px(0.)));
            self.lightbox = None;
            // a half-written review belongs to the app it was for
            self.composing = false;
            self.reviews_expanded = false;
            self.close_signin();
        }
        if self.page_open {
            return;
        }
        self.page_open = true;
        let urls: Vec<String> = self
            .find_app(app_id)
            .map(|app| {
                let mut urls = Vec::new();
                if app.icon_path.is_none() {
                    if let Some(url) = &app.icon_url {
                        urls.push(url.clone());
                    }
                }
                urls.extend(app.screenshots.iter().take(4).cloned());
                urls
            })
            .unwrap_or_default();
        // failed fetches get one retry per page open
        for url in &urls {
            if self.fetched.get(url) == Some(&None) {
                self.fetched.remove(url);
            }
        }
        self.ensure_fetched(urls, cx);
        // pre-install data: for reviewed apps the remote serves the
        // manifest and sizes of the commit that would install; for
        // publisher apps the artifact itself is read (digest check plus
        // sandbox labels in one manifest fetch)
        let source = self.find_app(app_id).map(|a| a.source);
        let not_installed = !self.installed.contains_key(app_id);
        match source {
            Some(Source::Reviewed) if not_installed => self.ensure_remote_data(app_id, cx),
            Some(Source::Publisher) if not_installed => self.ensure_publisher_data(app_id, cx),
            _ => {}
        }
        self.ensure_reviews(cx);
        cx.notify();
    }

    /// Fetch an app's remote commit summary and sandbox manifest
    /// (once per session; failures retry on the next page open).
    fn ensure_remote_data(&mut self, app_id: &str, cx: &mut Context<Self>) {
        let key = app_key(app_id, self.selected_publisher.as_deref());
        if self.remote_metadata.contains_key(&key) && self.remote_sizes.contains_key(&key)
            || !self.remote_fetching.insert(key.clone())
        {
            return;
        }
        let app_id = app_id.to_string();
        let remote = FLATHUB_REMOTE.to_string();
        cx.spawn(async move |this, cx| {
            let id = app_id.clone();
            let remote_meta = remote.clone();
            let metadata = cx
                .background_executor()
                .spawn(async move {
                    orchard_flatpak::remote_metadata(&remote_meta, &id)
                        .output()
                        .ok()
                        .filter(|out| out.status.success())
                        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
                        .filter(|text| !text.is_empty())
                })
                .await;
            let id = app_id.clone();
            let sizes = cx
                .background_executor()
                .spawn(async move {
                    orchard_flatpak::remote_info(&remote, &id)
                        .output()
                        .ok()
                        .filter(|out| out.status.success())
                        .map(|out| {
                            orchard_flatpak::parse_remote_info(&String::from_utf8_lossy(
                                &out.stdout,
                            ))
                        })
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if let Some(metadata) = metadata {
                    this.remote_metadata.insert(key.clone(), metadata);
                }
                if let Some(sizes) = sizes {
                    this.remote_sizes.insert(key.clone(), sizes);
                }
                this.remote_fetching.remove(&key);
                cx.notify();
            });
        })
        .detach();
    }

    /// Replace the publisher-channel rows with a fresh read of the
    /// merged catalog (indexd). A no-op when nothing changed, so the
    /// periodic refresh never disturbs list state. Sandbox metadata
    /// read earlier in the session (from the artifact) is transplanted:
    /// that fetch already happened and the pin has not moved.
    fn apply_orchard_apps(&mut self, incoming: Vec<StoreApp>, cx: &mut Context<Self>) {
        let before = orchard_fingerprint(
            &self
                .apps
                .iter()
                .filter(|app| app.source == Source::Publisher)
                .cloned()
                .collect::<Vec<_>>(),
        );
        let after = orchard_fingerprint(&incoming);
        if before == after {
            return;
        }
        let old_rows: Vec<StoreApp> = self
            .apps
            .extract_if(.., |app: &mut StoreApp| app.source == Source::Publisher)
            .collect();
        let mut old_metadata: HashMap<String, Option<String>> = old_rows
            .into_iter()
            .map(|app| (app.app_id.clone(), app.metadata))
            .collect();
        let mut incoming = incoming;
        for app in &mut incoming {
            if app.metadata.is_none() {
                if let Some(Some(metadata)) = old_metadata.get(&app.app_id) {
                    app.metadata = Some(metadata.clone());
                }
            }
            old_metadata.remove(&app.app_id);
        }
        // an app that vanished (unpublished, key revoked) closes the
        // page; so does the selected publisher vanishing while another
        // keeps the same app id alive
        // publisher-key continuity: fold the fresh rows in (they are
        // the whole publisher catalog); a key new for a known app id
        // is the change the page will disclose
        let batch: Vec<(String, String)> = incoming
            .iter()
            .filter_map(|app| {
                app.publisher
                    .as_ref()
                    .map(|pubkey| (app.app_id.clone(), pubkey.clone()))
            })
            .collect();
        self.apps.extend(incoming);
        if !batch.is_empty() && absorb_continuity(&mut self.continuity, &batch) {
            save_continuity(&self.continuity);
        }
        if self.page_open && self.selected_app().is_none() {
            self.selected = None;
            self.selected_publisher = None;
            self.page_open = false;
        }
        self.ensure_nip05_checks(cx);
        cx.notify();
    }

    /// The key's verified NIP-05 claim: the domain backed it AND the
    /// profile still carries the same claim (a changed claim reads as
    /// unverified until rechecked).
    fn verified_nip05(&self, pubkey: &str) -> Option<String> {
        let verdict = self.nip05.get(pubkey)?;
        if !verdict.verified {
            return None;
        }
        match self
            .profiles
            .get(pubkey)
            .and_then(|p| p.nip05.as_deref())
        {
            Some(claim) if claim == verdict.claim => Some(verdict.claim.clone()),
            _ => None,
        }
    }

    /// Kick a NIP-05 check for every publisher whose profile claims an
    /// address and whose claim has no verdict yet: the domain behind
    /// the claim is the only party that can confirm it, so the client
    /// asks, once per claim per session, and renders the address only
    /// when the answer is yes. A claim on its own shows nothing. The
    /// signed-in identity's own claim joins the scan (the account
    /// menu wears the same check), and so do comment authors: the
    /// review rows play by the same rule.
    fn ensure_nip05_checks(&mut self, cx: &mut Context<Self>) {
        let mut candidates: std::collections::BTreeSet<String> = self
            .apps
            .iter()
            .filter(|app| app.source == Source::Publisher)
            .filter_map(|app| app.publisher.clone())
            // the signed-in identity's own claim: the account menu
            .chain(self.reviewer.as_ref().map(|r| r.signer_pubkey.clone()))
            .collect();
        // comment authors too: their rows get the same treatment
        for review in self.reviews.values().flatten() {
            candidates.insert(review.pubkey.clone());
        }
        let pending: Vec<(String, String)> = candidates
            .into_iter()
            .filter(|pubkey| {
                !self.nip05.contains_key(pubkey) && !self.nip05_checking.contains(pubkey)
            })
            .filter_map(|pubkey| {
                let claim = self.profiles.get(&pubkey)?.nip05.clone()?;
                Some((pubkey, claim))
            })
            .collect();
        for (pubkey, claim) in pending {
            self.nip05_checking.push(pubkey.clone());
            cx.spawn(async move |this, cx| {
                let check_key = pubkey.clone();
                let check_claim = claim.clone();
                let verified = cx
                    .background_executor()
                    .spawn(async move { verify_nip05(&check_claim, &check_key) })
                    .await;
                let _ = this.update(cx, |this, cx| {
                    this.nip05_checking.retain(|key| key != &pubkey);
                    this.nip05.insert(pubkey, Nip05Verdict { claim, verified });
                    cx.notify();
                });
            })
            .detach();
        }
    }

    /// Pre-install artifact check for publisher apps: one manifest fetch
    /// verifies the pinned digest and reads the sandbox labels, so the
    /// page shows exactly what install will pin. Same fetch the install
    /// button runs; failures surface on the page and retry on the next
    /// page open.
    fn ensure_publisher_data(&mut self, app_id: &str, cx: &mut Context<Self>) {
        let key = app_key(app_id, self.selected_publisher.as_deref());
        if self.verified.contains_key(&key)
            || self.verify_errors.contains_key(&key)
            || !self.publisher_fetching.insert(key.clone())
        {
            return;
        }
        let Some(app) = self.find_app(app_id).cloned() else {
            self.publisher_fetching.remove(&key);
            return;
        };
        let publisher = app.publisher.clone();
        let location = match app
            .location
            .as_deref()
            .and_then(|raw| oci::Location::parse(raw).ok())
        {
            Some(location) => location,
            None => {
                self.publisher_fetching.remove(&key);
                return;
            }
        };
        let pinned = app.pinned_digest.clone();
        let app_id = app_id.to_string();
        cx.spawn(async move |this, cx| {
            let report = cx
                .background_executor()
                .spawn(async move { oci::verify(&location, pinned.as_deref()) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.publisher_fetching.remove(&key);
                match report {
                    Ok(report) => {
                        this.verified.insert(key.clone(), report.digest.clone());
                        if let Some(app) = this.apps.iter_mut().find(|a| {
                            a.app_id == app_id && a.publisher == publisher
                        }) {
                            if app.metadata.is_none() {
                                app.metadata = report.metadata.clone();
                            }
                            this.remote_sizes.insert(
                                key.clone(),
                                (
                                    Some(human_size(report.download_size)),
                                    report.installed_size.map(human_size),
                                ),
                            );
                        }
                    }
                    Err(err) => {
                        // the pin did not match or the artifact is
                        // unreadable: show it, and keep retrying on the
                        // next page open
                        this.verify_errors
                            .insert(key.clone(), format!("{err:#}"));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    // ----- reviews (kind 30065) -----

    /// Fetch every profile indexd holds: display names and picture
    /// urls for the account trigger, review rows, and the publisher
    /// line. Public data; parsed once on arrival.
    fn ensure_profiles(&mut self, cx: &mut Context<Self>) {
        if self.profiles_fetching {
            return;
        }
        self.profiles_fetching = true;
        cx.spawn(async move |this, cx| {
            let fetched = cx
                .background_executor()
                .spawn(async move { load_profiles() })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.profiles_fetching = false;
                match fetched {
                    Ok(profiles) => {
                        // merge, not replace: the signed-in identity's
                        // own profile (fetched from the social relays,
                        // which indexd need not share) must survive
                        for (pubkey, bits) in profiles {
                            this.profiles.insert(pubkey, bits);
                        }
                        this.ensure_nip05_checks(cx);
                        cx.notify();
                    }
                    Err(err) => {
                        // keep the old profiles; the next tick retries
                        eprintln!("profiles fetch failed: {err:#}");
                    }
                }
            });
        })
        .detach();
    }

    /// The signed-in identity's own profile, fetched read-only and
    /// anonymously from the social relays: indexd only holds profiles
    /// its catalog relays carried, and the dev relay never sees the
    /// user's kind 0. Lands in the same profiles map the account
    /// trigger and "Signed in as" read.
    fn ensure_own_profile(&mut self, cx: &mut Context<Self>) {
        let Some(reviewer) = self.reviewer.as_ref() else {
            return;
        };
        let pubkey = reviewer.signer_pubkey.clone();
        if self.own_profile_fetching || self.profiles.contains_key(&pubkey) {
            return;
        }
        self.own_profile_fetching = true;
        let key = pubkey.clone();
        cx.spawn(async move |this, cx| {
            let bits = cx
                .background_executor()
                .spawn(async move { load_own_profile(&key) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.own_profile_fetching = false;
                if let Some(bits) = bits {
                    // the name shows up in "Signed in as" even when
                    // indexd's feed never carried the profile
                    if let Some(reviewer) = this.reviewer.as_mut() {
                        if reviewer.signer_pubkey == pubkey && reviewer.name.is_none() {
                            reviewer.name = bits.name.clone();
                        }
                    }
                    this.profiles.insert(pubkey, bits);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Fetch the reviewer contact-list graph from indexd (startup and
    /// the refresh tick; the crawler over there refreshes on its own
    /// cadence). Page-independent: any open reviews section renders
    /// from it.
    fn ensure_graph(&mut self, cx: &mut Context<Self>) {
        if self.graph_fetching {
            return;
        }
        self.graph_fetching = true;
        cx.spawn(async move |this, cx| {
            let fetched = cx
                .background_executor()
                .spawn(async move { load_graph() })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.graph_fetching = false;
                match fetched {
                    Ok(graph) => {
                        this.graph = graph;
                        cx.notify();
                    }
                    Err(err) => {
                        // keep the old graph; the next tick retries
                        eprintln!("graph fetch failed: {err:#}");
                    }
                }
            });
        })
        .detach();
    }

    /// Fetch the reviews document from indexd (once per page open; the
    /// refresh loop repeats the call while a page is open). The whole
    /// document is small at this scale; per-app filtering happens here.
    fn ensure_reviews(&mut self, cx: &mut Context<Self>) {
        if self.reviews_fetching {
            return;
        }
        self.reviews_fetching = true;
        cx.spawn(async move |this, cx| {
            let fetched = cx
                .background_executor()
                .spawn(async move { load_reviews() })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.reviews_fetching = false;
                match fetched {
                    Ok((grouped, first_seen)) => {
                        this.reviews = grouped;
                        this.first_seen = first_seen;
                        // fresh comment authors may carry claims; the
                        // scan covers them on arrival, not next tick
                        this.ensure_nip05_checks(cx);
                        cx.notify();
                    }
                    Err(err) => {
                        // keep the old reviews; the next page open or
                        // refresh tick retries
                        eprintln!("reviews fetch failed: {err:#}");
                    }
                }
            });
        })
        .detach();
    }

    /// Fetch the signed-in identity's contact list from the social
    /// relays: one kind 3 per relay, read-only and anonymous. Guarded
    /// like the other fetches. An empty union keeps the old set: a
    /// relay outage must not erase a known circle. Kind 3 edits mid
    /// session are picked up on the next launch.
    fn ensure_follows(&mut self, cx: &mut Context<Self>) {
        let Some(reviewer) = self.reviewer.as_ref() else {
            return;
        };
        if self.follows_fetching {
            return;
        }
        self.follows_fetching = true;
        let pubkey = reviewer.signer_pubkey.clone();
        cx.spawn(async move |this, cx| {
            let fetched = cx
                .background_executor()
                .spawn(async move { load_follows(&pubkey) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.follows_fetching = false;
                if !fetched.is_empty() {
                    this.follows = fetched;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Post the draft review for the open page: sign with the local
    /// identity, send to the relays, refresh the section.
    fn post_review(&mut self, app_id: &str, cx: &mut Context<Self>) {
        if self.review_posting {
            return;
        }
        if self.reviewer.is_none() {
            self.review_error = Some("Sign in to post.".into());
            cx.notify();
            return;
        };
        let Some(rating) = self.review_draft.0 else {
            self.review_error = Some("Pick a rating first (keys 1 to 5).".into());
            cx.notify();
            return;
        };
        let reviewer = self.reviewer.clone().unwrap();
        let content = self.review_draft.1.trim().to_string();
        self.review_posting = true;
        self.review_error = None;
        let app_id = app_id.to_string();
        cx.spawn(async move |this, cx| {
            let posted = cx
                .background_executor()
                .spawn(async move { post_review(&reviewer, &app_id, rating, content) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.review_posting = false;
                match posted {
                    Ok(()) => {
                        this.review_draft = (None, String::new());
                        this.review_cursor = 0;
                        this.review_anchor = None;
                        this.review_layout = None;
                        this.composing = false;
                        this.ensure_reviews(cx);
                    }
                    Err(err) => {
                        this.review_error = Some(format!("{err:#}"));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Enter the review flow from the page: compose when signed in,
    /// the sign-in modal when not. Both expand the section.
    fn start_review(&mut self, cx: &mut Context<Self>) {
        self.reviews_expanded = true;
        self.review_error = None;
        if self.reviewer.is_some() {
            self.composing = true;
        } else {
            self.open_signin_modal(true, cx);
        }
        cx.notify();
    }

    /// Open the sign-in modal and start a fresh pairing session: an
    /// ephemeral client key, the pairing URI with its anti-spoofing
    /// secret, and the QR image on disk. The connect relay is the
    /// first review relay; it has to be one the signer app can reach.
    fn open_signin_modal(&mut self, for_review: bool, cx: &mut Context<Self>) {
        self.close_signin();
        self.signin_modal = true;
        self.signin_for_review = for_review;
        self.start_pairing(cx);
    }

    /// Build one pairing session and start its poll loop. Runs again
    /// for "new pairing code".
    fn start_pairing(&mut self, cx: &mut Context<Self>) {
        use nostr::key::GetPublicKey as _;
        use nostr::prelude::*;
        self.review_error = None;
        let relay = connect_relay();
        let keys = Keys::generate();
        // the anti-spoofing secret: the signer echoes it back in the
        // connect response, a pairing answer without it is noise. A
        // fresh key's secret hex is 64 random characters, enough for
        // an echo token.
        let secret = Keys::generate().secret_key().to_secret_hex();
        let client_pubkey = keys.get_public_key().expect("generated key has a pubkey");
        let uri = match orchard_nostr::connect::connect_uri(
            &client_pubkey,
            &relay,
            "Orchard",
            "https://github.com/Letdown2491/orchard",
            // the whole request surface: reviews and profile metadata,
            // nothing else
            "sign_event:30065,sign_event:0",
            &secret,
        ) {
            Ok(uri) => uri,
            Err(err) => {
                self.review_error = Some(format!("pairing URI failed: {err:#}"));
                return;
            }
        };
        let qr_path = match crate::qr::write_qr_png(&uri, &qr_cache_dir()) {
            Ok(path) => path,
            Err(err) => {
                self.review_error = Some(format!("QR image failed: {err:#}"));
                return;
            }
        };
        self.connect = Some(ConnectSession {
            keys: keys.clone(),
            qr_path,
            uri,
            relay: relay.clone(),
        });
        cx.notify();

        // the poll loop: while the modal shows the QR, listen for the
        // signer's connect response; pairing completes from here
        cx.spawn(async move |this, cx| loop {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(1200))
                .await;
            let waiting = this
                .update(cx, |this, _| {
                    this.signin_modal && this.connect.is_some() && this.pending_reviewer.is_none()
                })
                .unwrap_or(false);
            if !waiting {
                break;
            }
            let answer = cx
                .background_executor()
                .spawn({
                    let relay = relay.clone();
                    let keys = keys.clone();
                    let secret = secret.clone();
                    async move {
                        orchard_nostr::connect::poll_connect(
                            &relay,
                            &keys,
                            &secret,
                            std::time::Duration::from_millis(1500),
                        )
                    }
                })
                .await;
            match answer {
                Ok(Some((signer_pubkey, secret))) => {
                    let _ = this.update(cx, |this, cx| {
                        this.pairing_connected(signer_pubkey, secret, cx);
                    });
                    break;
                }
                // nothing yet: poll again; relay hiccups retry the
                // same way until the modal closes
                _ => {}
            }
        })
        .detach();
    }

    /// A signer answered the pairing: keep the session as a pending
    /// reviewer and ask the optional name question.
    fn pairing_connected(&mut self, signer_pubkey: String, secret: String, cx: &mut Context<Self>) {
        let Some(session) = self.connect.take() else {
            return;
        };
        self.pending_reviewer = Some(Reviewer {
            keys: session.keys,
            relay: session.relay,
            signer_pubkey,
            secret,
            name: None,
        });
        self.review_draft = (None, String::new());
        self.review_cursor = 0;
        self.review_anchor = None;
        self.review_layout = None;
        self.review_error = None;
        cx.notify();
    }

    /// Finish sign-in from the name question: persist the pairing
    /// (plus the optional published name), close the modal, drop into
    /// compose when it opened from the reviews flow.
    fn finish_sign_in(&mut self, cx: &mut Context<Self>) {
        let Some(pending) = self.pending_reviewer.take() else {
            self.close_signin();
            cx.notify();
            return;
        };
        let name = self.review_draft.1.trim().to_string();
        let name = (!name.is_empty()).then_some(name);
        let from_reviews = self.signin_for_review;
        let mut reviewer = pending;
        reviewer.name = name.clone();
        cx.spawn(async move |this, cx| {
            let done = cx
                .background_executor()
                .spawn(async move {
                    let publish = (|| -> anyhow::Result<()> {
                        save_reviewer(&reviewer)?;
                        if let Some(published) = &name {
                            // proper profile management: merge against
                            // the author's latest kind 0 so every field
                            // they ever published survives; when indexd
                            // cannot answer, publish nothing rather
                            // than guess
                            match fetch_profile_raws() {
                                Ok(raws) => {
                                    let merged = merge_profile_raw(
                                        raws.get(&reviewer.signer_pubkey).map(String::as_str),
                                        published,
                                    );
                                    if let Err(err) = send_signed(
                                        &reviewer,
                                        orchard_nostr::KIND_PROFILE as u16,
                                        Vec::new(),
                                        &merged,
                                    ) {
                                        return Err(anyhow::anyhow!(
                                            "profile publish failed: {err:#}"
                                        ));
                                    }
                                }
                                Err(err) => {
                                    return Err(anyhow::anyhow!(
                                        "could not read your existing profile ({err:#}); \
                                         the name is kept on this device only"
                                    ));
                                }
                            }
                        }
                        Ok(())
                    })();
                    (reviewer, publish)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                let (reviewer, publish) = done;
                match publish {
                    Ok(()) => {
                        let named_locally = reviewer.name.is_some();
                        this.reviewer = Some(reviewer);
                        this.signin_modal = false;
                        this.signin_for_review = false;
                        this.composing = from_reviews;
                        this.review_draft = (None, String::new());
                        this.review_cursor = 0;
                        this.review_anchor = None;
                        this.review_layout = None;
                        this.review_error = None;
                        if !named_locally {
                            // an established key: their name comes from
                            // the profile they already published
                            this.resolve_reviewer_name(cx);
                        }
                        // past reviews by this key pick up the new
                        // name once the profile propagates
                        this.ensure_reviews(cx);
                        this.ensure_follows(cx);
                        this.ensure_own_profile(cx);
                    }
                    // keep the pairing: the name question stays up with
                    // the error under it, so the user can retry
                    Err(err) => {
                        this.pending_reviewer = Some(reviewer);
                        this.review_error = Some(format!("{err:#}"));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Fill the signed-in reviewer's display name from their kind 0
    /// profile when it was not chosen locally. Runs in the background;
    /// a missing or unreachable profile leaves the pubkey fragment.
    fn resolve_reviewer_name(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let raws = cx
                .background_executor()
                .spawn(async move { fetch_profile_raws() })
                .await;
            let _ = this.update(cx, |this, cx| {
                if let Ok(raws) = raws {
                    if let Some(reviewer) = this.reviewer.as_mut() {
                        if reviewer.name.is_none() {
                            if let Some(raw) = raws.get(&reviewer.signer_pubkey) {
                                reviewer.name = ProfileBits::parse(raw).name;
                            }
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// End the session: the pairing stays in identity.json on this
    /// device and restores the session on the next launch. The secret
    /// key itself never lives here at all.
    fn sign_out(&mut self, cx: &mut Context<Self>) {
        self.reviewer = None;
        self.composing = false;
        cx.notify();
    }

    /// Close the sign-in modal, dropping the pairing session and any
    /// half-paired signer. The reviews draft resets with it: the
    /// shared input belongs to whatever form opens next.
    fn close_signin(&mut self) {
        self.signin_modal = false;
        self.signin_for_review = false;
        self.connect = None;
        self.pending_reviewer = None;
        self.review_draft = (None, String::new());
        self.review_cursor = 0;
        self.review_anchor = None;
        self.review_layout = None;
        self.review_error = None;
    }

    /// Close the detail page. Selection survives so keyboard
    /// navigation resumes where it left off.
    fn close_page(&mut self, cx: &mut Context<Self>) {
        self.page_open = false;
        self.lightbox = None;
        self.composing = false;
        self.reviews_expanded = false;
        self.confirm_install = None;
        self.close_signin();
        cx.notify();
    }

    /// Start a background fetch for each not-yet-cached url. Results
    /// land in `fetched` and notify.
    fn ensure_fetched(&mut self, urls: Vec<String>, cx: &mut Context<Self>) {
        let mut todo = Vec::new();
        for url in urls {
            if self.fetched.contains_key(&url) || self.fetching.contains(&url) {
                continue;
            }
            self.fetching.insert(url.clone());
            todo.push(url);
        }
        if todo.is_empty() {
            return;
        }
        cx.spawn(async move |this, cx| {
            for url in todo {
                let target = url.clone();
                let path = cx
                    .background_executor()
                    .spawn(async move { crate::cache::fetch(&target) })
                    .await;
                let _ = this.update(cx, |this, cx| {
                    this.fetching.remove(&url);
                    this.fetched.insert(url, path);
                    cx.notify();
                });
            }
        })
        .detach();
    }

    // ----- actions -----

    fn install(&mut self, app_id: &str, cx: &mut Context<Self>) {
        self.confirm_install = None;
        // the selection's publisher names the twin: two publishers of
        // one app id install from different artifacts
        let Some(app) = self
            .apps
            .iter()
            .find(|a| {
                a.app_id == app_id
                    && a.publisher.as_deref() == self.selected_publisher.as_deref()
            })
            .cloned()
        else {
            return;
        };
        if self.jobs.contains_key(app_id) {
            return;
        }
        match app.source {
            Source::Reviewed => {
                let cmd = orchard_flatpak::install_ref(&app.app_id, FLATHUB_REMOTE);
                self.jobs
                    .insert(app.app_id.clone(), JobState::Installing);
                self.run_command(cmd, &app.app_id, "Install", cx);
            }
            Source::Publisher => {
                self.verify_then_install(app, cx);
            }
        }
        cx.notify();
    }

    /// Update an installed app from its origin remote. Detection is
    /// version-based; the update itself lets flatpak resolve the
    /// origin's current commit.
    fn update(&mut self, app_id: &str, cx: &mut Context<Self>) {
        if self.jobs.contains_key(app_id) {
            return;
        }
        self.jobs
            .insert(app_id.to_string(), JobState::Updating);
        let cmd = orchard_flatpak::update_ref(app_id);
        self.run_command(cmd, app_id, "Update", cx);
        cx.notify();
    }

    /// Publisher channel: verify the registry manifest against the pinned
    /// digest, then hand flatpak the image location. The install refuses to
    /// start on digest mismatch.
    fn verify_then_install(&mut self, app: StoreApp, cx: &mut Context<Self>) {
        let Some(location_text) = app.location.clone() else {
            return;
        };
        let location = match oci::Location::parse(&location_text) {
            Ok(location) => location,
            Err(err) => {
                self.error = Some(format!("bad artifact location: {err:#}"));
                return;
            }
        };
        let install_arg = location.install_arg();
        self.jobs
            .insert(app.app_id.clone(), JobState::Verifying);
        cx.notify();

        cx.spawn(async move |this, cx| {
            let pinned = app.pinned_digest.clone();
            let mut app = app;
            let report = cx
                .background_executor()
                .spawn(async move { oci::verify(&location, pinned.as_deref()) })
                .await;
            let verified = report.map(|report| {
                (
                    report.flatpak_ref.clone(),
                    report.digest.clone(),
                    report.metadata.clone(),
                )
            });
            let _ = this.update(cx, |this, cx| {
                match verified {
                    Ok((flatpak_ref, digest, metadata)) => {
                        this.verified
                            .insert(app_key(&app.app_id, app.publisher.as_deref()), digest.clone());
                        // Prefer the image's own metadata for the viewer.
                        if app.metadata.is_none() {
                            app.metadata = metadata;
                        }
                        // The catalog ref must match what the image labels
                        // claim; flatpak enforces this again at pull time.
                        let _ = flatpak_ref;
                        let cmd = orchard_flatpak::install_image(&install_arg);
                        this.jobs
                            .insert(app.app_id.clone(), JobState::Installing);
                        this.run_command_detached(cmd, &app.app_id, cx);
                    }
                    Err(err) => {
                        this.jobs.remove(&app.app_id);
                        this.error =
                            Some(format!("verification failed for {}: {err:#}", app.name));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn uninstall(&mut self, app_id: &str, cx: &mut Context<Self>) {
        if self.jobs.contains_key(app_id) {
            return;
        }
        let cmd = orchard_flatpak::uninstall(app_id);
        self.jobs
            .insert(app_id.to_string(), JobState::Removing);
        self.run_command(cmd, app_id, "Remove", cx);
    }

    fn open(&mut self, app_id: &str, cx: &mut Context<Self>) {
        if orchard_flatpak::run(app_id).spawn().is_err() {
            self.error = Some(format!("failed to launch {app_id}"));
            cx.notify();
        }
    }

    /// Run a flatpak command in the background, then refresh installed state.
    fn run_command(
        &mut self,
        cmd: std::process::Command,
        app_id: &str,
        label: &'static str,
        cx: &mut Context<Self>,
    ) {
        let app_id = app_id.to_string();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let mut cmd = cmd;
                    cmd.output().map(|output| {
                        (
                            output.status.success(),
                            String::from_utf8_lossy(&output.stderr).trim().to_string(),
                        )
                    })
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok((true, _)) => {
                        this.error = None;
                    }
                    Ok((false, stderr)) => {
                        this.error =
                            Some(format!("{label} failed: {stderr}"));
                    }
                    Err(err) => {
                        this.error = Some(format!("{label}: {err}"));
                    }
                }
                this.jobs.remove(&app_id);
                this.refresh_installed(cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// Like [run_command] but for commands spawned from inside another async
    /// update (verification chain).
    fn run_command_detached(
        &mut self,
        cmd: std::process::Command,
        app_id: &str,
        cx: &mut Context<Self>,
    ) {
        let app_id = app_id.to_string();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let mut cmd = cmd;
                    cmd.output().map(|output| {
                        (
                            output.status.success(),
                            String::from_utf8_lossy(&output.stderr).trim().to_string(),
                        )
                    })
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok((true, _)) => this.error = None,
                    Ok((false, stderr)) => {
                        this.error = Some(format!("Install failed: {stderr}"))
                    }
                    Err(err) => this.error = Some(format!("Install: {err}")),
                }
                this.jobs.remove(&app_id);
                this.refresh_installed(cx);
                cx.notify();
            });
        })
        .detach();
    }

    // ----- keyboard -----

    /// The character a keystroke produces: gpui reports the key
    /// label printed on the key (unshifted, so shift-a's label is
    /// "a"), while key_char carries the character the layout and
    /// modifiers actually produce. Text insertion wants the latter;
    /// the label is only the fallback for keys that make none.
    fn produced_char(keystroke: &gpui::Keystroke) -> String {
        keystroke
            .key_char
            .as_ref()
            .filter(|char| char.chars().count() == 1)
            .cloned()
            .unwrap_or_else(|| keystroke.key.clone())
    }

    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        let key = keystroke.key.as_str();
        let shift = keystroke.modifiers.shift;
        let ctrl = keystroke.modifiers.control || keystroke.modifiers.platform;

        // ----- dropdown menus: an open menu swallows everything but
        // Escape; a click away closes it through the overlay -----
        if self.channel_menu.is_some() || self.account_menu.is_some() {
            if key == "escape" {
                self.channel_menu = None;
                self.account_menu = None;
                cx.notify();
            }
            return;
        }

        // ----- sign-in modal: QR phase swallows keys (Escape closes);
        // the name question takes the shared input like the composer -----
        if self.signin_modal {
            if self.pending_reviewer.is_none() {
                if key == "escape" {
                    self.close_signin();
                    cx.notify();
                }
                return;
            }
            match key {
                "escape" => {
                    self.close_signin();
                }
                "enter" => self.finish_sign_in(cx),
                "left" | "right" | "home" | "end" | "backspace" | "delete" => {
                    self.move_draft_cursor(key, shift)
                }
                "a" if ctrl => {
                    self.review_anchor = Some(0);
                    self.review_cursor = self.review_draft.1.len();
                }
                "v" if ctrl => {
                    if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                        let pasted: String = text.split_whitespace().collect();
                        self.insert_draft_text(&pasted);
                    }
                }
                "space" => self.insert_draft_text(" "),
                other if other.chars().count() == 1 && !ctrl => {
                    self.insert_draft_text(&Self::produced_char(keystroke));
                }
                _ => {}
            }
            cx.notify();
            return;
        }

        // ----- review compose: keystrokes go to the draft, not the
        // search box, and typing does not close the page -----
        if self.composing {
            match key {
                "escape" => {
                    // compose first, page second: a stray Escape does
                    // not throw the reader out of the app
                    self.composing = false;
                }"enter" => {
                    if !self.review_posting {
                        let app_id = self.selected.clone().unwrap_or_default();
                        self.post_review(&app_id, cx);
                    }
                }
                // digits 1 to 5 set the star rating, discoverable and
                // one keypress each; shifted they are text (!, @)
                "1" | "2" | "3" | "4" | "5" if !shift => {
                    self.review_draft.0 = Some(key.parse::<u8>().unwrap() as f32);
                    self.review_error = None;
                }
                "left" | "right" | "home" | "end" | "backspace" | "delete" => {
                    self.move_draft_cursor(key, shift);
                }
                "space" => self.insert_draft_text(" "),
                other if other.chars().count() == 1 && !ctrl => {
                    self.insert_draft_text(&Self::produced_char(keystroke));
                }
                _ => {}
            }
            cx.notify();
            return;
        }

        // r starts a review from the page, before the type-to-close
        // rule eats the keystroke
        if self.page_open && key == "r" && !ctrl {
            self.start_review(cx);
            cx.notify();
            return;
        }

        match key {
            // ----- search editing: the input is always live -----
            "left" => self.move_cursor(false, shift),
            "right" => self.move_cursor(true, shift),
            "home" => {
                self.cursor = 0;
                if !shift {
                    self.anchor = None;
                }
            }
            "end" => {
                self.cursor = self.search.len();
                if !shift {
                    self.anchor = None;
                }
            }
            "a" if ctrl => {
                self.anchor = Some(0);
                self.cursor = self.search.len();
            }
            "c" if ctrl => {
                let (start, end) = self.selection();
                if start != end {
                    cx.write_to_clipboard(ClipboardItem::new_string(
                        self.search[start..end].to_string(),
                    ));
                }
            }
            "x" if ctrl => {
                let (start, end) = self.selection();
                if start != end {
                    cx.write_to_clipboard(ClipboardItem::new_string(
                        self.search[start..end].to_string(),
                    ));
                    self.delete_text(false);
                }
            }
            "v" if ctrl => {
                if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                    if self.page_open {
                        self.close_page(cx);
                    }
                    self.insert_text(&text.replace(['\n', '\r'], " "));
                }
            }
            "backspace" => {
                if self.page_open {
                    self.close_page(cx);
                }
                self.delete_text(true);
            }
            "delete" => {
                if self.page_open {
                    self.close_page(cx);
                }
                self.delete_text(false);
            }
            "escape" => {
                // back out one level: lightbox, page, then selection,
                // then text, then row selection
                self.confirm_install = None;
                if self.lightbox.is_some() {
                    self.lightbox = None;
                } else if self.page_open {
                    self.close_page(cx);
                } else if self.anchor.is_some() {
                    self.anchor = None;
                } else if !self.search.is_empty() {
                    self.search.clear();
                    self.cursor = 0;
                } else if self.selected.is_some() {
                    self.selected = None;
                    self.selected_publisher = None;
                }
            }
            "up" | "down" => {
                if self.page_open {
                    self.close_page(cx);
                }
                let rows = self.filtered();
                if rows.is_empty() {
                    return;
                }
                let current = self
                    .selected
                    .as_deref()
                    .and_then(|id| {
                        // prefer the selected twin's own row; a publisher
                        // may be filtered out entirely, so fall back to
                        // the first row of the id
                        rows.iter()
                            .position(|&ix| {
                                let app = &self.apps[ix];
                                app.app_id == id
                                    && self.selected_publisher.as_deref()
                                        == app.publisher.as_deref()
                            })
                            .or_else(|| {
                                rows.iter()
                                    .position(|&ix| self.apps[ix].app_id == id)
                            })
                    });
                let next = match (current, key) {
                    (Some(ix), "down") => (ix + 1).min(rows.len() - 1),
                    (Some(ix), "up") => ix.saturating_sub(1),
                    (None, "down") => 0,
                    (None, "up") => rows.len() - 1,
                    _ => return,
                };
                let app_id = self.apps[rows[next]].app_id.clone();
                self.set_selected(&app_id, cx);
                self.reveal_row(next, window);
            }
            "enter" => {
                // install stays a click action; Enter only opens pages
                if self.page_open {
                    return;
                }
                let target = match &self.selected {
                    Some(id) => Some(id.clone()),
                    None => self
                        .filtered()
                        .first()
                        .map(|&first| self.apps[first].app_id.clone()),
                };
                if let Some(app_id) = target {
                    self.open_page(&app_id, cx);
                }
            }
            "space" => {
                if self.page_open {
                    // typing means back to the list
                    self.close_page(cx);
                }
                self.insert_text(" ");
            }
            other if other.chars().count() == 1 && !ctrl => {
                if self.page_open {
                    self.close_page(cx);
                }
                self.insert_text(&Self::produced_char(keystroke));
            }
            _ => return,
        }
        cx.notify();
    }

    /// Insert text at the cursor, replacing any selection.
    fn insert_text(&mut self, text: &str) {
        let (start, end) = self.selection();
        self.search.replace_range(start..end, text);
        self.cursor = start + text.len();
        self.anchor = None;
    }

    /// Delete the selection, or one char before the cursor
    /// (`backspace`) / after it (`delete`).
    fn delete_text(&mut self, backspace: bool) {
        let (start, end) = self.selection();
        if start != end {
            self.search.replace_range(start..end, "");
            self.cursor = start;
            self.anchor = None;
            return;
        }
        if backspace && start > 0 {
            let prev = self.search[..start]
                .chars()
                .next_back()
                .map_or(0, |c| c.len_utf8());
            self.search.replace_range(start - prev..start, "");
            self.cursor = start - prev;
        } else if !backspace && end < self.search.len() {
            let next = self.search[end..].chars().next().map_or(0, |c| c.len_utf8());
            self.search.replace_range(end..end + next, "");
        }
        self.anchor = None;
    }

    /// Move the cursor one char, extending the selection with shift.
    fn move_cursor(&mut self, right: bool, extend: bool) {
        self.cursor = if right {
            self.search[self.cursor..]
                .chars()
                .next()
                .map_or(self.cursor, |c| self.cursor + c.len_utf8())
        } else {
            self.search[..self.cursor]
                .chars()
                .next_back()
                .map_or(0, |c| self.cursor - c.len_utf8())
        };
        if !extend {
            self.anchor = None;
        }
    }

    /// Draft-review text edits: the same model as the search box,
    /// pointed at the review draft instead. `key` is a gpui key name.
    fn insert_draft_text(&mut self, text: &str) {
        let (start, end) = self.draft_selection();
        self.review_draft.1.replace_range(start..end, text);
        self.review_cursor = start + text.len();
        self.review_anchor = None;
    }

    fn delete_draft_text(&mut self, backspace: bool) {
        let (start, end) = self.draft_selection();
        let body = &mut self.review_draft.1;
        if start != end {
            body.replace_range(start..end, "");
            self.review_cursor = start;
            self.review_anchor = None;
            return;
        }
        if backspace && start > 0 {
            let prev = body[..start]
                .chars()
                .next_back()
                .map_or(0, |c| c.len_utf8());
            body.replace_range(start - prev..start, "");
            self.review_cursor = start - prev;
        } else if !backspace && end < body.len() {
            let next = body[end..].chars().next().map_or(0, |c| c.len_utf8());
            body.replace_range(end..end + next, "");
        }
        self.review_anchor = None;
    }

    fn move_draft_cursor(&mut self, key: &str, extend: bool) {
        if key == "backspace" || key == "delete" {
            self.delete_draft_text(key == "backspace");
            return;
        }
        let body = &self.review_draft.1;
        self.review_cursor = match key {
            "left" => body[..self.review_cursor]
                .chars()
                .next_back()
                .map_or(0, |c| self.review_cursor - c.len_utf8()),
            "right" => body[self.review_cursor..]
                .chars()
                .next()
                .map_or(self.review_cursor, |c| self.review_cursor + c.len_utf8()),
            "home" => 0,
            "end" => body.len(),
            _ => return,
        };
        if !extend {
            self.review_anchor = None;
        }
    }

    /// The draft's selected byte range, ordered; `(cursor, cursor)`
    /// when empty.
    fn draft_selection(&self) -> (usize, usize) {
        match self.review_anchor {
            Some(anchor) if anchor != self.review_cursor => {
                (anchor.min(self.review_cursor), anchor.max(self.review_cursor))
            }
            _ => (self.review_cursor, self.review_cursor),
        }
    }

    /// The selected byte range, ordered. `(cursor, cursor)` when empty.
    fn selection(&self) -> (usize, usize) {
        match self.anchor {
            Some(anchor) => (anchor.min(self.cursor), anchor.max(self.cursor)),
            None => (self.cursor, self.cursor),
        }
    }

    /// Scroll the virtualized list so row `ix` is visible (keyboard
    /// navigation). uniform_list 0.2.2 has no nearest strategy, so
    /// pick top or bottom from the current offset.
    fn reveal_row(&self, ix: usize, window: &Window) {
        let state = self.list_handle.0.borrow();
        let item_h = state
            .last_item_size
            .map(|size| f32::from(size.item.height))
            .unwrap_or(ROW_HEIGHT);
        let first_visible = -f32::from(state.base_handle.offset().y) / item_h;
        drop(state);
        let visible = (f32::from(window.viewport_size().height) / item_h).max(1.0);
        if (ix as f32) < first_visible {
            self.list_handle.scroll_to_item(ix, ScrollStrategy::Top);
        } else if (ix as f32) >= first_visible + visible {
            self.list_handle.scroll_to_item(ix, ScrollStrategy::Bottom);
        }
    }

    // ----- rendering -----

    /// The browse surface: category chip row over the virtualized
    /// list, with skeleton rows while the catalog loads and an
    /// actionable empty state otherwise.
    fn render_browse(&self, cx: &mut Context<Self>) -> AnyElement {
        let rows = self.filtered();
        let body: AnyElement = if !rows.is_empty() {
            let view = cx.entity();
            uniform_list("app-list", rows.len(), move |range, window, cx| {
                view.update(cx, |root, cx| root.render_rows(&rows, range, window, cx))
            })
            .track_scroll(self.list_handle.clone())
            .flex_1()
            .min_w_0()
            .bg(ui::inset())
            .into_any_element()
        } else if self.flathub_loading {
            self.render_skeleton()
        } else if self.filter == Filter::Installed {
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_2()
                .bg(ui::inset())
                .child(ui::icon("icons/search.svg", 20., ui::text_dim()))
                .child(
                    div()
                        .text_size(px(ui::TEXT_SIZE_EMPHASIS))
                        .text_color(ui::text())
                        .child("Nothing installed yet"),
                )
                .child(
                    div()
                        .text_size(px(ui::TEXT_SIZE))
                        .text_color(ui::text_dim())
                        .child("Apps you install will show up here."),
                )
                .into_any_element()
        } else {
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_2()
                .bg(ui::inset())
                .child(ui::icon("icons/search.svg", 20., ui::text_dim()))
                .child(
                    div()
                        .text_size(px(ui::TEXT_SIZE_EMPHASIS))
                        .text_color(ui::text())
                        .child("No apps match"),
                )
                .child(
                    div()
                        .text_size(px(ui::TEXT_SIZE))
                        .text_color(ui::text_dim())
                        .child(format!(
                            "Nothing matches {:?}. Escape clears the search.",
                            self.search
                        )),
                )
                .into_any_element()
        };

        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(self.render_category_bar(cx))
            .child(body)
            .into_any_element()
    }

    /// The category chip row: the browse dimension. Only categories
    /// present in the catalog get a chip.
    fn render_category_bar(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_1()
            .px_4()
            .py_1p5()
            .border_b_1()
            .border_color(ui::divider())
            .bg(ui::panel())
            .child(self.category_chip("All categories", None, cx))
            .children(self.category_counts().into_iter().map(|(name, _)| {
                self.category_chip(name, Some(name.to_string()), cx)
            }))
            .into_any_element()
    }

    fn category_chip(&self, label: &str, value: Option<String>, cx: &mut Context<Self>) -> AnyElement {
        let active = self.category == value;
        div()
            .id(SharedString::from(format!("cat-{label}")))
            .px_2()
            .py_0p5()
            .rounded_sm()
            .text_size(px(ui::TEXT_SIZE_SMALL))
            .cursor_pointer()
            .when(active, |chip| {
                chip.bg(ui::accent()).text_color(ui::accent_fg())
            })
            .when(!active, |chip| {
                chip.text_color(ui::text_dim())
                    .hover(|style| style.bg(ui::surface()).text_color(ui::text()))
            })
            .on_click(cx.listener(move |this, _e: &ClickEvent, _w, cx| {
                this.category = value.clone();
                cx.notify();
            }))
            .child(label.to_string())
            .into_any_element()
    }

    /// Counts per browse category across the whole catalog, in chip
    /// order.
    fn category_counts(&self) -> Vec<(&'static str, usize)> {
        BROWSE_CATEGORIES
            .into_iter()
            .map(|name| {
                let count = self
                    .apps
                    .iter()
                    .filter(|app| app.categories.iter().any(|c| c == name))
                    .count();
                (name, count)
            })
            .filter(|(_, count)| *count > 0)
            .collect()
    }

    /// Dim placeholder rows while the catalog loads: same rhythm as
    /// real rows so the swap does not jump.
    fn render_skeleton(&self) -> AnyElement {
        div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .bg(ui::inset())
            .children((0..12).map(|_| {
                div()
                    .h(px(ROW_HEIGHT))
                    .w_full()
                    .flex()
                    .items_center()
                    .gap_3()
                    .px_4()
                    .border_b_1()
                    .border_color(ui::divider_soft())
                    .child(div().w(px(40.)).h(px(40.)).rounded_full().bg(ui::surface()))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1p5()
                            .child(div().w(px(180.)).h(px(10.)).rounded_sm().bg(ui::surface()))
                            .child(div().w(px(280.)).h(px(8.)).rounded_sm().bg(ui::surface())),
                    )
            }))
            .with_animation(
                "skeleton",
                gpui::Animation::new(Duration::from_millis(1400)).repeat(),
                |rows, delta| rows.opacity(0.55 + 0.35 * delta),
            )
            .into_any_element()
    }

    /// Build one screenful of rows for the virtualized list.
    fn render_rows(
        &self,
        rows: &[usize],
        range: Range<usize>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let selected = self.selected.as_deref();
        range
            .filter_map(|ix| rows.get(ix))
            .map(|&app_ix| {
                let app = &self.apps[app_ix];
                let app_id = app.app_id.clone();
                let action_id = app_id.clone();
                let row_publisher = app.publisher.clone();
                // the row's bookkeeping key: two publishers of one app
                // id keep separate digests
                let row_key = app_key(&app.app_id, row_publisher.as_deref());
                // unique element ids: twin rows share the app id, and
                // duplicate ids collapse hover and state
                let row_id = format!(
                    "app-row-{app_id}-{}",
                    row_publisher.as_deref().unwrap_or("")
                );
                let is_selected = selected == Some(app.app_id.as_str())
                    && self.selected_publisher.as_deref() == app.publisher.as_deref();
                let is_installed = self.installed.contains_key(&app.app_id);
                let outdated = self
                    .installed
                    .get(app.app_id.as_str())
                    .is_some_and(|info| update_available(app, info));
                let job = self.jobs.get(&app.app_id).map(|job| job.label().to_string());
                let verified_digest = self.verified.get(&row_key).cloned();
                let row_update_id = app_id.clone();
                // one publisher clone per closure: two move closures
                // cannot share the original
                let click_publisher = row_publisher.clone();
                let install_publisher = row_publisher.clone();

                div()
                    .id(SharedString::from(row_id))
                    .h(px(ROW_HEIGHT))
                    .w_full()
                    .flex()
                    .items_center()
                    .gap_3()
                    .px_4()
                    .border_b_1()
                    .border_color(ui::divider_soft())
                    .cursor_pointer()
                    .hover(|style| style.bg(ui::surface()))
                    .when(is_selected, |row| row.bg(ui::row_selected()))
                    .on_click(cx.listener(move |this, _event: &ClickEvent, _window, cx| {
                        // a twin row names its publisher: the page is
                        // about THIS row, not the first same-id row
                        this.selected_publisher = click_publisher.clone();
                        this.open_page(&app_id, cx);
                    }))                    .child(app_icon(app.icon_path.as_deref(), &app.name, 40.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap_0p5()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        div()
                                            .text_size(px(ui::TEXT_SIZE_EMPHASIS))
                                            .font_weight(gpui::FontWeight::MEDIUM)
                                            .text_color(ui::text())
                                            .overflow_hidden()
                                            .whitespace_nowrap()
                                            .child(app.name.clone()),
                                    )
                                    .child(match app.source {
                                        Source::Reviewed => badge("Flathub", ui::text_dim()),
                                        Source::Publisher => badge("Orchard", ui::accent()),
                                    })
                                    .when(verified_digest.is_some(), |row| {
                                        row.child(badge("digest ok", ui::risk_color(
                                            orchard_core::Risk::Low,
                                        )))
                                    }),
                            )
                            .child(
                                div()
                                    .text_size(px(ui::TEXT_SIZE))
                                    .text_color(ui::text_dim())
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    // a second publisher of the same app
                                    // id is a second row: name them in
                                    // the list, quiet by default
                                    .child(match app.source {
                                        Source::Publisher => {
                                            let who = app
                                                .publisher
                                                .as_deref()
                                                .and_then(|pubkey| {
                                                    self.profiles.get(pubkey)
                                                })
                                                .and_then(|p| p.name.clone())
                                                .unwrap_or_else(|| {
                                                    short_npub(
                                                        app.publisher.as_deref().unwrap_or(""),
                                                    )
                                                });
                                            format!("by {who} \u{b7} {}", app.summary)
                                        }
                                        Source::Reviewed => app.summary.clone(),
                                    }),
                            ),
                    )
                    .child(match job {
                        Some(label) => badge(
                            &label,
                            ui::risk_color(orchard_core::Risk::Medium),
                        )
                        .into_any_element(),
                        None if is_installed => div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(if outdated {
                                badge(
                                    "Update available",
                                    ui::risk_color(orchard_core::Risk::Medium),
                                )
                            } else {
                                badge("Installed", ui::risk_color(orchard_core::Risk::Low))
                            })
                            .child(ui::small_button(
                                format!("row-open-{action_id}-{}", row_publisher.as_deref().unwrap_or("")),
                                Some("icons/play.svg"),
                                "Open",
                                false,
                            )
                            .on_click(cx.listener(move |this, _e: &ClickEvent, _w, cx| {
                                cx.stop_propagation();
                                this.open(action_id.as_str(), cx);
                            })))
                            .when(outdated, |slot| {
                                let update_id = row_update_id.clone();
                                slot.child(
                                    ui::small_button(
                                        format!(
                                            "row-update-{update_id}-{}",
                                            row_publisher.as_deref().unwrap_or("")
                                        ),
                                        Some("icons/update.svg"),
                                        "Update",
                                        false,
                                    )
                                    .on_click(cx.listener(
                                        move |this, _e: &ClickEvent, _w, cx| {
                                            cx.stop_propagation();
                                            this.update(update_id.as_str(), cx);
                                        },
                                    )),
                                )
                            })
                            .into_any_element(),
                        None if app.version.is_none() && app.source == Source::Publisher => {
                            // the chain exists but nothing built for
                            // this machine: the honest note replaces
                            // an install that cannot work
                            div()
                                .flex_none()
                                .child(badge(&no_build_note(app), ui::text_dim()))
                                .into_any_element()
                        }
                        None => {
                            // two-press install: the first press arms
                            // the button, the second starts the job
                            let confirming = self.confirm_install.as_deref()
                                == Some(action_id.as_str());
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .children(app.version.as_deref().map(|v| badge(v, ui::text_dim())))
                                .child(
                                    ui::small_button(
                                        format!(
                                            "row-install-{action_id}-{}",
                                            row_publisher.as_deref().unwrap_or("")
                                        ),
                                        Some("icons/plus.svg"),
                                        if confirming { "Sure?" } else { "Install" },
                                        true,
                                    )
                                    .on_click(cx.listener(move |this, _e: &ClickEvent, _w, cx| {
                                        cx.stop_propagation();
                                        // the twin this row stands for
                                        this.selected_publisher = install_publisher.clone();
                                        if confirming {
                                            this.install(action_id.as_str(), cx);
                                        } else {
                                            this.confirm_install = Some(action_id.clone());
                                            cx.notify();
                                        }
                                    })),
                                )
                                .into_any_element()
                        }
                    })
                    .into_any_element()
            })
            .collect()
    }

    /// The full app page: identity, screenshots, provenance,
    /// permissions, and the install action in one scrollable view.
    /// The page is where the install decision happens, so the action
    /// bar stays pinned while the content scrolls.
    fn render_page(&mut self, app: StoreApp, font: gpui::Font, cx: &mut Context<Self>) -> AnyElement {
        let app_id = app.app_id.clone();
        // the page's bookkeeping key: the selected twin's publisher
        // names which digest, sizes, and verify error belong here
        let page_key = app_key(&app_id, app.publisher.as_deref());
        let installed = self.installed.contains_key(&app_id);
        let job = self.jobs.get(&app_id).cloned();
        let verified = self.verified.get(&page_key).cloned();

        // the permissions source, in order: catalog metadata (Orchard
        // channel), the deploy's ini (installed apps), the remote's
        // manifest (pre-install check on the reviewed channel)
        let sandbox_metadata: Option<String> = app
            .metadata
            .clone()
            .or_else(|| {
                self.installed
                    .get(&app_id)
                    .and_then(|info| info.metadata.clone())
            })
            .or_else(|| self.remote_metadata.get(&page_key).cloned());

        // the header icon: cached AppStream PNG, or a disk-cache fetch
        // of the remote icon, or the initials tile
        let header_icon = match (&app.icon_path, &app.icon_url) {
            (Some(path), _) => app_icon(Some(path), &app.name, 56.),
            (None, Some(url)) => match self.fetched.get(url) {
                Some(Some(path)) => app_icon(Some(path), &app.name, 56.),
                _ => app_icon(None, &app.name, 56.),
            },
            (None, None) => app_icon(None, &app.name, 56.),
        };

        // the quiet provenance line: what the user is relying on,
        // without cryptographic jargon by default
        let digest_failed = self.verify_errors.get(&page_key).cloned();
        let digest_badge = if digest_failed.is_none() {
            verified.clone().map(|digest| {
                let short = &digest[..digest.len().min(12)];
                // the pin itself is the fact; the mechanism lives on
                // hover. The wrapper carries the hover style (the
                // repaint trigger) and a unique id (tooltip state
                // lives under the element's global id), same rules as
                // the review glyphs.
                div()
                    .id("digest-pin")
                    .flex_none()
                    .hover(|style| style.bg(gpui::transparent_black()))
                    .tooltip(|_window, cx| {
                        cx.new(|_| {
                            TextTooltip(
                                "Install requires an exact match to this publisher-signed digest".into(),
                            )
                        })
                        .into()
                    })
                    .child(badge(
                        &format!("digest {short}"),
                        ui::risk_color(Risk::Low),
                    ))
            })
        } else {
            None
        };
        let provenance = match app.source {
            Source::Reviewed => div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap_2()
                .child(badge("Reviewed by Flathub", ui::text()))
                .child(
                    div()
                        .text_size(px(ui::TEXT_SIZE))
                        .text_color(ui::text())
                        .child("Moderator-reviewed listing; installs from your flathub remote."),
                )
                .into_any_element(),
            Source::Publisher => div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap_2()
                // a verified check, not a text badge: the words live
                // on hover, same rules as the review glyphs
                .child(
                    div()
                        .id("publisher-verified")
                        .flex_none()
                        .hover(|style| style.bg(gpui::transparent_black()))
                        .tooltip(|_window, cx| {
                            cx.new(|_| {
                                TextTooltip(
                                    "Publisher-verified: the release events carry this publisher's signature".into(),
                                )
                            })
                            .into()
                        })
                        .child(ui::icon("icons/check.svg", 12., ui::accent())),
                )
                .children(app.publisher.as_ref().map(|pubkey| {
                    let profile = self.profiles.get(pubkey);
                    // the identity: the domain-backed address when the
                    // claim checked out, else the profile name, else
                    // the truncated npub. The verified check earlier in
                    // this row is about signatures; the address earns
                    // its place by being one.
                    let label = match self.verified_nip05(pubkey) {
                        Some(claim) => format!("Published by {claim}."),
                        None => {
                            match profile.and_then(|p| p.name.clone()) {
                                Some(name) => format!("Published by {name}."),
                                None => format!("Published by {}.", short_npub(pubkey)),
                            }
                        }
                    };
                    div()
                        .flex()
                        .gap_1()
                        .items_center()
                        .text_size(px(ui::TEXT_SIZE))
                        .text_color(ui::text())
                        .child(label)
                }))
                // continuity: this app id was published by another key
                // before this client ever saw this one; a signed
                // handover from that key reads calmer than an
                // unexplained new key
                .children(app.publisher.as_ref().filter(|pubkey| {
                    self.continuity
                        .apps
                        .get(&app.app_id)
                        .is_some_and(|entry| entry.flagged.contains(pubkey))
                }).map(|pubkey| {
                    let authorized = app.successor_to.is_some();
                    let (label, note) = if authorized {
                        (
                            "Publisher migrated",
                            "The previous key signed a statement handing this app id to this key. The digest pin still verifies the artifact.",
                        )
                    } else {
                        (
                            "New publisher key",
                            "This app id was published by another key before; this key is new since then. The digest pin is what your install verifies.",
                        )
                    };
                    div()
                        .id(SharedString::from(format!(
                            "key-changed-{}-{pubkey}",
                            app.app_id
                        )))
                        .flex_none()
                        .hover(|style| style.bg(gpui::transparent_black()))
                        .tooltip(|_window, cx| {
                            cx.new(|_| TextTooltip(note.into())).into()
                        })
                        .child(badge(
                            label,
                            ui::risk_color(if authorized {
                                Risk::Low
                            } else {
                                Risk::Medium
                            }),
                        ))
                }))
                .children(digest_badge)
                .children(digest_failed.clone().map(|err| {
                    div()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap_2()
                        .child(badge("Digest check failed", ui::risk_color(Risk::High)))
                        .child(
                            div()
                                .text_size(px(ui::TEXT_SIZE))
                                .text_color(ui::risk_color(Risk::High))
                                .child(err),
                        )
                }))
                .into_any_element(),
        };

        let mut content = div()
            .w_full()
            .min_w_0()
            .max_w(px(760.))
            .px_8()
            .py_6()
            .flex()
            .flex_col()
            .gap_5()
            .child(
                div()
                    .id("back")
                    .flex()
                    .items_center()
                    .gap_1p5()
                    .cursor_pointer()
                    .text_color(ui::accent())
                    .hover(|style| style.text_color(ui::text()))
                    .on_click(cx.listener(|this, _e: &ClickEvent, _w, cx| {
                        // a mouse close hands the list back clean; the
                        // keyboard anchor only survives Esc
                        this.selected = None;
                        this.close_page(cx);
                    }))
                    .child(ui::icon("icons/arrow_left.svg", 13., ui::accent()))
                    .child("All apps"),
            )
            .child(
                // identity header
                div().flex().items_start().gap_4().child(header_icon).child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(
                            div()
                                .text_size(px(ui::TEXT_SIZE_EMPHASIS + 4.))
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .text_color(ui::text())
                                .child(app.name.clone()),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_wrap()
                                .items_center()
                                .gap_2()
                                .text_size(px(ui::TEXT_SIZE))
                                .text_color(ui::text())
                                .child(div().child(app.app_id.clone()))
                                .children(app.version.clone().map(|v| div().child(format!("· {v}"))))
                                .children(
                                    app.release_date
                                        .clone()
                                        .map(|date| div().child(format!("· updated {date}"))),
                                )
                                .children(match self.installed.get(&app_id) {
                                    // installed: the size on disk; not
                                    // installed: what the remote would
                                    // download (or occupy once it lands)
                                    Some(info) => info
                                        .size
                                        .clone()
                                        .map(|size| div().child(format!("· {size}"))),
                                    None => self
                                        .remote_sizes
                                        .get(&page_key)
                                        .and_then(|(download, installed)| {
                                            download
                                                .clone()
                                                .map(|size| div().child(format!("· {size}")))
                                                .or_else(|| {
                                                    installed.clone().map(|size| {
                                                        div().child(format!("· {size}"))
                                                    })
                                                })
                                        }),
                                })
                                .children(
                                    app.license.clone().map(|l| div().child(format!("· {l}"))),
                                ),
                        )
                        .child(provenance),
                ),
            );

        if !app.screenshots.is_empty() {
            content = content.child(
                div()
                    .id("screenshots")
                    .flex()
                    .gap_2()
                    .overflow_x_scroll()
                    .track_scroll(&self.shot_handle)
                    // gpui hands every wheel axis to every scrollable
                    // under the pointer, so a horizontal swipe's small
                    // vertical drift would scroll the page too. Own
                    // x-dominant gestures here and stop the event before
                    // the page's scroll handler runs.
                    .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, window, cx| {
                        let delta = event.delta.pixel_delta(window.line_height());
                        if delta.x.abs() > delta.y.abs() {
                            let mut offset = this.shot_handle.offset();
                            offset.x += delta.x;
                            this.shot_handle.set_offset(offset);
                            cx.stop_propagation();
                            cx.notify();
                        }
                    }))
                    .children(app.screenshots.iter().take(6).map(|url| {
                        match self.fetched.get(url) {
                            Some(Some(path)) => div()
                                .id(SharedString::from(format!("shot-{url}")))
                                .flex_none()
                                .cursor_pointer()
                                .rounded_sm()
                                .border_1()
                                .border_color(ui::divider())
                                .overflow_hidden()
                                .hover(|style| style.border_color(ui::accent()))
                                .on_click(cx.listener({
                                    let url = url.clone();
                                    move |this, _e: &ClickEvent, _w, cx| {
                                        this.lightbox = Some(url.clone());
                                        cx.notify();
                                    }
                                }))
                                .child(
                                    gpui::img(path.clone())
                                        .w(px(280.))
                                        .h(px(158.))
                                        .object_fit(gpui::ObjectFit::Cover),
                                )
                                .into_any_element(),
                            slot => image_placeholder(slot.is_none()),
                        }
                    })),
            );
        }

        let has_sandbox = sandbox_metadata.is_some();
        content = content
            .child(
                div()
                    .text_size(px(ui::TEXT_SIZE_EMPHASIS))
                    .text_color(ui::text())
                    .child(app.summary.clone()),
            )
            .children(app.description.clone().map(|description| {
                // block text: one line per div so paragraphs and bullet
                // lists keep their structure; full contrast for reading
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .w_full()
                    .text_size(px(ui::TEXT_SIZE))
                    .text_color(ui::text())
                    .children(description.lines().map(|line| div().w_full().child(line.to_string())))
            }))
            .children(app.homepage.clone().map(|url| project_link("homepage", "icons/globe.svg", url)))
            .children(app.repository.clone().map(|url| project_link("repository", "icons/git.svg", url)))
            .child(self.render_reviews(&app_id, app.publisher.as_deref(), font.clone(), cx))
            .when_some(sandbox_metadata, |section, metadata| {
                section.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_1p5()
                                .text_size(px(ui::TEXT_SIZE_EMPHASIS - 1.))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(ui::text())
                                .child(ui::icon("icons/shield.svg", 13., ui::text()))
                                .child("What it can access"),
                        )
                        .child(render_permissions_static(&metadata)),
                )
            })
            .when(!has_sandbox, |section| {
                // no sandbox data yet: it is being fetched from the
                // artifact or the remote, or flatpak could not produce it
                let note = if self.verify_errors.contains_key(&page_key) {
                    "Sandbox info unavailable: the artifact did not pass its digest check."
                } else if self.publisher_fetching.contains(&page_key) {
                    "Reading the sandbox manifest from the artifact…"
                } else if self.remote_fetching.contains(&page_key) {
                    "Reading the sandbox manifest from flathub…"
                } else {
                    "Flatpak apps run in a sandbox; this app's permissions appear here once its manifest is available."
                };
                section.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1p5()
                        .text_size(px(ui::TEXT_SIZE))
                        .text_color(ui::text())
                        .child(ui::icon("icons/shield.svg", 13., ui::text_dim()))
                        .child(note),
                )
            });

        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .bg(ui::panel())
            .child(
                div()
                    .id("page-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.page_scroll)
                    .child(div().flex().justify_center().child(content)),
            )
            .child(
                // action bar: pinned under the scrolling content
                div().flex().items_center().gap_2().px_8().py_3().border_t_1()
                    .border_color(ui::divider()).bg(ui::panel())
                    .children(match job {
                        Some(state) => vec![badge(
                            state.label(),
                            ui::risk_color(Risk::Medium),
                        )
                        .into_any_element()],
                        None => {
                            let mut buttons: Vec<gpui::AnyElement> = Vec::new();
                            if installed {
                                let open_id = app_id.clone();
                                buttons.push(
                                    button("btn-open", Some("icons/play.svg"), "Open", false)
                                        .on_click(
                                            cx.listener(move |this, _e: &ClickEvent, _w, cx| {
                                                this.open(open_id.as_str(), cx);
                                            }),
                                        )
                                        .into_any_element(),
                                );
                                if self
                                    .installed
                                    .get(&app_id)
                                    .is_some_and(|info| update_available(&app, info))
                                {
                                    let update_id = app_id.clone();
                                    buttons.push(
                                        button(
                                            "btn-update",
                                            Some("icons/update.svg"),
                                            "Update",
                                            false,
                                        )
                                        .on_click(cx.listener(
                                            move |this, _e: &ClickEvent, _w, cx| {
                                                this.update(update_id.as_str(), cx);
                                            },
                                        ))
                                        .into_any_element(),
                                    );
                                }
                                let remove_id = app_id.clone();
                                buttons.push(
                                    button("btn-remove", Some("icons/trash.svg"), "Remove", false)
                                        .on_click(
                                            cx.listener(move |this, _e: &ClickEvent, _w, cx| {
                                                this.uninstall(remove_id.as_str(), cx);
                                            }),
                                        )
                                        .into_any_element(),
                                );
                            } else if app.version.is_none() && app.source == Source::Publisher {
                                // the honest note replaces an install
                                // that cannot work
                                buttons.push(
                                    badge(&no_build_note(&app), ui::text_dim()).into_any_element(),
                                );
                            } else {
                                let install_id = app_id.clone();
                                // two-press install: same idiom as the
                                // list rows; the label carries the
                                // publisher flavor until armed
                                let confirming = self.confirm_install.as_deref()
                                    == Some(app_id.as_str());
                                let label = if confirming {
                                    "Sure?"
                                } else if app.source == Source::Publisher {
                                    "Verify & install"
                                } else {
                                    "Install"
                                };
                                buttons.push(
                                    button("btn-install", Some("icons/plus.svg"), label, true)
                                        .on_click(cx.listener(
                                            move |this, _e: &ClickEvent, _w, cx| {
                                                if confirming {
                                                    this.install(install_id.as_str(), cx);
                                                } else {
                                                    this.confirm_install =
                                                        Some(install_id.clone());
                                                    cx.notify();
                                                }
                                            },
                                        ))
                                        .into_any_element(),
                                );
                            }
                            buttons
                        }
                    }),
            )
            .into_any_element()
    }
}

fn risk_label(risk: Risk) -> &'static str {
    match risk {
        Risk::Low => "low",
        Risk::Medium => "medium",
        Risk::High => "high",
    }
}

/// Fixed list row height: `uniform_list` needs uniform items, and a
/// pinned height keeps the layout work per frame trivial.
const ROW_HEIGHT: f32 = 56.0;

/// Canonical browse categories, display order: the chip row folds to
/// this set, anything absent from it never shows a chip.
const BROWSE_CATEGORIES: [&str; 10] = [
    "Games",
    "Utilities",
    "Audio & Video",
    "Development",
    "Network",
    "Office",
    "Graphics",
    "Science",
    "Education",
    "System",
];

/// The screenshot slot while its image fetch is in flight, or after it
/// failed.
fn image_placeholder(loading: bool) -> gpui::AnyElement {
    div()
        .w(px(176.))
        .h(px(108.))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded_sm()
        .bg(ui::inset())
        .border_1()
        .border_color(ui::divider_soft())
        .text_size(px(ui::TEXT_SIZE_LABEL))
        .text_color(ui::text_dim())
        .child(if loading { "loading…" } else { "unavailable" })
        .into_any_element()
}

/// A hover tooltip: quiet chrome, one line of text. The new-key
/// sprout uses it so the glyph stays quiet and the meaning is on
/// demand.
struct TextTooltip(SharedString);

impl gpui::Render for TextTooltip {
    fn render(
        &mut self,
        _window: &mut gpui::Window,
        _cx: &mut gpui::Context<Self>,
    ) -> impl gpui::IntoElement {
        div()
            .px_2()
            .py_1()
            .rounded_sm()
            .border_1()
            .border_color(ui::divider())
            .bg(ui::panel())
            .text_size(px(ui::TEXT_SIZE_LABEL))
            .text_color(ui::text())
            .child(self.0.to_string())
    }
}

/// Is this review written by the app's own publisher?
fn is_publisher_review(review: &orchard_core::Review, publisher: Option<&str>) -> bool {
    publisher == Some(review.pubkey.as_str())
}

impl StoreRoot {
    /// Reviewers two steps away: followed by someone the signed-in
    /// identity follows, not followed directly, never the identity
    /// itself. Personal interpretation of the public graph, computed
    /// from small sets.
    fn circle2(&self) -> std::collections::HashSet<String> {
        circle2_set(
            &self.follows,
            &self.graph,
            self.reviewer.as_ref().map(|r| r.signer_pubkey.as_str()),
        )
    }
}

/// The two-hop set: everything the identity's follows follow, minus
/// the follows themselves and the identity.
fn circle2_set(
    follows: &std::collections::HashSet<String>,
    graph: &std::collections::BTreeMap<String, Vec<String>>,
    me: Option<&str>,
) -> std::collections::HashSet<String> {
    let mut set = std::collections::HashSet::new();
    for followed in follows {
        for pk in graph.get(followed).into_iter().flatten() {
            if Some(pk.as_str()) != me && !follows.contains(pk) {
                set.insert(pk.clone());
            }
        }
    }
    set
}

/// Is this reviewer's key new to the feed (first seen under a week
/// ago)? Absence from the table means unknown age, never new.
fn is_new_key(
    first_seen: &std::collections::BTreeMap<String, u64>,
    pubkey: &str,
    now: i64,
) -> bool {
    const WEEK: i64 = 7 * 24 * 60 * 60;
    first_seen
        .get(pubkey)
        .is_some_and(|seen| (now - *seen as i64) < WEEK)
}

/// The reviews aggregate for one app. Publisher self-reviews are
/// excluded from the average and count (their rows stay, flagged);
/// reviews from new keys count fully and are only reported: a weak
/// heuristic never drops a vote. Circle reviews (the signed-in
/// identity's follows) are counted for the circle disclosure and its
/// own average, under the same publisher exclusion.
#[derive(Default, Debug, PartialEq)]
struct ReviewsAggregate {
    average: Option<f32>,
    counted: usize,
    new_keys: usize,
    publisher_reviews: usize,
    circle: usize,
    circle_average: Option<f32>,
    circle2: usize,
}

fn reviews_aggregate(
    reviews: &[orchard_core::Review],
    publisher: Option<&str>,
    first_seen: &std::collections::BTreeMap<String, u64>,
    now: i64,
    follows: &std::collections::HashSet<String>,
    circle2: &std::collections::HashSet<String>,
) -> ReviewsAggregate {
    let mut aggregate = ReviewsAggregate::default();
    let mut rated: Vec<f32> = Vec::new();
    let mut circle_rated: Vec<f32> = Vec::new();
    for review in reviews {
        if is_publisher_review(review, publisher) {
            aggregate.publisher_reviews += 1;
            continue;
        }
        if let Some(rating) = review.rating {
            rated.push(rating);
        }
        if is_new_key(first_seen, &review.pubkey, now) {
            aggregate.new_keys += 1;
        }
        if follows.contains(&review.pubkey) {
            aggregate.circle += 1;
            if let Some(rating) = review.rating {
                circle_rated.push(rating);
            }
        } else if circle2.contains(&review.pubkey) {
            aggregate.circle2 += 1;
        }
    }
    aggregate.average = (!rated.is_empty())
        .then(|| rated.iter().sum::<f32>() / rated.len() as f32);
    aggregate.circle_average = (!circle_rated.is_empty())
        .then(|| circle_rated.iter().sum::<f32>() / circle_rated.len() as f32);
    aggregate.counted = reviews.len() - aggregate.publisher_reviews;
    aggregate
}

impl StoreRoot {
    /// The reviews section of the app page: a quiet header that doubles
    /// as the expand/collapse toggle (the list is hidden until asked
    /// for), the review list flush with the other page fields, and the
    /// compose or sign-in form when expanded.
    fn render_reviews(
        &mut self,
        app_id: &str,
        publisher: Option<&str>,
        font: gpui::Font,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let reviews = self
            .reviews
            .get(app_id)
            .cloned()
            .unwrap_or_default();

        // header: same shape as the other section headers, so the text
        // lines up; the score formats with one decimal, no float junk
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(i64::MAX);
        let circle2_set = self.circle2();
        let ReviewsAggregate {
            average,
            counted,
            new_keys,
            publisher_reviews,
            circle,
            circle_average,
            circle2,
        } = reviews_aggregate(
            &reviews,
            publisher,
            &self.first_seen,
            now,
            &self.follows,
            &circle2_set,
        );
        let label = if reviews.is_empty() {
            "No reviews yet".to_string()
        } else {
            let score = average
                .map(|value| format!(" · {:.1} {}", value, stars(value)))
                .unwrap_or_default();
            format!("Reviews{} · {}", score, counted)
        };
        let mut section = div().flex().flex_col().gap_2().w_full().child(
            div()
                .id("reviews-toggle")
                .flex()
                .items_center()
                .gap_1p5()
                .cursor_pointer()
                .text_size(px(ui::TEXT_SIZE_EMPHASIS - 1.))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(ui::text())
                .hover(|style| style.text_color(ui::accent()))
                .on_click(cx.listener(|this, _e: &ClickEvent, _w, cx| {
                    this.reviews_expanded = !this.reviews_expanded;
                    cx.notify();
                }))
                .child(if self.reviews_expanded {
                    ui::icon("icons/chevron_down.svg", 13., ui::text())
                } else {
                    ui::icon("icons/chevron_right.svg", 13., ui::text())
                })
                .child(label),
        );

        // the reviews themselves: indented one step from the header,
        // the same inset the permissions box uses, so the page reads
        // as header, then its content
        if self.reviews_expanded {
            let mut body = div().flex().flex_col().gap_2().pl_3();
            // the trust breakdown, disclosed rather than hidden: what
            // the aggregate left out and which reviews are young keys
            let mut notes: Vec<String> = Vec::new();
            if publisher_reviews > 0 {
                notes.push(format!(
                    "{publisher_reviews} publisher review{} not counted",
                    if publisher_reviews == 1 { "" } else { "s" }
                ));
            }
            if new_keys > 0 {
                notes.push(format!("{new_keys} of {counted} from new keys"));
            }
            if circle > 0 {
                let mut circle_note = format!("{circle} in your circle");
                if let Some(value) = circle_average {
                    circle_note.push_str(&format!(", avg {value:.1}"));
                }
                notes.push(circle_note);
            }
            if circle2 > 0 {
                notes.push(format!("{circle2} in your circle's circle"));
            }
            if !notes.is_empty() {
                body = body.child(
                    div()
                        .text_size(px(ui::TEXT_SIZE_LABEL))
                        .text_color(ui::text_dim())
                        .child(notes.join(" · ")),
                );
            }
            let mut avatar_urls: Vec<String> = Vec::new();
            for (index, review) in reviews.iter().enumerate() {
                // the author's identity on the row: the profile name
                // when there is one, the truncated npub otherwise. A
                // NIP-05 claim never renders as a name; it renders as
                // the check beside one, and only when the domain
                // backed it.
                let author = review
                    .name
                    .clone()
                    .unwrap_or_else(|| short_npub(&review.pubkey));
                let nip05_note = self.verified_nip05(&review.pubkey).and_then(|claim| {
                    split_nip05(&claim)
                        .map(|(local, domain)| format!("{domain} lists {local} as this key"))
                });
                let when = relative_time(review.created_at);
                let flagged_publisher = is_publisher_review(review, publisher);
                let fresh = is_new_key(&self.first_seen, &review.pubkey, now);
                let in_circle = self.follows.contains(&review.pubkey);
                let in_circle2 = !in_circle && circle2_set.contains(&review.pubkey);
                // the author's picture when their profile carries one;
                // the glyph stands in until the fetch lands
                let picture = self
                    .profiles
                    .get(&review.pubkey)
                    .and_then(|p| p.picture.clone());
                if let Some(url) = &picture {
                    avatar_urls.push(url.clone());
                }
                let avatar_path = picture
                    .as_deref()
                    .and_then(|url| self.fetched.get(url))
                    .and_then(|f| f.as_deref());
                let mut row = div().flex().flex_col().gap_1().w_full().child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .text_size(px(ui::TEXT_SIZE_LABEL))
                        .text_color(ui::text_dim())
                        .child(avatar(avatar_path, 14., ui::text_dim()))
                        .child(div().child(author))
                        .children(nip05_note.map(|note| {
                            // the domain backed this author's claim;
                            // the words live on hover, same rules as
                            // the sprout
                            div()
                                .id(SharedString::from(format!("nip05-review-{index}")))
                                .flex_none()
                                .hover(|style| style.bg(gpui::transparent_black()))
                                .tooltip(move |_window, cx| {
                                    cx.new(|_| TextTooltip(note.clone().into())).into()
                                })
                                .child(ui::icon("icons/check.svg", 11., ui::accent()))
                        }))
                        .children(review.rating.map(|value| {
                            div().text_color(ui::accent()).child(stars(value))
                        }))
                        .child(div().child(when))
                        .children(flagged_publisher.then(|| {
                            div().text_color(ui::text()).child("publisher")
                        }))
                        .children(fresh.then(|| {
                            // a sprout: a key young in the feed; the
                            // words live on hover and in the line below.
                            // The transparent hover style is load-bearing:
                            // gpui registers its repaint-on-hover-change
                            // listener only for elements with a hover
                            // style, and a visible tooltip hides during
                            // prepaint, so without it the tooltip never
                            // sees the mouse leave. The id must be unique
                            // per row: the tooltip state lives under the
                            // element's global id, and duplicate ids
                            // collapse every row's tooltip onto one slot.
                            div()
                                .id(SharedString::from(format!("new-key-{index}")))
                                .flex_none()
                                .hover(|style| style.bg(gpui::transparent_black()))
                                .tooltip(|_window, cx| {
                                    cx.new(|_| {
                                        TextTooltip("New key: first seen on the relays under a week ago".into())
                                    })
                                    .into()
                                })
                                .child(ui::icon("icons/sprout.svg", 11., ui::accent()))
                        }))
                        .children(in_circle.then(|| {
                            // the circle glyph: a reviewer the signed-in
                            // identity follows. Same hover rules as the
                            // sprout (the style is the repaint trigger,
                            // and the id must be unique per row).
                            div()
                                .id(SharedString::from(format!("circle-{index}")))
                                .flex_none()
                                .hover(|style| style.bg(gpui::transparent_black()))
                                .tooltip(|_window, cx| {
                                    cx.new(|_| {
                                        TextTooltip("You follow this reviewer".into())
                                    })
                                    .into()
                                })
                                .child(ui::icon("icons/person.svg", 11., ui::accent()))
                        }))
                        .children(in_circle2.then(|| {
                            // the two-hop glyph: a reviewer someone in
                            // the circle follows. A second hue rather
                            // than a dimmed accent: the two distances
                            // read apart at a glance. Same hover rules.
                            div()
                                .id(SharedString::from(format!("circle2-{index}")))
                                .flex_none()
                                .hover(|style| style.bg(gpui::transparent_black()))
                                .tooltip(|_window, cx| {
                                    cx.new(|_| {
                                        TextTooltip("Followed by someone you follow".into())
                                    })
                                    .into()
                                })
                                .child(ui::icon("icons/person.svg", 11., ui::circle2()))
                        })),
                );
                if !review.content.is_empty() {
                    row = row.child(
                        div()
                            .flex()
                            .flex_col()
                            .text_size(px(ui::TEXT_SIZE))
                            .text_color(ui::text())
                            .children(
                                review
                                    .content
                                    .lines()
                                    .map(|line| div().w_full().child(line.to_string())),
                            ),
                    );
                }
                body = body.child(row);
            }
            // the rows' avatar fetches: results notify and repaint
            self.ensure_fetched(avatar_urls, cx);

            // compose or invitation, depending on session state (the
            // sign-in modal is window-level, not part of the section)
            if self.composing {
                body = body.child(self.render_review_compose(app_id, font.clone(), cx));
            } else {
                let signed_in = self.reviewer.is_some();
                let invitation = div().flex().items_center().gap_2().child(
                    div()
                        .id("write-review")
                        .cursor_pointer()
                        .text_size(px(ui::TEXT_SIZE))
                        .text_color(ui::accent())
                        .hover(|style| style.text_color(ui::text()))
                        .on_click(cx.listener(|this, _e: &ClickEvent, _w, cx| {
                            this.start_review(cx);
                        }))
                        .child(if signed_in {
                            "Write a review (r)".to_string()
                        } else {
                            "Sign in to review".to_string()
                        }),
                );
                body = body.child(invitation);
            }
            section = section.child(body);
        }

        section.into_any_element()
    }

    /// The sign-in modal body: the pairing code while it waits, the
    /// optional name question once a signer answered. The chrome
    /// (backdrop, centering) is around it in render_signin_modal.
    fn render_signin_modal(&mut self, font: gpui::Font, cx: &mut Context<Self>) -> gpui::AnyElement {
        let body = if self.pending_reviewer.is_some() {
            self.render_name_phase(font, cx)
        } else if self.connect.is_some() {
            self.render_pairing_phase(cx)
        } else {
            div()
                .text_size(px(ui::TEXT_SIZE))
                .text_color(ui::text_dim())
                .child("Preparing the pairing…")
                .into_any_element()
        };
        div()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            // the dimmed click-away is a FIRST sibling; the panel after
            // it wins every click that lands on it, and swallows the
            // mouse down so the backdrop's close never sees it
            .child(
                div()
                    .id("signin-modal-backdrop")
                    .absolute()
                    .inset_0()
                    .bg(gpui::rgba(0x000000B3))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _e: &MouseDownEvent, _w, cx| {
                            this.close_signin();
                            // swallow the down: the Sign in button
                            // under the modal never sees it, so its
                            // click cannot reopen what this closed
                            cx.stop_propagation();
                            cx.notify();
                        }),
                    ),
            )
            .child(
                div().on_mouse_down(
                    MouseButton::Left,
                    |_e: &MouseDownEvent, _w, cx: &mut gpui::App| cx.stop_propagation(),
                ).child(body),
            )
            .into_any_element()
    }

    /// The pairing panel: the QR and the URI, both clickable (the
    /// signer app opens on click; the URI lands on the clipboard
    /// either way), a status line. Closing the modal and reopening
    /// starts a fresh pairing, so there is no retry button.
    fn render_pairing_phase(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(session) = self.connect.as_ref() else {
            return div().into_any_element();
        };
        let qr = session.qr_path.clone();
        let uri = session.uri.clone();
        let uri_for_click = uri.clone();
        let uri_for_qr_click = uri.clone();
        let mut panel = div()
            .w(px(440.))
            .flex()
            .flex_col()
            .gap_3()
            .rounded_md()
            .border_1()
            .border_color(ui::divider())
            .bg(ui::panel())
            .p_4()
            .child(
                div()
                    .text_size(px(ui::TEXT_SIZE_EMPHASIS))
                    .text_color(ui::text())
                    .child("Sign in"),
            )
            .child(
                div().flex().justify_center().child(
                    div()
                        .id("connect-qr")
                        .cursor_pointer()
                        .rounded_sm()
                        .on_click(cx.listener(move |_this, _e: &ClickEvent, _w, cx| {
                            cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                                uri_for_qr_click.clone(),
                            ));
                            cx.open_url(&uri_for_qr_click);
                        }))
                        .child(gpui::img(qr.clone()).w(px(240.)).h(px(240.))),
                ),
            )
            .child(
                div()
                    .id("connect-uri")
                    .cursor_pointer()
                    .w_full()
                    .rounded_sm()
                    .border_1()
                    .border_color(ui::divider())
                    .bg(ui::surface())
                    .px_3()
                    .py_2()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_size(px(ui::TEXT_SIZE_SMALL))
                    .text_color(ui::text_dim())
                    .hover(|style| {
                        style.text_color(ui::text()).border_color(ui::accent())
                    })
                    .on_click(cx.listener(move |_this, _e: &ClickEvent, _w, cx| {
                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                            uri_for_click.clone(),
                        ));
                        cx.open_url(&uri_for_click);
                    }))
                    .child(uri.clone()),
            )
            .child(
                div()
                    .text_size(px(ui::TEXT_SIZE_LABEL))
                    .text_color(ui::text_dim())
                    .child("Click to pair, or scan with a signer app."),
            )
            .child(
                div()
                    .text_size(px(ui::TEXT_SIZE_LABEL))
                    .text_color(ui::text_dim())
                    .child("Waiting for approval…"),
            );
        if let Some(error) = &self.review_error {
            panel = panel.child(
                div()
                    .text_size(px(ui::TEXT_SIZE_LABEL))
                    .text_color(ui::risk_color(Risk::Medium))
                    .child(error.clone()),
            );
        }
        panel.into_any_element()
    }

    /// The name question after a successful pairing: one optional
    /// line, published as a merged kind 0 profile through the signer.
    fn render_name_phase(&mut self, font: gpui::Font, cx: &mut Context<Self>) -> gpui::AnyElement {
        let text = self.review_draft.1.clone();
        let input = self.render_text_input(
            "signin-input",
            &text,
            "Display name (optional)",
            font,
        );
        div()
            .w(px(440.))
            .flex()
            .flex_col()
            .gap_3()
            .rounded_md()
            .border_1()
            .border_color(ui::divider())
            .bg(ui::panel())
            .p_4()
            .child(
                div()
                    .text_size(px(ui::TEXT_SIZE_EMPHASIS))
                    .text_color(ui::text())
                    .child("Paired"),
            )
            .child(input)
            .child(
                div().flex().items_center().gap_3().child(
                    button("btn-start-reviewing", None, "Start reviewing", true).on_click(
                        cx.listener(|this, _e: &ClickEvent, _w, cx| {
                            this.finish_sign_in(cx);
                        }),
                    ),
                ),
            )
            .child(
                div()
                    .text_size(px(ui::TEXT_SIZE_LABEL))
                    .text_color(ui::text_dim())
                    .child("Enter confirms. A name publishes a public profile."),
            )
            .children(self.review_error.clone().map(|error| {
                div()
                    .text_size(px(ui::TEXT_SIZE_LABEL))
                    .text_color(ui::risk_color(Risk::Medium))
                    .child(error)
            }))
            .into_any_element()
    }

    /// The shared single-line text input: StyledText with selection
    /// runs, a static caret, click-to-enter. Used by the review draft
    /// and the sign-in form (the search box keeps its own wiring in
    /// render, where the window's font style is at hand).
    fn render_text_input(
        &mut self,
        id: &'static str,
        text: &str,
        placeholder: &str,
        font: gpui::Font,
    ) -> gpui::Stateful<gpui::Div> {
        let empty = text.is_empty();
        let mut styled = StyledText::new(text.to_string());
        if !empty {
            let (start, end) = self.draft_selection();
            let plain: Hsla = ui::text().into();
            let mut highlighted: Hsla = ui::accent().into();
            highlighted.a = 0.35;
            let mut runs: Vec<TextRun> = Vec::new();
            let make_run = |len: usize, bg: Option<Hsla>| TextRun {
                len,
                font: font.clone(),
                color: plain,
                background_color: bg,
                underline: None,
                strikethrough: None,
            };
            if start > 0 {
                runs.push(make_run(start, None));
            }
            if end > start {
                runs.push(make_run(end - start, Some(highlighted)));
            }
            if end < text.len() {
                runs.push(make_run(text.len() - end, None));
            }
            styled = styled.with_runs(runs);
        }
        let live_layout = styled.layout().clone();
        let prev_layout = self.review_layout.clone();
        if !empty {
            self.review_layout = Some(live_layout);
        }
        let caret_x = if empty {
            Some(px(0.))
        } else {
            prev_layout.and_then(|layout| {
                layout
                    .position_for_index(self.review_cursor.min(text.len()))
                    .map(|pos| pos.x - layout.bounds().origin.x)
            })
        };

        let mut input = div()
            .id(SharedString::from(id))
            .cursor_pointer()
            .relative()
            .flex()
            .items_center()
            .min_h(px(22.))
            .text_size(px(ui::TEXT_SIZE))
            .text_color(if empty { ui::text_dim() } else { ui::text() })
            .when(empty, |slot| slot.child(placeholder.to_string()))
            // the StyledText itself must paint: measurement happens at
            // paint time, and the caret's next-frame position_for_index
            // panics on an unmeasured handle
            .when(!empty, |slot| slot.child(styled));

        if let Some(caret_x) = caret_x {
            input = input.child(
                div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left_0()
                    .flex()
                    .items_center()
                    .child(div().ml(caret_x).w(px(1.5)).h(px(16.)).bg(ui::text())),
            );
        }
        input
    }

    /// The live compose box: star row, draft input with a real caret,
    /// post button, error line.
    fn render_review_compose(&mut self, app_id: &str, font: gpui::Font, cx: &mut Context<Self>) -> gpui::AnyElement {
        let (draft_rating, draft_text) = (self.review_draft.0, self.review_draft.1.clone());

        // star row: five clicks, or keys 1 to 5
        let mut star_row = div().flex().items_center().gap_1();
        for value in 1u8..=5 {
            let filled = draft_rating.is_some_and(|r| r >= value as f32);
            star_row = star_row.child(
                div()
                    .id(SharedString::from(format!("star-{value}")))
                    .cursor_pointer()
                    .text_size(px(ui::TEXT_SIZE_EMPHASIS))
                    .text_color(if filled { ui::accent() } else { ui::text_dim() })
                    .hover(|style| style.text_color(ui::accent()))
                    .on_click(cx.listener(move |this, _e: &ClickEvent, _w, cx| {
                        this.review_draft.0 = Some(value as f32);
                        this.review_error = None;
                        cx.notify();
                    }))
                    .child("★"),
            );
        }

        let input = self.render_text_input(
            "review-input",
            &draft_text,
            "What did you think? Optional; Enter posts your review.",
            font,
        );
        let mut compose = div()
            .flex()
            .flex_col()
            .gap_2()
            .rounded_sm()
            .border_1()
            .border_color(ui::accent())
            .px_3()
            .py_2()
            .child(star_row)
            .child(input);

        compose = compose.child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .child(
                    div()
                        .text_size(px(ui::TEXT_SIZE_LABEL))
                        .text_color(ui::text_dim())
                        .child(if self.review_posting {
                            "Posting…"
                        } else {
                            "Enter posts, Esc keeps editing, 1 to 5 rate"
                        }),
                )
                .child(
                    button("btn-post-review", None, "Post review", true).on_click(
                        cx.listener({
                            let app_id = app_id.to_string();
                            move |this, _e: &ClickEvent, _w, cx| {
                                this.post_review(&app_id, cx);
                            }
                        }),
                    ),
                ),
        );

        if let Some(error) = &self.review_error {
            compose = compose.child(
                div()
                    .text_size(px(ui::TEXT_SIZE_LABEL))
                    .text_color(ui::risk_color(Risk::Medium))
                    .child(error.clone()),
            );
        }

        compose.into_any_element()
    }
}

/// Five-star text: rounded to the nearest star, filled over dim.
fn stars(rating: f32) -> String {
    let filled = rating.round().clamp(0.0, 5.0) as usize;
    let mut out = String::new();
    for _ in 0..filled {
        out.push('★');
    }
    for _ in filled..5 {
        out.push('☆');
    }
    out
}

/// Human relative time for a review's created_at (unix seconds).
fn relative_time(secs: i64) -> String {
    let delta = (chrono_now() - secs).max(0);
    const MINUTE: i64 = 60;
    const HOUR: i64 = 60 * MINUTE;
    const DAY: i64 = 24 * HOUR;
    match delta {
        0..=MINUTE => "just now".into(),
        d if d < HOUR => format!("{} minutes ago", d / MINUTE),
        d if d < DAY => format!("{} hours ago", d / HOUR),
        d => format!("{} days ago", d / DAY),
    }
}

fn chrono_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn render_permissions_static(metadata: &str) -> gpui::AnyElement {
    // Borrow-free re-render of the permissions section (the method version
    // cannot be called from a non-self context).
    let grants = permissions::grants_from_metadata(metadata);
    div()
        .flex()
        .flex_col()
        .gap_2()
        .p_3()
        .rounded_sm()
        .bg(ui::inset())
        .border_1()
        .border_color(ui::divider_soft())
        .child(
            div()
                .text_size(px(ui::TEXT_SIZE_EMPHASIS))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(ui::text())
                .child("Permissions"),
        )
        .when(grants.is_empty(), |section| {
            section.child(
                div()
                    .text_size(px(ui::TEXT_SIZE))
                    .text_color(ui::text_dim())
                    .child("No sandbox grants declared (app runs with flatpak defaults)."),
            )
        })
        .children(grants.iter().map(|grant| {
            // the badge is fixed; the texts live in a shrinkable column
            // so long permission lines wrap inside the box instead of
            // painting past it
            div()
                .flex()
                .items_start()
                .gap_2()
                .text_size(px(ui::TEXT_SIZE))
                .child(
                    badge(risk_label(grant.risk), ui::risk_color(grant.risk)).flex_none(),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap_0p5()
                        .child(
                            div()
                                .w_full()
                                .text_color(ui::text())
                                .child(grant.summary.clone()),
                        )
                        .children((!grant.arg.is_empty()).then(|| {
                            div()
                                .w_full()
                                .text_color(ui::text_dim())
                                .child(grant.arg.clone())
                        })),
                )
        }))
        .into_any_element()
}

impl Render for StoreRoot {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let page_open = self.page_open;
        let selected = self.selected_app().cloned();
        let filter = self.filter;
        let total = self.apps.len();
        let installed_count = self.installed.len();
        let search_focused = self.focus_handle.is_focused(window);

        div()
            .key_context("StoreRoot")
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::key_down))
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .bg(ui::inset())
            .text_color(ui::text())
            // the dropdown click-away overlays paint behind everything,
            // so their close handler runs after every content handler
            // in the bubble chain: a trigger's own toggle (which stops
            // propagation) wins, instead of losing to the close and
            // reopening
            .children(self.render_menu_overlays(cx))
            // top bar: the mark, the search centered across the full
            // window width, channel and account controls on the right
            .child(
                div()
                    .relative()
                    .h(px(46.))
                    .flex()
                    .items_center()
                    .gap_3()
                    .px_4()
                    .border_b_1()
                    .border_color(ui::divider())
                    .bg(ui::panel())
                    .child(ui::icon("icons/orchard.svg", 22., ui::accent()))
                    // the search, centered on the window width: an
                    // absolute layer spanning the bar, content centered
                    .child(
                        div()
                            .absolute()
                            .left_0()
                            .right_0()
                            .top_0()
                            .bottom_0()
                            .flex()
                            .items_center()
                            .justify_center()
                            .child({
                        // The search input: a real caret and selection.
                        // StyledText carries the selection as a styled
                        // run; the layout handle maps byte indices to
                        // pixels (caret x, click position). The caret
                        // queries last frame's handle: one frame stale,
                        // invisible at 120fps.
                        let display: String = self.search.clone();
                        let placeholder = self.search.is_empty();
                        let font = window.text_style().font();
                        let mut styled = StyledText::new(display.clone());
                        if !self.search.is_empty() {
                            let (start, end) = self.selection();
                            let plain: Hsla = ui::text().into();
                            let mut highlighted: Hsla = ui::accent().into();
                            highlighted.a = 0.35;
                            let mut runs: Vec<TextRun> = Vec::new();
                            let make_run = |len: usize, bg: Option<Hsla>| TextRun {
                                len,
                                font: font.clone(),
                                color: plain,
                                background_color: bg,
                                underline: None,
                                strikethrough: None,
                            };
                            if start > 0 {
                                runs.push(make_run(start, None));
                            }
                            if end > start {
                                runs.push(make_run(end - start, Some(highlighted)));
                            }
                            if end < self.search.len() {
                                runs.push(make_run(self.search.len() - end, None));
                            }
                            styled = styled.with_runs(runs);
                        }
                        let prev_layout = self.search_layout.clone();
                        let live_layout = styled.layout().clone();
                        // only a painted text can be queried next frame:
                        // an unrendered layout panics on measurement
                        if !placeholder {
                            self.search_layout = Some(live_layout.clone());
                        }

                        // the caret queries last frame's handle; it has
                        // been measured by now. With the placeholder
                        // showing, the caret belongs at the slot's left
                        // edge without querying anything.
                        let caret_x = if placeholder {
                            Some(px(0.))
                        } else {
                            prev_layout.and_then(|layout| {
                                layout
                                    .position_for_index(self.cursor.min(self.search.len()))
                                    .map(|pos| pos.x - layout.bounds().origin.x)
                            })
                        };

                        div()
                            .id("search-box")
                            .max_w(px(460.))
                            .flex_1()
                            .flex()
                            .items_center()
                            .px_3()
                            .py_1()
                            .rounded_sm()
                            .bg(ui::inset())
                            .border_1()
                            .border_color(if search_focused {
                                ui::accent()
                            } else {
                                ui::divider()
                            })
                            .gap_2()
                            .text_size(px(ui::TEXT_SIZE))
                            .text_color(if self.search.is_empty() {
                                ui::text_dim()
                            } else {
                                ui::text()
                            })
                            .child(ui::icon("icons/search.svg", 13., ui::text_dim()))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, event: &MouseDownEvent, _window, _cx| {
                                    // place the caret where clicked; one
                                    // frame of layout lag, invisible in
                                    // practice. Empty input: everything
                                    // lands at the slot's left edge
                                    if this.search.is_empty() {
                                        this.cursor = 0;
                                        this.anchor = None;
                                        return;
                                    }
                                    let local = event.position - live_layout.bounds().origin;
                                    let ix = match live_layout.index_for_position(local) {
                                        Ok(ix) | Err(ix) => ix,
                                    };
                                    let ix = ix.min(this.search.len()).max(0);
                                    if event.modifiers.shift && this.anchor.is_some() {
                                        this.cursor = ix;
                                    } else {
                                        this.cursor = ix;
                                        this.anchor = None;
                                    }
                                }),
                            )
                            .child(
                                div()
                                    .relative()
                                    .flex_1()
                                    .min_w_0()
                                    .when(placeholder, |slot| {
                                        // the placeholder sits a step
                                        // right of the caret slot so the
                                        // caret does not touch the "T"
                                        slot.child(
                                            div()
                                                .pl(px(4.))
                                                .text_color(ui::text_dim())
                                                .child("Type to search…"),
                                        )
                                    })
                                    .when(!placeholder, |slot| slot.child(styled))
                                    .children(caret_x.map(|x| {
                                        div()
                                            .absolute()
                                            .left(x)
                                            .top(px(4.))
                                            .w(px(1.5))
                                            .h(px(15.))
                                            .bg(ui::accent())
                                    })),
                            )
                    }),
                    )
                    // the flex_1 keeps the right-hand controls at the
                    // edge; the absolute layer above centers the search
                    .child(div().flex_1())
                    .child(self.render_channel_trigger(filter, cx))
                    .child(self.render_account_trigger(cx)),
            )
            // body: browse (category chips + virtualized list) or the
            // full app page
            .child(if page_open {
                match selected {
                    Some(app) => self.render_page(app, window.text_style().font(), cx).into_any_element(),
                    None => self.render_browse(cx),
                }
            } else {
                self.render_browse(cx)
            })
            // status bar
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .px_4()
                    .py_1()
                    .border_t_1()
                    .border_color(ui::divider())
                    .bg(ui::panel())
                    .text_size(px(ui::TEXT_SIZE_SMALL))
                    .text_color(ui::text_dim())
                    .child(match self.flathub_loading {
                        true => format!("{total} apps (loading Flathub…) · {installed_count} installed"),
                        false => format!("{total} apps · {installed_count} installed"),
                    })
                    .child(div().flex_1())
                    .children(self.error.clone().map(|error| {
                        div()
                            .text_color(ui::urgent())
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .child(format!("⚠ {error}"))
                    })),
            )
            .when_some(self.lightbox.clone(), |root, url| {
                // the lightbox: a dimmed overlay over everything with
                // the fetched image contained in the viewport; any
                // click or Escape dismisses it
                root.child(
                    div()
                        .id("lightbox")
                        .absolute()
                        .top_0()
                        .left_0()
                        .w_full()
                        .h_full()
                        .bg(gpui::rgba(0x000000B3))
                        .flex()
                        .items_center()
                        .justify_center()
                        .p_6()
                        .cursor_pointer()
                        .on_click(cx.listener(|this, _e: &ClickEvent, _w, cx| {
                            this.lightbox = None;
                            cx.notify();
                        }))
                        .children(
                            self.fetched
                                .get(&url)
                                .and_then(|slot| slot.clone())
                                .map(|path| {
                                    gpui::img(path)
                                        .size_full()
                                        .object_fit(gpui::ObjectFit::Contain)
                                }),
                        ),
                )
            })
            // dropdown panels and the header sign-in panel paint after
            // everything (the lightbox included)
            .when(self.any_layer_open(), |root| {
                root.child(self.render_layers(window.text_style().font(), cx))
            })
            .id("store-root")
            .on_click(cx.listener(|this, _e: &ClickEvent, window, _cx| {
                // Keep the keyboard attached to the search context.
                window.focus(&this.focus_handle);
            }))
    }
}

impl StoreRoot {
    /// Is any window-level layer (channel dropdown, account dropdown,
    /// sign-in modal) showing?
    fn any_layer_open(&self) -> bool {
        self.channel_menu.is_some() || self.account_menu.is_some() || self.signin_modal
    }

    /// The window-level layers, if any: the dropdown panels and the
    /// sign-in modal, painted after everything (the lightbox
    /// included). The click-away overlays they pair with live behind
    /// the content instead (render_menu_overlays). The panel must
    /// swallow mouse down for exactly this reason: the behind-overlay's
    /// close would otherwise fire after the panel's own handlers and
    /// unmount the panel between the user's down and up, and a click
    /// whose element is gone before the up never fires.
    /// The modal paints last: it covers the dropdowns too.
    fn render_layers(&mut self, font: gpui::Font, cx: &mut Context<Self>) -> gpui::AnyElement {
        let mut layers = div().absolute().inset_0();
        if self.channel_menu.is_some() {
            layers = layers.child(self.render_channel_menu(cx));
        }
        if self.account_menu.is_some() {
            layers = layers.child(self.render_account_menu(cx));
        }
        if self.signin_modal {
            layers = layers.child(self.render_signin_modal(font, cx));
        }
        layers.into_any_element()
    }

    /// The click-away overlay for the open dropdowns, painted behind
    /// everything: transparent, full-window, and last in the bubble
    /// chain. Any click the content does not stop lands here and
    /// closes the dropdowns.
    fn render_menu_overlays(&mut self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        if self.channel_menu.is_none() && self.account_menu.is_none() {
            return None;
        }
        Some(
            div()
                .id("menu-overlay")
                .absolute()
                .inset_0()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _e: &MouseDownEvent, _w, cx| {
                        this.channel_menu = None;
                        this.account_menu = None;
                        cx.notify();
                    }),
                )
                .into_any_element(),
        )
    }

    /// The channel dropdown: filter options with the active one marked.
    /// The panel stops mouse down so its items' clicks survive to the
    /// up, out of reach of the behind-everything overlay.
    fn render_channel_menu(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let anchor = self.channel_menu.unwrap_or_default();
        let left = (anchor.x - px(60.)).max(px(8.));
        div()
            .absolute()
            .left(left)
            .top(anchor.y + px(6.))
            .w(px(180.))
            .flex()
            .flex_col()
            .rounded_sm()
            .border_1()
            .border_color(ui::divider())
            .bg(ui::panel())
            .py_1()
            .on_mouse_down(
                MouseButton::Left,
                |_e: &MouseDownEvent, _w, cx: &mut gpui::App| cx.stop_propagation(),
            )
            .children(
                [
                    Filter::All,
                    Filter::Reviewed,
                    Filter::Publisher,
                    Filter::Installed,
                ]
                .iter()
                .map(|f| {
                    let active = *f == self.filter;
                    div()
                        .id(SharedString::from(format!("channel-item-{}", f.label())))
                        .flex()
                        .items_center()
                        .justify_between()
                        .px_3()
                        .py_1()
                        .cursor_pointer()
                        .text_size(px(ui::TEXT_SIZE))
                        .text_color(if active {
                            ui::accent()
                        } else {
                            ui::text()
                        })
                        .hover(|style| style.bg(ui::surface()))
                        .on_click(cx.listener(move |this, _e: &ClickEvent, _w, cx| {
                            this.filter = *f;
                            this.channel_menu = None;
                            // a pending install confirm does not
                            // survive a channel switch
                            this.confirm_install = None;
                            cx.notify();
                        }))
                        .child(f.label().to_string())
                        .children(active.then(|| "✓".to_string()))
                }),
            )
            .into_any_element()
    }

    /// The account dropdown: the signed-in session and sign-out. The
    /// panel stops mouse down, same as the channel menu: Sign out's
    /// click survives to the up, out of reach of the behind-everything
    /// overlay.
    fn render_account_menu(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let anchor = self.account_menu.unwrap_or_default();
        let left = (anchor.x - px(170.)).max(px(8.));
        let (display, signer) = match self.reviewer.as_ref() {
            Some(reviewer) => (reviewer.display(), Some(reviewer.signer_pubkey.clone())),
            None => (String::new(), None),
        };
        let note = signer
            .as_deref()
            .and_then(|pubkey| self.verified_nip05(pubkey))
            .and_then(|claim| {
                split_nip05(&claim)
                    .map(|(local, domain)| format!("{domain} lists {local} as this key"))
            });
        div()
            .absolute()
            .left(left)
            .top(anchor.y + px(6.))
            .w(px(220.))
            .flex()
            .flex_col()
            .rounded_sm()
            .border_1()
            .border_color(ui::divider())
            .bg(ui::panel())
            .py_1()
            .on_mouse_down(
                MouseButton::Left,
                |_e: &MouseDownEvent, _w, cx: &mut gpui::App| cx.stop_propagation(),
            )
            .child(
                div()
                    .px_3()
                    .py_1()
                    .flex()
                    .gap_1()
                    .items_center()
                    .text_size(px(ui::TEXT_SIZE))
                    .text_color(ui::text())
                    .child(format!("Signed in as {display}"))
                    .children(note.map(|note| {
                        // the domain backed this identity's claim;
                        // the words live on hover
                        div()
                            .id("account-nip05")
                            .flex_none()
                            .hover(|style| style.bg(gpui::transparent_black()))
                            .tooltip(move |_window, cx| {
                                cx.new(|_| TextTooltip(note.clone().into())).into()
                            })
                            .child(ui::icon("icons/check.svg", 11., ui::text()))
                    })),
            )
            .child(
                div()
                    .id("account-sign-out")
                    .px_3()
                    .py_1()
                    .cursor_pointer()
                    .text_size(px(ui::TEXT_SIZE))
                    .text_color(ui::text())                    .hover(|style| style.bg(ui::surface()))
                    .on_click(cx.listener(|this, _e: &ClickEvent, _w, cx| {
                        this.account_menu = None;
                        this.sign_out(cx);
                    }))
                    .child("Sign out"),
            )
            .into_any_element()
    }

    /// The channel dropdown trigger: the active filter with a chevron.
    /// Mouse down, not click: the toggle stops propagation, so the
    /// behind-everything overlay never closes what the trigger just
    /// opened, and a second press closes instead of reopening.
    fn render_channel_trigger(&mut self, filter: Filter, cx: &mut Context<Self>) -> gpui::AnyElement {
        div()
            .id("channel-trigger")
            .flex()
            .items_center()
            .gap_1()
            .px_2()
            .py_1()
            .rounded_sm()
            .cursor_pointer()
            .text_size(px(ui::TEXT_SIZE))
            .text_color(ui::text())
            .hover(|style| style.bg(ui::surface()))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, _w, cx| {
                    let open = this.channel_menu.is_some();
                    this.account_menu = None;
                    this.channel_menu = if open { None } else { Some(event.position) };
                    cx.stop_propagation();
                    cx.notify();
                }),
            )
            .child(filter.label().to_string())
            .child(ui::icon("icons/chevron_down.svg", 11., ui::text_dim()))
            .into_any_element()
    }

    /// The account control: the sign-in entry point when signed out,
    /// the session menu trigger when not.
    fn render_account_trigger(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        // the profile picture when the identity's kind 0 carries one;
        // fetched through the shared cache, glyph until it lands
        let picture = self
            .reviewer
            .as_ref()
            .and_then(|r| self.profiles.get(&r.signer_pubkey))
            .and_then(|p| p.picture.clone());
        if let Some(url) = &picture {
            self.ensure_fetched(vec![url.clone()], cx);
        }
        let path = picture
            .as_deref()
            .and_then(|url| self.fetched.get(url))
            .and_then(|f| f.as_deref());
        match self.reviewer.as_ref() {
            Some(_reviewer) => {
                div()
                    .id("account-trigger")
                    .flex()
                    .items_center()
                    .gap_1()
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .cursor_pointer()
                    .hover(|style| style.bg(ui::surface()))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, event: &MouseDownEvent, _w, cx| {
                            let open = this.account_menu.is_some();
                            this.channel_menu = None;
                            this.account_menu = if open {
                                None
                            } else {
                                Some(event.position)
                            };
                            cx.stop_propagation();
                            cx.notify();
                        }),
                    )
                    .child(avatar(path, 14., ui::text()))
                    .child(ui::icon("icons/chevron_down.svg", 11., ui::text_dim()))
                    .into_any_element()
            }
            None => button("btn-sign-in-header", None, "Sign in", false)
                .on_click(cx.listener(|this, _e: &ClickEvent, _w, cx| {
                    this.open_signin_modal(false, cx);
                }))
                .into_any_element(),
        }
    }
}

// ----- catalog loaders (background) -----

/// Flathub channel: parse the local AppStream for the system install.
/// Session-stability fingerprint of the publisher rows: any change in
/// identity, version, pin, or listing text should re-render; anything
/// else is an idle refresh.
fn orchard_fingerprint(rows: &[StoreApp]) -> String {
    use std::fmt::Write as _;
    let mut fingerprint = String::new();
    for app in rows {
        let _ = writeln!(
            fingerprint,
            "{}\u{1}{}\u{1}{}\u{1}{}\u{1}{}\u{1}{}\u{1}{}",
            app.app_id,
            app.publisher.as_deref().unwrap_or(""),
            app.version.as_deref().unwrap_or(""),
            app.pinned_digest.as_deref().unwrap_or(""),
            app.name,
            app.summary,
            app.successor_to.as_deref().unwrap_or(""),
        );
    }
    fingerprint
}

/// The per-publisher bookkeeping key: two publishers of one app id
/// keep separate digests, fetch states, and verify errors. Reviewed
/// apps have no publisher; their key is the bare id's shape.
fn app_key(app_id: &str, publisher: Option<&str>) -> String {
    format!("{app_id}\u{1}{}", publisher.unwrap_or(""))
}

/// Fold a catalog batch of `(app id, publisher key)` pairs into the
/// continuity record. Keys new for an app id met before are flagged
/// as a change (the fact sticks); an app id met for the first time
/// seeds all its keys unflagged: a fresh client has no history to
/// disclose. True when the record changed.
fn absorb_continuity(
    file: &mut orchard_core::ContinuityFile,
    batch: &[(String, String)],
) -> bool {
    let mut current: std::collections::BTreeMap<&str, Vec<&str>> = Default::default();
    for (app_id, pubkey) in batch {
        current.entry(app_id).or_default().push(pubkey);
    }
    let mut changed = false;
    for (app_id, keys) in &current {
        let had_history = file.apps.contains_key(*app_id);
        let entry = file.apps.entry((*app_id).to_string()).or_default();
        for key in keys {
            if entry.keys.iter().any(|known| known == key) {
                continue;
            }
            changed = true;
            if had_history {
                entry.flagged.push((*key).to_string());
            }
            entry.keys.push((*key).to_string());
        }
    }
    changed
}

/// The continuity record lives beside identity.json: private client
/// state, never published.
fn continuity_path() -> Option<std::path::PathBuf> {
    identity_path().map(|path| path.with_file_name("continuity.json"))
}

/// Load the continuity record. A missing or unreadable file is a
/// fresh client, not an error.
fn load_continuity() -> orchard_core::ContinuityFile {
    let Some(path) = continuity_path() else {
        return Default::default();
    };
    std::fs::read(&path)
        .map(|bytes| serde_json::from_slice(&bytes).unwrap_or_default())
        .unwrap_or_default()
}

fn save_continuity(file: &orchard_core::ContinuityFile) {
    let Some(path) = continuity_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(bytes) = serde_json::to_vec_pretty(file) {
        let _ = std::fs::write(&path, bytes);
    }
}

/// Human byte count, flatpak-style SI units: `9.8 MB`, `1.2 GB`.
fn human_size(bytes: u64) -> String {
    const UNITS: [(&str, u64); 4] = [
        ("GB", 1_000_000_000),
        ("MB", 1_000_000),
        ("kB", 1_000),
        ("B", 1),
    ];
    for (unit, size) in UNITS {
        if bytes >= size {
            let value = bytes as f64 / size as f64;
            return format!("{value:.1} {unit}");
        }
    }
    format!("{bytes} B")
}

/// The honest state for an app whose chain exists but builds nothing
/// for this machine. This one line is the whole arch story: quiet
/// everywhere else, told in full exactly where it matters.
/// One clickable project link row: an icon, the URL, the host's
/// default handler on click. Homepage and repository share the shape.
fn project_link(id: &'static str, icon: &'static str, url: String) -> impl IntoElement {
    let open = url.clone();
    div()
        .id(SharedString::from(format!("link-{id}")))
        .flex()
        .items_center()
        .gap_1p5()
        .cursor_pointer()
        .text_size(px(ui::TEXT_SIZE))
        .text_color(ui::accent())
        .hover(|style| style.text_color(ui::text()))
        .on_click(move |_e: &ClickEvent, _window, _cx| {
            // host default handler, fire and forget
            let _ = std::process::Command::new("xdg-open").arg(&open).spawn();
        })
        .child(ui::icon(icon, 13., ui::accent()))
        .child(url)
}

fn no_build_note(app: &StoreApp) -> String {
    let host = orchard_core::host_ref_arch();
    if app
        .platforms
        .iter()
        .any(|platform| platform == &format!("linux-{host}"))
    {
        "no installable build for this machine yet".to_string()
    } else {
        let published = app
            .platforms
            .iter()
            .filter_map(|platform| platform.strip_prefix("linux-"))
            .collect::<Vec<_>>()
            .join(", ");
        if published.is_empty() {
            format!("no {host} build yet")
        } else {
            format!("no {host} build yet, published for {published}")
        }
    }
}

fn load_flathub() -> Vec<StoreApp> {
    for path in
        orchard_core::appstream::appstream_paths(FLATHUB_REMOTE, &orchard_core::host_ref_arch())
    {
        if !path.exists() {
            continue;
        }
        let icons_root = path
            .parent()
            .map(|active| active.join("icons"))
            .unwrap_or_default();
        if let Ok(apps) = orchard_core::appstream::load(&path, &icons_root) {
            return apps
                .iter()
                .map(StoreApp::from_appstream)
                .collect();
        }
    }
    Vec::new()
}

/// Orchard channel: catalog over HTTP from indexd, file fallback.
fn load_orchard_catalog() -> Vec<StoreApp> {
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(5))
        .build();
    let catalog: Option<CatalogFile> = agent
        .get(&format!("http://127.0.0.1:{INDEXD_PORT}/v1/catalog"))
        .call()
        .ok()
        .and_then(|response| response.into_json::<CatalogFile>().ok())
        .or_else(|| {
            for path in crate::catalog_file_paths() {
                if let Ok(bytes) = std::fs::read(&path) {
                    if let Ok(catalog) = serde_json::from_slice::<CatalogFile>(&bytes) {
                        return Some(catalog);
                    }
                }
            }
            None
        });
    catalog
        .map(|catalog| catalog.apps.iter().map(StoreApp::from_catalog).collect())
        .unwrap_or_default()
}

/// The raw kind 0 metadata per pubkey from indexd, for merging a new
/// display name against the author's existing profile.
fn fetch_profile_raws() -> anyhow::Result<std::collections::BTreeMap<String, String>> {
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(5))
        .build();
    let file: orchard_core::ProfilesFile = agent
        .get(&format!("http://127.0.0.1:{INDEXD_PORT}/v1/profiles"))
        .call()?
        .into_json()?;
    Ok(file.profiles)
}

/// Merge a new display name into the author's existing kind 0
/// metadata. Kind 0 is replaceable, so the merge must round-trip every
/// field the author ever published; a non-object payload (empty or
/// corrupt) starts a fresh object.
fn merge_profile_raw(existing: Option<&str>, name: &str) -> String {
    let mut value: serde_json::Value = existing
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
        .filter(|value| value.is_object())
        .unwrap_or_else(|| serde_json::json!({}));
    value["name"] = serde_json::Value::String(name.to_string());
    value.to_string()
}

/// Reviews from indexd, grouped by app id (newest first within an
/// app), plus the per-reviewer first-seen table.
fn load_reviews() -> anyhow::Result<(
    HashMap<String, Vec<orchard_core::Review>>,
    std::collections::BTreeMap<String, u64>,
)> {
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(5))
        .build();
    let file: orchard_core::ReviewsFile = agent
        .get(&format!("http://127.0.0.1:{INDEXD_PORT}/v1/reviews"))
        .call()?
        .into_json()?;
    let mut grouped: HashMap<String, Vec<orchard_core::Review>> = HashMap::new();
    for review in file.reviews {
        grouped
            .entry(review.app_id.clone())
            .or_default()
            .push(review);
    }
    Ok((grouped, file.first_seen))
}

/// Fetch the reviewer contact-list graph from indexd. Blocking: runs
/// on the background pool.
fn load_graph() -> anyhow::Result<std::collections::BTreeMap<String, Vec<String>>> {
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(5))
        .build();
    let file: orchard_core::GraphFile = agent
        .get(&format!("http://127.0.0.1:{INDEXD_PORT}/v1/graph"))
        .call()?
        .into_json()?;
    Ok(file.graph)
}

/// What the UI needs from a kind 0 profile, parsed once on arrival.
#[derive(Default, Clone)]
struct ProfileBits {
    name: Option<String>,
    picture: Option<String>,
    /// The NIP-05 address the profile claims, `name@domain` or a bare
    /// domain. A claim is marketing until the domain backs it.
    nip05: Option<String>,
}

impl ProfileBits {
    /// Pull the display fields out of a raw kind 0 payload. Extra
    /// fields (and hostile JSON) are not our problem: wrong shapes
    /// parse as absent.
    fn parse(raw: &str) -> ProfileBits {
        let value: serde_json::Value = serde_json::from_str(raw).unwrap_or_default();
        ProfileBits {
            name: value["name"].as_str().map(str::to_string),
            picture: value["picture"].as_str().map(str::to_string),
            nip05: value["nip05"].as_str().map(str::to_string),
        }
    }
}

/// Fetch and parse every profile indexd holds. Blocking: runs on the
/// background pool.
fn load_profiles() -> anyhow::Result<std::collections::BTreeMap<String, ProfileBits>> {
    Ok(fetch_profile_raws()?
        .into_iter()
        .map(|(pubkey, raw)| (pubkey, ProfileBits::parse(&raw)))
        .collect())
}

/// The signed-in identity's own kind 0 from the social relays: the
/// first relay that answers. None when no relay has one. Blocking:
/// runs on the background pool.
fn load_own_profile(pubkey: &str) -> Option<ProfileBits> {
    let timeout = std::time::Duration::from_secs(5);
    for relay in social_relays() {
        match orchard_nostr::relay::fetch_profile_raw(&relay, pubkey, timeout) {
            Ok(Some(raw)) => return Some(ProfileBits::parse(&raw)),
            Ok(None) => {}
            Err(err) => eprintln!("own profile: {relay}: {err:#}"),
        }
    }
    None
}

/// A NIP-05 verdict: the claim that was checked and whether the
/// domain's well-known record named this key.
#[derive(Clone)]
struct Nip05Verdict {
    claim: String,
    verified: bool,
}

/// Split a NIP-05 claim into (local part, domain). A bare domain is
/// the `_` local part. Anything the URL would lie about (empty
/// pieces, a second `@`, a path) is not a claim.
fn split_nip05(claim: &str) -> Option<(String, String)> {
    let claim = claim.trim().to_ascii_lowercase();
    if claim.is_empty() {
        return None;
    }
    let (local, domain) = match claim.split_once('@') {
        Some((local, domain)) => (local, domain),
        None => ("_", claim.as_str()),
    };
    let ok = |piece: &str| {
        !piece.is_empty()
            && !piece.contains(['@', '/', '?', '#', ' '])
    };
    (ok(local) && ok(domain)).then(|| (local.to_string(), domain.to_string()))
}

/// The domain's NIP-05 record: the names it publishes, keyed by local
/// part. None when the fetch or the JSON fails; hostile payloads are
/// unverified, never a guess. `base` is the scheme plus host, so the
/// tests can point at a local server.
fn fetch_nip05_names(base: &str, local: &str) -> Option<std::collections::BTreeMap<String, String>> {
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(5))
        .build();
    let record: serde_json::Value = agent
        .get(&format!("{base}/.well-known/nostr.json?name={local}"))
        .call()
        .ok()?
        .into_json()
        .ok()?;
    let names = record.get("names")?.as_object()?;
    Some(
        names
            .into_iter()
            .filter_map(|(local, key)| {
                let key = key.as_str()?.to_string();
                Some((local.to_string(), key))
            })
            .collect(),
    )
}

/// The claim checks out when the base url's well-known record names
/// this exact key. Case-insensitive, per the NIP. Blocking: runs on
/// the background pool.
fn claim_matches_record(base: &str, local: &str, pubkey: &str) -> bool {
    fetch_nip05_names(base, local)
        .and_then(|names| names.get(local).cloned())
        .is_some_and(|key| key.eq_ignore_ascii_case(pubkey))
}

/// NIP-05 verification: the domain behind the claim must name this
/// key. Any failure is unverified: the check is a fact earned, never
/// a guess. Blocking: runs on the background pool.
fn verify_nip05(claim: &str, pubkey: &str) -> bool {
    let Some((local, domain)) = split_nip05(claim) else {
        return false;
    };
    claim_matches_record(&format!("https://{domain}"), &local, pubkey)
}

/// Sign and send one review event through the signer, then publish it
/// to the review relays. Blocking: runs on the background pool.
fn post_review(reviewer: &Reviewer, app_id: &str, rating: f32, content: String) -> anyhow::Result<()> {
    let mut tags: Vec<Vec<String>> = vec![vec!["d".into(), app_id.to_string()]];
    tags.push(vec!["rating".into(), format!("{rating:.1}")]);
    send_signed(reviewer, orchard_nostr::KIND_REVIEW as u16, tags, &content)?;
    Ok(())
}

/// Relays to post reviews to: the same list the embedded indexd reads
/// from (ORCHARD_RELAYS or the rendezvous set).
fn review_relays() -> Vec<String> {
    orchard_nostr::relay::relay_urls(std::env::var("ORCHARD_RELAYS").ok().as_deref())
}

/// Social relays to read the signed-in identity's own contact list
/// from (ORCHARD_SOCIAL_RELAYS or a default public set). Read-only,
/// anonymous: the fetch carries only the key, nothing else.
fn social_relays() -> Vec<String> {
    orchard_nostr::relay::social_relay_urls(
        std::env::var("ORCHARD_SOCIAL_RELAYS").ok().as_deref(),
    )
}

/// Read the identity's contact list from every social relay; the
/// union of the `p` tags each serves. Blocking: runs on the
/// background pool. Relays that fail shrink the union silently; the
/// caller keeps the old set when the union comes back empty.
fn load_follows(pubkey: &str) -> std::collections::HashSet<String> {
    let timeout = std::time::Duration::from_secs(5);
    let mut follows = std::collections::HashSet::new();
    for relay in social_relays() {
        match orchard_nostr::relay::fetch_contacts(&relay, pubkey, timeout) {
            Ok(contacts) => follows.extend(contacts),
            Err(err) => eprintln!("follows: {relay}: {err:#}"),
        }
    }
    follows
}

/// A live pairing session: the ephemeral client key, the QR image and
/// URI on screen, and the relay both ends must reach. The
/// anti-spoofing secret rides in the poll loop that waits for the
/// connect response.
struct ConnectSession {
    keys: nostr::prelude::Keys,
    qr_path: std::path::PathBuf,
    uri: String,
    relay: String,
}

/// The signed-in reviewer: a NIP-46 session, not a key. `keys` is the
/// ephemeral client keypair the connect channel encrypts with; the
/// identity reviews carry is the REMOTE signer's pubkey, and every
/// signature is a `sign_event` request the user approves on their
/// device. The client never sees the identity's secret key.
#[derive(Clone)]
pub struct Reviewer {
    keys: nostr::prelude::Keys,
    pub signer_pubkey: String,
    pub secret: String,
    pub relay: String,
    pub name: Option<String>,
}

impl Reviewer {
    /// What to call this reviewer in the UI: the published profile
    /// name, else a truncated npub (the format Nostr users expect,
    /// not raw hex).
    pub fn display(&self) -> String {
        self.name
            .clone()
            .unwrap_or_else(|| short_npub(&self.signer_pubkey))
    }
}

/// A round avatar from a cached image path; a person glyph when
/// there is none (or the fetch has not landed yet).
fn avatar(path: Option<&std::path::Path>, size: f32, fallback: gpui::Rgba) -> gpui::AnyElement {
    match path {
        // the radius must sit on the img itself: gpui does not clip
        // children to the parent's rounded corners, so a rounded
        // wrapper alone leaves a square picture
        Some(path) => div()
            .flex_none()
            .w(px(size))
            .h(px(size))
            .child(
                gpui::img(path)
                    .size_full()
                    .object_fit(gpui::ObjectFit::Cover)
                    .rounded_full(),
            )
            .into_any_element(),
        None => ui::icon("icons/person.svg", size * 0.85, fallback),
    }
}

/// The identity's fallback display: a truncated npub, head and tail
/// around an ellipsis, the format Nostr users expect, not raw hex.
/// The full npub is the identifier of record; a label is not the
/// place for 63 characters. Unparseable keys keep the hex fragment.
fn short_npub(pubkey: &str) -> String {
    use nostr::nips::nip19::ToBech32 as _;
    let npub = nostr::prelude::PublicKey::from_hex(pubkey)
        .ok()
        .and_then(|key| key.to_bech32().ok());
    match npub {
        Some(npub) if npub.len() > 19 => {
            format!("{}…{}", &npub[..14], &npub[npub.len() - 4..])
        }
        Some(npub) => npub,
        None => format!("{}…", &pubkey[..pubkey.len().min(12)]),
    }
}

/// Load the saved NIP-46 session, if one is stored. The file holds the
/// ephemeral client key, the signer relationship, and the optional
/// locally chosen name; it survives sign-outs (the signer holds the
/// real key, so this file is not the identity).
fn load_reviewer() -> Option<Reviewer> {
    let path = identity_path()?;
    let raw = std::fs::read_to_string(path).ok()?;
    let saved: serde_json::Value = serde_json::from_str(&raw).ok()?;
    // files from the nsec-key experiment have no signer_pubkey: they
    // are ignored, the flow starts over with a pairing
    let keys = nostr::prelude::Keys::parse(saved["client_key"].as_str()?).ok()?;
    Some(Reviewer {
        signer_pubkey: saved["signer_pubkey"].as_str()?.to_string(),
        secret: saved["secret"].as_str()?.to_string(),
        relay: saved["relay"].as_str()?.to_string(),
        name: saved["name"].as_str().map(str::to_string),
        keys,
    })
}

/// Persist the NIP-46 session: the client's ephemeral key, the signer
/// relationship, and the optional published name.
fn save_reviewer(reviewer: &Reviewer) -> anyhow::Result<()> {
    let path = identity_path().ok_or_else(|| anyhow::anyhow!("XDG_DATA_HOME not set"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let saved = serde_json::json!({
        "client_key": reviewer.keys.secret_key().to_secret_hex(),
        "signer_pubkey": reviewer.signer_pubkey,
        "secret": reviewer.secret,
        "relay": reviewer.relay,
        "name": reviewer.name,
    });
    std::fs::write(&path, serde_json::to_vec_pretty(&saved)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// Sign one event through the signer (NIP-46 `sign_event`) and publish
/// it to the review relays. Blocking: runs on the background pool.
fn send_signed(
    reviewer: &Reviewer,
    kind: u16,
    tags: Vec<Vec<String>>,
    content: &str,
) -> anyhow::Result<()> {
    let unsigned = serde_json::json!({
        "pubkey": reviewer.signer_pubkey,
        "created_at": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs(),
        "kind": kind,
        "tags": tags,
        "content": content,
    })
    .to_string();
    let event = orchard_nostr::connect::sign_event(
        &reviewer.relay,
        &reviewer.keys,
        &reviewer.signer_pubkey.parse()?,
        &unsigned,
    )?;
    for url in review_relays() {
        orchard_nostr::relay::send(&url, std::slice::from_ref(&event))?;
    }
    Ok(())
}

fn identity_path() -> Option<std::path::PathBuf> {
    if let Some(data) = std::env::var_os("XDG_DATA_HOME") {
        return Some(std::path::PathBuf::from(data).join("orchard/identity.json"));
    }
    std::env::var_os("HOME").map(|home| {
        std::path::PathBuf::from(home).join(".local/share/orchard/identity.json")
    })
}

/// Where the pairing QR lands: the XDG cache when set, `$HOME/.cache`
/// otherwise, /tmp last. Unique names per call dodge the image cache.
fn qr_cache_dir() -> std::path::PathBuf {
    if let Some(cache) = std::env::var_os("XDG_CACHE_HOME") {
        return std::path::PathBuf::from(cache).join("orchard");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return std::path::PathBuf::from(home).join(".cache/orchard");
    }
    std::env::temp_dir().join("orchard")
}

/// The relay the connect channel rides: a public NIP-46 relay, so a
/// signer app on a phone or another desktop can reach it too.
/// Override with ORCHARD_CONNECT_RELAY.
fn connect_relay() -> String {
    match std::env::var("ORCHARD_CONNECT_RELAY") {
        Ok(value) if !value.trim().is_empty() => value.trim().to_string(),
        _ => "wss://relay.nip46.com".to_string(),
    }
}

/// Parse `flatpak list --app --columns=application,version,origin,installation`.
fn list_installed() -> HashMap<String, InstalledInfo> {
    let Ok(output) = orchard_flatpak::list_installed().output() else {
        return HashMap::new();
    };
    let text = String::from_utf8_lossy(&output.stdout);
    text.lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let app_id = fields.next()?.trim().to_string();
            if app_id.is_empty() {
                return None;
            }
            let version = fields
                .next()
                .map(|f| f.trim().to_string())
                .filter(|version| !version.is_empty() && version != "—");
            let _origin = fields.next().map(|f| f.trim().to_string());
            Some((
                app_id.clone(),
                InstalledInfo {
                    version,
                    size: installed_size(&app_id),
                    metadata: orchard_flatpak::installed_metadata(&app_id),
                },
            ))
        })
        .collect()
}

/// The "Installed Size" line of `flatpak info`, e.g. `1.0 GB`. None when
/// flatpak or the app is missing.
fn installed_size(app_id: &str) -> Option<String> {
    let output = orchard_flatpak::info(app_id).output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.trim().strip_prefix("Installed Size:"))
        .map(str::trim)
        .filter(|size| !size.is_empty())
        .map(String::from)
}

/// True when the remote's version is strictly newer than the
/// installed one. Dot components compare numerically; anything
/// non-numeric is inconclusive and reads as up to date — flatpak's
/// deploy version `1.2.95.453.g0eeebbed` must not read as an update
/// against the appstream's `1.2.95.453`.
fn version_outdated(installed: &str, available: &str) -> bool {
    if installed == available {
        return false;
    }
    let mut installed = installed.split('.');
    let mut available = available.split('.');
    loop {
        match (installed.next(), available.next()) {
            (Some(a), Some(b)) => match (a.parse::<u64>(), b.parse::<u64>()) {
                (Ok(a), Ok(b)) if a != b => return b > a,
                // a non-numeric component on either side: different
                // versioning scheme, cannot judge, stay quiet
                (Err(_), _) | (Ok(_), Err(_)) => return false,
                _ => {}
            },
            // trailing numeric components mean the remote moved ahead
            (None, Some(b)) => return b.parse::<u64>().is_ok_and(|n| n > 0),
            (Some(_), None) | (None, None) => return false,
        }
    }
}

/// Update-available detection for one app: both versions must be
/// known and the remote's strictly newer by the numeric rule.
fn update_available(app: &StoreApp, installed: &InstalledInfo) -> bool {
    match (&app.version, &installed.version) {
        (Some(available), Some(installed)) => version_outdated(installed, available),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orchard_core::{CatalogApp, Publisher};

    #[test]
    fn version_outdated_numeric_compare() {        assert!(version_outdated("0.5.21", "0.5.22"));
        assert!(version_outdated("1.2.9", "1.2.10"));
        assert!(!version_outdated("0.5.22", "0.5.22"));
        assert!(!version_outdated("0.5.23", "0.5.22"));
        // flatpak deploy suffixes must not read as updates
        assert!(!version_outdated(
            "1.2.95.453.g0eeebbed",
            "1.2.95.453"
        ));
        assert!(!version_outdated("1.23.2b", "1.23.2b-x"));
        assert!(!version_outdated("1.0", "1.0.0"));
        // trailing numeric components mean the remote moved ahead
        assert!(version_outdated("1.2", "1.2.1"));
        assert!(!version_outdated("1.2.1", "1.2"));
    }

    #[test]
    fn profile_merge_keeps_published_fields() {
        // kind 0 is replaceable: the merge must round-trip everything
        // the author already published, only the name is ours
        let merged = merge_profile_raw(
            Some(r#"{"name":"Old","nip05":"me@example.com","picture":"https://x/y.png"}"#),
            "New",
        );
        let value: serde_json::Value = serde_json::from_str(&merged).unwrap();
        assert_eq!(value["name"], "New");
        assert_eq!(value["nip05"], "me@example.com");
        assert_eq!(value["picture"], "https://x/y.png");

        // no existing profile (fresh key): a fresh object
        let merged = merge_profile_raw(None, "Fresh");
        let value: serde_json::Value = serde_json::from_str(&merged).unwrap();
        assert_eq!(value["name"], "Fresh");
        assert!(value.get("nip05").is_none());

        // corrupt payload: start over rather than carry junk
        let merged = merge_profile_raw(Some("not json"), "New");
        let value: serde_json::Value = serde_json::from_str(&merged).unwrap();
        assert_eq!(value["name"], "New");
    }

    #[test]
    fn orchard_fingerprint_changes_with_updates() {
        let mut app = StoreApp::from_catalog(&CatalogApp {
            app_id: "com.example.Hello".into(),
            name: "Hello".into(),
            summary: "A demo app".into(),
            description: None,
            icon_url: None,
            categories: Vec::new(),
            publisher: Publisher {
                pubkey: "k".into(),
                nip05: None,
            },
            version: "1.0.0".into(),
            platforms: vec!["linux-x86_64".into()],
            flatpak_ref: "app/com.example.Hello/x86_64/stable".into(),
            location: "oci:/srv/hello".into(),
            digest: Some("sha256:aa".into()),
            metadata: None,
            repository: None,
            homepage: None,
            notes: None,
            published_at: 0,
            successor_to: None,
        });
        let same = orchard_fingerprint(std::slice::from_ref(&app));
        assert_eq!(same, orchard_fingerprint(std::slice::from_ref(&app)));
        // a new release changes the fingerprint (row + page must refresh)
        let mut updated = app.clone();
        updated.version = Some("1.1.0".into());
        assert_ne!(same, orchard_fingerprint(std::slice::from_ref(&updated)));
        // so does a listing edit
        app.summary = "Edited summary".into();
        assert_ne!(same, orchard_fingerprint(std::slice::from_ref(&app)));
    }

    fn trust_review(pubkey: &str, rating: Option<f32>) -> orchard_core::Review {
        orchard_core::Review {
            app_id: "com.orchard.Hello".into(),
            pubkey: pubkey.into(),
            name: None,
            nip05: None,
            rating,
            content: String::new(),
            created_at: 0,
        }
    }

    #[test]
    fn publisher_reviews_are_flagged_and_excluded() {
        let reviews = vec![
            trust_review("pub-a", Some(5.0)),
            trust_review("pub-b", Some(1.0)),
            trust_review("pub-a", None),
        ];
        let aggregate = reviews_aggregate(
            &reviews,
            Some("pub-a"),
            &Default::default(),
            0,
            &Default::default(),
            &Default::default(),
        );
        assert_eq!(aggregate.average, Some(1.0));
        assert_eq!(aggregate.counted, 1);
        assert_eq!(aggregate.new_keys, 0);
        assert_eq!(aggregate.publisher_reviews, 2);
    }

    #[test]
    fn new_keys_are_counted_but_never_excluded() {
        let reviews = vec![
            trust_review("fresh", Some(5.0)),
            trust_review("old", Some(1.0)),
        ];
        let mut first_seen = std::collections::BTreeMap::new();
        first_seen.insert("fresh".to_string(), 0);
        // the fresh key is first seen a minute before now; the other
        // key is absent from the table, which means unknown, not new
        let aggregate =
            reviews_aggregate(&reviews, None, &first_seen, 60, &Default::default(), &Default::default());
        assert_eq!(aggregate.average, Some(3.0));
        assert_eq!(aggregate.counted, 2);
        assert_eq!(aggregate.new_keys, 1);
        assert_eq!(aggregate.publisher_reviews, 0);
    }

    #[test]
    fn short_npub_truncates_and_falls_back() {
        // the demo publisher key, bech32-encoded, head…tail
        let npub = short_npub("ec2c39e7a8a4c38b9c8b9e3c8f3ab4b09bd0e3b98b6c1b3b0f2e3d4c5b6a7988");
        assert!(npub.starts_with("npub1"));
        // the ellipsis sits in the middle, not at the end
        let mid = npub.find('…').expect("ellipsis");
        assert_eq!(mid, 14);
        // head 14 + ellipsis + tail 4, in characters (bytes would
        // count the ellipsis as three)
        assert_eq!(npub.chars().count(), 19);
        // garbage keys keep the hex fragment
        assert_eq!(short_npub("not-hex"), "not-hex…");
    }

    #[test]
    fn circle_reviews_are_counted_and_averaged() {        let reviews = vec![
            trust_review("friend", Some(4.0)),
            trust_review("stranger", Some(1.0)),
            trust_review("mute", None),
        ];
        let mut follows = std::collections::HashSet::new();
        follows.insert("friend".to_string());
        follows.insert("mute".to_string());
        let mut first_seen = std::collections::BTreeMap::new();
        first_seen.insert("friend".to_string(), 0);
        let aggregate =
            reviews_aggregate(&reviews, None, &first_seen, 60, &follows, &Default::default());
        assert_eq!(aggregate.circle, 2);
        assert_eq!(aggregate.circle_average, Some(4.0));
        // the global aggregate is unchanged by the circle lens
        assert_eq!(aggregate.average, Some(2.5));
        assert_eq!(aggregate.counted, 3);
        // the followed key also shows its sprout
        assert_eq!(aggregate.new_keys, 1);
    }

    #[test]
    fn continuity_flags_only_changes_with_history() {
        let mut file = orchard_core::ContinuityFile::default();
        // first sight: two publishers of one app id seed unflagged (a
        // fresh client has no history to disclose); the record saves
        let batch = vec![
            ("com.x.A".to_string(), "k1".to_string()),
            ("com.x.A".to_string(), "k2".to_string()),
        ];
        assert!(absorb_continuity(&mut file, &batch));
        assert_eq!(file.apps["com.x.A"].flagged.len(), 0);
        // the same keys again: nothing changed
        assert!(!absorb_continuity(&mut file, &batch));
        // a third key joins a known app id: a change, flagged once
        let third = vec![("com.x.A".to_string(), "k3".to_string())];
        assert!(absorb_continuity(&mut file, &third));
        assert_eq!(file.apps["com.x.A"].flagged, vec!["k3".to_string()]);
        // and it stays flagged: the fact is not un-seen
        assert!(!absorb_continuity(&mut file, &third));
        assert_eq!(file.apps["com.x.A"].flagged, vec!["k3".to_string()]);
        // a second app id, first sight: unflagged even though the key
        // was seen elsewhere
        let other = vec![("com.x.B".to_string(), "k1".to_string())];
        assert!(absorb_continuity(&mut file, &other));
        assert_eq!(file.apps["com.x.B"].flagged.len(), 0);
        assert_eq!(file.apps["com.x.A"].keys.len(), 3);
        assert_eq!(file.apps["com.x.B"].keys, vec!["k1".to_string()]);
    }

    #[test]
    fn profile_bits_parses_display_fields() {
        let bits = ProfileBits::parse(r#"{"name":"Marta","picture":"https://x/m.png","nip05":"marta@x.org"}"#);
        assert_eq!(bits.name.as_deref(), Some("Marta"));
        assert_eq!(bits.picture.as_deref(), Some("https://x/m.png"));
        assert_eq!(bits.nip05.as_deref(), Some("marta@x.org"));
        // wrong shapes parse as absent, never crash
        let junk = ProfileBits::parse("not json");
        assert!(junk.name.is_none());
        assert!(junk.picture.is_none());
        assert!(junk.nip05.is_none());
        let empty = ProfileBits::parse(r#"{"about":""}"#);
        assert!(empty.name.is_none());
        assert!(empty.picture.is_none());
        assert!(empty.nip05.is_none());
    }

    #[test]
    fn nip05_claims_split_or_refuse() {
        let claim = |claim| split_nip05(claim);
        assert_eq!(
            claim("me@example.com"),
            Some(("me".into(), "example.com".into()))
        );
        // uppercase and stray spaces are presentation, not identity
        assert_eq!(
            claim("  ME@Example.COM "),
            Some(("me".into(), "example.com".into()))
        );
        // a bare domain claims the `_` local part
        assert_eq!(
            claim("example.com"),
            Some(("_".into(), "example.com".into()))
        );
        // anything the url would lie about refuses
        assert_eq!(claim(""), None);
        assert_eq!(claim("@example.com"), None);
        assert_eq!(claim("me@"), None);
        assert_eq!(claim("a@b@c"), None);
        assert_eq!(claim("a@example.com/x"), None);
    }

    /// The record check against a live local server: the domain's
    /// names decide, and everything else is unverified.
    #[test]
    fn nip05_check_against_a_local_record() {
        let record = r#"{"names": {"alice": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "bob": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{record}",
            record.len()
        );
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            use std::io::Write as _;
            stream.write_all(response.as_bytes()).unwrap();
        });
        let base = format!("http://127.0.0.1:{port}");
        let alice = "A".repeat(64);
        // the record names alice with the same key, case-insensitively
        assert!(claim_matches_record(&base, "alice", &alice));
        // bob's claim against alice's key is not verified
        assert!(!claim_matches_record(&base, "bob", &alice));
        // a claim for a name the record does not carry: unverified
        assert!(!claim_matches_record(&base, "carol", &alice));
        // a malformed claim never checks out
        assert!(!verify_nip05("@", &alice));
    }

    #[test]
    fn circle2_is_two_hops_from_the_graph() {
        // the identity ("me") follows friend and mute; friend follows
        // twohop, the identity themself, and someone already followed
        let mut follows = std::collections::HashSet::new();
        follows.insert("friend".to_string());
        follows.insert("mute".to_string());
        let mut graph = std::collections::BTreeMap::new();
        graph.insert(
            "friend".to_string(),
            vec![
                "twohop".to_string(),
                "me".to_string(),
                "mute".to_string(),
            ],
        );
        let two = circle2_set(&follows, &graph, Some("me"));
        assert_eq!(two, std::collections::HashSet::from_iter(["twohop".to_string()]));

        // the aggregate counts a two-hop review in circle2, not circle
        let reviews = vec![trust_review("twohop", Some(3.0)), trust_review("stranger", Some(2.0))];
        let aggregate =
            reviews_aggregate(&reviews, None, &Default::default(), 0, &follows, &two);
        assert_eq!(aggregate.circle, 0);
        assert_eq!(aggregate.circle2, 1);
        assert_eq!(aggregate.counted, 2);
        // signed out there are no follows, so no two hops at all
        assert!(circle2_set(&Default::default(), &graph, None).is_empty());
    }
}
