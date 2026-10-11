//! Embedded SVG icons, served through gpui's asset source so `svg()`
//! elements can use them. Stroke-based, tinted by the text color.

use gpui::{AssetSource, Result, SharedString};

pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<std::borrow::Cow<'static, [u8]>>> {
        let bytes: Option<&'static [u8]> = match path {
            "icons/arrow_left.svg" => Some(include_bytes!("../icons/arrow_left.svg")),
            "icons/chevron_down.svg" => Some(include_bytes!("../icons/chevron_down.svg")),
            "icons/chevron_right.svg" => Some(include_bytes!("../icons/chevron_right.svg")),
            "icons/globe.svg" => Some(include_bytes!("../icons/globe.svg")),
            "icons/lock.svg" => Some(include_bytes!("../icons/lock.svg")),
            "icons/lock_open.svg" => Some(include_bytes!("../icons/lock_open.svg")),
            "icons/play.svg" => Some(include_bytes!("../icons/play.svg")),
            "icons/plus.svg" => Some(include_bytes!("../icons/plus.svg")),
            "icons/search.svg" => Some(include_bytes!("../icons/search.svg")),
            "icons/shield.svg" => Some(include_bytes!("../icons/shield.svg")),
            "icons/trash.svg" => Some(include_bytes!("../icons/trash.svg")),
            _ => None,
        };
        Ok(bytes.map(std::borrow::Cow::Borrowed))
    }

    fn list(&self, _path: &str) -> Result<Vec<SharedString>> {
        Ok(vec![])
    }
}
