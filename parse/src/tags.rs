use rayon::prelude::*;
use zip::ZipArchive;
use regex::Regex;
use std::{
    collections::HashMap,
    env, fs,
    io::Read,
    process,
    sync::{Arc, LazyLock, Mutex},
};

static RE_CHAPTER_FILE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i).*/(?:file|chapter)\d+(?:_u\d+)?\.x?html$").unwrap());
static RE_TAG_NAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)<(/?[a-zA-Z][a-zA-Z0-9-]*)").unwrap());

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
    let bundle = Arc::new(Mutex::new(
        ZipArchive::new(file).unwrap_or_else(|e| {
            eprintln!("Failed to read zip {}: {e}", args[1]);
            process::exit(1);
        }),
    ));
    let epubs: Vec<String> = bundle
        .lock()
        .unwrap()
        .file_names()
        .filter(|n| n.to_lowercase().ends_with(".epub"))
        .map(String::from)
        .collect();
    eprintln!("Found {} EPUBs", epubs.len());

    let read_epub = |name: &str| -> Option<Vec<u8>> {
        let mut archive = bundle.lock().unwrap();
        archive.by_name(name).ok().and_then(|mut f| {
            let mut buf = Vec::new();
            f.read_to_end(&mut buf).ok().map(|_| buf)
        })
    };

    let counts: Mutex<HashMap<String, u64>> = Mutex::new(HashMap::new());
    epubs.par_iter().for_each(|name| {
        let Some(epub) = read_epub(name) else { return };
        let Ok(mut archive) = zip::ZipArchive::new(std::io::Cursor::new(&epub)) else { return };
        let names: Vec<String> = archive.file_names().filter(|n| RE_CHAPTER_FILE.is_match(n)).map(String::from).collect();
        let mut local: HashMap<String, u64> = HashMap::new();
        for ch in names {
            let Ok(mut entry) = archive.by_name(&ch) else { continue };
            let mut raw = Vec::new();
            if entry.read_to_end(&mut raw).is_err() {
                continue;
            }
            let text = String::from_utf8_lossy(&raw);
            for cap in RE_TAG_NAME.captures_iter(&text) {
                *local.entry(cap[1].to_lowercase()).or_default() += 1;
            }
        }
        if !local.is_empty() {
            let mut global = counts.lock().unwrap();
            for (k, v) in local {
                *global.entry(k).or_default() += v;
            }
        }
    });

    let mut rows: Vec<(String, u64)> = counts.into_inner().unwrap().into_iter().collect();
    rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    println!("{:<20} {}", "tag", "count");
    for (tag, count) in rows {
        println!("{tag:<20} {count}");
    }
}
