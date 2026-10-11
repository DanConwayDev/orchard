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
    /// First paragraphs of the (untranslated) description, markup stripped.
    pub description: Option<String>,
    /// Latest `<release>` version, if any.
    pub version: Option<String>,
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
                        current = Some(Component::default());
                    }
                    b"description" => {
                        // subtree_text consumes the whole balanced subtree
                        // including this element's End tag, so the depth
                        // counter must not count this Start either.
                        if let Some(c) = current.as_mut() {
                            if c.description.is_none() {
                                let text = subtree_text(&mut reader, &mut buf)?;
                                c.description = Some(text.into_boxed_str());
                            } else {
                                let _ = subtree_text(&mut reader, &mut buf)?;
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
                                    if c.version.is_none() {
                                        c.version = attr_value(e, b"version")?.map(Box::from);
                                    }
                                }
                                b"category" => {
                                    if let Some(cat) =
                                        leaf_text(&mut reader, &mut buf, e)?
                                    {
                                        c.categories.push(cat);
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
                    if let Some(c) = current.as_mut() {
                        if c.version.is_none() {
                            c.version = attr_value(e, b"version")?.map(Box::from);
                        }
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

/// Collect human text from a subtree (stops at the matching close tag).
/// Runs are flushed at element boundaries so paragraphs join with single
/// spaces while entity-split fragments stay glued together.
fn subtree_text(
    reader: &mut quick_xml::Reader<impl BufRead>,
    buf: &mut Vec<u8>,
) -> Result<String> {
    fn flush_run(run: &mut String, pieces: &mut Vec<String>) {
        let trimmed = run.trim();
        if !trimmed.is_empty() {
            pieces.push(trimmed.to_string());
        }
        run.clear();
    }

    let mut depth = 0usize;
    let mut run = String::new();
    let mut pieces: Vec<String> = Vec::new();
    loop {
        buf.clear();
        match reader.read_event_into(buf)? {
            Event::Start(_) => {
                flush_run(&mut run, &mut pieces);
                depth += 1;
            }
            Event::End(_) => {
                flush_run(&mut run, &mut pieces);
                if depth == 0 {
                    break;
                }
                depth -= 1;
            }
            Event::Text(t) => run.push_str(&t.xml_content()?),
            // Mid-sentence references continue the current run.
            Event::GeneralRef(r) => run.push_str(&resolve_ref(&r)),
            Event::Eof => {
                flush_run(&mut run, &mut pieces);
                break;
            }
            _ => {}
        }
    }
    Ok(pieces.join(" "))
}

fn attr_value(start: &BytesStart<'_>, key: &[u8]) -> Result<Option<String>> {
    for attr in start.attributes().flatten() {
        if attr.key.as_ref() == key {
            return Ok(Some(attr.unescape_value()?.into_owned()));
        }
    }
    Ok(None)
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
    license: Option<Box<str>>,
    version: Option<Box<str>>,
    categories: Vec<String>,
    cached_icon_name: Option<String>,
    icon_url: Option<String>,
    screenshots: Vec<ScreenshotImage>,
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
    <releases>
      <release version="1.2.3" timestamp="1700000000"/>
      <release version="1.2.2" timestamp="1600000000"/>
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
        assert_eq!(full.license.as_deref(), Some("MIT"));
        assert_eq!(full.categories, ["Utilities", "Games"]);
        // Description keeps the text, drops the markup.
        assert!(full
            .description
            .as_deref()
            .is_some_and(|d| d.contains("First paragraph.")));
        assert!(full
            .description
            .as_deref()
            .is_some_and(|d| d.contains("Point two")));
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
