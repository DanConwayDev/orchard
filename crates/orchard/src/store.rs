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
const APPSTREAM_ARCH: &str = "x86_64";

/// Lifecycle of a per-app operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobState {
    Verifying,
    Installing,
    Removing,
}

impl JobState {
    fn label(&self) -> &'static str {
        match self {
            JobState::Verifying => "Verifying digest…",
            JobState::Installing => "Installing…",
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
    /// Remote-metadata fetches in flight, keyed by app id.
    remote_metadata_fetching: HashSet<String>,
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
            remote_metadata_fetching: HashSet::new(),
            page_open: false,
            category: None,
            cursor: 0,
            anchor: None,
            search_layout: None,
            shot_handle: ScrollHandle::new(),
            list_handle: UniformListScrollHandle::new(),
        };

        // Orchard channel: read the catalog over HTTP from the in-process
        // indexd (the client is the remote), fall back to the file. Blocking
        // calls on the background pool keep startup snappy.
        cx.spawn(async move |this, cx| {
            let orchard_apps = cx
                .background_executor()
                .spawn(async move { load_orchard_catalog() })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.apps.extend(orchard_apps);
                cx.notify();
            });
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
        self.apps
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
            .collect()
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
            // the strip's horizontal scroll must not leak between apps
            self.shot_handle.set_offset(Point::new(px(0.), px(0.)));
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
        // pre-install sandbox metadata for reviewed apps: the remote
        // serves the manifest of the commit that would install
        let reviewed = self
            .apps
            .iter()
            .find(|a| a.app_id == app_id)
            .is_some_and(|app| app.source == Source::Reviewed);
        if reviewed && !self.installed.contains_key(app_id) {
            self.ensure_remote_metadata(app_id, cx);
        }
        cx.notify();
    }

    /// Fetch an app's sandbox metadata from the remote (once per
    /// session; failures retry on the next page open).
    fn ensure_remote_metadata(&mut self, app_id: &str, cx: &mut Context<Self>) {
        if self.remote_metadata.contains_key(app_id)
            || !self.remote_metadata_fetching.insert(app_id.to_string())
        {
            return;
        }
        let app_id = app_id.to_string();
        let remote = FLATHUB_REMOTE.to_string();
        cx.spawn(async move |this, cx| {
            let key = app_id.clone();
            let metadata = cx
                .background_executor()
                .spawn(async move {
                    orchard_flatpak::remote_metadata(&remote, &app_id)
                        .output()
                        .ok()
                        .filter(|out| out.status.success())
                        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
                        .filter(|text| !text.is_empty())
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if let Some(metadata) = metadata {
                    this.remote_metadata.insert(key.clone(), metadata);
                }
                this.remote_metadata_fetching.remove(&key);
                cx.notify();
            });
        })
        .detach();
    }

    /// Close the detail page. Selection survives so keyboard
    /// navigation resumes where it left off.
    fn close_page(&mut self, cx: &mut Context<Self>) {
        self.page_open = false;
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
                // back out one level: page, then selection, then text,
                // then row selection
                if self.page_open {
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
                let job = self.jobs.get(&app.app_id).map(|job| job.label().to_string());
                let verified_digest = self.verified.get(&app.app_id).cloned();

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
                            .child(badge(
                                "Installed",
                                ui::risk_color(orchard_core::Risk::Low),
                            ))
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
    fn render_page(&self, app: StoreApp, cx: &mut Context<Self>) -> AnyElement {
        let app_id = app.app_id.clone();
        let installed = self.installed.contains_key(&app_id);
        let job = self.jobs.get(&app_id);
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
        let provenance = match app.source {
            Source::Reviewed => div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap_2()
                .child(badge("Reviewed by Flathub", ui::text_dim()))
                .child(
                    div()
                        .text_size(px(ui::TEXT_SIZE))
                        .text_color(ui::text_dim())
                        .child("Moderator-reviewed listing; installs from your flathub remote."),
                )
                .into_any_element(),
            Source::Publisher => div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap_2()
                .child(badge("Publisher-verified", ui::accent()))
                .children(verified.clone().map(|digest| {
                    let short = &digest[..digest.len().min(12)];
                    badge(&format!("digest {short}"), ui::risk_color(Risk::Low))
                }))
                .children(app.publisher.as_ref().map(|pubkey| {
                    let short = &pubkey[..pubkey.len().min(16)];
                    div()
                        .text_size(px(ui::TEXT_SIZE))
                        .text_color(ui::text_dim())
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
                                .children(
                                    self.installed
                                        .get(&app_id)
                                        .and_then(|info| info.size.clone())
                                        .map(|size| div().child(format!("· {size}"))),
                                )
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
                            Some(Some(path)) => gpui::img(path.clone())
                                .w(px(280.))
                                .h(px(158.))
                                .flex_none()
                                .object_fit(gpui::ObjectFit::Cover)
                                .rounded_sm()
                                .border_1()
                                .border_color(ui::divider())
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
                // remote, or flatpak could not produce it
                section.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1p5()
                        .text_size(px(ui::TEXT_SIZE))
                        .text_color(ui::text())
                        .child(ui::icon("icons/shield.svg", 13., ui::text_dim()))
                        .child(if self.remote_metadata_fetching.contains(&app_id) {
                            "Reading the sandbox manifest from flathub…"
                        } else {
                            "Flatpak apps run in a sandbox; this app's permissions appear here once its manifest is available."
                        }),
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
            div()
                .flex()
                .items_center()
                .gap_2()
                .text_size(px(ui::TEXT_SIZE))
                .child(badge(risk_label(grant.risk), ui::risk_color(grant.risk)))
                .child(
                    div()
                        .text_color(ui::text())
                        .child(grant.summary.clone()),
                )
                .child(div().text_color(ui::text_dim()).child(grant.arg.clone()))
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
            .flex()
            .flex_col()
            .bg(ui::inset())
            .text_color(ui::text())
            // top bar
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .px_4()
                    .py_2()
                    .border_b_1()
                    .border_color(ui::divider())
                    .bg(ui::panel())
                    .child(
                        div()
                            .text_size(px(ui::TEXT_SIZE_EMPHASIS))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(ui::accent())
                            .child("Orchard"),
                    )
                    .child({
                        // The search input: a real caret and selection.
                        // StyledText carries the selection as a styled
                        // run; the layout handle maps byte indices to
                        // pixels (caret x, click position). The caret
                        // queries last frame's handle: one frame stale,
                        // invisible at 120fps.
                        let display: String = if self.search.is_empty() {
                            "Type to search…".to_string()
                        } else {
                            self.search.clone()
                        };
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
                        self.search_layout = Some(live_layout.clone());

                        // the caret queries last frame's handle; it has
                        // been measured by now
                        let caret_x = prev_layout.and_then(|layout| {
                            layout
                                .position_for_index(self.cursor.min(self.search.len()))
                                .map(|pos| pos.x - layout.bounds().origin.x)
                        });

                        div()
                            .id("search-box")
                            .flex_1()
                            .max_w(px(360.))
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
                                    // practice
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
                                    .child(styled)
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
                    })
                    .child(div().flex_1())
                    // source filters are a browse concern; the page
                    // hides them
                    .when(!page_open, |bar| {
                        bar.child(div().flex().gap_1().children(
                            [
                                Filter::All,
                                Filter::Reviewed,
                                Filter::Publisher,
                                Filter::Installed,
                            ]
                            .iter()
                            .map(|f| {
                                let active = *f == filter;
                                div()
                                    .id(("filter", *f as usize))
                                    .px_2()
                                    .py_1()
                                    .rounded_sm()
                                    .text_size(px(ui::TEXT_SIZE))
                                    .cursor_pointer()
                                    .when(active, |chip| {
                                        chip.bg(ui::accent()).text_color(ui::accent_text())
                                    })
                                    .when(!active, |chip| {
                                        chip.text_color(ui::text_dim()).hover(|style| {
                                            style.bg(ui::surface())
                                        })
                                    })
                                    .on_click(cx.listener(move |this, _e: &ClickEvent, _w, cx| {
                                        this.filter = *f;
                                        cx.notify();
                                    }))
                                    .child(f.label().to_string())
                            }),
                        ))
                    }),
            )
            // body: browse (category chips + virtualized list) or the
            // full app page
            .child(if page_open {
                match selected {
                    Some(app) => self.render_page(app, cx).into_any_element(),
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
            .id("store-root")
            .on_click(cx.listener(|this, _e: &ClickEvent, window, _cx| {
                // Keep the keyboard attached to the search context.
                window.focus(&this.focus_handle);
            }))
    }
}

// ----- catalog loaders (background) -----

/// Flathub channel: parse the local AppStream for the system install.
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
            let _version = fields.next().map(|f| f.trim().to_string());
            let _origin = fields.next().map(|f| f.trim().to_string());
            Some((
                app_id.clone(),
                InstalledInfo {
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
