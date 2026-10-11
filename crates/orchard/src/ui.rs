//! Small shared UI helpers: the palette, badges, and icon tiles.

use gpui::{div, prelude::*, px, rgb, Div, Hsla, Rgba, SharedString};

pub const BG: u32 = 0x1b_24_30;
pub const PANEL: u32 = 0x24_32_42;
pub const PANEL_BRIGHT: u32 = 0x2c_3c_4e;
pub const FG: u32 = 0xd8_de_e9;
pub const MUTED: u32 = 0x8a_9b_ad;
pub const ACCENT: u32 = 0x8f_bc_bb;
pub const BORDER: u32 = 0x37_47_59;
pub const RED: u32 = 0xd0_71_61;
pub const GREEN: u32 = 0xa3_be_8c;
pub const YELLOW: u32 = 0xeb_cb_8b;

pub fn color(value: u32) -> Rgba {
    rgb(value)
}

/// Risk color for permission badges.
pub fn risk_color(risk: orchard_core::Risk) -> Rgba {
    match risk {
        orchard_core::Risk::Low => color(GREEN),
        orchard_core::Risk::Medium => color(YELLOW),
        orchard_core::Risk::High => color(RED),
    }
}

/// Small rounded label, e.g. a channel badge.
pub fn badge(text: &str, fg: Rgba) -> Div {
    div()
        .px_2()
        .py_0p5()
        .rounded_md()
        .text_size(px(11.))
        .text_color(fg)
        .bg(color(BG))
        .border_1()
        .border_color(fg)
        .child(text.to_string())
}

/// A colored square with the app's initials, standing in for icons.
pub fn icon_tile(app_id: &str, name: &str) -> Div {
    let hue = app_id
        .bytes()
        .fold(0u32, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u32));
    let tile = Hsla {
        h: (hue % 360) as f32 / 360.,
        s: 0.35,
        l: 0.38,
        a: 1.,
    };
    let initials: String = name
        .split_whitespace()
        .filter_map(|word| word.chars().next())
        .take(2)
        .collect();
    div()
        .w(px(40.))
        .h(px(40.))
        .rounded_lg()
        .bg(tile)
        .flex()
        .items_center()
        .justify_center()
        .text_color(color(FG))
        .text_size(px(16.))
        .child(initials.to_uppercase())
}

/// A button. `accent` styles the primary action.
pub fn button(
    id: &'static str,
    label: &str,
    accent: bool,
) -> gpui::Stateful<Div> {
    let (fg, bg) = if accent {
        (color(0x14_1a_21), color(ACCENT))
    } else {
        (color(FG), color(PANEL_BRIGHT))
    };
    div()
        .id(SharedString::from(id))
        .px_3()
        .py_1()
        .rounded_md()
        .text_size(px(13.))
        .text_color(fg)
        .bg(bg)
        .cursor_pointer()
        .hover(|style| style.bg(if accent { color(0x7a_a8_a7) } else { color(0x3a_4c_60) }))
        .child(label.to_string())
}
