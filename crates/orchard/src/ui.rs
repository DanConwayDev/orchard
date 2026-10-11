//! Small shared UI helpers: the kumaUI-flavored palette, type scale,
//! badges, buttons, and app icons.
//!
//! The palette follows kuma's pattern (kuma-files/src/theme.rs): every
//! color is an atomic static read through accessor functions, so a
//! wallpaper-derived palette can swap it wholesale at runtime. Default
//! values are kuma-shell's Theme::default (Catppuccin Mocha).

use std::sync::atomic::{AtomicU32, Ordering};

use gpui::{div, prelude::*, px, rgb, rgba, Div, ObjectFit, Rgba, SharedString};

use orchard_core::Risk;

// ---------- palette (kuma-shell Theme::default, Catppuccin Mocha) ----------

/// Window backing and list background: the darkest surface.
static INSET: AtomicU32 = AtomicU32::new(0x11111B);
/// Raised chrome: header, detail pane, status bar.
static PANEL: AtomicU32 = AtomicU32::new(0x181825);
/// Cards and hovered rows.
static SURFACE: AtomicU32 = AtomicU32::new(0x313244);
/// Pressed/secondary hover step.
static SURFACE_HOVER: AtomicU32 = AtomicU32::new(0x45475A);
static DIVIDER: AtomicU32 = AtomicU32::new(0x45475A);
static DIVIDER_SOFT: AtomicU32 = AtomicU32::new(0x45475A66);
static TEXT: AtomicU32 = AtomicU32::new(0xCDD6F4);
static TEXT_DIM: AtomicU32 = AtomicU32::new(0x6C7086);
static ACCENT: AtomicU32 = AtomicU32::new(0x89B4FA);
static ACCENT_HOVER: AtomicU32 = AtomicU32::new(0x9CC1FB);
static ACCENT_TEXT: AtomicU32 = AtomicU32::new(0x11111B);
/// Error red; never palette-derived in kuma, same deal here.
static URGENT: AtomicU32 = AtomicU32::new(0xF38BA8);
/// Selection rows and icon tiles: quiet accent hues, re-derived on accent swap.
static ROW_SELECTED: AtomicU32 = AtomicU32::new(0x1B3C73);
static TILE_BG: AtomicU32 = AtomicU32::new(0x41455B);

fn get(slot: &AtomicU32) -> Rgba {
    let value = slot.load(Ordering::Relaxed);
    // alpha-carrying slots (divider_soft) keep their alpha byte
    if value > 0xFFFFFF {
        rgba(value)
    } else {
        rgb(value)
    }
}

fn store(slot: &AtomicU32, value: u32) {
    slot.store(value, Ordering::Relaxed);
}

pub(crate) fn inset() -> Rgba {
    get(&INSET)
}
pub(crate) fn panel() -> Rgba {
    get(&PANEL)
}
pub(crate) fn surface() -> Rgba {
    get(&SURFACE)
}
#[allow(dead_code)]
pub(crate) fn surface_hover() -> Rgba {
    get(&SURFACE_HOVER)
}

#[allow(dead_code)]
pub(crate) fn accent_hex() -> u32 {
    ACCENT.load(Ordering::Relaxed)
}
pub(crate) fn divider() -> Rgba {
    get(&DIVIDER)
}
pub(crate) fn divider_soft() -> Rgba {
    get(&DIVIDER_SOFT)
}
pub(crate) fn text() -> Rgba {
    get(&TEXT)
}
pub(crate) fn text_dim() -> Rgba {
    get(&TEXT_DIM)
}
pub(crate) fn accent() -> Rgba {
    get(&ACCENT)
}
/// The readable foreground on the current accent: the palette's
/// accent text on a light accent (the wallpaper picked it for
/// exactly that), near white on a dark one, where the palette text
/// was meant for a light accent and would vanish.
pub(crate) fn accent_fg() -> Rgba {
    let value = ACCENT.load(Ordering::Relaxed) & 0xFFFFFF;
    let r = ((value >> 16) & 0xFF) as f32;
    let g = ((value >> 8) & 0xFF) as f32;
    let b = (value & 0xFF) as f32;
    // Rec. 601 luma, the usual pick-a-legible-ink rule
    if 0.299 * r + 0.587 * g + 0.114 * b > 155. {
        get(&ACCENT_TEXT)
    } else {
        rgb(0xf8f8fc)
    }
}
pub(crate) fn urgent() -> Rgba {
    get(&URGENT)
}
pub(crate) fn row_selected() -> Rgba {
    get(&ROW_SELECTED)
}

/// The colors a wallpaper palette hands over; mirrors kuma-shell's
/// `theme::Theme` field for field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    pub panel_bg: u32,
    pub surface: u32,
    pub surface_hover: u32,
    pub inset: u32,
    pub divider: u32,
    pub divider_soft: u32,
    pub text: u32,
    pub text_dim: u32,
    pub accent: u32,
    pub accent_text: u32,
}

impl Default for Palette {
    fn default() -> Self {
        Palette {
            panel_bg: 0x181825,
            surface: 0x313244,
            surface_hover: 0x45475A,
            inset: 0x11111B,
            divider: 0x45475A,
            divider_soft: 0x45475A66,
            text: 0xCDD6F4,
            text_dim: 0x6C7086,
            accent: 0x89B4FA,
            accent_text: 0x11111B,
        }
    }
}

/// Swap the chrome for a wallpaper palette (kuma-shell) wholesale.
pub fn apply_palette(palette: &Palette) {
    store(&PANEL, palette.panel_bg & 0xFFFFFF);
    store(&SURFACE, palette.surface);
    store(&SURFACE_HOVER, palette.surface_hover);
    store(&INSET, palette.inset);
    store(&DIVIDER, palette.divider);
    store(&DIVIDER_SOFT, palette.divider_soft);
    store(&TEXT, palette.text);
    store(&TEXT_DIM, palette.text_dim);
    store(&ACCENT_TEXT, palette.accent_text);
    set_accent(palette.accent);
}

/// Swap the accent and re-derive the accent-hued surfaces.
pub fn set_accent(hex: u32) {
    store(&ACCENT, hex);
    ROW_SELECTED.store(derive(&hex, 0.68, 0.276), Ordering::Relaxed);
    TILE_BG.store(derive(&hex, 0.456, 0.300), Ordering::Relaxed);
    let (h, s, l) = rgb_to_hsl(hex);
    ACCENT_HOVER.store(hsl_to_rgb(h, s, (l + 0.08).min(1.)), Ordering::Relaxed);
}

/// A low-saturation tone in the accent's hue at lightness `l`.
fn derive(accent: &u32, sat_scale: f32, l: f32) -> u32 {
    let (h, s, _) = rgb_to_hsl(*accent);
    hsl_to_rgb(h, (s * sat_scale).clamp(0.0, 0.95), l)
}

// ---------- color space (from kuma-shell/src/palette.rs) ----------

/// sRGB u32 (0xRRGGBB) to HSL.
pub fn rgb_to_hsl(rgb: u32) -> (f32, f32, f32) {
    let r = ((rgb >> 16) & 0xFF) as f32 / 255.;
    let g = ((rgb >> 8) & 0xFF) as f32 / 255.;
    let b = (rgb & 0xFF) as f32 / 255.;
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.;
    if max == min {
        return (0., 0., l);
    }
    let d = max - min;
    let s = if l > 0.5 { d / (2. - max - min) } else { d / (max + min) };
    let h = if max == r {
        (g - b) / d + if g < b { 6. } else { 0. }
    } else if max == g {
        (b - r) / d + 2.
    } else {
        (r - g) / d + 4.
    } * 60.;
    (h, s, l)
}

/// HSL to sRGB u32 (0xRRGGBB), clamped.
pub fn hsl_to_rgb(h: f32, s: f32, l: f32) -> u32 {
    let h = h.rem_euclid(360.) / 360.;
    let s = s.clamp(0., 1.);
    let l = l.clamp(0., 1.);
    if s == 0. {
        let v = (l * 255. + 0.5) as u32;
        return (v << 16) | (v << 8) | v;
    }
    let q = if l < 0.5 { l * (1. + s) } else { l + s - l * s };
    let p = 2. * l - q;
    let channel = |mut t: f32| {
        t = t.rem_euclid(1.);
        let v = if t < 1. / 6. {
            p + (q - p) * 6. * t
        } else if t < 0.5 {
            q
        } else if t < 2. / 3. {
            p + (q - p) * (2. / 3. - t) * 6.
        } else {
            p
        };
        ((v * 255.) + 0.5) as u32
    };
    (channel(h + 1. / 3.) << 16) | (channel(h) << 8) | channel(h - 1. / 3.)
}

// ---------- semantic colors ----------

/// Mocha's green (low risk / success).
pub const RISK_LOW: u32 = 0xA6E3A1;
/// Mocha's yellow (medium risk / in-progress).
pub const RISK_MED: u32 = 0xF9E2AF;
/// Mocha's mauve: the second web-of-trust hue. The one-hop circle
/// glyph wears the accent (blue or green by theme); the two-hop glyph
/// wears this, a sibling hue that stays distinct under either theme
/// without borrowing the risk colors' warn/danger meanings.
pub const CIRCLE2: u32 = 0xCBA6F7;

/// Risk color for permission badges.
pub fn risk_color(risk: Risk) -> Rgba {
    match risk {
        Risk::Low => rgb(RISK_LOW),
        Risk::Medium => rgb(RISK_MED),
        Risk::High => get(&URGENT),
    }
}

/// The two-hop web-of-trust color.
pub fn circle2() -> Rgba {
    rgb(CIRCLE2)
}

// ---------- type scale (kuma-shell: 12 body, 11 small, 10 label) ----------

pub const TEXT_SIZE: f32 = 12.;
pub const TEXT_SIZE_SMALL: f32 = 11.;
pub const TEXT_SIZE_LABEL: f32 = 10.;
pub const TEXT_SIZE_EMPHASIS: f32 = 13.;

// ---------- shared elements ----------

/// A small tinted label: colored text on a whisper of the same color,
/// kuma's rubber-band trick.
pub fn badge(text: &str, fg: Rgba) -> Div {
    // keep the rgb bytes, swap in a ~13% alpha fill
    let tint: u32 = (u32::from(fg) & 0xFF_FF_FF00) | 0x22;
    div()
        .px_2()
        .py_0p5()
        .rounded_sm()
        .text_size(px(TEXT_SIZE_SMALL))
        .text_color(fg)
        .bg(rgba(tint))
        .child(text.to_string())
}

/// An inline SVG icon tinted by the given color. Paths resolve through
/// the asset source (`icons/…svg`); the glyph follows the text color.
pub fn icon(path: &'static str, size: f32, color: Rgba) -> gpui::AnyElement {
    gpui::svg()
        .path(path)
        .size(px(size))
        .text_color(color)
        .into_any_element()
}

/// A button. `accent` styles the primary action: solid accent with
/// dark text, matching the active filter chips. Secondary buttons are
/// quiet surfaces with a border that brightens on hover. `icon` is an
/// optional leading glyph (`icons/…svg`).
pub fn button(
    id: impl Into<SharedString>,
    glyph: Option<&'static str>,
    label: &str,
    accent: bool,
) -> gpui::Stateful<Div> {
    let base = div()
        .id(id.into())
        .flex()
        .items_center()
        .gap_1p5()
        .px_3()
        .py_1()
        .rounded_sm()
        .text_size(px(TEXT_SIZE))
        .cursor_pointer();
    let fg = if accent { accent_fg() } else { text() };
    let base = base.children(glyph.map(|path| icon(path, 13., fg)));
    if accent {
        base.bg(get(&ACCENT))
            .text_color(accent_fg())
            .hover(|style| style.bg(get(&ACCENT_HOVER)))
            .child(label.to_string())
    } else {
        base.bg(surface())
            .border_1()
            .border_color(divider())
            .text_color(text())
            .hover(|style| {
                style
                    .bg(get(&SURFACE_HOVER))
                    .border_color(text())
            })
            .child(label.to_string())
    }
}

/// The app's icon: the local PNG (a cached AppStream icon, or a disk
/// cache entry fetched by `cache::fetch`) when present, otherwise a
/// quiet accent-tinted tile with the app's initials. Remote URLs never
/// go to gpui directly: its URI image source is a NullHttpClient in
/// 0.2.2, so `cache.rs` resolves them to files first.
pub fn app_icon(icon_path: Option<&std::path::Path>, name: &str, size: f32) -> gpui::AnyElement {
    let tile = div()
        .flex_none()
        .w(px(size))
        .h(px(size))
        .flex()
        .items_center()
        .justify_center()
        .overflow_hidden()
        .rounded_sm();
    if let Some(path) = icon_path {
        tile.child(
            gpui::img(path)
                .size_full()
                .object_fit(ObjectFit::Contain),
        )
        .into_any_element()
    } else {
        let initials: String = name
            .split_whitespace()
            .filter_map(|word| word.chars().next())
            .take(2)
            .collect();
        tile.bg(get(&TILE_BG))
            .text_color(text_dim())
            .text_size(px(size * 0.4))
            .child(initials.to_uppercase())
            .into_any_element()
    }
}

/// A compact quiet button for inline row actions.
pub fn small_button(
    id: impl Into<SharedString>,
    glyph: Option<&'static str>,
    label: &str,
    accent: bool,
) -> gpui::Stateful<Div> {
    let base = div()
        .id(id.into())
        .flex()
        .items_center()
        .gap_1()
        .px_2()
        .py_0p5()
        .rounded_sm()
        .text_size(px(TEXT_SIZE_SMALL))
        .cursor_pointer();
    let fg = if accent { accent_fg() } else { text_dim() };
    let base = base.children(glyph.map(|path| icon(path, 11., fg)));
    if accent {
        base.bg(get(&ACCENT))
            .text_color(accent_fg())
            .hover(|style| style.bg(get(&ACCENT_HOVER)))
            .child(label.to_string())
    } else {
        base.bg(surface())
            .border_1()
            .border_color(divider())
            .text_color(text_dim())
            .hover(|style| style.text_color(text()).bg(surface_hover()))
            .child(label.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hsl_round_trips() {
        for rgb in [0xFF0000, 0x00FF88, 0x89B4FA, 0x181825, 0xFFFFFF, 0x000000, 0xCDD6F4] {
            let (h, s, l) = rgb_to_hsl(rgb);
            assert_eq!(hsl_to_rgb(h, s, l), rgb & 0xFFFFFF, "round trip {rgb:#x}");
        }
    }

    #[test]
    fn set_accent_rederives_surfaces() {
        set_accent(0x89B4FA);
        let selected_before = ROW_SELECTED.load(Ordering::Relaxed);
        set_accent(0xA6E3A1);
        let selected_after = ROW_SELECTED.load(Ordering::Relaxed);
        assert_ne!(selected_before, selected_after, "selection follows the accent hue");
        // restore the default look for other tests / later runs
        apply_palette(&Palette::default());
        assert_eq!(ROW_SELECTED.load(Ordering::Relaxed), selected_before);
    }
}
