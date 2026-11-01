use rayon::prelude::*;
use std::{
    env,
    fs,
    io::{Cursor, Read, Seek, Write},
    process,
};

use quick_xml::{events::Event, Reader};
use regex::Regex;
use scraper::{Html, Selector};
use serde::Serialize;
use zip::ZipArchive;

fn read_file_sirocco<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
    name: &str,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let idx = archive.index_for_name(name).ok_or_else(|| format!("File not found in archive: {name}"))?;

    let comp = archive.by_index(idx)?.compression();
    if matches!(comp, zip::CompressionMethod::Deflated) {
        let mut raw = archive.by_index_raw(idx)?;
        let mut compressed = Vec::new();
        raw.read_to_end(&mut compressed)?;
        sirocco::decompress(sirocco::Format::Deflate, &compressed, 64 << 20)
            .map_err(|e| format!("Sirocco decompress error for {name}: {e:?}").into())
    } else {
        let mut f = archive.by_index(idx)?;
        let mut buf = Vec::new();
        f.read_to_end(&mut buf)?;
        Ok(buf)
    }
}

const AO3_CATEGORIES: &[&str] = &["F/F", "F/M", "Gen", "M/M", "Multi", "Other"];

static RE_TITLE_PAGE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r"(?i).*title_page(_u\d+)?\.x?html$").unwrap());
static RE_CHAPTER_FILE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r"(?i).*/(file|chapter)\d+(_u\d+)?\.x?html$").unwrap());
static RE_WORKS_ID: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r"/works/(\d+)").unwrap());
static RE_SERIES_IDX: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r"\[(\d+)\]|Part\s+(\d+)").unwrap());
static RE_CHAPTER_HEADING: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r#"(?is)<(?:div|span)[^>]*class="[^"]*fff_chapter_title[^"]*"[^>]*>(.*?)</(?:div|span)>|<h[23][^>]*>(.*?)</h[23]>"#)
        .unwrap()
});
static RE_STRIP_TAGS: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r"<[^>]*>").unwrap());

#[derive(Serialize, Default)]
struct Author {
    name: String,
    url: Option<String>,
}

#[derive(Serialize, Default)]
struct Series {
    name: String,
    index: Option<u64>,
    url: Option<String>,
}

#[derive(Serialize, Default)]
struct Collection {
    name: String,
    url: Option<String>,
}

#[derive(Serialize, Default)]
struct ChapterInfo {
    title: Option<String>,
    url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    date: Option<String>,
}

#[derive(Serialize, Default)]
struct Dates {
    #[serde(skip_serializing_if = "Option::is_none")]
    published: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    updated: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    packaged: Option<String>,
}

#[derive(Serialize, Default)]
struct Stats {
    #[serde(skip_serializing_if = "Option::is_none")]
    kudos: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    hits: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bookmarks: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    comments: Option<u64>,
}

#[derive(Serialize, Default)]
struct Metadata {
    title: Option<String>,
    authors: Vec<Author>,
    story_id: Option<String>,
    url: Option<String>,
    rating: Option<String>,
    warnings: Vec<String>,
    category: Vec<String>,
    fandoms: Vec<String>,
    relationships: Vec<String>,
    characters: Vec<String>,
    freeform_tags: Vec<String>,
    language: Option<String>,
    status: Option<String>,
    words: Option<u64>,
    chapters_count: Option<u64>,
    chapters_total: Option<u64>,
    dates: Dates,
    stats: Stats,
    series: Vec<Series>,
    collections: Vec<Collection>,
    summary: Option<String>,
    chapters: Vec<ChapterInfo>,
}

fn to_int(val: &str) -> Option<u64> {
    let clean: String = val.chars().filter(|c| c.is_ascii_digit()).collect();
    (!clean.is_empty()).then(|| clean.parse().ok()).flatten()
}

fn read_zip_entry<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
    name: &str,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut f = archive.by_name(name)?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    Ok(buf)
}

fn extract_opf_info(opf_xml: &[u8]) -> (Option<String>, Option<String>) {
    let mut reader = Reader::from_reader(opf_xml);
    reader.config_mut().trim_text(true);
    let mut title = None;
    let mut source = None;
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                let name = e.name();
                let local = name.as_ref();
                let is_title = local.ends_with(b":title") || local == b"title";
                let is_source = local.ends_with(b":source") || local == b"source";

                if is_title || is_source {
                    if let Ok(text) = reader.read_text(e.to_end().name()) {
                        let t = text.trim();
                        if !t.is_empty() {
                            let val = Some(t.to_string());
                            if is_title { title = val; } else { source = val; }
                        }
                    }
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    (title, source)
}

fn get_container_rootfile(container_xml: &[u8]) -> String {
    let mut reader = Reader::from_reader(container_xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                let name = e.name();
                let local = name.as_ref();
                if local.ends_with(b"rootfile") || local == b"rootfile" {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"full-path" {
                            return String::from_utf8_lossy(&attr.value).into_owned();
                        }
                    }
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    String::new()
}

fn parse_split_csv(text: &str) -> Vec<String> {
    text.split(',').map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect()
}

fn parse_title_page(html: &str, meta: &mut Metadata) {
    let doc = Html::parse_document(html);
    let a_sel = Selector::parse("a").unwrap();
    let author_sel = Selector::parse("a.authorlink").unwrap();

    if let Some(h3) = doc.select(&Selector::parse("h3").unwrap()).next() {
        for link in h3.select(&a_sel) {
            let is_author = link.select(&author_sel).next().is_some()
                || link.value().classes().any(|c| c == "authorlink");
            let name = link.text().collect::<String>().trim().to_string();

            if is_author {
                if !name.is_empty() && !meta.authors.iter().any(|a| a.name == name) {
                    meta.authors.push(Author {
                        url: link.value().attr("href").map(String::from),
                        name,
                    });
                }
            } else {
                if !name.is_empty() { meta.title = Some(name); }
                if let Some(href) = link.value().attr("href") { meta.url = Some(href.to_string()); }
            }
        }
    }

    let mut entries: Vec<(String, String)> = Vec::new();
    let table_sel = Selector::parse("table").unwrap();
    let tr_sel = Selector::parse("tr").unwrap();
    let td_sel = Selector::parse("td").unwrap();
    let b_sel = Selector::parse("b").unwrap();

    if let Some(table) = doc.select(&table_sel).next() {
        for tr in table.select(&tr_sel) {
            let tds: Vec<_> = tr.select(&td_sel).collect();
            match tds.len() {
                2 => {
                    let label = tds[0].text().collect::<String>().trim().trim_end_matches(':').to_string();
                    entries.push((label, tds[1].html()));
                }
                1 => {
                    if let Some(b) = tds[0].select(&b_sel).next() {
                        let label = b.text().collect::<String>().trim().trim_end_matches(':').to_string();
                        entries.push((label, tds[0].html()));
                    }
                }
                _ => {}
            }
        }
    } else {
        for b in doc.select(&b_sel) {
            let label = b.text().collect::<String>().trim().trim_end_matches(':').to_string();
            let mut val_parts = String::new();
            let mut curr = b.next_sibling();

            while let Some(node) = curr {
                if node.value().is_element() {
                    if let Some(el) = node.value().as_element() {
                        let tag = el.name();
                        if tag.eq_ignore_ascii_case("br") || tag.eq_ignore_ascii_case("b") {
                            break;
                        }
                    }
                }
                if let Some(el_ref) = scraper::ElementRef::wrap(node) {
                    val_parts.push_str(&el_ref.text().collect::<String>());
                } else if let Some(text) = node.value().as_text() {
                    val_parts.push_str(&text.to_string());
                }
                curr = node.next_sibling();
            }
            if !val_parts.is_empty() {
                entries.push((label, val_parts));
            }
        }
    }

    for (label, val_html) in &entries {
        let label_l = label.to_lowercase();
        let val_doc = Html::parse_fragment(val_html);
        let clean = val_doc.root_element().text().collect::<String>().trim().to_string();

        if label_l.contains("category") || label_l.contains("fandom") {
            meta.fandoms = parse_split_csv(&clean);
        } else if label_l.contains("relationship") || label_l.contains("ship") {
            meta.relationships = parse_split_csv(&clean);
        } else if label_l.contains("character") {
            meta.characters = parse_split_csv(&clean);
        } else if label_l.contains("genre") {
            for tag in parse_split_csv(&clean) {
                if AO3_CATEGORIES.contains(&tag.as_str()) {
                    if !meta.category.contains(&tag) { meta.category.push(tag); }
                } else if !meta.freeform_tags.contains(&tag) {
                    meta.freeform_tags.push(tag);
                }
            }
        } else if label_l.contains("warning") {
            meta.warnings = parse_split_csv(&clean);
        } else if label_l.contains("rating") {
            meta.rating = Some(clean);
        } else if label_l.contains("status") {
            meta.status = Some(clean);
        } else if label_l.contains("language") {
            meta.language = Some(clean);
        } else if label_l.contains("word") {
            meta.words = to_int(&clean);
        } else if label_l.contains("chapter") {
            if let Some((count, total)) = clean.split_once('/') {
                meta.chapters_count = to_int(count);
                meta.chapters_total = (total.trim() != "?").then(|| to_int(total)).flatten();
            } else {
                meta.chapters_count = to_int(&clean);
            }
        } else if label_l.contains("published") {
            meta.dates.published = Some(clean);
        } else if label_l.contains("updated") {
            meta.dates.updated = Some(clean);
        } else if label_l.contains("packaged") || label_l.contains("created") {
            meta.dates.packaged = Some(clean);
        } else if label_l.contains("summary") || label_l.contains("description") {
            meta.summary = Some(clean);
        } else if label_l.contains("series") {
            for a_link in val_doc.select(&a_sel) {
                let s_name = a_link.text().collect::<String>().trim().to_string();
                let s_url = a_link.value().attr("href").map(String::from);
                let idx = RE_SERIES_IDX.captures(&clean).and_then(|c| {
                    c.get(1).or_else(|| c.get(2)).and_then(|m| m.as_str().parse().ok())
                });
                if !s_name.is_empty() && !meta.series.iter().any(|s| s.name == s_name) {
                    meta.series.push(Series { name: s_name, index: idx, url: s_url });
                }
            }
        } else if label_l.contains("collection") {
            for a_link in val_doc.select(&a_sel) {
                let name = a_link.text().collect::<String>().trim().to_string();
                if !name.is_empty() {
                    meta.collections.push(Collection {
                        name,
                        url: a_link.value().attr("href").map(String::from),
                    });
                }
            }
        } else if let Some(field) = ["kudos", "hits", "bookmarks", "comments"]
            .iter()
            .find(|&&f| label_l == f)
        {
            let v = to_int(&clean);
            match *field {
                "kudos" => meta.stats.kudos = v,
                "hits" => meta.stats.hits = v,
                "bookmarks" => meta.stats.bookmarks = v,
                "comments" => meta.stats.comments = v,
                _ => unreachable!(),
            }
        }
    }
}

fn parse_chapter(html: &[u8]) -> ChapterInfo {
    let mut info = ChapterInfo::default();
    let mut reader = Reader::from_reader(html);
    reader.config_mut().trim_text(false);
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) if e.name().as_ref().eq_ignore_ascii_case(b"meta") => {
                let attrs: std::collections::HashMap<String, String> = e
                    .attributes()
                    .flatten()
                    .map(|a| (String::from_utf8_lossy(a.key.as_ref()).to_lowercase(), String::from_utf8_lossy(&a.value).into_owned()))
                    .collect();

                if let (Some(name), Some(content)) = (attrs.get("name"), attrs.get("content")) {
                    match name.as_str() {
                        "fff_chapter_data" => {
                            if let Ok(v) = serde_json::from_str::<serde_json::Value>(content) {
                                info.title = v.get("title").or_else(|| v.get("chapter")).and_then(|x| x.as_str()).map(String::from).or(info.title);
                                if info.url.is_none() { info.url = v.get("url").and_then(|x| x.as_str()).map(String::from); }
                                if info.date.is_none() { info.date = v.get("date").and_then(|x| x.as_str()).map(String::from); }
                            }
                        }
                        "chapterurl" if info.url.is_none() => info.url = Some(content.clone()),
                        "chaptertitle" if info.title.is_none() => info.title = Some(content.clone()),
                        _ => {}
                    }
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buf.clear();
    }

    if info.title.is_none() {
        let html_str = String::from_utf8_lossy(html);
        if let Some(caps) = RE_CHAPTER_HEADING.captures(&html_str) {
            let raw = caps.get(1).or_else(|| caps.get(2)).map_or("", |m| m.as_str());
            let stripped = RE_STRIP_TAGS.replace_all(raw, "").trim().to_string();
            if !stripped.is_empty() {
                info.title = Some(stripped);
            }
        }
    }

    info
}

fn parse_fff_ao3_epub(epub: &[u8]) -> Result<Metadata, Box<dyn std::error::Error>> {
    let mut archive = ZipArchive::new(Cursor::new(epub))?;
    let namelist: Vec<String> = archive.file_names().map(String::from).collect();

    let mut meta = Metadata::default();

    let container_xml = read_zip_entry(&mut archive, "META-INF/container.xml")?;
    let opf_path = get_container_rootfile(&container_xml);

    let opf_xml = read_zip_entry(&mut archive, &opf_path)?;
    let (title, source) = extract_opf_info(&opf_xml);
    meta.title = title;
    meta.url = source;

    if let Some(tp_path) = namelist.iter().find(|p| RE_TITLE_PAGE.is_match(p)) {
        let tp_html = String::from_utf8_lossy(&read_zip_entry(&mut archive, tp_path)?).into_owned();
        parse_title_page(&tp_html, &mut meta);
    }

    if meta.status.as_deref().is_some_and(|s| s.eq_ignore_ascii_case("Completed")) && meta.chapters_total.is_none() {
        meta.chapters_total = meta.chapters_count;
    }

    if meta.story_id.is_none() {
        if let Some(url) = &meta.url {
            if let Some(m) = RE_WORKS_ID.captures(url) {
                meta.story_id = Some(m[1].to_string());
            }
        }
    }

    let mut chapter_files: Vec<_> = namelist.iter().filter(|p| RE_CHAPTER_FILE.is_match(p)).collect();
    chapter_files.sort();

    for ch_path in chapter_files {
        let ch_data = read_file_sirocco(&mut archive, ch_path)?;
        meta.chapters.push(parse_chapter(&ch_data));
    }

    Ok(meta)
}

fn parse_date(val: &Option<String>) -> String {
    match val {
        Some(s) if s.len() >= 10 && s.as_bytes()[4] == b'-' => s[..10].to_string(),
        _ => "1970-01-01".to_string(),
    }
}

fn sql_string(value: &str) -> String {
    format!("'{}'", value.replace('\\', "\\\\").replace('\'', "\\'"))
}

// Resolve tag names to ao3_tags ids the same way the search tags do:
// canonical, unmerged tags of the right type, matched case-insensitively.
// Only canonical tags are in ao3_tags' primary-key set, so this stays cheap.
fn tag_ids_sql(names: &[String], tag_type: &str) -> String {
    if names.is_empty() {
        return "[]".to_string();
    }
    let lowered: Vec<String> = names.iter().map(|n| sql_string(&n.to_lowercase())).collect();
    format!(
        "(SELECT groupUniqArray(id) FROM ao3_tags WHERE type = {} AND canonical AND merger_id = 0 AND lower(name) IN ({}))",
        sql_string(tag_type),
        lowered.join(", ")
    )
}

fn meta_to_row(meta: &Metadata) -> serde_json::Value {
    let work_id: u64 = meta.story_id.as_deref().and_then(|s| s.parse().ok()).unwrap_or(0);
    let authors: Vec<&str> = meta.authors.iter().map(|a| a.name.as_str()).collect();
    let series_names: Vec<&str> = meta.series.iter().map(|s| s.name.as_str()).collect();
    let series_indexes: Vec<u64> = meta.series.iter().map(|s| s.index.unwrap_or(0)).collect();
    let collections: Vec<&str> = meta.collections.iter().map(|c| c.name.as_str()).collect();
    let chapters_count = meta.chapters_count.unwrap_or(meta.chapters.len() as u64);
    let is_completed = meta
        .status
        .as_deref()
        .is_some_and(|s| s.eq_ignore_ascii_case("Completed") || s.eq_ignore_ascii_case("Complete"));

    serde_json::json!({
        "work_id": work_id,
        "title": meta.title.as_deref().unwrap_or(""),
        "authors": authors,
        "rating": meta.rating.as_deref().unwrap_or("Not Rated"),
        "warnings": meta.warnings,
        "category": meta.category,
        "language": meta.language.as_deref().unwrap_or("English"),
        "is_completed": is_completed,
        "fandom_ids": tag_ids_sql(&meta.fandoms, "Fandom"),
        "relationship_ids": tag_ids_sql(&meta.relationships, "Relationship"),
        "character_ids": tag_ids_sql(&meta.characters, "Character"),
        "freeform_ids": tag_ids_sql(&meta.freeform_tags, "Freeform"),
        "words": meta.words,
        "chapters_count": chapters_count,
        "chapters_total": meta.chapters_total,
        "published_date": parse_date(&meta.dates.published),
        "updated_date": meta.dates.updated.as_deref().and_then(|u| {
            (u.len() >= 10 && u.as_bytes()[4] == b'-').then(|| &u[..10])
        }),
        "kudos": meta.stats.kudos,
        "comments": meta.stats.comments,
        "bookmarks": meta.stats.bookmarks,
        "hits": meta.stats.hits,
        "series_names": series_names,
        "series_indexes": series_indexes,
        "collections": collections,
        "summary": meta.summary,
    })
}

const WORKS_PER_CHUNK: usize = 2048;
const REPORT_EVERY: usize = 10_000;

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: {} <input.zip>", args[0]);
        process::exit(1);
    }

    let file = fs::File::open(&args[1]).unwrap_or_else(|e| {
        eprintln!("Failed to open {}: {e}", args[1]);
        process::exit(1);
    });
    let mut bundle = ZipArchive::new(file).unwrap_or_else(|e| {
        eprintln!("Failed to read zip {}: {e}", args[1]);
        process::exit(1);
    });

    let epub_names: Vec<String> = bundle
        .file_names()
        .filter(|n| n.to_lowercase().ends_with(".epub"))
        .map(String::from)
        .collect();
    let total = epub_names.len();
    eprintln!("Found {} EPUBs in {}", total, args[1]);

    let mut child = process::Command::new("chctl")
        .args(["local", "client", "--query", "INSERT INTO works FORMAT JSONEachRow"])
        .stdin(process::Stdio::piped())
        .stdout(process::Stdio::null())
        .stderr(process::Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| {
            eprintln!("Failed to spawn chctl: {e}");
            process::exit(1);
        });

    let start = std::time::Instant::now();
    let mut stdin = child.stdin.take().unwrap();
    let mut inserted = 0usize;
    let mut last_report = 0usize;

    for chunk in epub_names.chunks(WORKS_PER_CHUNK) {
        let epubs: Vec<Option<Vec<u8>>> = chunk
            .iter()
            .map(|name| {
                bundle.by_name(name).ok().and_then(|mut f| {
                    let mut buf = Vec::new();
                    f.read_to_end(&mut buf).ok().map(|_| buf)
                })
            })
            .collect();

        let metas: Vec<Metadata> = epubs
            .par_iter()
            .zip(chunk)
            .filter_map(|(epub, name)| {
                let epub = epub.as_ref()?;
                parse_fff_ao3_epub(epub)
                    .map_err(|e| eprintln!("Error processing {name}: {e}"))
                    .ok()
            })
            .collect();

        for meta in &metas {
            let row = meta_to_row(meta).to_string();
            stdin.write_all(row.as_bytes()).unwrap();
            stdin.write_all(b"\n").unwrap();
        }
        stdin.flush().unwrap();

        inserted += metas.len();
        if inserted / REPORT_EVERY > last_report / REPORT_EVERY {
            last_report = inserted;
            let elapsed = start.elapsed().as_secs_f64().max(1e-9);
            eprintln!(
                "[insert {}/{}] {:.0} works/sec | elapsed {:.1}m | ETA {:.1}m",
                inserted,
                total,
                inserted as f64 / elapsed,
                elapsed / 60.0,
                (total - inserted) as f64 / (inserted as f64 / elapsed) / 60.0,
            );
        }
    }
    drop(stdin);

    let output = child.wait_with_output().unwrap();
    if !output.status.success() {
        eprintln!("ClickHouse insert failed: {}", String::from_utf8_lossy(&output.stderr));
        process::exit(1);
    }

    eprintln!("Inserted {} rows in {:.1}m", inserted, start.elapsed().as_secs_f64() / 60.0);
}
