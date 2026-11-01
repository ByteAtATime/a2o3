use rayon::prelude::*;
use regex::Regex;
use std::{
    env, fs,
    io::{BufWriter, Read, Write},
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

const WORKS_PER_CHUNK: usize = 8192;
const FRAME_SIZE: usize = 16 << 20;
const SHARD_TARGET_BYTES: usize = 2 << 30;
const ZSTD_LEVEL: i32 = 11;
const REPORT_EVERY: usize = 10_000;
const MAX_INFLIGHT_FRAMES: usize = 64;

static RE_LANG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)<dc:language>([^<]+)</dc:language>").unwrap());
static RE_CREATOR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)<dc:creator[^>]*>([^<]+)</dc:creator>").unwrap());
static RE_FANDOM: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)<b>(?:Category|Fandom):</b>\s*([^<,\n]+)").unwrap());
static RE_WORKS_ID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"works/(\d+)").unwrap());
static RE_FILENAME_ID: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)[-_](\d+)\.epub$").unwrap());
static RE_CHAPTER_FILE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i).*/(?:file|chapter)\d+(?:_u\d+)?\.x?html$").unwrap());
static RE_USERSTUFF: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?is)<div[^>]*class="userstuff[^"]*"[^>]*>(.*?)</div>\s*(?:<div class="fff_chapter_notes|</body>)"#).unwrap()
});
static RE_TITLE_TAG: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)<h[1-6][^>]*class="fff_chapter_title"[^>]*>(.*?)</h[1-6]>"#).unwrap()
});
static RE_TITLE_FALLBACK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)<title>(.*?)</title>").unwrap());
static RE_BR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)<br\s*/?>").unwrap());
static RE_BLOCK_END: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)</(?:p|div|h[1-6]|blockquote|tr|li|section|article)>").unwrap());
static RE_HR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)<hr\s*/?>").unwrap());
static RE_TAGS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<[^>]+>").unwrap());
static RE_SPACES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[ \t\r\f\v]+").unwrap());
static RE_MULTI_NL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\n{3,}").unwrap());

#[derive(Clone)]
struct SortKey {
    lang: String,
    fandom: String,
    author: String,
    work_id: u64,
    path: PathBuf,
}

struct Work {
    work_id: u64,
    text: Vec<u8>,
}

fn clean_html_to_text(html: &str) -> String {
    let text = RE_BR.replace_all(html, "\n");
    let text = RE_BLOCK_END.replace_all(&text, "\n\n");
    let text = RE_HR.replace_all(&text, "\n\n---\n\n");
    let text = RE_TAGS.replace_all(&text, "");
    let text = html_escape_decode(&text);
    let text = RE_SPACES.replace_all(&text, " ");
    let text = RE_MULTI_NL.replace_all(&text, "\n\n");
    text.trim().to_string()
}

fn html_escape_decode(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&nbsp;", " ")
}

fn work_id_from_path(path: &Path) -> u64 {
    path.to_str()
        .and_then(|s| RE_FILENAME_ID.captures(s))
        .and_then(|m| m[1].parse().ok())
        .unwrap_or(0)
}

fn scan_metadata(path: &Path) -> Option<SortKey> {
    let file = fs::File::open(path).ok()?;
    let mut archive = ZipArchive::new(file).ok()?;
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
                if let Some(m) = RE_WORKS_ID.captures(&text) {
                    work_id = m[1].parse().unwrap_or(0);
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
            }
        }
    }

    if work_id == 0 {
        work_id = work_id_from_path(path);
    }
    if work_id == 0 {
        return None;
    }

    Some(SortKey { lang, fandom, author, work_id, path: path.to_path_buf() })
}

fn extract_prose(path: &Path, work_id: u64) -> Option<Work> {
    let file = fs::File::open(path).ok()?;
    let mut archive = ZipArchive::new(file).ok()?;
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
        let html = String::from_utf8_lossy(&raw);

        let ch_title = RE_TITLE_TAG
            .captures(&html)
            .or_else(|| RE_TITLE_FALLBACK.captures(&html))
            .map(|m| clean_html_to_text(&m[1]))
            .unwrap_or_default();

        let body_html =
            RE_USERSTUFF.captures(&html).map(|m| m[1].to_string()).unwrap_or_else(|| html.to_string());
        let body_text = clean_html_to_text(&body_html);

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
    zstd::encode_all(data, ZSTD_LEVEL).expect("zstd compression failed")
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

struct ShardStats {
    start: Instant,
    bytes_out: AtomicU64,
}

enum FrameMsg {
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

fn collect_epubs(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_epubs(&path, out);
        } else if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("epub")) {
            out.push(path);
        }
    }
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 3 {
        eprintln!("Usage: {} <epub_dir> <output_dir>", args[0]);
        process::exit(1);
    }

    let input_dir = Path::new(&args[1]);
    let output_dir = Path::new(&args[2]);
    fs::create_dir_all(output_dir).expect("failed to create output dir");

    let start = Instant::now();
    let mut epubs = Vec::new();
    collect_epubs(input_dir, &mut epubs);
    println!("Found {} EPUBs in {:.1}s.", with_commas(epubs.len() as u64), start.elapsed().as_secs_f64());

    let scan_start = Instant::now();
    let scan_done = AtomicUsize::new(0);
    let mut keys: Vec<SortKey> = epubs
        .par_iter()
        .filter_map(|p| {
            let key = scan_metadata(p);
            let d = scan_done.fetch_add(1, Ordering::Relaxed) + 1;
            if d % 100_000 == 0 {
                let elapsed = scan_start.elapsed().as_secs_f64();
                eprintln!(
                    "[scan {}/{}] {:.0} files/sec",
                    with_commas(d as u64),
                    with_commas(epubs.len() as u64),
                    d as f64 / elapsed.max(1e-9),
                );
            }
            key
        })
        .collect();
    drop(epubs);
    let scan_elapsed = scan_start.elapsed();
    println!(
        "[Pass 1/2] Scanned {} works in {:.1}s ({:.0} files/sec)",
        with_commas(keys.len() as u64),
        scan_elapsed.as_secs_f64(),
        keys.len() as f64 / scan_elapsed.as_secs_f64().max(1e-9),
    );

    keys.sort_unstable_by(|a, b| {
        a.lang.cmp(&b.lang)
            .then_with(|| a.fandom.cmp(&b.fandom))
            .then_with(|| a.author.cmp(&b.author))
            .then_with(|| a.work_id.cmp(&b.work_id))
    });

    println!("[Pass 2/2] Extracting and packing");
    let pack_start = Instant::now();
    let total = keys.len();
    let stats = Arc::new(ShardStats { start: pack_start, bytes_out: AtomicU64::new(0) });
    let extracted = AtomicUsize::new(0);
    let last_report = AtomicUsize::new(0);

    let (frame_tx, frame_rx) = mpsc::channel::<FrameMsg>();
    let writer_stats = Arc::clone(&stats);
    let writer_dir = output_dir.to_path_buf();
    let writer = std::thread::spawn(move || {
        let mut shard_idx = 0usize;
        let mut zst: Option<BufWriter<fs::File>> = None;
        let mut seek_entries: Vec<(u32, u32)> = Vec::new();
        let mut idx_entries: Vec<(u32, u32, u32)> = Vec::new();
        while let Ok(msg) = frame_rx.recv() {
            match msg {
                FrameMsg::Frame { idx, d_size, rx } => {
                    let compressed = rx.recv().expect("frame compressor died");
                    if zst.is_none() {
                        let base = writer_dir.join(format!("ao3_text_{shard_idx:04}"));
                        let f =
                            fs::File::create(base.with_extension("seekable.zst")).expect("create zst");
                        zst = Some(BufWriter::with_capacity(1 << 20, f));
                    }
                    zst.as_mut().unwrap().write_all(&compressed).expect("write frame");
                    seek_entries.push((compressed.len() as u32, d_size));
                    idx_entries.extend(idx);
                }
                FrameMsg::ShardEnd { works } => {
                    let base = writer_dir.join(format!("ao3_text_{shard_idx:04}"));
                    {
                        let mut f = zst.take().expect("shard end with no frames");
                        write_seek_table(&mut f, &seek_entries);
                        f.flush().expect("flush zst");
                    }
                    let mut idx_file = BufWriter::with_capacity(
                        1 << 20,
                        fs::File::create(base.with_extension("idx")).expect("create idx"),
                    );
                    for &(work_id, off, len) in &idx_entries {
                        idx_file.write_all(&work_id.to_le_bytes()).unwrap();
                        idx_file.write_all(&off.to_le_bytes()).unwrap();
                        idx_file.write_all(&len.to_le_bytes()).unwrap();
                    }
                    idx_file.flush().unwrap();

                    let shard_compressed: u64 = seek_entries.iter().map(|&(c, _)| c as u64).sum();
                    writer_stats.bytes_out.fetch_add(shard_compressed, Ordering::Relaxed);
                    eprintln!(
                        "[pack] shard {shard_idx:04} done: {} works, {} frames, {} MiB | {} MiB total | elapsed {:.1}m",
                        with_commas(works as u64),
                        with_commas(seek_entries.len() as u64),
                        with_commas(shard_compressed >> 20),
                        with_commas(writer_stats.bytes_out.load(Ordering::Relaxed) >> 20),
                        writer_stats.start.elapsed().as_secs_f64() / 60.0,
                    );
                    seek_entries.clear();
                    idx_entries.clear();
                    shard_idx += 1;
                }
            }
        }
        shard_idx
    });

    let limiter = FrameLimiter::new(MAX_INFLIGHT_FRAMES);
    let mut frame_works: Vec<Work> = Vec::new();
    let mut frame_idx: Vec<(u32, u32, u32)> = Vec::new();
    let mut frame_bytes = 0usize;
    let mut shard_bytes = 0usize;
    let mut shard_works = 0usize;
    let mut shard_offset = 0u32;
    let mut shard_count = 0usize;

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
        let _ = frame_tx
            .send(FrameMsg::Frame { idx: std::mem::take(frame_idx), d_size, rx: crx });
        frame_works.clear();
        *frame_bytes = 0;
    };

    for chunk in keys.chunks(WORKS_PER_CHUNK) {
        let chunk_works: Vec<Work> = chunk
            .par_iter()
            .filter_map(|key| {
                let work = extract_prose(&key.path, key.work_id)
                    .filter(|w| !w.text.is_empty() && w.text.len() <= u32::MAX as usize);
                let d = extracted.fetch_add(1, Ordering::Relaxed) + 1;
                let bucket = d / REPORT_EVERY;
                if bucket > 0 && last_report.fetch_max(bucket, Ordering::Relaxed) < bucket {
                    let elapsed = pack_start.elapsed().as_secs_f64().max(1e-9);
                    eprintln!(
                        "[pack {}/{}] {:.0} works/sec | elapsed {:.1}m | ETA {:.1}m",
                        with_commas(d as u64),
                        with_commas(total as u64),
                        d as f64 / elapsed,
                        elapsed / 60.0,
                        (total - d) as f64 / (d as f64 / elapsed) / 60.0,
                    );
                }
                work
            })
            .collect();

        for work in chunk_works {
            let len = work.text.len();
            frame_idx.push((work.work_id as u32, shard_offset, len as u32));
            shard_offset += len as u32;
            frame_works.push(work);
            frame_bytes += len;
            shard_bytes += len;
            shard_works += 1;
            if frame_bytes >= FRAME_SIZE {
                send_frame(&mut frame_works, &mut frame_idx, &mut frame_bytes);
            }
            if shard_bytes >= SHARD_TARGET_BYTES {
                if frame_bytes > 0 {
                    send_frame(&mut frame_works, &mut frame_idx, &mut frame_bytes);
                }
                let _ = frame_tx.send(FrameMsg::ShardEnd { works: shard_works });
                shard_bytes = 0;
                shard_works = 0;
                shard_offset = 0;
                shard_count += 1;
            }
        }
    }
    if frame_bytes > 0 {
        send_frame(&mut frame_works, &mut frame_idx, &mut frame_bytes);
    }
    if shard_works > 0 {
        let _ = frame_tx.send(FrameMsg::ShardEnd { works: shard_works });
        shard_count += 1;
    }
    drop(frame_tx);
    let _ = writer.join();
    drop(keys);

    let works_total = extracted.load(Ordering::Relaxed);
    let compressed_bytes = stats.bytes_out.load(Ordering::Relaxed);
    let elapsed = pack_start.elapsed().as_secs_f64();
    println!(
        "\nFinished! {} works in {} shards in {:.1}m ({:.0} works/sec), {} MiB compressed",
        with_commas(works_total as u64),
        shard_count,
        elapsed / 60.0,
        works_total as f64 / elapsed.max(1e-9),
        with_commas(compressed_bytes >> 20),
    );
}
