//! Orchard — the permissionless Flatpak store client.
//!
//! M1 target: browse the local Flathub AppStream catalog, search, install.
//! For now this is the GPUI shell (window + placeholder view) so the UI
//! framework is pinned and building early.
//!
//! Note: GPUI's API is still fluid; this file targets the published
//! `gpui = "0.2"` crate and may need small signature adjustments on upgrade.

use gpui::{
    div, prelude::*, px, rgb, size, App, Application, Bounds, Context, Window, WindowBounds,
    WindowOptions,
};

const BG: u32 = 0x1b_24_30;
const FG: u32 = 0xd8_de_e9;
const ACCENT: u32 = 0x8f_bc_bb;

struct Shell;

impl gpui::Render for Shell {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_2()
            .bg(rgb(BG))
            .text_color(rgb(FG))
            .child(
                div()
                    .text_size(px(28.))
                    .child("Orchard"),
            )
            .child(
                div()
                    .text_size(px(13.))
                    .text_color(rgb(ACCENT))
                    .child("The permissionless Flatpak store — see DESIGN.md for the roadmap"),
            )
    }
}

fn main() {
    Application::new().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(960.), px(600.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_, cx| cx.new(|_| Shell),
        )
        .expect("failed to open Orchard window");
        cx.activate(true);
    });
}
