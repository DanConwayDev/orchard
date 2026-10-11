//! Streaming parser for flatpak's AppStream catalog.
//!
//! Flatpak materializes every remote's metadata as an AppStream XML document
//! under `<install>/appstream/<remote>/<arch>/active/appstream.xml.gz`
//! (flathub's is around 50 MB uncompressed). We parse it with quick-xml in a
//! single streaming pass and keep only what the store front page needs. Only
//! the default (untranslated) strings are kept: the MVP UI is English-only.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use quick_xml::events::{BytesStart, Event};

/// An app entry from AppStream, already reduced to store-front shape.
#[derive(Debug, Clone)]
pub struct AppstreamApp {
    pub id: String,
    pub name: String,
    pub summary: String,
    /// The full (untranslated) description as plain text: block elements
    /// become lines, list items carry a bullet, inline markup stripped.
    pub description: Option<String>,
    /// Latest `<release>` version, if any.
    pub version: Option<String>,
    /// Latest `<release>` date, ISO `YYYY-MM-DD` (from a `date` attribute
    /// or a unix `timestamp`).
    pub release_date: Option<String>,
    /// `<url type="homepage">`, if any.
    pub homepage: Option<String>,
    pub license: Option<String>,
    /// Resolved path of a cached icon on disk (type="cached", width >= 64).
    pub icon_path: Option<PathBuf>,
    /// Remote icon URL (type="remote", width >= 64), fallback for [icon_path].
    pub icon_url: Option<String>,
    pub categories: Vec<String>,
    /// Screenshot image URLs, best first: source images in file order,
    /// then thumbnails widest first.
    pub screenshots: Vec<String>,
}

/// Candidate locations of the appstream for a remote, user install first,
/// then the system install. Flatpak keeps per-arch directories; callers pick
/// the first path that exists.
pub fn appstream_paths(remote: &str, arch: &str) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        roots.push(PathBuf::from(&home).join(".local/share/flatpak"));
    }
    if let Some(data) = std::env::var_os("XDG_DATA_HOME") {
        roots.push(PathBuf::from(data).join("flatpak"));
    }
    roots.push(PathBuf::from("/var/lib/flatpak"));

    roots
        .iter()
        .map(|root| {
            root.join("appstream")
                .join(remote)
                .join(arch)
                .join("active")
                .join("appstream.xml.gz")
        })
        .collect()
}

/// Parse an appstream.xml.gz file. `icons_root` is the `active/icons`
/// directory that `type="cached"` icon names resolve against.
pub fn load(path: &Path, icons_root: &Path) -> Result<Vec<AppstreamApp>> {
    let file =
        std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let gz = flate2::read::GzDecoder::new(file);
    let mut reader = quick_xml::Reader::from_reader(BufReader::new(gz));
    // trim_text stays off: quick-xml splits text around general entity
    // references (`&quot;` arrives as its own event) and trimming each
    // fragment would eat the whitespace between them. Element helpers
    // accumulate raw fragments and trim once instead.

    let mut apps = Vec::new();
    let mut current: Option<Component> = None;
    let mut buf = Vec::with_capacity(8192);
    // Today's date, the cutoff for future-dated release entries.
    let today = today_iso();
    // Nesting depth of the stream, the depth <component> sits at, and the
    // depth of the <screenshots> group (zero when outside one).
    let mut depth = 0usize;
    let mut comp_depth = 0usize;
    let mut screenshots_depth = 0usize;

    loop {
        // into_owned detaches the event from `buf` so helpers can reuse it.
        let event = reader.read_event_into(&mut buf)?.into_owned();
        match event {
            Event::Start(ref e) => {
                match e.name().as_ref() {
                    b"component" => {
                        comp_depth = depth;
                        depth += 1;
                        // only user-installable apps become rows:
                        // runtimes, addons, and the BaseApp plumbing
                        // (generic components) are not things a person
                        // installs from a store
                        let kind = attr_value(e, b"type")?.unwrap_or_default();
                        current = if matches!(
                            kind.as_str(),
                            "desktop-application" | "desktop" | "console-application"
                        ) {
                            Some(Component::default())
                        } else {
                            None
                        };
                    }
                    b"description" => {
                        // description_text consumes the whole balanced
                        // subtree including this element's End tag, so
                        // the depth counter must not count this Start
                        // either.
                        if let Some(c) = current.as_mut() {
                            if is_translation(e) {
                                // Per-language variant: appstream ships
                                // one <description> element per language
                                // (Lutris alone has twenty); the app's
                                // own wording is the untranslated one.
                                let text = description_text(&mut reader, &mut buf)?;
                                if c.description.is_none() && c.description_fallback.is_none() {
                                    c.description_fallback = Some(text.into_boxed_str());
                                }
                            } else if c.description.is_none() {
                                let text = description_text(&mut reader, &mut buf)?;
                                c.description = Some(text.into_boxed_str());
                            } else {
                                let _ = description_text(&mut reader, &mut buf)?;
                            }
                        }
                    }
                    _ => {
                        // Whether a helper already consumed this element's
                        // End tag; if so the depth counter must not count
                        // the subtree either.
                        let mut consumed = false;
                        if let Some(c) = current.as_mut() {
                            match e.name().as_ref() {
                                // The component id is only valid as a direct
                                // child. Nested ids (<replaces>, <provides>,
                                // <extends>, <developer>...) must never
                                // clobber it: apps keyed under a replaced id
                                // vanish from installed lookups.
                                b"id" if depth == comp_depth + 1 && c.id.is_none() => {
                                    c.id = leaf_text(&mut reader, &mut buf, e)?.map(Box::from);
                                    consumed = true;
                                }
                                b"name" => {
                                    c.name =
                                        first_leaf(&mut reader, &mut buf, e, c.name.take())?;
                                    consumed = true;
                                }
                                b"summary" => {
                                    c.summary = first_leaf(
                                        &mut reader, &mut buf, e, c.summary.take(),
                                    )?;
                                    consumed = true;
                                }
                                b"project_license" => {
                                    c.license = first_leaf(
                                        &mut reader, &mut buf, e, c.license.take(),
                                    )?;
                                    consumed = true;
                                }
                                b"release" => {
                                    let version = attr_value(e, b"version")?.map(Box::from);
                                    let date = release_date(e)?;
                                    if let Some(c) = current.as_mut() {
                                        c.add_release(version, date, &today);
                                    }
                                    consumed = true;
                                }
                                b"category" => {
                                    if let Some(cat) =
                                        leaf_text(&mut reader, &mut buf, e)?
                                    {
                                        c.categories.push(cat);
                                    }
                                    consumed = true;
                                }
                                b"url" if depth == comp_depth + 1 => {
                                    // only direct `<url type="…">` children
                                    // count; anything nested is noise
                                    let kind = attr_value(e, b"type")?.unwrap_or_default();
                                    if kind == "homepage" && c.homepage.is_none() {
                                        c.homepage =
                                            leaf_text(&mut reader, &mut buf, e)?
                                                .map(String::into_boxed_str);
                                    } else {
                                        consume_element(&mut reader, &mut buf)?;
                                    }
                                    consumed = true;
                                }
                                b"icon" => {
                                    read_icon(&mut reader, &mut buf, e, false, c)?;
                                    consumed = true;
                                }
                                b"image" if screenshots_depth > 0 => {
                                    read_screenshot_image(
                                        &mut reader, &mut buf, e, c,
                                    )?;
                                    consumed = true;
                                }
                                b"screenshots" => {
                                    screenshots_depth = depth;
                                }
                                _ => {}
                            }
                        }
                        if !consumed {
                            depth += 1;
                        }
                    }
                }
            }
            Event::End(ref e) => {
                let closing = e.name();
                if closing.as_ref() == b"screenshots"
                    && screenshots_depth == depth.saturating_sub(1)
                {
                    screenshots_depth = 0;
                }
                depth = depth.saturating_sub(1);
                if closing.as_ref() == b"component" {
                    if let Some(app) = current.take().and_then(|c| c.finish(icons_root)) {
                        apps.push(app);
                    }
                }
            }
            Event::Empty(ref e) => match e.name().as_ref() {
                b"release" => {
                    let version = attr_value(e, b"version")?.map(Box::from);
                    let date = release_date(e)?;
                    if let Some(c) = current.as_mut() {
                        c.add_release(version, date, &today);
                    }
                }
                b"icon" => {
                    if let Some(c) = current.as_mut() {
                        read_icon(&mut reader, &mut buf, e, true, c)?;
                    }
                }
                _ => {}
            },
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    Ok(apps)
}

/// Text of a leaf element, unless it carries an xml:lang (a translation we
/// skip in favor of the untranslated string). Consumes through the
/// element's End tag either way.
fn leaf_text(
    reader: &mut quick_xml::Reader<impl BufRead>,
    buf: &mut Vec<u8>,
    start: &BytesStart<'_>,
) -> Result<Option<String>> {
    if is_translation(start) {
        consume_element(reader, buf)?;
        return Ok(None);
    }
    consume_element(reader, buf)
}

/// Consume events through the matching End tag and return the element's
/// character data. quick-xml splits text around general entity references
/// (a `&quot;` arrives as its own `GeneralRef` event), so fragments are
/// concatenated raw and the whole is trimmed once.
fn consume_element(
    reader: &mut quick_xml::Reader<impl BufRead>,
    buf: &mut Vec<u8>,
) -> Result<Option<String>> {
    let mut text = String::new();
    loop {
        buf.clear();
        match reader.read_event_into(buf)? {
            Event::Text(t) => text.push_str(&t.xml_content()?),
            Event::GeneralRef(r) => text.push_str(&resolve_ref(&r)),
            Event::End(_) => break,
            Event::Eof => break,
            // Unexpected child structure inside a leaf: keep reading so
            // alignment holds.
            _ => {}
        }
    }
    let text = text.trim();
    if text.is_empty() {
        Ok(None)
    } else {
        Ok(Some(text.to_string()))
    }
}

/// Resolve an entity reference: XML predefined entities and numeric
/// character references; anything else (appstream defines no custom
/// entities) drops out.
fn resolve_ref(r: &quick_xml::events::BytesRef<'_>) -> String {
    let name = String::from_utf8_lossy(r.as_ref());
    if let Some(digits) = name.strip_prefix('#') {
        let code = if let Some(hex) = digits.strip_prefix('x').or_else(|| digits.strip_prefix('X'))
        {
            u32::from_str_radix(hex, 16).ok()
        } else {
            digits.parse::<u32>().ok()
        };
        return code.and_then(char::from_u32).map(String::from).unwrap_or_default();
    }
    quick_xml::escape::resolve_predefined_entity(&name)
        .unwrap_or_default()
        .to_string()
}

/// `first_leaf` but keeps the existing value: the first untranslated
/// occurrence wins.
fn first_leaf(
    reader: &mut quick_xml::Reader<impl BufRead>,
    buf: &mut Vec<u8>,
    start: &BytesStart<'_>,
    existing: Option<Box<str>>,
) -> Result<Option<Box<str>>> {
    if existing.is_some() {
        // Already have one; still consume the element for stream alignment.
        consume_element(reader, buf)?;
        return Ok(existing);
    }
    leaf_text(reader, buf, start).map(|t| t.map(String::into_boxed_str))
}

/// Collect the human text of a description subtree (stops at the matching
/// close tag). Block elements (`<p>`, `<li>`, headings, `<br/>`) close the
/// current line and list items get a bullet prefix; inline markup
/// (`<em>`, `<code>`, ...) continues the line. quick-xml splits text around
/// general entity references (a `&quot;` arrives as its own `GeneralRef`
/// event), so fragments are concatenated raw.
fn description_text(
    reader: &mut quick_xml::Reader<impl BufRead>,
    buf: &mut Vec<u8>,
) -> Result<String> {
    fn flush_run(run: &mut String, lines: &mut Vec<String>, item: bool) {
        // XML source newlines and indentation live inside runs; squash
        // every whitespace run to single spaces
        let words: Vec<&str> = run.split_whitespace().collect();
        if !words.is_empty() {
            let line = words.join(" ");
            if item {
                lines.push(format!("\u{2022} {line}"));
            } else {
                lines.push(line);
            }
        }
        run.clear();
    }

    let mut depth = 0usize;
    let mut run = String::new();
    let mut lines: Vec<String> = Vec::new();
    let mut in_item = false;
    loop {
        buf.clear();
        match reader.read_event_into(buf)? {
            Event::Start(e) => {
                depth += 1;
                match e.name().as_ref() {
                    b"li" => {
                        flush_run(&mut run, &mut lines, in_item);
                        in_item = true;
                    }
                    b"p" | b"h1" | b"h2" | b"h3" | b"h4" => {
                        flush_run(&mut run, &mut lines, in_item);
                        in_item = false;
                    }
                    _ => {}
                }
            }
            Event::Empty(e) => {
                // <br/> and friends: a hard break closes the line
                if e.name().as_ref() == b"br" {
                    flush_run(&mut run, &mut lines, in_item);
                }
            }
            Event::End(_) => {
                flush_run(&mut run, &mut lines, in_item);
                if depth == 0 {
                    break;
                }
                depth -= 1;
            }
            Event::Text(t) => run.push_str(&t.xml_content()?),
            // Mid-sentence references continue the current run.
            Event::GeneralRef(r) => run.push_str(&resolve_ref(&r)),
            Event::Eof => {
                flush_run(&mut run, &mut lines, in_item);
                break;
            }
            _ => {}
        }
    }
    Ok(lines.join("\n"))
}

fn attr_value(start: &BytesStart<'_>, key: &[u8]) -> Result<Option<String>> {
    for attr in start.attributes().flatten() {
        if attr.key.as_ref() == key {
            return Ok(Some(attr.unescape_value()?.into_owned()));
        }
    }
    Ok(None)
}

/// The `<release>` date: the ISO `date` attribute if present, else the
/// unix `timestamp` rendered as a civil date. Hand-rolled conversion keeps
/// the parser free of a date-time dependency.
fn release_date(start: &BytesStart<'_>) -> Result<Option<Box<str>>> {
    if let Some(date) = attr_value(start, b"date")? {
        let date = date.trim();
        if !date.is_empty() {
            return Ok(Some(date.to_string().into_boxed_str()));
        }
    }
    let Some(stamp) = attr_value(start, b"timestamp")?.and_then(|t| t.trim().parse::<i64>().ok())
    else {
        return Ok(None);
    };
    let (y, m, d) = days_to_ymd(stamp.div_euclid(86_400));
    Ok(Some(format!("{y:04}-{m:02}-{d:02}").into_boxed_str()))
}

/// Today as an ISO date, from the system clock: the cutoff that
/// demotes future-dated release entries. Public because the client
/// re-checks arrival at sort and display time: a pre-listed date
/// starts ranking the day it arrives, no restart.
pub fn today_iso() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (y, m, d) = days_to_ymd((secs / 86_400) as i64);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Days since the unix epoch to a `(year, month, day)` civil date:
/// Howard Hinnant's algorithm, the inverse of `days_from_civil`.
fn days_to_ymd(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe as i64 + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

fn is_translation(start: &BytesStart<'_>) -> bool {
    start
        .attributes()
        .flatten()
        .any(|a| a.key.as_ref() == b"xml:lang")
}

/// Handle an `<icon>` element: attributes carry type/width, the text node
/// carries the name or URL.
fn read_icon(
    reader: &mut quick_xml::Reader<impl BufRead>,
    buf: &mut Vec<u8>,
    start: &BytesStart<'_>,
    empty: bool,
    component: &mut Component,
) -> Result<()> {
    let kind = attr_value(start, b"type")?.unwrap_or_default();
    let width: u32 = attr_value(start, b"width")?
        .and_then(|w| w.parse().ok())
        .unwrap_or(0);
    let value = if empty {
        String::new()
    } else {
        leaf_text(reader, buf, start)?.unwrap_or_default()
    };
    if width < 64 || value.is_empty() {
        return Ok(());
    }
    match kind.as_str() {
        "cached" if component.cached_icon_name.is_none() => {
            component.cached_icon_name = Some(value);
        }
        "remote" if component.icon_url.is_none() => {
            component.icon_url = Some(value);
        }
        _ => {}
    }
    Ok(())
}

/// Handle an `<image>` element inside `<screenshots>`: attributes carry
/// type/width, the text node carries the URL.
fn read_screenshot_image(
    reader: &mut quick_xml::Reader<impl BufRead>,
    buf: &mut Vec<u8>,
    start: &BytesStart<'_>,
    component: &mut Component,
) -> Result<()> {
    let kind = attr_value(start, b"type")?.unwrap_or_default();
    let width: u32 = attr_value(start, b"width")?
        .and_then(|w| w.parse().ok())
        .unwrap_or(0);
    let url = leaf_text(reader, buf, start)?.unwrap_or_default();
    if url.is_empty() {
        return Ok(());
    }
    component.screenshots.push(ScreenshotImage {
        kind: kind.into_boxed_str(),
        width,
        url,
    });
    Ok(())
}

/// One `<image>` entry from a `<screenshot>`.
struct ScreenshotImage {
    /// AppStream type: "source" (full resolution) or "thumbnail".
    kind: Box<str>,
    width: u32,
    url: String,
}

/// Accumulator for one `<component>` element.
#[derive(Default)]
struct Component {
    id: Option<Box<str>>,
    name: Option<Box<str>>,
    summary: Option<Box<str>>,
    description: Option<Box<str>>,
    /// First per-language description element seen, kept only for
    /// components that ship no default (untranslated) one.
    description_fallback: Option<Box<str>>,
    license: Option<Box<str>>,
    version: Option<Box<str>>,
    release_date: Option<Box<str>>,
    /// The stored release date is in the future (an upstream
    /// pre-listed release); a past-dated one may still replace it.
    release_date_future: bool,
    homepage: Option<Box<str>>,
    categories: Vec<String>,
    cached_icon_name: Option<String>,
    icon_url: Option<String>,
    screenshots: Vec<ScreenshotImage>,
}

impl Component {
    /// Fold one `<release>` element's version and date. The first
    /// version seen wins; the stored date prefers the newest release
    /// that has actually happened, since upstreams pre-list
    /// future-dated entries that would otherwise read as "updated
    /// <future date>".
    fn add_release(
        &mut self,
        version: Option<Box<str>>,
        date: Option<Box<str>>,
        today: &str,
    ) {
        if self.version.is_none() {
            self.version = version;
        }
        if let Some(date) = date {
            let future = date.as_ref() > today;
            let store = match &self.release_date {
                None => true,
                // a past date upgrades a future-stamped one; a later
                // future date never beats an earlier one
                Some(_) => self.release_date_future && !future,
            };
            if store {
                self.release_date = Some(date);
                self.release_date_future = future;
            }
        }
    }
}

impl Component {
    /// Only components with an id, name, and summary become store rows.
    fn finish(mut self, icons_root: &Path) -> Option<AppstreamApp> {
        let id = self.id.take()?.into_string();
        let name = self.name.take()?.into_string();
        let summary = self.summary.take()?.into_string();
        if id.is_empty() || name.is_empty() {
            return None;
        }
        // Components that ship only per-language description elements
        // still get their first one rather than nothing.
        if self.description.is_none() {
            self.description = self.description_fallback.take();
        }
        let icon_path = self
            .cached_icon_name
            .take()
            .and_then(|basename| {
                // Flatpak stores cached icons under size directories; the
                // 64-wide slot covers small sizes, 128 the rest.
                ["64x64", "128x128"]
                    .iter()
                    .map(|size| icons_root.join(size).join(&basename))
                    .find(|candidate| candidate.exists())
            });
        // Screenshots: source images in file order first, then thumbnails
        // widest first, deduplicated.
        let raw = std::mem::take(&mut self.screenshots);
        let mut seen = std::collections::HashSet::new();
        let mut screenshots: Vec<String> = Vec::new();
        let mut thumbnails: Vec<&ScreenshotImage> =
            raw.iter().filter(|img| img.kind.as_ref() != "source").collect();
        thumbnails.sort_by(|a, b| b.width.cmp(&a.width));
        for img in raw.iter().filter(|img| img.kind.as_ref() == "source").chain(thumbnails) {
            if seen.insert(img.url.clone()) && screenshots.len() < 6 {
                screenshots.push(img.url.clone());
            }
        }
        Some(AppstreamApp {
            id,
            name,
            summary,
            description: self.description.take().map(String::from),
            version: self.version.take().map(String::from),
            release_date: self.release_date.take().map(String::from),
            homepage: self.homepage.take().map(String::from),
            license: self.license.take().map(String::from),
            icon_path,
            icon_url: self.icon_url.take(),
            categories: canonicalize_categories(&self.categories),
            screenshots,
        })
    }
}

/// Fold an AppStream category onto the top-level set the store browses
/// by: subcategories map to their parent (ArcadeGame to Games), and
/// names outside the browse set drop out. `None` for anything the
/// browse row does not show.
pub fn canonical_category(raw: &str) -> Option<&'static str> {
    let name = match raw {
        "Game" | "ArcadeGame" | "Adventure" | "BlocksGame" | "BoardGame" | "CardGame"
        | "Emulator" | "KidsGame" | "LogicGame" | "RolePlaying" | "ShooterGame"
        | "SimulationGame" | "SportsGame" | "StrategyGame" | "Sports" => "Games",
        "AudioVideo" | "Audio" | "Video" | "Music" | "Player" | "Midi" | "Mixer"
        | "Recorder" | "DiscBurning" | "Tuner" => "Audio & Video",
        "Development" | "IDE" | "Debugger" | "GUIDesigner" | "Profiling"
        | "RevisionControl" | "Translation" | "WebDevelopment" => "Development",
        "Network" | "Chat" | "Email" | "FileTransfer" | "HamRadio" | "InstantMessaging"
        | "IRCClient" | "Monitor" | "News" | "P2P" | "RemoteAccess" | "Telephony"
        | "VideoConference" | "WebBrowser" => "Network",
        "Office" | "Calendar" | "ContactManagement" | "Chart" | "Finance" | "FlowChart"
        | "Notebook" | "Presentation" | "ProjectManagement" | "Publishing"
        | "Spreadsheet" | "WordProcessor" => "Office",
        "Graphics" | "2DGraphics" | "3DGraphics" | "VectorGraphics" | "RasterGraphics"
        | "Scanning" | "Photography" | "Art" => "Graphics",
        "Science" | "Astronomy" | "Biology" | "Chemistry" | "Geology" | "Math"
        | "NumericalAnalysis" | "Physics" | "Robotics" => "Science",
        "Education" | "Languages" | "Literature" | "ArtificialIntelligence" => "Education",
        "System" | "Core" | "Electric" | "Emergency" | "HardwareSettings"
        | "PackageManager" | "Settings" | "TerminalEmulator" => "System",
        "Utility" | "Accessibility" | "Archiving" | "Calculator" | "Clock" | "Compression"
        | "Security" | "TextEditor" | "Dictionary" | "Viewer" => "Utilities",
        _ => return None,
    };
    Some(name)
}

/// Canonicalize and deduplicate, keeping first-occurrence order.
pub fn canonicalize_categories(raw: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for name in raw.iter().filter_map(|raw| canonical_category(raw)) {
        if !out.iter().any(|seen| seen == name) {
            out.push(name.to_string());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_categories_fold_and_dedup() {
        let raw: Vec<String> = ["ArcadeGame", "Game", "Widget", "Utility", "Player"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            canonicalize_categories(&raw),
            ["Games", "Utilities", "Audio & Video"]
        );
        assert_eq!(canonical_category("CompletelyMadeUp"), None);
    }

    /// A tiny but representative appstream: two components, one with every
    /// field we care about, one minimal.
    const SAMPLE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<components version="0.8" origin="test">
  <component type="desktop-application">
    <id>com.example.Full</id>
    <name>Full App</name>
    <summary>Does everything</summary>
    <summary xml:lang="de">Macht alles</summary>
    <project_license>MIT</project_license>
    <description>
      <p>First paragraph.</p>
      <ul><li>Point one</li><li>Point two</li></ul>
    </description>
    <replaces>
      <id>com.example.Old</id>
    </replaces>
    <developer id="com.example.Full">
      <name>Example Team</name>
    </developer>
    <url type="homepage">https://example.com/full</url>
    <releases>
      <release version="1.2.3" date="2026-02-01"/>
      <release version="1.2.2" timestamp="1700000000"/>
    </releases>
    <categories><category>Utility</category><category>Game</category></categories>
    <icon height="64" type="cached" width="64">com.example.Full.png</icon>
    <icon height="128" type="remote" width="128">https://example.com/full.png</icon>
    <screenshots>
      <screenshot type="default">
        <image type="source" width="2424" height="1515">https://example.com/shot-full.png</image>
        <image type="thumbnail" width="752" height="471">https://example.com/shot-small.png</image>
      </screenshot>
      <screenshot type="extra">
        <image type="thumbnail" width="1120" height="700">https://example.com/shot2-small.png</image>
      </screenshot>
    </screenshots>
  </component>
  <component type="desktop-application">
    <id>com.example.Minimal</id>
    <name>Minimal</name>
    <summary>Just the basics</summary>
  </component>
  <component type="desktop-application">
    <id>com.example.Quoted</id>
    <name>Quoted</name>
    <summary>&quot;Five in a row&quot; board &amp; puzzle game</summary>
    <description>
      <p>It&apos;s a game with &amp; symbols</p>
    </description>
    <releases>
      <release version="0.9.0" timestamp="1700000000"/>
    </releases>
  </component>
</components>"#;

    #[test]
    fn parses_components() {
        let dir = std::env::temp_dir().join("orchard-appstream-test");
        let icons = dir.join("icons/64x64");
        std::fs::create_dir_all(&icons).unwrap();
        std::fs::write(icons.join("com.example.Full.png"), b"png").unwrap();
        let xml_path = dir.join("appstream.xml.gz");
        let file = std::fs::File::create(&xml_path).unwrap();
        let mut gz = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        std::io::Write::write_all(&mut gz, SAMPLE.as_bytes()).unwrap();
        gz.finish().unwrap();

        let apps = load(&xml_path, &dir.join("icons")).unwrap();
        assert_eq!(apps.len(), 3);

        let full = &apps[0];
        assert_eq!(full.id, "com.example.Full");
        assert_eq!(full.name, "Full App");
        assert_eq!(full.summary, "Does everything");
        assert_eq!(full.version.as_deref(), Some("1.2.3"));
        assert_eq!(full.release_date.as_deref(), Some("2026-02-01"));
        assert_eq!(full.homepage.as_deref(), Some("https://example.com/full"));
        assert_eq!(full.license.as_deref(), Some("MIT"));
        assert_eq!(full.categories, ["Utilities", "Games"]);
        // Description keeps every block as a line; list items get bullets.
        assert_eq!(
            full.description.as_deref(),
            Some("First paragraph.\n\u{2022} Point one\n\u{2022} Point two")
        );
        // Cached icon resolves under 64x64.
        assert_eq!(
            full.icon_path.as_deref(),
            Some(icons.join("com.example.Full.png").as_path())
        );
        assert_eq!(
            full.icon_url.as_deref(),
            Some("https://example.com/full.png")
        );
        // The nested id in <replaces> must not clobber the component id,
        // and the developer name must not clobber the app name.
        assert_eq!(full.id, "com.example.Full");
        assert_eq!(full.name, "Full App");
        // Source screenshot first, then thumbnails widest first.
        assert_eq!(
            full.screenshots,
            [
                "https://example.com/shot-full.png",
                "https://example.com/shot2-small.png",
                "https://example.com/shot-small.png"
            ]
        );

        let minimal = &apps[1];
        assert_eq!(minimal.id, "com.example.Minimal");
        assert!(minimal.description.is_none());
        assert!(minimal.version.is_none());
        assert!(minimal.release_date.is_none());
        assert!(minimal.homepage.is_none());

        // Text starting with an entity reference must not be lost
        // (quick-xml emits the reference as a separate event), and
        // spacing around split fragments must survive.
        let quoted = &apps[2];
        assert_eq!(quoted.id, "com.example.Quoted");
        assert_eq!(
            quoted.summary,
            "\"Five in a row\" board & puzzle game"
        );
        assert_eq!(quoted.description.as_deref(), Some("It's a game with & symbols"));
        // A unix timestamp release date renders as a civil date.
        assert_eq!(quoted.version.as_deref(), Some("0.9.0"));
        assert_eq!(quoted.release_date.as_deref(), Some("2023-11-14"));
    }

    #[test]
    fn description_skips_per_language_elements() {
        // Lutris-style appstream: one <description> element per
        // language, the untranslated one not first. The app's own
        // wording wins over whatever translation comes first.
        let apps = parse_doc("multi-lang", r#"<?xml version="1.0" encoding="UTF-8"?>
<components version="0.8" origin="test">
  <component type="desktop-application">
    <id>com.example.Multi</id>
    <name>Multi</name>
    <summary>Does things</summary>
    <description xml:lang="ar">وصف بالعربية</description>
    <description xml:lang="de">Beschreibung auf Deutsch</description>
    <description><p>The real description.</p></description>
  </component>
</components>"#);
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].description.as_deref(), Some("The real description."));
    }

    #[test]
    fn description_falls_back_to_first_language() {
        // No default description element at all: better to show a
        // translation than nothing.
        let apps = parse_doc("translated-only", r#"<?xml version="1.0" encoding="UTF-8"?>
<components version="0.8" origin="test">
  <component type="desktop-application">
    <id>com.example.Translated</id>
    <name>Translated</name>
    <summary>Does things</summary>
    <description xml:lang="ar">وصف بالعربية</description>
    <description xml:lang="de">Beschreibung auf Deutsch</description>
  </component>
</components>"#);
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].description.as_deref(), Some("وصف بالعربية"));
    }

    #[test]
    fn junk_components_are_skipped() {
        // Addons, runtimes, and generic plumbing (BaseApps) are not
        // things a person installs from a store.
        let apps = parse_doc("junk", r#"<?xml version="1.0" encoding="UTF-8"?>
<components version="0.8" origin="test">
  <component type="generic">
    <id>io.qt.qtwebkit.BaseApp</id>
    <name>Qt WebKit BaseApp</name>
    <summary>Plumbing</summary>
  </component>
  <component type="addon">
    <id>com.example.Some.Addon</id>
    <name>Addon</name>
    <summary>An addon</summary>
  </component>
  <component type="desktop-application">
    <id>com.example.Real</id>
    <name>Real App</name>
    <summary>Does things</summary>
  </component>
</components>"#);
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].id, "com.example.Real");
    }

    #[test]
    fn future_dated_releases_are_demoted() {
        // Upstreams pre-list future-dated releases; the newest release
        // that has actually happened is the honest "updated" date.
        let apps = parse_doc("future-release", r#"<?xml version="1.0" encoding="UTF-8"?>
<components version="0.8" origin="test">
  <component type="desktop-application">
    <id>com.example.Future</id>
    <name>Future</name>
    <summary>Does things</summary>
    <releases>
      <release version="2.0" date="2099-01-01"/>
      <release version="1.9" date="2026-08-01"/>
    </releases>
  </component>
</components>"#);
        assert_eq!(apps.len(), 1);
        // the version stays from the first-listed release; the date
        // comes from the newest one that is not in the future
        assert_eq!(apps[0].version.as_deref(), Some("2.0"));
        assert_eq!(apps[0].release_date.as_deref(), Some("2026-08-01"));
    }

    #[test]
    fn future_only_release_keeps_the_claim() {
        // an app whose only dated release is in the future keeps the
        // claim (the client shows it as "dated", not "updated") but
        // the client ranks such an app as undated; there is no past
        // release for the parser to demote to
        let apps = parse_doc("future-only", r#"<?xml version="1.0" encoding="UTF-8"?>
<components version="0.8" origin="test">
  <component type="desktop-application">
    <id>com.example.Prelist</id>
    <name>Prelist</name>
    <summary>Does things</summary>
    <releases>
      <release version="2.0" date="2099-01-01"/>
    </releases>
  </component>
</components>"#);
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].version.as_deref(), Some("2.0"));
        assert_eq!(apps[0].release_date.as_deref(), Some("2099-01-01"));
    }

    /// Parse one appstream document through the same path `load` uses
    /// (gzip file), returning its components. Each test gets its own
    /// file: tests run in parallel threads.
    fn parse_doc(name: &str, doc: &str) -> Vec<AppstreamApp> {
        let dir = std::env::temp_dir().join(format!("orchard-appstream-test-{name}"));
        std::fs::create_dir_all(&dir).unwrap();
        let xml_path = dir.join("appstream.xml.gz");
        let file = std::fs::File::create(&xml_path).unwrap();
        let mut gz = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        std::io::Write::write_all(&mut gz, doc.as_bytes()).unwrap();
        gz.finish().unwrap();
        load(&xml_path, &dir.join("icons")).unwrap()
    }

    #[test]
    fn paths_cover_user_and_system_installs() {
        let paths = appstream_paths("flathub", "x86_64");
        assert!(paths.len() >= 2);
        assert_eq!(
            paths.last().unwrap(),
            &PathBuf::from("/var/lib/flatpak/appstream/flathub/x86_64/active/appstream.xml.gz")
        );
    }
}
