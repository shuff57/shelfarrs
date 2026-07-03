//! Scan-time metadata extraction, from the book files themselves — no external
//! APIs. EPUB: OPF title/author/description + cover image. CBZ: first page as
//! cover. Everything else: nothing (placeholder tile).
// ponytail: hand-rolled tag scan like the libgen plugin, not an XML crate —
// OPFs are machine-generated and regular. Swap in quick-xml if wild files break it.

use crate::AppState;
use anyhow::{anyhow, Context, Result};
use sqlx::Row;
use std::io::Read;
use std::path::Path;

#[derive(Default)]
pub struct Extracted {
    pub title: Option<String>,
    pub author: Option<String>,
    pub description: Option<String>,
    pub series: Option<String>,
    pub series_index: Option<f64>,
    /// (bytes, file extension)
    pub cover: Option<(Vec<u8>, String)>,
}

const IMAGE_EXTS: &[&str] = &["jpg", "jpeg", "png", "gif", "webp"];

pub fn extract(path: &Path, format: &str) -> Extracted {
    let res = match format {
        "epub" => extract_epub(path),
        "cbz" => extract_cbz(path),
        _ => return Extracted::default(),
    };
    res.unwrap_or_else(|e| {
        tracing::warn!("metadata extraction failed for {}: {e}", path.display());
        Extracted::default()
    })
}

/// Enrich every book not yet metadata-scanned; marks each done regardless of
/// outcome so failures don't rescan forever. Returns how many were processed.
pub async fn enrich_pending(state: &AppState) -> usize {
    let rows = sqlx::query("SELECT id, path, format FROM books WHERE meta_done=0")
        .fetch_all(&state.pool)
        .await
        .unwrap_or_default();
    let mut n = 0;
    for r in rows {
        let id: i64 = r.get("id");
        let path: String = r.get("path");
        let format: String = r.get("format");
        let ex = extract(Path::new(&path), &format);

        let mut cover_name: Option<String> = None;
        if let Some((bytes, ext)) = &ex.cover {
            let name = format!("{id}.{ext}");
            match std::fs::write(state.covers_dir.join(&name), bytes) {
                Ok(_) => cover_name = Some(name),
                Err(e) => tracing::warn!("could not save cover for book {id}: {e}"),
            }
        }

        // Extracted fields win when present (epub metadata beats a filename);
        // NULLs keep whatever the scan or source candidate provided.
        if let Err(e) = sqlx::query(
            "UPDATE books SET title=COALESCE(?,title), author=COALESCE(?,author),
             description=COALESCE(?,description), cover=COALESCE(?,cover),
             series=COALESCE(?,series), series_index=COALESCE(?,series_index), meta_done=1
             WHERE id=?",
        )
        .bind(&ex.title)
        .bind(&ex.author)
        .bind(&ex.description)
        .bind(&cover_name)
        .bind(&ex.series)
        .bind(ex.series_index)
        .bind(id)
        .execute(&state.pool)
        .await
        {
            tracing::warn!("could not store metadata for book {id}: {e}");
        }
        n += 1;
    }
    n
}

// ---- epub ----

fn extract_epub(path: &Path) -> Result<Extracted> {
    let mut zip = zip::ZipArchive::new(std::fs::File::open(path)?)?;
    let container = read_string(&mut zip, "META-INF/container.xml")?;
    let rootfile = find_tag(&container, "rootfile")
        .and_then(|t| attr(&t, "full-path"))
        .context("container.xml has no rootfile")?;
    let opf = read_string(&mut zip, &rootfile)?;
    let opf_dir = Path::new(&rootfile).parent().unwrap_or(Path::new(""));

    let (series, series_index) = series_from_opf(&opf);
    let mut ex = Extracted {
        title: tag_text(&opf, "dc:title"),
        author: tag_text(&opf, "dc:creator"),
        description: tag_text(&opf, "dc:description").map(|d| strip_tags(&d)),
        series,
        series_index,
        cover: None,
    };

    if let Some(href) = cover_href(&opf) {
        let full = join_zip_path(opf_dir, &href);
        if let Ok(bytes) = read_bytes(&mut zip, &full) {
            let ext = href.rsplit('.').next().unwrap_or("jpg").to_lowercase();
            let ext = if ext == "jpeg" { "jpg".into() } else { ext };
            ex.cover = Some((bytes, ext));
        }
    }
    Ok(ex)
}

/// Series name + position: Calibre's `calibre:series[_index]` metas, else the
/// epub3 `belongs-to-collection` property with its `group-position` refine.
fn series_from_opf(opf: &str) -> (Option<String>, Option<f64>) {
    let metas = meta_elements(opf);
    let by_name = |n: &str| {
        metas
            .iter()
            .find(|(t, _)| attr(t, "name").as_deref() == Some(n))
            .and_then(|(t, _)| attr(t, "content"))
    };
    let mut name = by_name("calibre:series");
    let mut idx: Option<f64> = by_name("calibre:series_index").and_then(|s| s.parse().ok());

    if name.is_none() {
        if let Some((tag, text)) =
            metas.iter().find(|(t, _)| attr(t, "property").as_deref() == Some("belongs-to-collection"))
        {
            let t = unescape(text.trim());
            if !t.is_empty() {
                name = Some(t);
                if let Some(id) = attr(tag, "id") {
                    let refines = format!("#{id}");
                    idx = metas
                        .iter()
                        .find(|(t, _)| {
                            attr(t, "refines").as_deref() == Some(refines.as_str())
                                && attr(t, "property").as_deref() == Some("group-position")
                        })
                        .and_then(|(_, x)| x.trim().parse().ok());
                }
            }
        }
    }
    (name, idx)
}

/// Every `<meta ...>` element as (opening tag, inner text — empty when self-closing).
fn meta_elements(xml: &str) -> Vec<(String, String)> {
    let mut out = vec![];
    let mut at = 0;
    while let Some(i) = xml[at..].find("<meta") {
        let start = at + i;
        let after = start + 5;
        at = after;
        match xml.as_bytes().get(after) {
            Some(b) if b.is_ascii_whitespace() || *b == b'>' || *b == b'/' => {}
            _ => continue,
        }
        let Some(gt) = xml[start..].find('>') else { break };
        let open_end = start + gt + 1;
        let tag = xml[start..open_end].to_string();
        let inner = if tag.ends_with("/>") {
            String::new()
        } else {
            xml[open_end..]
                .find("</meta")
                .map(|c| xml[open_end..open_end + c].to_string())
                .unwrap_or_default()
        };
        out.push((tag, inner));
        at = open_end;
    }
    out
}

/// The cover image href from an OPF manifest: epub3 `properties="cover-image"`,
/// else epub2 `<meta name="cover" content="ID">`, else any image item named cover.
fn cover_href(opf: &str) -> Option<String> {
    let items: Vec<String> = all_tags(opf, "item");
    // epub3
    if let Some(href) = items
        .iter()
        .find(|t| attr(t, "properties").is_some_and(|p| p.contains("cover-image")))
        .and_then(|t| attr(t, "href"))
    {
        return Some(href);
    }
    // epub2: meta name="cover" content="<manifest id>"
    if let Some(id) = all_tags(opf, "meta")
        .iter()
        .find(|t| attr(t, "name").as_deref() == Some("cover"))
        .and_then(|t| attr(t, "content"))
    {
        if let Some(href) =
            items.iter().find(|t| attr(t, "id").as_deref() == Some(id.as_str())).and_then(|t| attr(t, "href"))
        {
            return Some(href);
        }
    }
    // fallback: an image item that mentions "cover"
    items
        .iter()
        .filter(|t| attr(t, "media-type").is_some_and(|m| m.starts_with("image/")))
        .find(|t| {
            let id = attr(t, "id").unwrap_or_default().to_lowercase();
            let href = attr(t, "href").unwrap_or_default().to_lowercase();
            id.contains("cover") || href.contains("cover")
        })
        .and_then(|t| attr(t, "href"))
}

// ---- cbz ----

fn extract_cbz(path: &Path) -> Result<Extracted> {
    let mut zip = zip::ZipArchive::new(std::fs::File::open(path)?)?;
    let mut names: Vec<String> = (0..zip.len())
        .filter_map(|i| zip.by_index(i).ok().map(|f| f.name().to_string()))
        .filter(|n| {
            let ext = n.rsplit('.').next().unwrap_or("").to_lowercase();
            IMAGE_EXTS.contains(&ext.as_str())
        })
        .collect();
    names.sort();
    let first = names.first().context("no images in cbz")?;
    let bytes = read_bytes(&mut zip, first)?;
    let ext = first.rsplit('.').next().unwrap_or("jpg").to_lowercase();
    let ext = if ext == "jpeg" { "jpg".into() } else { ext };
    Ok(Extracted { cover: Some((bytes, ext)), ..Default::default() })
}

// ---- zip + tag-scan helpers ----

fn read_bytes<R: Read + std::io::Seek>(zip: &mut zip::ZipArchive<R>, name: &str) -> Result<Vec<u8>> {
    let real = if zip.by_name(name).is_ok() {
        name.to_string()
    } else {
        name.replace("%20", " ")
    };
    let mut f = zip.by_name(&real).map_err(|_| anyhow!("no zip entry {name}"))?;
    let mut buf = Vec::with_capacity(f.size() as usize);
    f.read_to_end(&mut buf)?;
    Ok(buf)
}

fn read_string<R: Read + std::io::Seek>(zip: &mut zip::ZipArchive<R>, name: &str) -> Result<String> {
    Ok(String::from_utf8_lossy(&read_bytes(zip, name)?).into_owned())
}

/// Resolve `href` relative to the OPF's directory into a /-separated zip path,
/// collapsing `..` segments.
fn join_zip_path(base: &Path, href: &str) -> String {
    let base = base.to_string_lossy();
    let mut parts: Vec<&str> = base.split(['/', '\\']).filter(|s| !s.is_empty()).collect();
    for seg in href.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    parts.join("/")
}

/// First `<tag ...>` occurrence, returned with its attributes (up to `>`).
fn find_tag(xml: &str, tag: &str) -> Option<String> {
    all_tags(xml, tag).into_iter().next()
}

/// Every `<tag ...>` occurrence in document order.
fn all_tags(xml: &str, tag: &str) -> Vec<String> {
    let needle = format!("<{tag}");
    let mut out = vec![];
    let mut at = 0;
    while let Some(i) = xml[at..].find(&needle) {
        let start = at + i;
        let after = start + needle.len();
        at = after;
        // must be followed by whitespace, '>' or '/' — not a longer tag name
        match xml.as_bytes().get(after) {
            Some(b) if b.is_ascii_whitespace() || *b == b'>' || *b == b'/' => {}
            _ => continue,
        }
        if let Some(gt) = xml[start..].find('>') {
            out.push(xml[start..start + gt + 1].to_string());
            at = start + gt + 1;
        }
    }
    out
}

/// Text content of the first non-empty `<tag ...>text</tag>`.
fn tag_text(xml: &str, tag: &str) -> Option<String> {
    let needle = format!("<{tag}");
    let close = format!("</{tag}");
    let mut at = 0;
    while let Some(i) = xml[at..].find(&needle) {
        let start = at + i;
        let after = start + needle.len();
        at = after;
        match xml.as_bytes().get(after) {
            Some(b) if b.is_ascii_whitespace() || *b == b'>' => {}
            _ => continue,
        }
        let Some(gt) = xml[start..].find('>') else { break };
        let open_end = start + gt + 1;
        if xml.as_bytes()[open_end - 2] == b'/' {
            continue; // self-closing
        }
        let Some(c) = xml[open_end..].find(&close) else { continue };
        let inner = unescape(xml[open_end..open_end + c].trim());
        if !inner.is_empty() {
            return Some(inner);
        }
    }
    None
}

/// `name="value"` attribute from a raw tag string.
fn attr(tag: &str, name: &str) -> Option<String> {
    let needle = format!("{name}=\"");
    let i = tag.find(&needle)? + needle.len();
    let end = tag[i..].find('"')? + i;
    Some(unescape(&tag[i..end]))
}

fn unescape(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&#39;", "'")
}

/// Drop `<p>`-style markup some epubs embed in dc:description.
fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn zip_with(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut w = zip::ZipWriter::new(&mut buf);
            let opts: zip::write::FileOptions<()> = zip::write::FileOptions::default();
            for (name, data) in entries {
                w.start_file(*name, opts).unwrap();
                w.write_all(data).unwrap();
            }
            w.finish().unwrap();
        }
        buf.into_inner()
    }

    fn temp_file(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("meta-{}-{name}", std::process::id()));
        std::fs::write(&p, bytes).unwrap();
        p
    }

    #[test]
    fn epub_metadata_and_cover() {
        let container = br#"<?xml version="1.0"?><container>
            <rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles>
        </container>"#;
        let opf = br#"<?xml version="1.0"?><package>
          <metadata>
            <dc:title>Alice's Adventures &amp; More</dc:title>
            <dc:creator opf:role="aut">Lewis Carroll</dc:creator>
            <dc:description>&lt;p&gt;A girl falls down a rabbit hole.&lt;/p&gt;</dc:description>
            <meta name="cover" content="cov"/>
          </metadata>
          <manifest>
            <item id="cov" href="images/cover.jpg" media-type="image/jpeg"/>
            <item id="c1" href="ch1.xhtml" media-type="application/xhtml+xml"/>
          </manifest>
        </package>"#;
        let z = zip_with(&[
            ("META-INF/container.xml", container.as_slice()),
            ("OEBPS/content.opf", opf.as_slice()),
            ("OEBPS/images/cover.jpg", b"JPEGBYTES"),
        ]);
        let p = temp_file("book.epub", &z);
        let ex = extract(&p, "epub");
        assert_eq!(ex.title.as_deref(), Some("Alice's Adventures & More"));
        assert_eq!(ex.author.as_deref(), Some("Lewis Carroll"));
        assert_eq!(ex.description.as_deref(), Some("A girl falls down a rabbit hole."));
        let (bytes, ext) = ex.cover.expect("cover found");
        assert_eq!(bytes, b"JPEGBYTES");
        assert_eq!(ext, "jpg");
        std::fs::remove_file(p).ok();
    }

    #[test]
    fn series_calibre_and_epub3() {
        let calibre = r#"<metadata>
            <meta name="calibre:series" content="Wonderland Saga"/>
            <meta name="calibre:series_index" content="2.0"/>
        </metadata>"#;
        assert_eq!(
            series_from_opf(calibre),
            (Some("Wonderland Saga".into()), Some(2.0))
        );

        let epub3 = r##"<metadata>
            <meta property="belongs-to-collection" id="c01">The Empyrean</meta>
            <meta refines="#c01" property="collection-type">series</meta>
            <meta refines="#c01" property="group-position">3</meta>
        </metadata>"##;
        assert_eq!(series_from_opf(epub3), (Some("The Empyrean".into()), Some(3.0)));

        assert_eq!(series_from_opf("<metadata></metadata>"), (None, None));
    }

    #[test]
    fn cbz_first_page_is_cover() {
        let z = zip_with(&[("p02.png", b"PAGE2".as_slice()), ("p01.jpg", b"PAGE1")]);
        let p = temp_file("comic.cbz", &z);
        let ex = extract(&p, "cbz");
        let (bytes, ext) = ex.cover.expect("cover found");
        assert_eq!(bytes, b"PAGE1");
        assert_eq!(ext, "jpg");
        assert!(ex.title.is_none());
        std::fs::remove_file(p).ok();
    }

    #[test]
    fn unknown_format_is_empty() {
        let ex = extract(Path::new("nope.pdf"), "pdf");
        assert!(ex.title.is_none() && ex.cover.is_none());
    }
}
