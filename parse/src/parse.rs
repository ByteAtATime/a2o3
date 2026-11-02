use rayon::prelude::*;
use std::{
    collections::{HashMap, HashSet},
    env, fs,
    io::{Cursor, Read, Seek},
    process,
};

mod common;

use quick_xml::{Reader, events::Event};
use regex::Regex;
use scraper::{Html, Selector};
use serde::Serialize;
use zip::ZipArchive;

fn read_file_sirocco<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
    name: &str,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let idx = archive
        .index_for_name(name)
        .ok_or_else(|| format!("File not found in archive: {name}"))?;

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
static RE_SERIES_ID: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r"/series/(\d+)").unwrap());
static RE_COLLECTION_ID: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r"/collections/([^/?#]+)").unwrap());
static RE_SERIES_IDX: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r"\[(\d+)\]|Part\s+(\d+)").unwrap());
static RE_CHAPTER_HEADING: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r#"(?is)<(?:div|span)[^>]*class="[^"]*fff_chapter_title[^"]*"[^>]*>(.*?)</(?:div|span)>|<h[23][^>]*>(.*?)</h[23]>"#)
        .unwrap()
});
static RE_STRIP_TAGS: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r"<[^>]*>").unwrap());
static RE_NOTES: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r#"(?is)(?:<div class="fff_chapter_notes fff_(head|foot)_notes">(?P<notes>.*?)</div>)+\s*</body>"#).unwrap()
});
static RE_QUOTE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r"(?is)<blockquote[^>]*>(.*?)</blockquote>").unwrap());

#[derive(Serialize, Default)]
struct Author {
    name: String,
    url: Option<String>,
}

#[derive(Serialize, Default)]
struct Series {
    name: String,
    index: Option<u64>,
    id: Option<u64>,
}

#[derive(Serialize, Default)]
struct Collection {
    name: String,
    id: Option<String>,
}

#[derive(Serialize, Default)]
struct ChapterInfo {
    title: Option<String>,
    url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    head_notes: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    foot_notes: Option<String>,
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

fn id_from_url(re: &Regex, url: Option<&str>) -> Option<u64> {
    re.captures(url?).and_then(|c| c[1].parse().ok())
}

fn slug_from_url(re: &Regex, url: Option<&str>) -> Option<String> {
    re.captures(url?).map(|c| c[1].to_string())
}

fn series_id_from_url(url: Option<&str>) -> Option<u64> {
    id_from_url(&RE_SERIES_ID, url)
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
                            if is_title {
                                title = val;
                            } else {
                                source = val;
                            }
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
    text.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

fn is_author_content(el: &scraper::ElementRef<'_>) -> bool {
    el.ancestors().any(|node| {
        scraper::ElementRef::wrap(node).is_some_and(|e| {
            e.value()
                .classes()
                .any(|class| class == "userstuff" || class == "notes")
        })
    })
}

fn collect_label_value(label: scraper::ElementRef<'_>) -> String {
    let mut parts = String::new();
    let mut curr = label.next_sibling();

    while let Some(node) = curr {
        if let Some(el) = node.value().as_element() {
            let tag = el.name();
            if tag.eq_ignore_ascii_case("br") || tag.eq_ignore_ascii_case("b") {
                break;
            }
        }
        if let Some(el_ref) = scraper::ElementRef::wrap(node) {
            parts.push_str(&el_ref.text().collect::<String>());
        } else if let Some(text) = node.value().as_text() {
            parts.push_str(text);
        }
        curr = node.next_sibling();
    }
    parts
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
            let href = link.value().attr("href");

            if is_author {
                if !name.is_empty() && !meta.authors.iter().any(|a| a.name == name) {
                    meta.authors.push(Author {
                        name,
                        url: href.map(String::from),
                    });
                }
            } else {
                if !name.is_empty() {
                    meta.title = Some(name);
                }
                if let Some(href) = href {
                    meta.url = Some(href.to_string());
                }
            }
        }
    }

    let mut entries: Vec<(String, String)> = Vec::new();
    let table_sel = Selector::parse("table").unwrap();
    let tr_sel = Selector::parse("tr").unwrap();
    let td_sel = Selector::parse("td").unwrap();
    let b_sel = Selector::parse("b").unwrap();

    if let Some(table) = doc.select(&table_sel).find(|t| !is_author_content(t)) {
        for tr in table.select(&tr_sel) {
            let tds: Vec<_> = tr.select(&td_sel).collect();
            match tds.len() {
                2 => {
                    let label = tds[0]
                        .text()
                        .collect::<String>()
                        .trim()
                        .trim_end_matches(':')
                        .to_string();
                    entries.push((label, tds[1].html()));
                }
                1 => {
                    if let Some(b) = tds[0].select(&b_sel).next() {
                        let label = b
                            .text()
                            .collect::<String>()
                            .trim()
                            .trim_end_matches(':')
                            .to_string();
                        let value = collect_label_value(b);
                        if !value.is_empty() {
                            entries.push((label, value));
                        }
                    }
                }
                _ => {}
            }
        }
    } else {
        for b in doc.select(&b_sel) {
            if is_author_content(&b) {
                continue;
            }
            let label = b
                .text()
                .collect::<String>()
                .trim()
                .trim_end_matches(':')
                .to_string();
            let val_parts = collect_label_value(b);
            if !val_parts.is_empty() {
                entries.push((label, val_parts));
            }
        }
    }

    for (label, val_html) in &entries {
        let label_l = label.to_lowercase();
        let val_doc = Html::parse_fragment(val_html);
        let clean = val_doc
            .root_element()
            .text()
            .collect::<String>()
            .trim()
            .to_string();
        let plain = !val_html.contains('<');

        if label_l.contains("category") || label_l.contains("fandom") {
            if meta.fandoms.is_empty() {
                meta.fandoms = parse_split_csv(&clean);
            }
        } else if label_l.contains("relationship") || label_l.contains("ship") {
            if meta.relationships.is_empty() {
                meta.relationships = parse_split_csv(&clean);
            }
        } else if label_l.contains("character") {
            if meta.characters.is_empty() {
                meta.characters = parse_split_csv(&clean);
            }
        } else if label_l.contains("genre") {
            for tag in parse_split_csv(&clean) {
                if AO3_CATEGORIES.contains(&tag.as_str()) {
                    if !meta.category.contains(&tag) {
                        meta.category.push(tag);
                    }
                } else if !meta.freeform_tags.contains(&tag) {
                    meta.freeform_tags.push(tag);
                }
            }
        } else if label_l.contains("warning") {
            if meta.warnings.is_empty() {
                meta.warnings = parse_split_csv(&clean);
            }
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
            if label_l.contains("url") {
                if let Some(id) = series_id_from_url(Some(clean.as_str())) {
                    for s in meta.series.iter_mut().filter(|s| s.id.is_none()) {
                        s.id = Some(id);
                    }
                }
                continue;
            }
            let idx = RE_SERIES_IDX.captures(&clean).and_then(|c| {
                c.get(1)
                    .or_else(|| c.get(2))
                    .and_then(|m| m.as_str().parse().ok())
            });
            let text_id = series_id_from_url(Some(clean.as_str()));
            let mut push = |s_name: &str, s_id: Option<u64>| {
                if !s_name.is_empty() && !meta.series.iter().any(|s| s.name == s_name) {
                    meta.series.push(Series {
                        name: s_name.to_string(),
                        index: idx,
                        id: s_id,
                    });
                }
            };
            for a_link in val_doc.select(&a_sel) {
                let s_name = a_link.text().collect::<String>().trim().to_string();
                push(&s_name, series_id_from_url(a_link.value().attr("href")));
            }
            if plain {
                for s_name in clean
                    .split(',')
                    .map(|s| RE_SERIES_IDX.replace(s, "").trim().to_string())
                {
                    push(&s_name, text_id);
                }
            }
        } else if label_l.contains("collection") {
            let mut push = |c_name: &str, c_id: Option<String>| {
                if !c_name.is_empty() && !meta.collections.iter().any(|c| c.name == c_name) {
                    meta.collections.push(Collection {
                        name: c_name.to_string(),
                        id: c_id,
                    });
                }
            };
            for a_link in val_doc.select(&a_sel) {
                let name = a_link.text().collect::<String>().trim().to_string();
                push(
                    &name,
                    slug_from_url(&RE_COLLECTION_ID, a_link.value().attr("href")),
                );
            }
            if plain {
                for name in parse_split_csv(&clean) {
                    push(&name, None);
                }
            }
        } else if let Some(field) = ["kudos", "comments"].iter().find(|&&f| label_l == f) {
            let v = to_int(&clean);
            match *field {
                "kudos" => meta.stats.kudos = v,
                "comments" => meta.stats.comments = v,
                _ => unreachable!(),
            }
        }
    }
}

fn parse_notes(html: &str) -> (Option<String>, Option<String>) {
    let Some(caps) = RE_NOTES.captures(html) else {
        return (None, None);
    };
    let text = RE_QUOTE
        .find_iter(&caps["notes"])
        .map(|m| {
            let inner = RE_QUOTE.replace(m.as_str(), "$1");
            RE_STRIP_TAGS
                .replace_all(&inner, " ")
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        })
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    let foot = caps.get(1).is_some_and(|m| m.as_str() == "foot");
    (foot.then(|| text.clone()), Some(text))
}

fn parse_chapter(html: &[u8]) -> ChapterInfo {
    let mut info = ChapterInfo::default();
    let mut reader = Reader::from_reader(html);
    reader.config_mut().trim_text(false);
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) | Ok(Event::Empty(e))
                if e.name().as_ref().eq_ignore_ascii_case(b"meta") =>
            {
                let attrs: std::collections::HashMap<String, String> = e
                    .attributes()
                    .flatten()
                    .map(|a| {
                        (
                            String::from_utf8_lossy(a.key.as_ref()).to_lowercase(),
                            String::from_utf8_lossy(&a.value).into_owned(),
                        )
                    })
                    .collect();

                if let (Some(name), Some(content)) = (attrs.get("name"), attrs.get("content")) {
                    match name.as_str() {
                        "fff_chapter_data" => {
                            if let Ok(v) = serde_json::from_str::<serde_json::Value>(content) {
                                info.title = v
                                    .get("title")
                                    .or_else(|| v.get("chapter"))
                                    .and_then(|x| x.as_str())
                                    .map(String::from)
                                    .or(info.title);
                                if info.url.is_none() {
                                    info.url =
                                        v.get("url").and_then(|x| x.as_str()).map(String::from);
                                }
                                if info.date.is_none() {
                                    info.date =
                                        v.get("date").and_then(|x| x.as_str()).map(String::from);
                                }
                            }
                        }
                        "chapterurl" if info.url.is_none() => info.url = Some(content.clone()),
                        "chaptertitle" if info.title.is_none() => {
                            info.title = Some(content.clone())
                        }
                        _ => {}
                    }
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buf.clear();
    }

    let html_str = String::from_utf8_lossy(html);
    (info.head_notes, info.foot_notes) = parse_notes(&html_str);

    if info.title.is_none() {
        if let Some(caps) = RE_CHAPTER_HEADING.captures(&html_str) {
            let raw = caps
                .get(1)
                .or_else(|| caps.get(2))
                .map_or("", |m| m.as_str());
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

    if meta
        .status
        .as_deref()
        .is_some_and(|s| s.eq_ignore_ascii_case("Completed"))
        && meta.chapters_total.is_none()
    {
        meta.chapters_total = meta.chapters_count;
    }

    if meta.story_id.is_none() {
        if let Some(url) = &meta.url {
            if let Some(m) = RE_WORKS_ID.captures(url) {
                meta.story_id = Some(m[1].to_string());
            }
        }
    }

    let mut chapter_files: Vec<_> = namelist
        .iter()
        .filter(|p| RE_CHAPTER_FILE.is_match(p))
        .collect();
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

fn meta_to_row(meta: &Metadata, tags: &TagResolver) -> serde_json::Value {
    let work_id: u64 = meta
        .story_id
        .as_deref()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let authors: Vec<&str> = meta.authors.iter().map(|a| a.name.as_str()).collect();
    let series_names: Vec<&str> = meta.series.iter().map(|s| s.name.as_str()).collect();
    let series_indexes: Vec<u64> = meta.series.iter().map(|s| s.index.unwrap_or(0)).collect();
    let author_urls: Vec<&str> = meta
        .authors
        .iter()
        .filter_map(|a| a.url.as_deref())
        .collect();
    let series_ids: Vec<u64> = meta.series.iter().map(|s| s.id.unwrap_or(0)).collect();
    let collections: Vec<&str> = meta.collections.iter().map(|c| c.name.as_str()).collect();
    let collection_ids: Vec<&str> = meta
        .collections
        .iter()
        .filter_map(|c| c.id.as_deref())
        .collect();
    let chapter_names: Vec<&str> = meta
        .chapters
        .iter()
        .filter_map(|c| c.title.as_deref())
        .collect();
    let chapter_head_notes: Vec<&str> = meta
        .chapters
        .iter()
        .filter_map(|c| c.head_notes.as_deref())
        .collect();
    let chapter_foot_notes: Vec<&str> = meta
        .chapters
        .iter()
        .filter_map(|c| c.foot_notes.as_deref())
        .collect();
    let chapters_count = meta.chapters_count.unwrap_or(meta.chapters.len() as u64);
    let is_completed = meta
        .status
        .as_deref()
        .is_some_and(|s| s.eq_ignore_ascii_case("Completed") || s.eq_ignore_ascii_case("Complete"));

    serde_json::json!({
        "work_id": work_id,
        "title": meta.title.as_deref().unwrap_or(""),
        "authors": authors,
        "author_urls": author_urls,
        "rating": meta.rating.as_deref().unwrap_or("Not Rated"),
        "warnings": meta.warnings,
        "category": meta.category,
        "language": meta.language.as_deref().unwrap_or("English"),
        "is_completed": is_completed,
        "fandom_ids": tag_ids(&meta.fandoms, TagType::Fandom, tags),
        "relationship_ids": tag_ids(&meta.relationships, TagType::Relationship, tags),
        "character_ids": tag_ids(&meta.characters, TagType::Character, tags),
        "freeform_ids": tag_ids(&meta.freeform_tags, TagType::Freeform, tags),
        "words": meta.words,
        "chapters_count": chapters_count,
        "chapters_total": meta.chapters_total,
        "published_date": parse_date(&meta.dates.published),
        "updated_date": meta.dates.updated.as_deref().and_then(|u| {
            (u.len() >= 10 && u.as_bytes()[4] == b'-').then(|| &u[..10])
        }),
        "kudos": meta.stats.kudos,
        "comments": meta.stats.comments,
        "series_names": series_names,
        "series_indexes": series_indexes,
        "series_ids": series_ids,
        "collections": collections,
        "collection_ids": collection_ids,
        "summary": meta.summary,
        "chapter_names": chapter_names,
        "chapter_head_notes": chapter_head_notes,
        "chapter_foot_notes": chapter_foot_notes,
    })
}

const WORKS_PER_CHUNK: usize = 2048;
const WORKS_PER_BATCH: usize = 50_000;
const MIN_WORKS_PER_INSERT: usize = 1024;
const MAX_CACHED_TAGS: usize = 500_000;
const REPORT_EVERY: usize = 10_000;

const CREATE_TAG_BATCH_SQL: &str = "CREATE TEMPORARY TABLE IF NOT EXISTS tag_batch (name String, lname String, type String) ENGINE = Memory";
const TRUNCATE_TAG_BATCH_SQL: &str = "TRUNCATE TABLE tag_batch";

const INSERT_MISSING_TAGS_SQL: &str = r#"
INSERT INTO ao3_tags
WITH (SELECT max(id) FROM ao3_tags) AS base
SELECT
    base + row_number() OVER () AS id,
    type,
    name,
    true AS canonical,
    0 AS merger_id
FROM (
    SELECT names.type AS type, min(names.name) AS name
    FROM tag_batch AS names
    LEFT JOIN (
        SELECT lower(name) AS lname, type
        FROM ao3_tags
        WHERE lower(name) IN (SELECT lname FROM tag_batch)
        GROUP BY lname, type
    ) AS known ON known.lname = names.lname AND known.type = names.type
    WHERE known.lname = ''
    GROUP BY names.type, names.lname
)
"#;

const RESOLVE_TAGS_SQL: &str = r#"
SELECT
    lower(tag.name) AS name,
    tag.type AS type,
    arraySlice(arraySort(groupUniqArray(
        if(tag.canonical OR tag.merger_id = 0, tag.id, tag.merger_id)
    )), 1, 16) AS ids
FROM ao3_tags AS tag
WHERE lower(tag.name) IN (SELECT lname FROM tag_batch)
GROUP BY name, type
FORMAT JSONEachRow
"#;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum TagType {
    Fandom,
    Relationship,
    Character,
    Freeform,
}

impl TagType {
    fn as_str(self) -> &'static str {
        match self {
            Self::Fandom => "Fandom",
            Self::Relationship => "Relationship",
            Self::Character => "Character",
            Self::Freeform => "Freeform",
        }
    }

    fn from_str(value: &str) -> Option<Self> {
        match value {
            "Fandom" => Some(Self::Fandom),
            "Relationship" => Some(Self::Relationship),
            "Character" => Some(Self::Character),
            "Freeform" => Some(Self::Freeform),
            _ => None,
        }
    }
}

#[derive(serde::Deserialize)]
struct ResolvedTag {
    name: String,
    #[serde(rename = "type")]
    tag_type: String,
    ids: Vec<u32>,
}

#[derive(Default)]
struct TagResolver {
    ids: HashMap<(String, TagType), Vec<u32>>,
}

impl TagResolver {
    fn contains(&self, name: &str, tag_type: TagType) -> bool {
        self.ids
            .contains_key(&(name.to_ascii_lowercase(), tag_type))
    }

    fn get(&self, name: &str, tag_type: TagType) -> &[u32] {
        self.ids
            .get(&(name.to_ascii_lowercase(), tag_type))
            .map_or(&[], Vec::as_slice)
    }

    fn len(&self) -> usize {
        self.ids.len()
    }

    fn trim(&mut self) {
        if self.ids.len() > MAX_CACHED_TAGS {
            self.ids.clear();
        }
    }

    fn resolve(&mut self, client: &ClickHouse, tags: &[(String, TagType)]) -> Result<(), String> {
        client.execute_session(CREATE_TAG_BATCH_SQL)?;
        client.execute_session(TRUNCATE_TAG_BATCH_SQL)?;

        let mut body = Vec::new();
        for (name, tag_type) in tags {
            let row = serde_json::json!({
                "name": name,
                "lname": name.to_ascii_lowercase(),
                "type": tag_type.as_str(),
            });
            body.extend_from_slice(row.to_string().as_bytes());
            body.push(b'\n');
        }
        client.insert_session("INSERT INTO tag_batch FORMAT JSONEachRow", &body)?;
        client.execute_session(INSERT_MISSING_TAGS_SQL)?;

        let resolved = client.query_session(RESOLVE_TAGS_SQL)?;
        for line in resolved.lines().filter(|line| !line.is_empty()) {
            let tag: ResolvedTag = serde_json::from_str(line)
                .map_err(|e| format!("Unexpected tag id row {line:?}: {e}"))?;
            if let Some(tag_type) = TagType::from_str(&tag.tag_type) {
                self.ids
                    .insert((tag.name.to_ascii_lowercase(), tag_type), tag.ids);
            }
        }
        for (name, tag_type) in tags {
            self.ids
                .entry((name.to_ascii_lowercase(), *tag_type))
                .or_default();
        }
        Ok(())
    }
}

fn work_tags(meta: &Metadata) -> [(TagType, &Vec<String>); 4] {
    [
        (TagType::Fandom, &meta.fandoms),
        (TagType::Relationship, &meta.relationships),
        (TagType::Character, &meta.characters),
        (TagType::Freeform, &meta.freeform_tags),
    ]
}

fn tag_ids(names: &[String], tag_type: TagType, resolver: &TagResolver) -> Vec<u32> {
    let mut ids: Vec<u32> = names
        .iter()
        .flat_map(|name| resolver.get(name, tag_type))
        .copied()
        .collect();
    ids.sort_unstable();
    ids.dedup();
    ids
}

fn write_batch(
    client: &ClickHouse,
    resolver: &mut TagResolver,
    batch: &[(u32, Metadata)],
) -> Result<(), String> {
    let started = std::time::Instant::now();
    let mut tags: Vec<(String, TagType)> = Vec::new();
    let mut queued: HashSet<(String, TagType)> = HashSet::new();
    for (_, meta) in batch {
        for (tag_type, names) in work_tags(meta) {
            for name in names {
                if !resolver.contains(name, tag_type)
                    && queued.insert((name.to_ascii_lowercase(), tag_type))
                {
                    tags.push((name.clone(), tag_type));
                }
            }
        }
    }
    if !tags.is_empty() {
        resolver.resolve(client, &tags)?;
    }
    let tags_done = started.elapsed().as_secs_f64();

    insert_works(client, batch, resolver)?;
    resolver.trim();
    eprintln!(
        "[batch] {} works, {} new tags: tags {:.1}s, insert {:.1}s, {} tags cached",
        batch.len(),
        tags.len(),
        tags_done,
        started.elapsed().as_secs_f64() - tags_done,
        resolver.len(),
    );
    Ok(())
}

fn insert_batch(
    client: &ClickHouse,
    resolver: &mut TagResolver,
    batch: &mut Vec<(u32, Metadata)>,
    seen: &mut common::Seen,
) {
    if batch.is_empty() {
        return;
    }
    if let Err(e) = write_batch(client, resolver, batch) {
        eprintln!("Writing {} works failed: {e}", batch.len());
        process::exit(1);
    }
    seen.insert_all(batch.iter().map(|(id, _)| *id));
    batch.clear();
}

fn insert_works(
    client: &ClickHouse,
    batch: &[(u32, Metadata)],
    resolver: &TagResolver,
) -> Result<(), String> {
    let shards = std::thread::available_parallelism().map_or(4, |n| n.get()).clamp(1, 8);
    let per_shard = batch.len().div_ceil(shards).max(MIN_WORKS_PER_INSERT);
    batch
        .chunks(per_shard)
        .collect::<Vec<_>>()
        .par_iter()
        .map(|shard| {
            let mut body = Vec::new();
            for (_, meta) in shard.iter() {
                body.extend_from_slice(meta_to_row(meta, resolver).to_string().as_bytes());
                body.push(b'\n');
            }
            client.insert("INSERT INTO works FORMAT JSONEachRow", &body)
        })
        .collect::<Result<Vec<()>, String>>()
        .map(|_| ())
}

struct ClickHouse {
    url: String,
    user: String,
    password: String,
    database: Option<String>,
    session: String,
}

impl ClickHouse {
    fn from_env() -> Self {
        Self {
            url: env::var("CH_URL").unwrap_or_else(|_| "http://localhost:8123".into()),
            user: env::var("CH_USER").unwrap_or_else(|_| "default".into()),
            password: env::var("CH_PASSWORD").unwrap_or_default(),
            database: env::var("CH_DATABASE").ok(),
            session: format!(
                "parse-{}-{}",
                process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
            ),
        }
    }

    fn redacted_url(&self) -> String {
        match url::Url::parse(&self.url) {
            Ok(mut endpoint) => {
                if !endpoint.username().is_empty() || endpoint.password().is_some() {
                    let _ = endpoint.set_username("");
                    let _ = endpoint.set_password(None);
                }
                endpoint.to_string()
            }
            Err(_) => self.url.clone(),
        }
    }

    fn request(
        &self,
        query: &str,
        session: bool,
    ) -> Result<ureq::RequestBuilder<ureq::typestate::WithBody>, String> {
        let mut endpoint = url::Url::parse(&self.url)
            .map_err(|e| format!("Invalid CH_URL {:?}: {e}", self.url))?;

        let url_user = if endpoint.username().is_empty() {
            None
        } else {
            Some(endpoint.username().to_string())
        };
        let url_password = endpoint.password().map(str::to_string);
        let _ = endpoint.set_username("");
        let _ = endpoint.set_password(None);

        endpoint.query_pairs_mut().append_pair("query", query);
        if session {
            endpoint
                .query_pairs_mut()
                .append_pair("session_id", &self.session);
        }
        if let Some(database) = &self.database {
            endpoint.query_pairs_mut().append_pair("database", database);
        }

        let mut request = ureq::post(endpoint.as_str())
            .config()
            .http_status_as_error(false)
            .build();

        let (user, password) = match url_user {
            Some(user) => (Some(user), url_password.unwrap_or_default()),
            None => (Some(self.user.clone()), self.password.clone()),
        };
        if let Some(user) = user.filter(|u| !u.is_empty()) {
            request = request
                .header("X-ClickHouse-User", &user)
                .header("X-ClickHouse-Key", &password);
        }
        Ok(request)
    }

    fn check(
        &self,
        response: Result<ureq::http::Response<ureq::Body>, ureq::Error>,
    ) -> Result<(), String> {
        let response = response.map_err(|e| format!("Could not reach {}: {e}", self.url))?;
        if response.status().is_success() {
            return Ok(());
        }
        let status = response.status();
        let mut response = response;
        let detail = response.body_mut().read_to_string().unwrap_or_default();
        let detail = detail.trim();
        if detail.is_empty() {
            Err(format!("ClickHouse returned HTTP {status}"))
        } else {
            Err(format!("ClickHouse returned HTTP {status}: {detail}"))
        }
    }

    fn execute_session(&self, sql: &str) -> Result<(), String> {
        self.check(self.request(sql, true)?.send(&[]))
    }

    fn query(&self, sql: &str) -> Result<String, String> {
        self.query_mode(sql, false)
    }

    fn query_session(&self, sql: &str) -> Result<String, String> {
        self.query_mode(sql, true)
    }

    fn query_mode(&self, sql: &str, session: bool) -> Result<String, String> {
        let response = self
            .request(sql, session)?
            .send(&[])
            .map_err(|e| format!("Could not reach {}: {e}", self.url))?;
        if !response.status().is_success() {
            return self.check(Ok(response)).map(|()| String::new());
        }
        response
            .into_body()
            .read_to_string()
            .map_err(|e| format!("Could not read the response: {e}"))
    }

    fn insert(&self, sql: &str, body: &[u8]) -> Result<(), String> {
        self.check(self.request(sql, false)?.send(body))
    }

    fn insert_session(&self, sql: &str, body: &[u8]) -> Result<(), String> {
        self.check(self.request(sql, true)?.send(body))
    }
}

fn load_work_ids(client: &ClickHouse) -> Vec<u32> {
    let text = client
        .query("SELECT work_id FROM works")
        .unwrap_or_else(|e| {
            eprintln!("Reading work ids failed: {e}");
            process::exit(1);
        });
    text.lines()
        .filter_map(|line| line.trim().parse().ok())
        .collect()
}

fn meta_work_id(meta: &Metadata) -> Option<u32> {
    meta.story_id
        .as_deref()
        .and_then(|s| s.parse::<u32>().ok())
        .filter(|&id| id != 0)
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: {} <zip>... | --from-list <file>", args[0]);
        process::exit(1);
    }

    let zips = common::collect_zips(&args[1..]);
    if zips.is_empty() {
        eprintln!("No input zips.");
        process::exit(1);
    }

    let mut total_epubs = 0usize;
    for zip_path in &zips {
        total_epubs += fs::File::open(zip_path)
            .ok()
            .and_then(|file| ZipArchive::new(file).ok())
            .map(|archive| {
                archive
                    .file_names()
                    .filter(|n| n.to_lowercase().ends_with(".epub"))
                    .count()
            })
            .unwrap_or(0);
    }

    let client = ClickHouse::from_env();
    eprintln!("ClickHouse: {}", client.redacted_url());

    let mut resolver = TagResolver::default();
    let mut seen = common::Seen::from_ids(load_work_ids(&client));
    eprintln!(
        "[parse] {} zips, {} works already in the database",
        zips.len(),
        seen.len()
    );

    let start = std::time::Instant::now();
    let mut total_inserted = 0usize;
    let mut total_skipped = 0usize;
    let mut total_rejected = 0usize;
    let mut processed = 0usize;
    let mut last_reported = 0usize;

    for zip_path in &zips {
        let zip_start = std::time::Instant::now();
        let file = match fs::File::open(zip_path) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("Failed to open {}: {e}", zip_path.display());
                process::exit(1);
            }
        };
        let mut bundle = match ZipArchive::new(file) {
            Ok(archive) => archive,
            Err(e) => {
                eprintln!("Failed to read zip {}: {e}", zip_path.display());
                process::exit(1);
            }
        };
        let epub_names: Vec<String> = bundle
            .file_names()
            .filter(|n| n.to_lowercase().ends_with(".epub"))
            .map(String::from)
            .collect();

        let mut inserted = 0usize;
        let mut skipped = 0usize;
        let mut rejected = 0usize;
        let mut new_ids: HashSet<u32> = HashSet::new();
        let mut batch: Vec<(u32, Metadata)> = Vec::new();

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

            processed += chunk.len();
            if processed / REPORT_EVERY > last_reported / REPORT_EVERY {
                last_reported = processed;
                let elapsed = start.elapsed().as_secs_f64().max(1e-9);
                let rate = processed as f64 / elapsed;
                let eta = total_epubs.saturating_sub(processed) as f64 / rate / 60.0;
                eprintln!(
                    "[insert {}/{}] {:.0} works/sec | elapsed {:.1}m | ETA {:.1}m",
                    processed,
                    total_epubs,
                    rate,
                    elapsed / 60.0,
                    eta,
                );
            }

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
            rejected += chunk.len() - metas.len();

            for meta in metas {
                let Some(id) = meta_work_id(&meta) else {
                    rejected += 1;
                    continue;
                };
                if seen.contains(id) || !new_ids.insert(id) {
                    skipped += 1;
                    continue;
                }
                batch.push((id, meta));
                inserted += 1;
            }

            if batch.len() >= WORKS_PER_BATCH {
                insert_batch(&client, &mut resolver, &mut batch, &mut seen);
            }
        }

        insert_batch(&client, &mut resolver, &mut batch, &mut seen);

        total_inserted += inserted;
        total_skipped += skipped;
        total_rejected += rejected;

        eprintln!(
            "[parse] {}: {} inserted, {} dup, {} rejected, {:.1}s",
            zip_path.display(),
            inserted,
            skipped,
            rejected,
            zip_start.elapsed().as_secs_f64(),
        );
    }

    eprintln!(
        "[parse] done: {} inserted, {} dup, {} rejected in {:.1}m",
        total_inserted,
        total_skipped,
        total_rejected,
        start.elapsed().as_secs_f64() / 60.0,
    );
}

#[cfg(test)]
mod title_page_tests {
    use super::*;

    fn parse(html: &str) -> Metadata {
        let mut meta = Metadata::default();
        parse_title_page(html, &mut meta);
        meta
    }

    fn page(body: &str) -> String {
        format!(
            r#"<html><body class="fff_titlepage">
<h3><a href="https://archiveofourown.org/works/1">T</a> by <a class="authorlink" href="/u/a">A</a></h3>
<div>{body}</div></body></html>"#
        )
    }

    const META: &str = r#"
<b>Category:</b> Fandom X<br />
<b>Characters:</b> Alice, Bob<br />
<b>Relationships:</b> Alice/Bob<br />
<b>Genre:</b> Fluff<br />
<b>Warnings:</b> No Archive Warnings Apply<br />"#;

    #[test]
    fn parses_plain_title_page() {
        let meta = parse(&page(META));
        assert_eq!(meta.characters, ["Alice", "Bob"]);
        assert_eq!(meta.relationships, ["Alice/Bob"]);
        assert_eq!(meta.fandoms, ["Fandom X"]);
    }

    #[test]
    fn ignores_table_inside_summary() {
        let meta = parse(&page(&format!(
            r#"{META}
<b>Summary:</b> <div class="userstuff"><p>Text.</p>
<table><tbody><tr><td>unrelated</td></tr></tbody></table></div><br />"#
        )));
        assert_eq!(meta.characters, ["Alice", "Bob"]);
        assert_eq!(meta.fandoms, ["Fandom X"]);
    }

    #[test]
    fn ignores_character_label_inside_summary() {
        let meta = parse(&page(&format!(
            r#"{META}
<b>Summary:</b> <div class="userstuff"><p><b>Characters:</b> Mallory, Eve. <b>Pairings:</b> Mallory/Eve</p></div><br />"#
        )));
        assert_eq!(meta.characters, ["Alice", "Bob"]);
        assert_eq!(meta.relationships, ["Alice/Bob"]);
    }

    #[test]
    fn does_not_match_character_deaths_as_characters() {
        let meta = parse(&page(&format!(
            r#"{META}
<b>Summary:</b> <div class="userstuff"><p>Dark. <b>Warnings: Child Abuse and Character Deaths.</b> Prompt: Alice, Bob</p></div><br />"#
        )));
        assert_eq!(meta.characters, ["Alice", "Bob"]);
        assert_eq!(meta.warnings, ["No Archive Warnings Apply"]);
    }

    #[test]
    fn strips_label_from_single_cell_value() {
        let meta = parse(
            r#"<html><body><table><tr><td><b>Characters:</b> Alice, Bob<br /></td></tr></table></body></html>"#,
        );
        assert_eq!(meta.characters, ["Alice", "Bob"]);
    }
}
