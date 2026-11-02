use rayon::prelude::*;
use regex::Regex;
use std::{
    collections::HashSet,
    env, fs,
    io::{BufWriter, Cursor, Read, Write},
    path::{Path, PathBuf},
    process,
    sync::{
        Arc, Condvar, LazyLock, Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering},
        mpsc,
    },
    time::Instant,
};
use zip::ZipArchive;

mod common;

const WORKS_PER_CHUNK: usize = 8192;
const FRAME_SIZE: usize = 16 << 20;
const SHARD_TARGET_BYTES: usize = 2 << 30;
const ZSTD_LEVEL: i32 = 11;
const MAX_INFLIGHT_FRAMES: usize = 64;
const REPORT_EVERY: usize = 10_000;

static RE_LANG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)<dc:language>([^<]+)</dc:language>").unwrap());
static RE_CREATOR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)<dc:creator[^>]*>([^<]+)</dc:creator>").unwrap());
static RE_FANDOM: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)<b>(?:Category|Fandom):</b>\s*([^<,\n]+)").unwrap());
static RE_WORKS_ID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"works/(\d+)").unwrap());
static RE_SOURCE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?is)<dc:source[^>]*>\s*([^<]+?)\s*</dc:source>").unwrap()
});
static RE_TITLE_LINK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?is)<h3[^>]*>\s*<a[^>]+href="[^"]*works/(\d+)"#).unwrap()
});
static RE_FILENAME_ID: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)[-_](\d+)\.epub$").unwrap());
static RE_CHAPTER_FILE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i).*/(?:file|chapter)\d+(?:_u\d+)?\.x?html$").unwrap());
static PRESERVED_TAGS: LazyLock<Vec<String>> = LazyLock::new(|| {
    [
        "span", "sup", "sub", "u", "small", "sml", "mark", "abbr", "cite", "q", "ruby", "rt",
        "bdi", "wbr", "ins", "del", "kbd", "samp", "var", "dfn", "time",
    ]
    .iter()
    .map(|tag| tag.to_string())
    .collect()
});

fn html_to_markdown(html: &str) -> String {
    let mut options = fast_h2m::ConversionOptions::default();
    options.preserve_tags = PRESERVED_TAGS.clone();
    options.escape_asterisks = true;
    options.escape_underscores = true;
    options.escape_misc = true;
    fast_h2m::convert(html, options)
        .ok()
        .and_then(|result| result.content)
        .map(|text| text.trim().to_string())
        .unwrap_or_default()
}

fn html_escape_decode(s: &str) -> String {
    html_escape::decode_html_entities(s).into_owned()
}

fn chapter_title(html: &str) -> String {
    let doc = scraper::Html::parse_document(html);
    let heading = scraper::Selector::parse("[class~=fff_chapter_title]")
        .ok()
        .and_then(|selector| doc.select(&selector).next())
        .map(|element| element.inner_html());
    let text = heading.or_else(|| {
        scraper::Selector::parse("title")
            .ok()
            .and_then(|selector| doc.select(&selector).next())
            .map(|element| element.inner_html())
    });
    text.map(|inner| {
        let mut plain = fast_h2m::ConversionOptions::default();
        plain.escape_asterisks = false;
        plain.escape_underscores = false;
        plain.escape_misc = false;
        fast_h2m::convert(&inner, plain)
            .ok()
            .and_then(|result| result.content)
            .unwrap_or_default()
            .replace('\n', " ")
            .trim()
            .to_string()
    })
    .unwrap_or_default()
}

fn userstuff_html(html: &str) -> String {
    let doc = scraper::Html::parse_document(html);
    let Ok(selector) = scraper::Selector::parse("div.userstuff") else {
        return html.to_string();
    };
    match doc.select(&selector).next() {
        Some(element) => element.inner_html(),
        None => html.to_string(),
    }
}

fn work_id_from_name(name: &str) -> u64 {
    RE_FILENAME_ID
        .captures(name)
        .and_then(|m| m[1].parse().ok())
        .unwrap_or(0)
}

#[derive(Clone)]
struct SortKey {
    lang: String,
    fandom: String,
    author: String,
    work_id: u64,
    name: String,
}

struct Work {
    work_id: u64,
    text: Vec<u8>,
}

fn scan_metadata(epub: &[u8], name: &str) -> Option<SortKey> {
    let mut archive = ZipArchive::new(Cursor::new(epub)).ok()?;
    let namelist: Vec<String> = archive.file_names().map(String::from).collect();

    let mut lang = "en".to_string();
    let mut author = String::new();
    let mut fandom = String::new();
    let mut work_id = 0u64;

    if let Some(opf_name) = namelist.iter().find(|n| n.ends_with(".opf")) {
        if let Ok(mut entry) = archive.by_name(opf_name) {
            let mut data = Vec::new();
            if entry.read_to_end(&mut data).is_ok() {
                let text = String::from_utf8_lossy(&data);
                if let Some(m) = RE_LANG.captures(&text) {
                    lang = m[1].trim().to_string();
                }
                if let Some(m) = RE_CREATOR.captures(&text) {
                    author = m[1].trim().to_string();
                }
                if let Some(m) = RE_SOURCE.captures(&text) {
                    if let Some(w) = RE_WORKS_ID.captures(&m[1]) {
                        work_id = w[1].parse().unwrap_or(0);
                    }
                }
            }
        }
    }

    if let Some(tp_name) = namelist.iter().find(|n| n.to_lowercase().contains("title_page")) {
        if let Ok(mut entry) = archive.by_name(tp_name) {
            let mut data = Vec::new();
            if entry.read_to_end(&mut data).is_ok() {
                let text = String::from_utf8_lossy(&data);
                if let Some(m) = RE_FANDOM.captures(&text) {
                    fandom = html_escape_decode(m[1].trim());
                }
                if let Some(m) = RE_TITLE_LINK.captures(&text) {
                    if let Ok(id) = m[1].parse() {
                        work_id = id;
                    }
                }
            }
        }
    }

    if work_id == 0 {
        work_id = work_id_from_name(name);
    }
    if work_id == 0 {
        return None;
    }

    Some(SortKey { lang, fandom, author, work_id, name: name.to_string() })
}

fn extract_prose(epub: &[u8], work_id: u64) -> Option<Work> {
    let mut archive = ZipArchive::new(Cursor::new(epub)).ok()?;
    let namelist: Vec<String> = archive.file_names().map(String::from).collect();

    let mut chapter_files: Vec<&String> =
        namelist.iter().filter(|p| RE_CHAPTER_FILE.is_match(p)).collect();
    chapter_files.sort();
    if chapter_files.is_empty() {
        return None;
    }

    let mut chapters: Vec<String> = Vec::with_capacity(chapter_files.len());
    for ch_path in chapter_files {
        let mut entry = match archive.by_name(ch_path) {
            Ok(e) => e,
            Err(_) => continue,
        };
        let mut raw = Vec::new();
        if entry.read_to_end(&mut raw).is_err() {
            continue;
        }
        let lossy;
        let html = match std::str::from_utf8(&raw) {
            Ok(s) => s,
            Err(_) => {
                lossy = String::from_utf8_lossy(&raw).into_owned();
                lossy.as_str()
            }
        };

        let ch_title = chapter_title(html);

        let body_html = userstuff_html(html);
        let body_text = html_to_markdown(&body_html);

        if !ch_title.is_empty() && !body_text.starts_with(&ch_title) {
            chapters.push(format!("### {ch_title}\n\n{body_text}"));
        } else {
            chapters.push(body_text);
        }
    }

    if chapters.is_empty() {
        return None;
    }

    let full_text = chapters.join("\n\n\n");
    Some(Work { work_id, text: full_text.into_bytes() })
}

fn compress_frame(data: &[u8]) -> Vec<u8> {
    std::thread_local! {
        static COMPRESSOR: std::cell::RefCell<zstd::bulk::Compressor<'static>> =
            std::cell::RefCell::new(zstd::bulk::Compressor::new(ZSTD_LEVEL).expect("zstd compressor"));
    }
    COMPRESSOR.with(|c| c.borrow_mut().compress(data).expect("zstd compression failed"))
}

fn write_seek_table<W: Write>(f: &mut W, entries: &[(u32, u32)]) {
    let mut table = zeekstd::SeekTable::new();
    for &(c_size, d_size) in entries {
        table.log_frame(c_size, d_size).expect("log_frame");
    }
    let mut serializer = table.into_serializer();
    let mut buf = [0u8; 4096];
    loop {
        let n = serializer.write_into(&mut buf);
        if n == 0 {
            break;
        }
        f.write_all(&buf[..n]).expect("write seek table");
    }
}

fn with_commas(n: u64) -> String {
    let s = n.to_string();
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, &c) in b.iter().enumerate() {
        if i > 0 && (b.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c as char);
    }
    out
}

fn report_progress(
    label: &str,
    unit: &str,
    done: usize,
    total: usize,
    start: Instant,
    last: &AtomicUsize,
) {
    if done % REPORT_EVERY != 0 {
        return;
    }
    let bucket = done / REPORT_EVERY;
    if last.fetch_max(bucket, Ordering::Relaxed) >= bucket {
        return;
    }
    let elapsed = start.elapsed().as_secs_f64().max(1e-9);
    let rate = done as f64 / elapsed;
    let eta = (total.saturating_sub(done)) as f64 / rate / 60.0;
    eprintln!(
        "[{label} {}/{}] {:.0} {unit}/sec | elapsed {:.1}m | ETA {:.1}m",
        with_commas(done as u64),
        with_commas(total as u64),
        rate,
        elapsed / 60.0,
        eta,
    );
}

struct ShardStats {
    start: Instant,
    bytes_out: AtomicU64,
}

enum FrameMsg {
    ShardStart { name: String },
    Frame {
        idx: Vec<(u32, u32, u32)>,
        d_size: u32,
        rx: mpsc::Receiver<Vec<u8>>,
    },
    ShardEnd { works: usize },
}

struct FrameLimiter {
    available: Mutex<usize>,
    released: Condvar,
}

impl FrameLimiter {
    fn new(limit: usize) -> Arc<Self> {
        Arc::new(Self { available: Mutex::new(limit), released: Condvar::new() })
    }

    fn acquire(&self) {
        let mut available = self.available.lock().unwrap();
        while *available == 0 {
            available = self.released.wait(available).unwrap();
        }
        *available -= 1;
    }

    fn release(&self) {
        let mut available = self.available.lock().unwrap();
        *available += 1;
        self.released.notify_one();
    }
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 3 {
        eprintln!(
            "Usage: {} <output_dir> <zip>... | <output_dir> --from-list <file>",
            args[0]
        );
        process::exit(1);
    }

    let output_dir = PathBuf::from(&args[1]);
    fs::create_dir_all(&output_dir).expect("failed to create output dir");

    let zips = common::collect_zips(&args[2..]);
    if zips.is_empty() {
        eprintln!("No input zips.");
        process::exit(1);
    }

    let zip_counts: Vec<usize> = zips
        .iter()
        .map(|zip_path| {
            fs::File::open(zip_path)
                .ok()
                .and_then(|file| ZipArchive::new(file).ok())
                .map(|archive| {
                    archive
                        .file_names()
                        .filter(|n| n.to_lowercase().ends_with(".epub"))
                        .count()
                })
                .unwrap_or(0)
        })
        .collect();
    let total_epubs: usize = zip_counts.iter().sum();

    cleanup_orphans(&output_dir);
    let mut seen = common::Seen::from_ids(load_packed_ids(&output_dir));
    eprintln!(
        "[pack] {} zips, {} works already packed",
        with_commas(zips.len() as u64),
        with_commas(seen.len() as u64)
    );

    let start = Instant::now();
    let scan_done = AtomicUsize::new(0);
    let last_scan_report = AtomicUsize::new(0);
    let pack_done = AtomicUsize::new(0);
    let last_pack_report = AtomicUsize::new(0);
    let stats = Arc::new(ShardStats { start, bytes_out: AtomicU64::new(0) });
    let (frame_tx, frame_rx) = mpsc::channel::<FrameMsg>();
    let writer_stats = Arc::clone(&stats);
    let writer_dir = output_dir.clone();
    let writer = std::thread::spawn(move || {
        let mut zst: Option<BufWriter<fs::File>> = None;
        let mut seek_entries: Vec<(u32, u32)> = Vec::new();
        let mut idx_entries: Vec<(u32, u32, u32)> = Vec::new();
        let mut shard_name: Option<String> = None;
        while let Ok(msg) = frame_rx.recv() {
            match msg {
                FrameMsg::ShardStart { name } => shard_name = Some(name),
                FrameMsg::Frame { idx, d_size, rx } => {
                    let compressed = rx.recv().expect("frame compressor died");
                    if zst.is_none() {
                        let name = shard_name.as_deref().expect("frame before shard start");
                        let f = fs::File::create(
                            writer_dir.join(name).with_extension("seekable.zst.tmp"),
                        )
                        .expect("create zst");
                        zst = Some(BufWriter::with_capacity(1 << 20, f));
                    }
                    zst.as_mut().unwrap().write_all(&compressed).expect("write frame");
                    seek_entries.push((compressed.len() as u32, d_size));
                    idx_entries.extend(idx);
                }
                FrameMsg::ShardEnd { works } => {
                    let name = shard_name.take().expect("shard end before shard start");
                    let base = writer_dir.join(&name);
                    let mut f = zst.take().expect("shard end with no frames");
                    write_seek_table(&mut f, &seek_entries);
                    f.flush().expect("flush zst");
                    drop(f);
                    let mut idx_file = BufWriter::with_capacity(
                        1 << 20,
                        fs::File::create(base.with_extension("idx.tmp")).expect("create idx"),
                    );
                    for &(work_id, off, len) in &idx_entries {
                        idx_file.write_all(&work_id.to_le_bytes()).unwrap();
                        idx_file.write_all(&off.to_le_bytes()).unwrap();
                        idx_file.write_all(&len.to_le_bytes()).unwrap();
                    }
                    idx_file.flush().unwrap();
                    drop(idx_file);
                    fs::rename(
                        base.with_extension("seekable.zst.tmp"),
                        base.with_extension("seekable.zst"),
                    )
                    .expect("rename zst");
                    fs::rename(base.with_extension("idx.tmp"), base.with_extension("idx"))
                        .expect("rename idx");

                    let shard_compressed: u64 = seek_entries.iter().map(|&(c, _)| c as u64).sum();
                    writer_stats.bytes_out.fetch_add(shard_compressed, Ordering::Relaxed);
                    eprintln!(
                        "[pack] shard {name} done: {} works, {} frames, {} MiB | {} MiB total | elapsed {:.1}m",
                        with_commas(works as u64),
                        with_commas(seek_entries.len() as u64),
                        with_commas(shard_compressed >> 20),
                        with_commas(writer_stats.bytes_out.load(Ordering::Relaxed) >> 20),
                        writer_stats.start.elapsed().as_secs_f64() / 60.0,
                    );
                    seek_entries.clear();
                    idx_entries.clear();
                }
            }
        }
    });

    let limiter = FrameLimiter::new(MAX_INFLIGHT_FRAMES);
    let mut frame_works: Vec<Work> = Vec::new();
    let mut frame_idx: Vec<(u32, u32, u32)> = Vec::new();
    let mut frame_bytes = 0usize;

    let send_frame = |frame_works: &mut Vec<Work>,
                          frame_idx: &mut Vec<(u32, u32, u32)>,
                          frame_bytes: &mut usize| {
        let d_size = *frame_bytes as u32;
        let mut buf = Vec::with_capacity(*frame_bytes);
        for work in frame_works.iter() {
            buf.extend_from_slice(&work.text);
        }
        limiter.acquire();
        let (ctx, crx) = mpsc::channel();
        let lim = Arc::clone(&limiter);
        rayon::spawn(move || {
            let compressed = compress_frame(&buf);
            let _ = ctx.send(compressed);
            lim.release();
        });
        let _ = frame_tx.send(FrameMsg::Frame { idx: std::mem::take(frame_idx), d_size, rx: crx });
        frame_works.clear();
        *frame_bytes = 0;
    };

    let mut taken = existing_prefixes(&output_dir);
    let mut skipped_out = BufWriter::new(
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(output_dir.join("skipped.txt"))
            .expect("open skipped.txt"),
    );

    let mut total_packed = 0u64;
    let mut total_dups = 0u64;
    let mut total_failed = 0u64;
    let mut total_no_text = 0u64;
    let mut total_shards = 0u64;

    for zip_path in &zips {
        let zip_start = Instant::now();
        let file = match fs::File::open(zip_path) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("Failed to open {}: {e}", zip_path.display());
                process::exit(1);
            }
        };
        let bundle = Arc::new(Mutex::new(match ZipArchive::new(file) {
            Ok(archive) => archive,
            Err(e) => {
                eprintln!("Failed to read zip {}: {e}", zip_path.display());
                process::exit(1);
            }
        }));
        let epubs: Vec<String> = bundle
            .lock()
            .unwrap()
            .file_names()
            .filter(|n| n.to_lowercase().ends_with(".epub"))
            .map(String::from)
            .collect();
        let total = epubs.len();

        let read_epub = |name: &str| -> Option<Vec<u8>> {
            let mut archive = bundle.lock().unwrap();
            archive.by_name(name).ok().and_then(|mut f| {
                let mut buf = Vec::new();
                f.read_to_end(&mut buf).ok().map(|_| buf)
            })
        };

        let mut keys: Vec<SortKey> = epubs
            .par_iter()
            .filter_map(|name| {
                let key = read_epub(name).and_then(|epub| scan_metadata(&epub, name));
                let done = scan_done.fetch_add(1, Ordering::Relaxed) + 1;
                report_progress("scan", "files", done, total_epubs, start, &last_scan_report);
                key
            })
            .collect();
        let scanned = keys.len();
        let failed = total - scanned;

        let mut new_ids: HashSet<u32> = HashSet::with_capacity(scanned);
        keys.retain(|key| {
            let id = key.work_id as u32;
            !seen.contains(id) && new_ids.insert(id)
        });
        let dups = scanned - keys.len();
        seen.insert_all(new_ids.iter().copied());

        keys.sort_unstable_by(|a, b| {
            a.lang
                .cmp(&b.lang)
                .then_with(|| a.fandom.cmp(&b.fandom))
                .then_with(|| a.author.cmp(&b.author))
                .then_with(|| a.work_id.cmp(&b.work_id))
        });

        let prefix = resolve_prefix(zip_path, &taken);
        let mut shard_part = 0usize;
        let mut shard_offset = 0u32;
        let mut shard_bytes = 0usize;
        let mut shard_works = 0usize;
        let mut shard_started = false;
        let mut shards_written = 0u64;
        let mut packed = 0u64;
        let mut no_text: Vec<u32> = Vec::new();

        for chunk in keys.chunks(WORKS_PER_CHUNK) {
            let chunk_works: Vec<(u64, Option<Work>)> = chunk
                .par_iter()
                .map(|key| {
                    (
                        key.work_id,
                        read_epub(&key.name).and_then(|epub| extract_prose(&epub, key.work_id)),
                    )
                })
                .collect();

            for (work_id, work) in chunk_works {
                let Some(work) =
                    work.filter(|w| !w.text.is_empty() && w.text.len() <= u32::MAX as usize)
                else {
                    no_text.push(work_id as u32);
                    continue;
                };
                if !shard_started {
                    taken.insert(prefix.clone());
                    let _ = frame_tx.send(FrameMsg::ShardStart {
                        name: format!("{prefix}_{shard_part:02}"),
                    });
                    shard_started = true;
                }
                let len = work.text.len();
                frame_idx.push((work.work_id as u32, shard_offset, len as u32));
                shard_offset += len as u32;
                frame_works.push(work);
                frame_bytes += len;
                shard_bytes += len;
                shard_works += 1;
                packed += 1;
                let done = pack_done.fetch_add(1, Ordering::Relaxed) + 1;
                report_progress("pack", "works", done, total_epubs, start, &last_pack_report);
                if frame_bytes >= FRAME_SIZE {
                    send_frame(&mut frame_works, &mut frame_idx, &mut frame_bytes);
                }
                if shard_bytes >= SHARD_TARGET_BYTES {
                    if frame_bytes > 0 {
                        send_frame(&mut frame_works, &mut frame_idx, &mut frame_bytes);
                    }
                    let _ = frame_tx.send(FrameMsg::ShardEnd { works: shard_works });
                    shards_written += 1;
                    shard_part += 1;
                    shard_started = false;
                    shard_bytes = 0;
                    shard_works = 0;
                    shard_offset = 0;
                }
            }
        }
        if frame_bytes > 0 {
            send_frame(&mut frame_works, &mut frame_idx, &mut frame_bytes);
        }
        if shard_works > 0 {
            let _ = frame_tx.send(FrameMsg::ShardEnd { works: shard_works });
            shards_written += 1;
        }
        drop(keys);

        for id in &no_text {
            writeln!(skipped_out, "{id}").unwrap();
        }
        skipped_out.flush().unwrap();

        total_packed += packed;
        total_dups += dups as u64;
        total_failed += failed as u64;
        total_no_text += no_text.len() as u64;
        total_shards += shards_written;

        eprintln!(
            "[pack] {}: {} packed, {} dup, {} failed, {} no-text, {} shards, {:.1}s",
            zip_path.display(),
            with_commas(packed),
            with_commas(dups as u64),
            with_commas(failed as u64),
            with_commas(no_text.len() as u64),
            shards_written,
            zip_start.elapsed().as_secs_f64(),
        );
    }

    drop(frame_tx);
    let _ = writer.join();

    let compressed_bytes = stats.bytes_out.load(Ordering::Relaxed);
    let elapsed = start.elapsed().as_secs_f64();
    println!(
        "\nFinished! {} works in {} shards, {} dup, {} failed, {} no-text, {} MiB compressed, {:.1}m",
        with_commas(total_packed),
        with_commas(total_shards),
        with_commas(total_dups),
        with_commas(total_failed),
        with_commas(total_no_text),
        with_commas(compressed_bytes >> 20),
        elapsed / 60.0,
    );
}

const IDX_RECORD_SIZE: usize = 12;

fn load_packed_ids(dir: &Path) -> Vec<u32> {
    let mut ids = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return ids;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.ends_with(".idx") {
            if let Ok(data) = fs::read(&path) {
                for record in data.chunks_exact(IDX_RECORD_SIZE) {
                    ids.push(u32::from_le_bytes([record[0], record[1], record[2], record[3]]));
                }
            }
        } else if name == "skipped.txt" {
            if let Ok(text) = fs::read_to_string(&path) {
                ids.extend(text.lines().filter_map(|line| line.trim().parse::<u32>().ok()));
            }
        }
    }
    ids
}

fn cleanup_orphans(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
    files.sort();

    let mut indexed: HashSet<String> = HashSet::new();
    for path in &files {
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            if let Some(stem) = name.strip_suffix(".idx") {
                indexed.insert(stem.to_string());
            }
        }
    }

    for path in &files {
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.ends_with(".tmp") {
            let _ = fs::remove_file(path);
        } else if let Some(stem) = name.strip_suffix(".seekable.zst") {
            if !indexed.contains(stem) {
                let _ = fs::remove_file(path);
            }
        }
    }
}

fn prefix_of(stem: &str) -> &str {
    match stem.rsplit_once('_') {
        Some((base, part)) if part.len() == 2 && part.bytes().all(|b| b.is_ascii_digit()) => base,
        _ => stem,
    }
}

fn existing_prefixes(dir: &Path) -> HashSet<String> {
    let mut prefixes = HashSet::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return prefixes;
    };
    for entry in entries.flatten() {
        if let Ok(name) = entry.file_name().into_string() {
            if let Some(stem) = name.strip_suffix(".idx") {
                prefixes.insert(prefix_of(stem).to_string());
            }
        }
    }
    prefixes
}

fn resolve_prefix(zip: &Path, taken: &HashSet<String>) -> String {
    let raw = zip.file_stem().and_then(|s| s.to_str()).unwrap_or("shard");
    let stem: String = raw
        .chars()
        .take(40)
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    let stem = if stem.is_empty() { "shard".to_string() } else { stem };
    let candidate = format!("ao3_text_{stem}");
    if !taken.contains(&candidate) {
        return candidate;
    }
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in zip.to_string_lossy().bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    format!("ao3_text_{stem}_{:06x}", hash & 0xff_ffff)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_emphasis_across_paragraphs_keeps_markers_adjacent() {
        let html = "<div class=\"userstuff\"><p><em>\n <span>One.</span>\n </em></p><p><em>\n <span>Two.</span>\n </em></p></div>";
        assert_eq!(html_to_markdown(html), "*<span>One.</span>*\n\n*<span>Two.</span>*");
    }

    #[test]
    fn inline_emphasis_survives() {
        assert_eq!(
            html_to_markdown("<p>He was <em>very</em> <strong>bold</strong>.</p>"),
            "He was *very* **bold**."
        );
    }

    #[test]
    fn links_keep_hrefs() {
        assert_eq!(
            html_to_markdown(r#"<p>See <a href="/works/1">this</a> now.</p>"#),
            "See [this](/works/1) now."
        );
    }

    #[test]
    fn horizontal_rule_is_not_escaped() {
        assert_eq!(html_to_markdown("<p>A</p><hr/><p>B</p>"), "A\n\n---\n\nB");
    }

    #[test]
    fn userstuff_body_is_selected() {
        let html = "<html><body><blockquote class=\"userstuff\">note</blockquote><div class=\"userstuff module\"><p>Body.</p></div></body></html>";
        assert_eq!(userstuff_html(html), "<p>Body.</p>");
    }

    #[test]
    fn userstuff_selection_is_well_formed_at_the_end() {
        let html = "<html><body><div class=\"userstuff\"><p>Prose.</p></div></div></body></html>";
        assert_eq!(html_to_markdown(&userstuff_html(html)), "Prose.");
    }

    #[test]
    fn chapter_title_prefers_heading() {
        let html = "<h3 class=\"fff_chapter_title\">1. Shock</h3><title>Web Title</title>";
        assert_eq!(chapter_title(html), "1. Shock");
    }

    #[test]
    fn chapter_title_falls_back_to_title_tag() {
        assert_eq!(chapter_title("<title>Fallback</title>"), "Fallback");
    }

    #[test]
    fn custom_tags_are_preserved() {
        assert_eq!(
            html_to_markdown(r#"<p><span class="s1">Hello.</span></p>"#),
            "<span class=\"s1\">Hello.</span>"
        );
        assert_eq!(html_to_markdown("<p>x<sup>2</sup></p>"), "x<sup>2</sup>");
    }

    #[test]
    fn author_markdown_is_escaped() {
        assert_eq!(
            html_to_markdown("<p>He *sighs* and _walks_.</p>"),
            "He \\*sighs\\* and \\_walks\\_."
        );
    }

    #[test]
    fn author_dash_rule_is_not_a_real_rule() {
        assert_eq!(
            html_to_markdown("<p>Before</p><p>---</p><p>After</p>"),
            "Before\n\n\\-\\-\\-\n\nAfter"
        );
    }

    #[test]
    fn entities_decode() {
        assert_eq!(html_escape_decode("&lt;barghest&gt; &amp; more"), "<barghest> & more");
    }
}
