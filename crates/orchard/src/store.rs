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

/// The default dev relay, shared by the embedded indexd and review posts.
const LOCAL_RELAY: &str = "ws://127.0.0.1:8008";

const FLATHUB_REMOTE: &str = "flathub";
const APPSTREAM_ARCH: &str = "x86_64";

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
    jobs: HashMap<String, JobState>,
    /// app_id -> verified manifest digest (publisher channel, post-verify).
    verified: HashMap<String, String>,
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
    /// Failed pre-install digest checks: app id -> why. Security
    /// relevant, so it shows on the page, not just in the log.
    verify_errors: HashMap<String, String>,
    /// Reviews from indexd (kind 30065): app id -> newest first.
    reviews: HashMap<String, Vec<orchard_core::Review>>,
    /// A reviews fetch in flight (whole document; small at this scale).
    reviews_fetching: bool,
    /// The reviews list is collapsed by default; the header toggles it.
    reviews_expanded: bool,
    /// The signed-in reviewer: the Nostr key reviews are signed with.
    /// None until the user signs in; the key file survives restarts.
    reviewer: Option<Reviewer>,
    /// The sign-in form is open: step one asks for the key, step two
    /// for an optional published name.
    signin_open: bool,
    signin_step: SigninStep,
    /// Where the sign-in form shows: the header account panel or the
    /// reviews section of the open page.
    signin_origin: SigninOrigin,
    /// The header sign-in panel's anchor (the click point).
    signin_anchor: Option<gpui::Point<gpui::Pixels>>,
    /// The key parsed in step one, signed in at the end of step two.
    pending_keys: Option<nostr::prelude::Keys>,
    /// The nsec of a freshly generated key, shown once for backup.
    new_key_notice: Option<String>,
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
            jobs: HashMap::new(),
            verified: HashMap::new(),
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
            reviews_fetching: false,
            reviews_expanded: false,
            reviewer: load_reviewer(),
            signin_open: false,
            signin_step: SigninStep::Key,
            signin_origin: SigninOrigin::Reviews,
            signin_anchor: None,
            pending_keys: None,
            new_key_notice: None,
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
        self.apps.iter().find(|app| &app.app_id == id)
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
            .apps
            .iter()
            .find(|a| a.app_id == app_id)
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
        let source = self.apps.iter().find(|a| a.app_id == app_id).map(|a| a.source);
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
        if (self.remote_metadata.contains_key(app_id) && self.remote_sizes.contains_key(app_id))
            || !self.remote_fetching.insert(app_id.to_string())
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
                    this.remote_metadata.insert(app_id.clone(), metadata);
                }
                if let Some(sizes) = sizes {
                    this.remote_sizes.insert(app_id.clone(), sizes);
                }
                this.remote_fetching.remove(&app_id);
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
        // an app that vanished (unpublished, key revoked) closes the page
        if let Some(selected) = self.selected.clone() {
            if !incoming.iter().any(|app| app.app_id == selected)
                && !self.apps.iter().any(|app| app.app_id == selected)
            {
                self.selected = None;
                self.page_open = false;
            }
        }
        self.apps.extend(incoming);
        cx.notify();
    }

    /// Pre-install artifact check for publisher apps: one manifest fetch
    /// verifies the pinned digest and reads the sandbox labels, so the
    /// page shows exactly what install will pin. Same fetch the install
    /// button runs; failures surface on the page and retry on the next
    /// page open.
    fn ensure_publisher_data(&mut self, app_id: &str, cx: &mut Context<Self>) {
        if self.verified.contains_key(app_id)
            || self.verify_errors.contains_key(app_id)
            || !self.publisher_fetching.insert(app_id.to_string())
        {
            return;
        }
        let Some(app) = self.apps.iter().find(|a| a.app_id == app_id) else {
            self.publisher_fetching.remove(app_id);
            return;
        };
        let location = match app
            .location
            .as_deref()
            .and_then(|raw| oci::Location::parse(raw).ok())
        {
            Some(location) => location,
            None => {
                self.publisher_fetching.remove(app_id);
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
                this.publisher_fetching.remove(&app_id);
                match report {
                    Ok(report) => {
                        this.verified.insert(app_id.clone(), report.digest.clone());
                        if let Some(app) = this.apps.iter_mut().find(|a| a.app_id == app_id) {
                            if app.metadata.is_none() {
                                app.metadata = report.metadata.clone();
                            }
                            this.remote_sizes.insert(
                                app_id.clone(),
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
                            .insert(app_id.clone(), format!("{err:#}"));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    // ----- reviews (kind 30065) -----

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
                    Ok(reviews) => {
                        this.reviews = reviews;
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
        let reviewer = self.reviewer.as_ref().unwrap();
        let Some(rating) = self.review_draft.0 else {
            self.review_error = Some("Pick a rating first (keys 1 to 5).".into());
            cx.notify();
            return;
        };
        let key_hex = reviewer.keys.secret_key().to_secret_hex();
        let content = self.review_draft.1.trim().to_string();
        self.review_posting = true;
        self.review_error = None;
        let app_id = app_id.to_string();
        cx.spawn(async move |this, cx| {
            let posted = cx
                .background_executor()
                .spawn(async move { post_review(&key_hex, &app_id, rating, content) })
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
    /// the sign-in form when not. Both expand the section.
    fn start_review(&mut self, cx: &mut Context<Self>) {
        self.reviews_expanded = true;
        self.review_error = None;
        if self.reviewer.is_some() {
            self.composing = true;
        } else {
            self.close_signin();
            self.signin_open = true;
            self.signin_origin = SigninOrigin::Reviews;
        }
        cx.notify();
    }

    /// Open the header account panel: the sign-in form when signed
    /// out, a session menu when not. Any open dropdown closes first.
    fn open_account_menu(&mut self, anchor: gpui::Point<gpui::Pixels>, cx: &mut Context<Self>) {
        self.channel_menu = None;
        if self.reviewer.is_some() {
            self.account_menu = Some(anchor);
            cx.notify();
            return;
        }
        self.close_signin();
        self.signin_open = true;
        self.signin_origin = SigninOrigin::Header;
        self.signin_anchor = Some(anchor);
        cx.notify();
    }

    /// Sign-in step one: parse the pasted key (nsec or hex). Valid
    /// keys move the form to the optional name question.
    fn accept_nsec(&mut self, cx: &mut Context<Self>) {
        use nostr::prelude::*;
        let raw = self.review_draft.1.trim();
        match Keys::parse(raw) {
            Ok(keys) => {
                self.pending_keys = Some(keys);
                self.signin_step = SigninStep::Name;
                self.review_draft = (None, String::new());
                self.review_cursor = 0;
                self.review_anchor = None;
                self.review_layout = None;
                self.review_error = None;
            }
            Err(_) => {
                self.review_error = Some(
                    "That does not parse as a Nostr key; paste nsec1… or 64 hex characters."
                        .into(),
                );
            }
        }
        cx.notify();
    }

    /// A fresh key for reviewers without one: nothing is written until
    /// sign-in completes, so this notice is the one chance to copy it.
    fn new_identity(&mut self, cx: &mut Context<Self>) {
        use nostr::nips::nip19::ToBech32 as _;
        use nostr::prelude::*;
        let keys = Keys::generate();
        // Err is Infallible: the encoding cannot fail
        let nsec = keys
            .secret_key()
            .to_bech32()
            .expect("nsec bech32 encoding is infallible");
        self.new_key_notice = Some(nsec);
        self.pending_keys = Some(keys);
        self.signin_step = SigninStep::Name;
        self.review_draft = (None, String::new());
        self.review_cursor = 0;
        self.review_anchor = None;
        self.review_layout = None;
        self.review_error = None;
        cx.notify();
    }

    /// Sign-in step two: persist the key (plus the optional published
    /// name), close the form, drop into compose.
    fn finish_sign_in(&mut self, cx: &mut Context<Self>) {
        let Some(keys) = self.pending_keys.take() else {
            self.signin_step = SigninStep::Key;
            cx.notify();
            return;
        };
        let name = self.review_draft.1.trim().to_string();
        let name = (!name.is_empty()).then_some(name);
        let from_reviews = self.signin_origin == SigninOrigin::Reviews;
        cx.spawn(async move |this, cx| {
            use nostr::key::GetPublicKey as _;
            let done = cx.background_executor().spawn(async move {
                save_reviewer(&keys, name.as_deref())?;
                let pubkey = keys.get_public_key()?.to_string();
                let mut warning = None;
                if let Some(published) = &name {
                    // proper profile management: merge against the
                    // author's latest kind 0 so every field they ever
                    // published survives; when indexd cannot answer,
                    // publish nothing rather than guess
                    match fetch_profile_raws() {
                        Ok(raws) => {
                            let merged = merge_profile_raw(
                                raws.get(&pubkey).map(String::as_str),
                                published,
                            );
                            if let Err(err) = send_profile(&keys, &merged) {
                                warning = Some(format!("profile publish failed: {err:#}"));
                            }
                        }
                        Err(err) => {
                            warning = Some(format!(
                                "could not read your existing profile ({err:#}); \
                                 the name is kept on this device only"
                            ));
                        }
                    }
                }
                Ok::<_, anyhow::Error>((
                    Reviewer {
                        pubkey,
                        name,
                        keys,
                    },
                    warning,
                ))
            }).await;
            let _ = this.update(cx, |this, cx| {
                match done {
                    Ok((reviewer, warning)) => {
                        let named_locally = reviewer.name.is_some();
                        this.reviewer = Some(reviewer);
                        this.signin_open = false;
                        this.signin_anchor = None;
                        this.new_key_notice = None;
                        this.composing = from_reviews;
                        this.review_draft = (None, String::new());
                        this.review_cursor = 0;
                        this.review_anchor = None;
                        this.review_layout = None;
                        this.review_error = None;
                        if let Some(warning) = warning {
                            this.error = Some(warning);
                        }
                        if !named_locally {
                            // an established key: their name comes from
                            // the profile they already published
                            this.resolve_reviewer_name(cx);
                        }
                        // past reviews by this key pick up the new
                        // name once the profile propagates
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
                            if let Some(raw) = raws.get(&reviewer.pubkey) {
                                let value: serde_json::Value =
                                    serde_json::from_str(raw).unwrap_or_default();
                                if let Some(name) = value["name"].as_str() {
                                    reviewer.name = Some(name.to_string());
                                }
                            }
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// End the session: the key stays in identity.json on this device
    /// and reloads on the next launch.
    fn sign_out(&mut self, cx: &mut Context<Self>) {
        self.reviewer = None;
        self.composing = false;
        cx.notify();
    }

    /// Close the sign-in form wherever it shows, dropping any
    /// half-entered key. The reviews draft resets with it: the shared
    /// input belongs to whatever form opens next.
    fn close_signin(&mut self) {
        self.signin_open = false;
        self.signin_anchor = None;
        self.signin_step = SigninStep::Key;
        self.pending_keys = None;
        self.new_key_notice = None;
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
        let Some(app) = self.apps.iter().find(|a| a.app_id == app_id) else {
            return;
        };
        if self.jobs.contains_key(app_id) {
            return;
        }
        let app = app.clone();
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
                            .insert(app.app_id.clone(), digest.clone());
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

        // ----- sign-in form: keystrokes go to the shared input (the
        // nsec on step one, the optional name on step two) -----
        if self.signin_open {
            match key {
                "escape" => {
                    self.close_signin();
                }
                "enter" => match self.signin_step {
                    SigninStep::Key => self.accept_nsec(cx),
                    SigninStep::Name => self.finish_sign_in(cx),
                },
                "left" | "right" | "home" | "end" | "backspace" | "delete" => {
                    self.move_draft_cursor(key, shift)
                }
                "a" if ctrl => {
                    self.review_anchor = Some(0);
                    self.review_cursor = self.review_draft.1.len();
                }
                "v" if ctrl => {
                    if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                        // a key is one token: strip every whitespace
                        // character on paste
                        let pasted: String = text.split_whitespace().collect();
                        self.insert_draft_text(&pasted);
                    }
                }
                "space" => self.insert_draft_text(" "),
                other if other.chars().count() == 1 && !ctrl => {
                    self.insert_draft_text(other)
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
                // one keypress each
                "1" | "2" | "3" | "4" | "5" => {
                    self.review_draft.0 = Some(key.parse::<u8>().unwrap() as f32);
                    self.review_error = None;
                }
                "left" | "right" | "home" | "end" | "backspace" | "delete" => {
                    self.move_draft_cursor(key, shift);
                }
                "space" => self.insert_draft_text(" "),
                other if other.chars().count() == 1 && !ctrl => {
                    self.insert_draft_text(other);
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
                        rows.iter()
                            .position(|&ix| self.apps[ix].app_id == id)
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
                self.insert_text(other);
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
                chip.bg(ui::accent()).text_color(ui::accent_text())
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
                let is_selected = selected == Some(app.app_id.as_str());
                let is_installed = self.installed.contains_key(&app.app_id);
                let outdated = self
                    .installed
                    .get(app.app_id.as_str())
                    .is_some_and(|info| update_available(app, info));
                let job = self.jobs.get(&app.app_id).map(|job| job.label().to_string());
                let verified_digest = self.verified.get(&app.app_id).cloned();
                let row_update_id = app_id.clone();

                div()
                    .id(SharedString::from(format!("app-row-{app_id}")))
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
                        this.open_page(&app_id, cx);
                    }))
                    .child(app_icon(app.icon_path.as_deref(), &app.name, 40.))
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
                                    .child(app.summary.clone()),
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
                                format!("row-open-{action_id}"),
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
                                        format!("row-update-{update_id}"),
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
                        None => div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .children(app.version.as_deref().map(|v| badge(v, ui::text_dim())))
                            .child(
                                ui::small_button(
                                    format!("row-install-{action_id}"),
                                    Some("icons/plus.svg"),
                                    "Install",
                                    false,
                                )
                                .on_click(cx.listener(move |this, _e: &ClickEvent, _w, cx| {
                                    cx.stop_propagation();
                                    this.install(action_id.as_str(), cx);
                                })),
                            )
                            .into_any_element(),
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
        let installed = self.installed.contains_key(&app_id);
        let job = self.jobs.get(&app_id).cloned();
        let verified = self.verified.get(&app_id).cloned();

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
            .or_else(|| self.remote_metadata.get(&app_id).cloned());

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
        let digest_failed = self.verify_errors.get(&app_id).cloned();
        let digest_badge = if digest_failed.is_none() {
            verified.clone().map(|digest| {
                let short = &digest[..digest.len().min(12)];
                badge(&format!("digest {short}"), ui::risk_color(Risk::Low))
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
                .child(badge("Publisher-verified", ui::accent()))
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
                .children(digest_badge)
                .children(app.publisher.as_ref().map(|pubkey| {
                    let short = &pubkey[..pubkey.len().min(16)];
                    div()
                        .text_size(px(ui::TEXT_SIZE))
                        .text_color(ui::text())
                        .child(format!(
                            "Published by {short}…; the artifact digest is checked before install."
                        ))
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
                                        .get(&app_id)
                                        .and_then(|(download, installed)| {
                                            download
                                                .clone()
                                                .map(|size| div().child(format!("· {size} download")))
                                                .or_else(|| {
                                                    installed.clone().map(|size| {
                                                        div().child(format!("· {size} installed"))
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
            .children(app.homepage.clone().map(|url| {
                let open = url.clone();
                div()
                    .id("homepage")
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
                    .child(ui::icon("icons/globe.svg", 13., ui::accent()))
                    .child(url)
            }))
            .child(self.render_reviews(&app_id, font.clone(), cx))
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
                let note = if self.verify_errors.contains_key(&app_id) {
                    "Sandbox info unavailable: the artifact did not pass its digest check."
                } else if self.publisher_fetching.contains(&app_id) {
                    "Reading the sandbox manifest from the artifact…"
                } else if self.remote_fetching.contains(&app_id) {
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
                            } else {
                                let install_id = app_id.clone();
                                let label = if app.source == Source::Publisher {
                                    "Verify & install"
                                } else {
                                    "Install"
                                };
                                buttons.push(
                                    button("btn-install", Some("icons/plus.svg"), label, true)
                                        .on_click(
                                            cx.listener(move |this, _e: &ClickEvent, _w, cx| {
                                                this.install(install_id.as_str(), cx);
                                            }),
                                        )
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

impl StoreRoot {
    /// The reviews section of the app page: a quiet header that doubles
    /// as the expand/collapse toggle (the list is hidden until asked
    /// for), the review list flush with the other page fields, and the
    /// compose or sign-in form when expanded.
    fn render_reviews(&mut self, app_id: &str, font: gpui::Font, cx: &mut Context<Self>) -> gpui::AnyElement {
        let reviews = self
            .reviews
            .get(app_id)
            .cloned()
            .unwrap_or_default();

        // header: same shape as the other section headers, so the text
        // lines up; the score formats with one decimal, no float junk
        let average = if reviews.is_empty() {
            None
        } else {
            let rated: Vec<f32> = reviews.iter().filter_map(|r| r.rating).collect();
            (!rated.is_empty()).then(|| rated.iter().sum::<f32>() / rated.len() as f32)
        };
        let label = if reviews.is_empty() {
            "No reviews yet".to_string()
        } else {
            let score = average
                .map(|value| format!(" · {:.1} {}", value, stars(value)))
                .unwrap_or_default();
            format!("Reviews{} · {}", score, reviews.len())
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

        // the reviews themselves, flush with the other fields: no
        // border chrome, the author line and text start at the same
        // edge as the description above
        if self.reviews_expanded {
            for review in &reviews {
                let author = review
                    .name
                    .clone()
                    .or_else(|| review.nip05.clone())
                    .unwrap_or_else(|| {
                        format!("{}…", &review.pubkey[..review.pubkey.len().min(12)])
                    });
                let when = relative_time(review.created_at);
                let mut row = div().flex().flex_col().gap_1().w_full().child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .text_size(px(ui::TEXT_SIZE_LABEL))
                        .text_color(ui::text_dim())
                        .child(div().child(author))
                        .children(review.rating.map(|value| {
                            div().text_color(ui::accent()).child(stars(value))
                        }))
                        .child(div().child(when)),
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
                section = section.child(row);
            }

            // compose or sign-in, depending on session state (the
            // header hosts its own sign-in panel)
            if self.signin_open && self.signin_origin == SigninOrigin::Reviews {
                section = section.child(self.render_review_signin(font.clone(), cx));
            } else if self.composing {
                section = section.child(self.render_review_compose(app_id, font.clone(), cx));
            } else {
                let mut invitation = div().flex().items_center().gap_2();
                if let Some(reviewer) = self.reviewer.as_ref() {
                    invitation = invitation
                        .child(
                            div()
                                .id("write-review")
                                .cursor_pointer()
                                .text_size(px(ui::TEXT_SIZE))
                                .text_color(ui::accent())
                                .hover(|style| style.text_color(ui::text()))
                                .on_click(cx.listener(|this, _e: &ClickEvent, _w, cx| {
                                    this.start_review(cx);
                                }))
                                .child(format!(
                                    "Write a review (r) as {}",
                                    reviewer.display()
                                )),
                        )
                        .child(
                            div()
                                .id("sign-out")
                                .cursor_pointer()
                                .text_size(px(ui::TEXT_SIZE_LABEL))
                                .text_color(ui::text_dim())
                                .hover(|style| style.text_color(ui::text()))
                                .on_click(cx.listener(|this, _e: &ClickEvent, _w, cx| {
                                    this.sign_out(cx);
                                }))
                                .child("sign out (key stays on device)"),
                        );
                } else {
                    invitation = invitation.child(
                        div()
                            .id("write-review")
                            .cursor_pointer()
                            .text_size(px(ui::TEXT_SIZE))
                            .text_color(ui::accent())
                            .hover(|style| style.text_color(ui::text()))
                            .on_click(cx.listener(|this, _e: &ClickEvent, _w, cx| {
                                this.start_review(cx);
                            }))
                            .child("Write a review (r)"),
                    );
                }
                section = section.child(invitation);
            }
        }

        section.into_any_element()
    }

    /// The sign-in form, in two steps: the Nostr key first (pasted or
    /// freshly generated), an optional published display name second.
    /// The key is the identity; the name is only a kind 0 profile.
    fn render_review_signin(&mut self, font: gpui::Font, cx: &mut Context<Self>) -> gpui::AnyElement {
        match self.signin_step {
            SigninStep::Key => {
                // the key never shows on screen: asterisks stand in
                // for every character, and byte counts match so the
                // caret maps through the same field state
                let masked = "*".repeat(self.review_draft.1.len());
                let input =
                    self.render_text_input("signin-input", &masked, "nsec1… or 64-hex private key", font);
                self.render_signin_frame(
                    "Sign in with your Nostr key",
                    input,
                    Some("Reviews are signed with this key; it stays on this device."),
                    "Continue",
                    Some("create a new key instead"),
                    cx,
                )
            }
            SigninStep::Name => {
                let text = self.review_draft.1.clone();
                let input = self.render_text_input(
                    "signin-input",
                    &text,
                    "Display name (optional, publishes a public profile)",
                    font,
                );
                let notice = self.new_key_notice.as_deref().map(|nsec| {
                    format!("New key, copy it now, it is your identity: {nsec}")
                });
                self.render_signin_frame(
                    "Signing in",
                    input,
                    notice.as_deref(),
                    "Start reviewing",
                    Some("Skip"),
                    cx,
                )
            }
        }
    }

    /// The sign-in form chrome around its single input: title, optional
    /// notice or hint line, confirm button and optional secondary link.
    fn render_signin_frame(
        &mut self,
        title: &str,
        input: gpui::Stateful<gpui::Div>,
        notice: Option<&str>,
        confirm: &str,
        secondary: Option<&str>,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let confirm_id: SharedString =
            format!("btn-{}", confirm.to_lowercase().replace(' ', "-")).into();
        let mut footer = div().flex().items_center().gap_3().child(
            button(confirm_id, None, confirm, true).on_click(cx.listener(|this, _e: &ClickEvent, _w, cx| {
                match this.signin_step {
                    SigninStep::Key => this.accept_nsec(cx),
                    SigninStep::Name => this.finish_sign_in(cx),
                }
            })),
        );
        if let Some(link) = secondary {
            footer = footer.child(
                div()
                    .id(SharedString::from(format!("link-{link}")))
                    .cursor_pointer()
                    .text_size(px(ui::TEXT_SIZE_LABEL))
                    .text_color(ui::text_dim())
                    .hover(|style| style.text_color(ui::text()))
                    .on_click(cx.listener(|this, _e: &ClickEvent, _w, cx| {
                        if this.signin_step == SigninStep::Key {
                            this.new_identity(cx);
                        } else {
                            this.finish_sign_in(cx);
                        }
                    }))
                    .child(link.to_string()),
            );
        }

        let mut form = div()
            .flex()
            .flex_col()
            .gap_2()
            .rounded_sm()
            .border_1()
            .border_color(ui::accent())
            .px_3()
            .py_2()
            .child(
                div()
                    .text_size(px(ui::TEXT_SIZE))
                    .text_color(ui::text())
                    .child(title.to_string()),
            );
        if let Some(notice) = notice {
            form = form.child(
                div()
                    .text_size(px(ui::TEXT_SIZE_LABEL))
                    .text_color(ui::text_dim())
                    .child(notice.to_string()),
            );
        }
        form = form.child(input).child(footer);
        if let Some(error) = &self.review_error {
            form = form.child(
                div()
                    .text_size(px(ui::TEXT_SIZE_LABEL))
                    .text_color(ui::risk_color(Risk::Medium))
                    .child(error.clone()),
            );
        }
        form.into_any_element()
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
    /// header sign-in panel) showing?
    fn any_layer_open(&self) -> bool {
        self.channel_menu.is_some()
            || self.account_menu.is_some()
            || (self.signin_open && self.signin_origin == SigninOrigin::Header)
    }

    /// The window-level layers, if any. The wrapper carries no
    /// hitbox of its own; each layer is a click-away overlay with the
    /// panel as a later sibling, so clicks on the panel stay the
    /// panel's and every click outside closes the layer.
    fn render_layers(&mut self, font: gpui::Font, cx: &mut Context<Self>) -> gpui::AnyElement {
        let mut layers = div().absolute().inset_0();
        if self.channel_menu.is_some() {
            layers = layers.child(self.render_channel_menu(cx));
        }
        if self.account_menu.is_some() {
            layers = layers.child(self.render_account_menu(cx));
        }
        if self.signin_open && self.signin_origin == SigninOrigin::Header {
            layers = layers.child(self.render_header_signin(font, cx));
        }
        layers.into_any_element()
    }

    /// The channel dropdown: filter options with the active one marked.
    fn render_channel_menu(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let anchor = self.channel_menu.unwrap_or_default();
        let left = (anchor.x - px(60.)).max(px(8.));
        div()
            .id("channel-menu-overlay")
            .absolute()
            .inset_0()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _e: &MouseDownEvent, _w, cx| {
                    this.channel_menu = None;
                    cx.notify();
                }),
            )
            .child(
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
                                    cx.notify();
                                }))
                                .child(f.label().to_string())
                                .children(active.then(|| "✓".to_string()))
                        }),
                    ),
            )
            .into_any_element()
    }

    /// The account dropdown: the signed-in session and sign-out.
    fn render_account_menu(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let anchor = self.account_menu.unwrap_or_default();
        let left = (anchor.x - px(170.)).max(px(8.));
        let display = self
            .reviewer
            .as_ref()
            .map(|reviewer| reviewer.display())
            .unwrap_or_default();
        div()
            .id("account-menu-overlay")
            .absolute()
            .inset_0()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _e: &MouseDownEvent, _w, cx| {
                    this.account_menu = None;
                    cx.notify();
                }),
            )
            .child(
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
                    .child(
                        div()
                            .px_3()
                            .py_1()
                            .text_size(px(ui::TEXT_SIZE_LABEL))
                            .text_color(ui::text_dim())
                            .child(format!("Signed in as {display}")),
                    )
                    .child(
                        div()
                            .id("account-sign-out")
                            .px_3()
                            .py_1()
                            .cursor_pointer()
                            .text_size(px(ui::TEXT_SIZE))
                            .text_color(ui::text())
                            .hover(|style| style.bg(ui::surface()))
                            .on_click(cx.listener(|this, _e: &ClickEvent, _w, cx| {
                                this.account_menu = None;
                                this.sign_out(cx);
                            }))
                            .child("Sign out (key stays on device)"),
                    ),
            )
            .into_any_element()
    }

    /// The header sign-in panel: the same two-step form the reviews
    /// section uses, floating over the window.
    fn render_header_signin(&mut self, font: gpui::Font, cx: &mut Context<Self>) -> gpui::AnyElement {
        let anchor = self.signin_anchor.unwrap_or_default();
        let left = (anchor.x - px(280.)).max(px(8.));
        div()
            .id("signin-overlay")
            .absolute()
            .inset_0()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _e: &MouseDownEvent, _w, cx| {
                    this.close_signin();
                    cx.notify();
                }),
            )
            .child(
                div()
                    .absolute()
                    .left(left)
                    .top(anchor.y + px(6.))
                    .w(px(320.))
                    .child(self.render_review_signin(font, cx)),
            )
            .into_any_element()
    }

    /// The channel dropdown trigger: the active filter with a chevron.
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
            .on_click(cx.listener(|this, event: &ClickEvent, _w, cx| {
                this.account_menu = None;
                this.close_signin();
                this.channel_menu = Some(event.position());
                cx.notify();
            }))
            .child(filter.label().to_string())
            .child(ui::icon("icons/chevron_down.svg", 11., ui::text_dim()))
            .into_any_element()
    }

    /// The account control: the sign-in entry point when signed out,
    /// the session menu trigger when not.
    fn render_account_trigger(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        match self.reviewer.as_ref() {
            Some(reviewer) => {
                let display = reviewer.display();
                let label: String = display.chars().take(24).collect();
                div()
                    .id("account-trigger")
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
                    .on_click(cx.listener(|this, event: &ClickEvent, _w, cx| {
                        this.channel_menu = None;
                        this.close_signin();
                        this.account_menu = Some(event.position());
                        cx.notify();
                    }))
                    .child(label)
                    .child(ui::icon("icons/chevron_down.svg", 11., ui::text_dim()))
                    .into_any_element()
            }
            None => button("btn-sign-in-header", None, "Sign in", false)
                .on_click(cx.listener(|this, event: &ClickEvent, _w, cx| {
                    this.open_account_menu(event.position(), cx);
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
            "{}\u{1}{}\u{1}{}\u{1}{}\u{1}{}",
            app.app_id,
            app.version.as_deref().unwrap_or(""),
            app.pinned_digest.as_deref().unwrap_or(""),
            app.name,
            app.summary,
        );
    }
    fingerprint
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

fn load_flathub() -> Vec<StoreApp> {
    for path in orchard_core::appstream::appstream_paths(FLATHUB_REMOTE, APPSTREAM_ARCH) {
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

/// Reviews from indexd, grouped by app id (newest first within an app).
fn load_reviews() -> anyhow::Result<HashMap<String, Vec<orchard_core::Review>>> {    let agent = ureq::AgentBuilder::new()
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
    Ok(grouped)
}

/// Sign and send one review event to the review relays. Blocking: runs
/// on the background pool. The key arrives as hex; the reviewer is
/// always signed in by the time this runs.
fn post_review(key_hex: &str, app_id: &str, rating: f32, content: String) -> anyhow::Result<()> {
    use nostr::prelude::*;
    let keys = Keys::parse(key_hex)?;
    let event = orchard_nostr::events::sign_review(
        &keys,
        &orchard_nostr::events::Review {
            app_id: app_id.to_string(),
            rating: Some(rating),
            content,
        },
    )?;
    for url in review_relays() {
        orchard_nostr::relay::send(&url, std::slice::from_ref(&event))?;
    }
    Ok(())
}

/// Relays to post reviews to: the same list the embedded indexd reads
/// from (ORCHARD_RELAYS or the local dev relay).
fn review_relays() -> Vec<String> {
    match std::env::var("ORCHARD_RELAYS") {
        Ok(value) if !value.trim().is_empty() => value
            .split(',')
            .map(str::trim)
            .filter(|url| !url.is_empty())
            .map(str::to_string)
            .collect(),
        _ => vec![LOCAL_RELAY.to_string()],
    }
}

/// Where the sign-in form is: the key question comes first, the
/// optional profile name second.
#[derive(Clone, Copy, PartialEq)]
enum SigninStep {
    Key,
    Name,
}

/// Which surface hosts the sign-in form: the header account panel or
/// the reviews section. The form state is shared; the origin decides
/// where it renders and what a successful sign-in does next.
#[derive(Clone, Copy, PartialEq)]
enum SigninOrigin {
    Header,
    Reviews,
}

/// The signed-in reviewer: the Nostr key the reviews are signed with,
/// its public key, and the display name if one was published (None:
/// reviews resolve to a pubkey fragment until a profile exists).
pub struct Reviewer {
    keys: nostr::prelude::Keys,
    pub pubkey: String,
    pub name: Option<String>,
}

impl Reviewer {
    /// What to call this reviewer in the UI: the published profile
    /// name, else a short pubkey fragment.
    pub fn display(&self) -> String {
        self.name.clone().unwrap_or_else(|| {
            format!("{}…", &self.pubkey[..self.pubkey.len().min(12)])
        })
    }
}

/// Load the device's reviewer identity, if one is saved. The file
/// holds the secret key (nsec or hex) and the optional published
/// name; the key is the identity, so it stays on disk across
/// sign-outs.
fn load_reviewer() -> Option<Reviewer> {
    use nostr::key::GetPublicKey as _;
    let path = identity_path()?;
    let raw = std::fs::read_to_string(path).ok()?;
    let saved: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let keys = nostr::prelude::Keys::parse(saved["secret_key"].as_str()?).ok()?;
    Some(Reviewer {
        pubkey: keys.get_public_key().ok()?.to_string(),
        name: saved["name"].as_str().map(str::to_string),
        keys,
    })
}

/// Persist the signed-in identity: the key as given plus the optional
/// published name. The key leaves the memory of this machine only
/// when the file is deleted.
fn save_reviewer(keys: &nostr::prelude::Keys, name: Option<&str>) -> anyhow::Result<()> {
    let path = identity_path().ok_or_else(|| anyhow::anyhow!("XDG_DATA_HOME not set"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let saved = serde_json::json!({
        "secret_key": keys.secret_key().to_secret_hex(),
        "name": name,
    });
    std::fs::write(&path, serde_json::to_vec_pretty(&saved)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// Sign and send one kind 0 metadata document (the merged profile) to
/// the review relays. Blocking: runs on the background pool.
fn send_profile(keys: &nostr::prelude::Keys, raw_metadata: &str) -> anyhow::Result<()> {
    let event = orchard_nostr::events::sign_profile_raw(keys, raw_metadata)?;
    for url in review_relays() {
        orchard_nostr::relay::send(&url, std::slice::from_ref(&event))?;
    }
    Ok(())
}

fn identity_path() -> Option<std::path::PathBuf> {
    std::env::var_os("XDG_DATA_HOME").map(|data| {
        std::path::PathBuf::from(data).join("orchard/identity.json")
    })
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
            flatpak_ref: "app/com.example.Hello/x86_64/stable".into(),
            location: "oci:/srv/hello".into(),
            digest: Some("sha256:aa".into()),
            metadata: None,
            notes: None,
            published_at: 0,
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
}
