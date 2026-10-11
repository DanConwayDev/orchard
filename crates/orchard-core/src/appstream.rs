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
    reader.config_mut().trim_text(true);

    let mut apps = Vec::new();
    let mut current: Option<Component> = None;
    let mut buf = Vec::with_capacity(8192);

    loop {
        // into_owned detaches the event from `buf` so helpers can reuse it.
        let event = reader.read_event_into(&mut buf)?.into_owned();
        match event {
            Event::Start(ref e) | Event::Empty(ref e) => {
                let empty = matches!(event, Event::Empty(_));
                match e.name().as_ref() {
                    b"component" => current = Some(Component::default()),
                    b"id" if !empty => {
                        if let Some(c) = current.as_mut() {
                            c.id = leaf_text(&mut reader, &mut buf, e)?.map(Box::from);
                        }
                    }
                    b"name" if !empty => {
                        if let Some(c) = current.as_mut() {
                            c.name = first_leaf(&mut reader, &mut buf, e, c.name.take())?;
                        }
                    }
                    b"summary" if !empty => {
                        if let Some(c) = current.as_mut() {
                            c.summary =
                                first_leaf(&mut reader, &mut buf, e, c.summary.take())?;
                        }
                    }
                    b"description" if !empty => {
                        if let Some(c) = current.as_mut() {
                            if c.description.is_none() {
                                let text = subtree_text(&mut reader, &mut buf)?;
                                c.description = Some(text.into_boxed_str());
                            }
                        }
                    }
                    b"project_license" if !empty => {
                        if let Some(c) = current.as_mut() {
                            c.license =
                                first_leaf(&mut reader, &mut buf, e, c.license.take())?;
                        }
                    }
                    b"release" => {
                        if let Some(c) = current.as_mut() {
                            if c.version.is_none() {
                                c.version = attr_value(e, b"version")?.map(Box::from);
                            }
                        }
                    }
                    b"category" if !empty => {
                        if let (Some(c), Some(cat)) = (
                            current.as_mut(),
                            leaf_text(&mut reader, &mut buf, e)?,
                        ) {
                            c.categories.push(cat);
                        }
                    }
                    b"icon" => {
                        if let Some(c) = current.as_mut() {
                            read_icon(&mut reader, &mut buf, e, empty, c)?;
                        }
                    }
                    _ => {}
                }
            }
            Event::End(ref e) if e.name().as_ref() == b"component" => {
                if let Some(app) = current.take().and_then(|c| c.finish(icons_root)) {
                    apps.push(app);
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    Ok(apps)
}

/// Text of a leaf element, unless it carries an xml:lang (a translation we
/// skip in favor of the untranslated string).
fn leaf_text(
    reader: &mut quick_xml::Reader<impl BufRead>,
    buf: &mut Vec<u8>,
    start: &BytesStart<'_>,
) -> Result<Option<String>> {
    if is_translation(start) {
        // Consume the text node so the stream stays aligned.
        let _ = reader.read_event_into(buf)?;
        return Ok(None);
    }
    if start.is_empty() {
        return Ok(None);
    }
    match reader.read_event_into(buf)? {
        Event::Text(t) => {
            let text = t.xml_content()?.trim().to_string();
            if text.is_empty() {
                Ok(None)
            } else {
                Ok(Some(text))
            }
        }
        _ => Ok(None),
    }
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
        // Already have one; still consume the text node for stream alignment.
        if !start.is_empty() {
            let _ = reader.read_event_into(buf)?;
        }
        return Ok(existing);
    }
    leaf_text(reader, buf, start).map(|t| t.map(String::into_boxed_str))
}

/// Collect human text from a subtree (stops at the matching close tag).
fn subtree_text(
    reader: &mut quick_xml::Reader<impl BufRead>,
    buf: &mut Vec<u8>,
) -> Result<String> {
    let mut depth = 0usize;
    let mut pieces: Vec<String> = Vec::new();
    loop {
        buf.clear();
        match reader.read_event_into(buf)? {
            Event::Start(_) => depth += 1,
            Event::End(_) => {
                if depth == 0 {
                    break;
                }
                depth -= 1;
            }
            Event::Text(t) => {
                let text = t.xml_content()?.trim().to_string();
                if !text.is_empty() {
                    pieces.push(text);
                }
            }
            Event::Eof => break,
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
        Some(AppstreamApp {
            id,
            name,
            summary,
            description: self.description.take().map(String::from),
            version: self.version.take().map(String::from),
            license: self.license.take().map(String::from),
            icon_path,
            icon_url: self.icon_url.take(),
            categories: self.categories,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    <releases>
      <release version="1.2.3" timestamp="1700000000"/>
      <release version="1.2.2" timestamp="1600000000"/>
    </releases>
    <categories><category>Utility</category><category>Game</category></categories>
    <icon height="64" type="cached" width="64">com.example.Full.png</icon>
    <icon height="128" type="remote" width="128">https://example.com/full.png</icon>
  </component>
  <component type="desktop-application">
    <id>com.example.Minimal</id>
    <name>Minimal</name>
    <summary>Just the basics</summary>
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
        assert_eq!(apps.len(), 2);

        let full = &apps[0];
        assert_eq!(full.id, "com.example.Full");
        assert_eq!(full.name, "Full App");
        assert_eq!(full.summary, "Does everything");
        assert_eq!(full.version.as_deref(), Some("1.2.3"));
        assert_eq!(full.license.as_deref(), Some("MIT"));
        assert_eq!(full.categories, ["Utility", "Game"]);
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

        let minimal = &apps[1];
        assert_eq!(minimal.id, "com.example.Minimal");
        assert!(minimal.description.is_none());
        assert!(minimal.version.is_none());
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
