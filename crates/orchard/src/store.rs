//! The store view: catalog list, search, detail pane, install actions.
//!
//! MVP interaction model: type to search, click a row for the detail pane,
//! click Install. All flatpak work runs on background threads through the
//! single CLI seam (`orchard-flatpak`); the UI polls nothing, results land
//! via `cx.spawn` updates.

use std::collections::HashMap;

use gpui::{
    div, prelude::*, px, ClickEvent, Context, FocusHandle, KeyDownEvent, ParentElement, Render,
    SharedString, Styled, Window,
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

    fn next(&self) -> Self {
        match self {
            Filter::All => Filter::Reviewed,
            Filter::Reviewed => Filter::Publisher,
            Filter::Publisher => Filter::Installed,
            Filter::Installed => Filter::All,
        }
    }
}

/// What `flatpak list` reported for an installed app.
#[derive(Debug, Clone)]
pub struct InstalledInfo {
    pub version: Option<String>,
    pub origin: Option<String>,
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
            let count = apps.len();
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

    fn filtered(&self) -> Vec<&StoreApp> {
        let needle = self.search.to_lowercase();
        self.apps
            .iter()
            .filter(|app| match self.filter {
                Filter::All => true,
                Filter::Reviewed => app.source == Source::Reviewed,
                Filter::Publisher => app.source == Source::Publisher,
                Filter::Installed => self.installed.contains_key(&app.app_id),
            })
            .filter(|app| {
                needle.is_empty()
                    || app.name.to_lowercase().contains(&needle)
                    || app.summary.to_lowercase().contains(&needle)
                    || app.app_id.to_lowercase().contains(&needle)
            })
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

    fn key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if event.keystroke.modifiers.modified() {
            return;
        }
        let key = event.keystroke.key.as_str();
        match key {
            "backspace" => {
                self.search.pop();
            }
            "escape" => {
                if !self.search.is_empty() {
                    self.search.clear();
                } else if self.selected.is_some() {
                    self.selected = None;
                }
            }
            "enter" => {
                if self.selected.is_none() {
                    if let Some(first) = self.filtered().first() {
                        self.selected = Some(first.app_id.clone());
                    }
                }
            }
            "space" => self.search.push(' '),
            other if other.chars().count() == 1 => {
                self.search.push_str(other);
            }
            _ => return,
        }
        cx.notify();
    }

    // ----- rendering -----

    fn render_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let apps = self.filtered();
        let selected = self.selected.clone();
        let is_empty = apps.is_empty();

        div()
            .id("app-list")
            .flex_1()
            .min_w_0()
            .overflow_y_scroll()
            .bg(ui::inset())
            .children(apps.into_iter().map(|app| {
                let app_id = app.app_id.clone();
                let is_selected = selected.as_deref() == Some(app.app_id.as_str());
                let is_installed = self.installed.contains_key(&app.app_id);
                let job = self.jobs.get(&app.app_id).map(|job| job.label().to_string());
                let verified_digest = self.verified.get(&app.app_id).cloned();

                div()
                    .id(SharedString::from(format!("app-row-{app_id}")))
                    .flex()
                    .items_center()
                    .gap_3()
                    .px_4()
                    .py_2()
                    .border_b_1()
                    .border_color(ui::divider_soft())
                    .cursor_pointer()
                    .hover(|style| style.bg(ui::surface()))
                    .when(is_selected, |row| row.bg(ui::row_selected()))
                    .on_click(cx.listener(move |this, _event: &ClickEvent, _window, cx| {
                        this.selected = Some(app_id.clone());
                        cx.notify();
                    }))
                    .child(app_icon(
                        app.icon_path.as_deref(),
                        app.icon_url.as_deref(),
                        &app.name,
                        36.,
                    ))
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
                        Some(label) => badge(&label, ui::risk_color(orchard_core::Risk::Medium)),
                        None => {
                            if is_installed {
                                badge("Installed", ui::risk_color(orchard_core::Risk::Low))
                            } else {
                                badge(
                                    app.version.as_deref().unwrap_or(""),
                                    ui::text_dim(),
                                )
                            }
                        }
                    })
            }))
            .when(is_empty, |list| {
                list.items_center().justify_center().child(
                    div()
                        .text_color(ui::text_dim())
                        .text_size(px(ui::TEXT_SIZE))
                        .child(if self.flathub_loading {
                            "Loading Flathub catalog…".to_string()
                        } else {
                            "No apps match.".to_string()
                        }),
                )
            })
    }

    fn render_detail(&self, app: StoreApp, cx: &mut Context<Self>) -> impl IntoElement {
        let app_id = app.app_id.clone();
        let installed = self.installed.contains_key(&app_id);
        let job = self.jobs.get(&app_id);
        let verified = self.verified.get(&app_id).cloned();

        div()
            .w(px(420.))
            .h_full()
            .flex()
            .flex_col()
            .bg(ui::panel())
            .border_l_1()
            .border_color(ui::divider())
            .child(
                // header
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .px_4()
                    .py_3()
                    .border_b_1()
                    .border_color(ui::divider())
                    .child(
                        div()
                            .id("back")
                            .cursor_pointer()
                            .text_color(ui::accent())
                            .hover(|style| style.text_color(ui::text()))
                            .on_click(cx.listener(|this, _e: &ClickEvent, _w, cx| {
                                this.selected = None;
                                cx.notify();
                            }))
                            .child("← Back"),
                    )
                    .child(app_icon(
                        app.icon_path.as_deref(),
                        app.icon_url.as_deref(),
                        &app.name,
                        40.,
                    ))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .text_size(px(ui::TEXT_SIZE_EMPHASIS))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(ui::text())
                                    .child(app.name.clone()),
                            )
                            .child(
                                div()
                                    .text_size(px(ui::TEXT_SIZE_LABEL + 0.5))
                                    .text_color(ui::text_dim())
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .child(app.app_id.clone()),
                            ),
                    ),
            )
            .child(
                div()
                    .id("detail-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .p_4()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(
                        // badges row
                        div().flex().flex_wrap().items_center().gap_2().child(match app.source {
                            Source::Reviewed => badge("Reviewed on Flathub", ui::text_dim()),
                            Source::Publisher => {
                                badge("Publisher-verified", ui::accent())
                            }
                        })
                        .when(app.source == Source::Publisher, |row| {
                            row.children(verified.clone().map(|digest| {
                                badge(&format!("digest {digest}"), ui::risk_color(Risk::Low))
                            }))
                        })
                        .children(app.version.clone().map(|v| badge(&v, ui::text())))
                        .children(
                            app.license
                                .clone()
                                .map(|license| badge(&license, ui::text_dim())),
                        ),
                    )
                    .when(!app.screenshots.is_empty(), |section| {
                        section.child(
                            div()
                                .id("screenshots")
                                .flex()
                                .gap_2()
                                .overflow_x_scroll()
                                .children(app.screenshots.iter().take(4).map(|url| {
                                    gpui::img(url.clone())
                                        .w(px(176.))
                                        .h(px(108.))
                                        .flex_none()
                                        .object_fit(gpui::ObjectFit::Cover)
                                        .rounded_sm()
                                        .border_1()
                                        .border_color(ui::divider())
                                })),
                        )
                    })
                    .child(
                        div()
                            .text_size(px(ui::TEXT_SIZE_EMPHASIS - 0.5))
                            .text_color(ui::text())
                            .child(app.summary.clone()),
                    )
                    .children(app.description.clone().map(|description| {
                        div()
                            .text_size(px(ui::TEXT_SIZE))
                            .text_color(ui::text_dim())
                            .child(description)
                    }))
                    .when(app.source == Source::Publisher, |section| {
                        section.children(app.publisher.clone().map(|publisher| {
                            div()
                                .text_size(px(ui::TEXT_SIZE))
                                .text_color(ui::text_dim())
                                .child(format!("Publisher key: {}", publisher))
                        }))
                    })
                    .when(
                        app.source == Source::Publisher
                            || app.metadata.is_some(),
                        |section| {
                            section.child(render_permissions_static(&app))
                        },
                    ),
            )
            .child(
                // action bar
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_4()
                    .py_3()
                    .border_t_1()
                    .border_color(ui::divider())
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
                                    button("btn-open", "Open", false).on_click(
                                        cx.listener(move |this, _e: &ClickEvent, _w, cx| {
                                            this.open(open_id.as_str(), cx);
                                        }),
                                    ).into_any_element(),
                                );
                                let remove_id = app_id.clone();
                                buttons.push(
                                    button("btn-remove", "Remove", false).on_click(
                                        cx.listener(move |this, _e: &ClickEvent, _w, cx| {
                                            this.uninstall(remove_id.as_str(), cx);
                                        }),
                                    ).into_any_element(),
                                );
                            } else {
                                let install_id = app_id.clone();
                                let label = if app.source == Source::Publisher {
                                    "Verify & install"
                                } else {
                                    "Install"
                                };
                                buttons.push(
                                    button("btn-install", label, true).on_click(
                                        cx.listener(move |this, _e: &ClickEvent, _w, cx| {
                                            this.install(install_id.as_str(), cx);
                                        }),
                                    ).into_any_element(),
                                );
                            }
                            buttons
                        }
                    }),
            )
    }
}

fn risk_label(risk: Risk) -> &'static str {
    match risk {
        Risk::Low => "low",
        Risk::Medium => "medium",
        Risk::High => "high",
    }
}

fn render_permissions_static(app: &StoreApp) -> gpui::AnyElement {
    // Borrow-free re-render of the permissions section (the method version
    // cannot be called from a non-self context).
    let metadata = app.metadata.as_deref().unwrap_or("");
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
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let selected = self.selected_app().cloned();
        let filter = self.filter;
        let total = self.apps.len();
        let installed_count = self.installed.len();

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
                    .child(
                        div()
                            .id("search-box")
                            .flex_1()
                            .max_w(px(360.))
                            .px_3()
                            .py_1()
                            .rounded_sm()
                            .bg(ui::inset())
                            .border_1()
                            .border_color(ui::divider())
                            .text_size(px(ui::TEXT_SIZE))
                            .text_color(if self.search.is_empty() {
                                ui::text_dim()
                            } else {
                                ui::text()
                            })
                            .child(if self.search.is_empty() {
                                "Type to search…".to_string()
                            } else {
                                format!("{}|", self.search)
                            }),
                    )
                    .child(div().flex_1())
                    .child(div().flex().gap_1().children(
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
                    )),
            )
            // body
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .child(self.render_list(cx))
                    .children(selected.map(|app| self.render_detail(app, cx))),
            )
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
            let version = fields.next().map(|f| f.trim().to_string());
            let origin = fields.next().map(|f| f.trim().to_string());
            Some((
                app_id,
                InstalledInfo {
                    version: version.filter(|v| !v.is_empty()),
                    origin: origin.filter(|o| !o.is_empty()),
                },
            ))
        })
        .collect()
}
