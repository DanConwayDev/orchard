//! Embedded SVG icons (the chevron and lock for the detail dock),
//! served through gpui's asset source so `svg()` elements can use them.

use gpui::{AssetSource, Result, SharedString};

pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<std::borrow::Cow<'static, [u8]>>> {
        let bytes: Option<&'static [u8]> = match path {
            "icons/chevron_down.svg" => Some(include_bytes!("../icons/chevron_down.svg")),
            "icons/chevron_right.svg" => Some(include_bytes!("../icons/chevron_right.svg")),
            "icons/lock.svg" => Some(include_bytes!("../icons/lock.svg")),
            "icons/lock_open.svg" => Some(include_bytes!("../icons/lock_open.svg")),
            _ => None,
        };
        Ok(bytes.map(std::borrow::Cow::Borrowed))
    }

    fn list(&self, _path: &str) -> Result<Vec<SharedString>> {
        Ok(vec![])
    }
}
