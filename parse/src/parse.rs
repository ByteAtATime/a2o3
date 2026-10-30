use rayon::prelude::*;
use std::{env, fs, io::Read as _, path::Path, process};

use quick_xml::{events::Event, Reader};
use regex::Regex;
use scraper::{Html, Selector};
use serde::Serialize;
use zip::ZipArchive;

fn read_file_sirocco(archive: &mut ZipArchive<fs::File>, name: &str) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
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

fn read_zip_entry(archive: &mut ZipArchive<fs::File>, name: &str) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
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
                if !meta.series.iter().any(|s| s.name == s_name) {
                    meta.series.push(Series { name: s_name, index: idx, url: s_url });
                }
            }
        } else if label_l.contains("collection") {
            for a_link in val_doc.select(&a_sel) {
                meta.collections.push(Collection {
                    name: a_link.text().collect::<String>().trim().to_string(),
                    url: a_link.value().attr("href").map(String::from),
                });
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

fn parse_fff_ao3_epub(epub_path: &str) -> Result<Metadata, Box<dyn std::error::Error>> {
    let path = Path::new(epub_path);
    if !path.is_file() {
        return Err(format!("File not found: {epub_path}").into());
    }

    let file = fs::File::open(path)?;
    let mut archive = ZipArchive::new(file)?;
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

    if meta.status.as_deref() == Some("Completed") && meta.chapters_total.is_none() {
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

fn collect_epubs(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut results = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                results.extend(collect_epubs(&path));
            } else if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("epub")) {
                results.push(path);
            }
        }
    }
    results
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: {} <directory>", args[0]);
        process::exit(1);
    }

    let target = Path::new(&args[1]);
    if !target.is_dir() {
        eprintln!("Error: {} is not a directory", args[1]);
        process::exit(1);
    }

    let epubs = collect_epubs(target);
    let results: Vec<_> = epubs
        .par_iter()
        .filter_map(|epub| {
            parse_fff_ao3_epub(epub.to_str().unwrap_or(""))
                .map_err(|e| eprintln!("Error processing {}: {e}", epub.display()))
                .ok()
        })
        .collect();

    println!("{}", serde_json::to_string_pretty(&results).expect("JSON serialization failed"));
}
