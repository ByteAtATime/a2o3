use serde::Deserialize;
use std::{fmt::Write as _, io::Write, process};

#[derive(Deserialize)]
struct Author {
    name: String,
}

#[derive(Deserialize)]
struct SeriesEntry {
    name: Option<String>,
    index: Option<u16>,
}

#[derive(Deserialize)]
struct Dates {
    published: Option<String>,
    updated: Option<String>,
}

#[derive(Deserialize)]
struct Stats {
    kudos: Option<u32>,
    comments: Option<u32>,
    bookmarks: Option<u32>,
    hits: Option<u32>,
}

#[derive(Deserialize)]
struct ChapterInfo {
    #[allow(dead_code)]
    title: Option<String>,
    #[allow(dead_code)]
    url: Option<String>,
}

#[derive(Deserialize)]
struct RawRecord {
    story_id: String,
    title: Option<String>,
    authors: Vec<Author>,
    rating: Option<String>,
    warnings: Option<Vec<String>>,
    category: Option<Vec<String>>,
    language: Option<String>,
    status: Option<String>,
    fandoms: Option<Vec<String>>,
    relationships: Option<Vec<String>>,
    characters: Option<Vec<String>>,
    freeform_tags: Option<Vec<String>>,
    words: Option<u32>,
    chapters_count: Option<u16>,
    chapters_total: Option<u16>,
    dates: Option<Dates>,
    stats: Option<Stats>,
    series: Option<Vec<SeriesEntry>>,
    collections: Option<Vec<String>>,
    summary: Option<String>,
    chapters: Option<Vec<ChapterInfo>>,
}

fn parse_date(val: &Option<String>) -> String {
    match val {
        Some(s) if s.len() >= 10 && s.as_bytes()[4] == b'-' => s[..10].to_string(),
        _ => "1970-01-01".to_string(),
    }
}

fn escape_json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => write!(out, "\\u{:04x}", c as u32).unwrap(),
            c => out.push(c),
        }
    }
    out
}

fn write_str_array(buf: &mut String, items: &[String]) {
    buf.push('[');
    for (i, s) in items.iter().enumerate() {
        if i > 0 { buf.push(','); }
        buf.push('"');
        buf.push_str(&escape_json_string(s));
        buf.push('"');
    }
    buf.push(']');
}

fn write_str_slice(buf: &mut String, items: &[&str]) {
    buf.push('[');
    for (i, s) in items.iter().enumerate() {
        if i > 0 { buf.push(','); }
        buf.push('"');
        buf.push_str(&escape_json_string(s));
        buf.push('"');
    }
    buf.push(']');
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = args.get(1).map(|s| s.as_str()).unwrap_or("metadata.json");

    let data = std::fs::read_to_string(path).unwrap_or_else(|e| {
        eprintln!("Failed to read {path}: {e}");
        process::exit(1);
    });

    let records: Vec<RawRecord> = serde_json::from_str(&data).unwrap_or_else(|e| {
        eprintln!("Failed to parse JSON: {e}");
        process::exit(1);
    });

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

    let stdin = child.stdin.as_mut().unwrap();
    let mut buf = String::with_capacity(2048);

    for rec in &records {
        buf.clear();

        let author_names: Vec<&str> = rec.authors.iter().map(|a| a.name.as_str()).collect();
        let series = rec.series.as_deref().unwrap_or(&[]);
        let series_names: Vec<&str> = series.iter().filter_map(|s| s.name.as_deref()).collect();
        let dates = rec.dates.as_ref();
        let stats = rec.stats.as_ref();
        let chapters_count = rec.chapters_count.unwrap_or_else(|| {
            rec.chapters.as_ref().map_or(0, |c| c.len() as u16)
        });
        let work_id: u32 = rec.story_id.parse().unwrap_or(0);
        let title = rec.title.as_deref().unwrap_or("");
        let rating = rec.rating.as_deref().unwrap_or("Not Rated");
        let language = rec.language.as_deref().unwrap_or("English");
        let is_completed = rec.status.as_deref() == Some("Complete");
        let published = parse_date(&dates.and_then(|d| d.published.clone()));
        let updated = dates.and_then(|d| d.updated.as_ref()).and_then(|u| {
            if u.len() >= 10 && u.as_bytes()[4] == b'-' { Some(&u[..10]) } else { None }
        });

        write!(buf, r#"{{"work_id":{},"title":"{}","authors":"#, work_id, escape_json_string(title)).unwrap();
        write_str_slice(&mut buf, &author_names);
        write!(buf, r#","rating":"{}","warnings":"#, escape_json_string(rating)).unwrap();
        write_str_array(&mut buf, rec.warnings.as_deref().unwrap_or(&[]));
        write!(buf, r#","category":"#).unwrap();
        write_str_array(&mut buf, rec.category.as_deref().unwrap_or(&[]));
        write!(buf, r#","language":"{}","is_completed":{},"fandoms":"#, escape_json_string(language), is_completed).unwrap();
        write_str_array(&mut buf, rec.fandoms.as_deref().unwrap_or(&[]));
        write!(buf, r#","relationships":"#).unwrap();
        write_str_array(&mut buf, rec.relationships.as_deref().unwrap_or(&[]));
        write!(buf, r#","characters":"#).unwrap();
        write_str_array(&mut buf, rec.characters.as_deref().unwrap_or(&[]));
        write!(buf, r#","freeform_tags":"#).unwrap();
        write_str_array(&mut buf, rec.freeform_tags.as_deref().unwrap_or(&[]));

        match rec.words {
            Some(w) => write!(buf, r#","words":{},"chapters_count":{}"#, w, chapters_count).unwrap(),
            None => write!(buf, r#","words":null,"chapters_count":{}"#, chapters_count).unwrap(),
        }

        match rec.chapters_total {
            Some(ct) => write!(buf, r#","chapters_total":{},"published_date":"{}""#, ct, published).unwrap(),
            None => write!(buf, r#","chapters_total":null,"published_date":"{}""#, published).unwrap(),
        }

        match updated {
            Some(u) => write!(buf, r#","updated_date":"{}""#, u).unwrap(),
            None => buf.push_str(r#","updated_date":null"#),
        }

        match stats.and_then(|s| s.kudos) {
            Some(v) => write!(buf, r#","kudos":{}"#, v).unwrap(),
            None => buf.push_str(r#","kudos":null"#),
        }
        match stats.and_then(|s| s.comments) {
            Some(v) => write!(buf, r#","comments":{}"#, v).unwrap(),
            None => buf.push_str(r#","comments":null"#),
        }
        match stats.and_then(|s| s.bookmarks) {
            Some(v) => write!(buf, r#","bookmarks":{}"#, v).unwrap(),
            None => buf.push_str(r#","bookmarks":null"#),
        }
        match stats.and_then(|s| s.hits) {
            Some(v) => write!(buf, r#","hits":{}"#, v).unwrap(),
            None => buf.push_str(r#","hits":null"#),
        }

        buf.push_str(r#","series_names":"#);
        write_str_slice(&mut buf, &series_names);
        buf.push_str(r#","series_indexes":["#);
        for (i, s) in series.iter().enumerate() {
            if i > 0 { buf.push(','); }
            write!(buf, "{}", s.index.unwrap_or(0)).unwrap();
        }
        buf.push_str(r#"],"collections":"#);
        write_str_array(&mut buf, rec.collections.as_deref().unwrap_or(&[]));

        match rec.summary.as_deref() {
            Some(s) => write!(buf, r#","summary":"{}"}}"#, escape_json_string(s)).unwrap(),
            None => buf.push_str(r#","summary":null}"#),
        }

        buf.push('\n');
        stdin.write_all(buf.as_bytes()).unwrap();
    }

    let output = child.wait_with_output().unwrap();
    if !output.status.success() {
        eprintln!("ClickHouse insert failed: {}", String::from_utf8_lossy(&output.stderr));
        process::exit(1);
    }

    eprintln!("Inserted {} rows", records.len());
}
