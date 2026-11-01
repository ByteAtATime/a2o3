use rayon::prelude::*;
use regex::Regex;
use std::{
    env, fs,
    io::{BufWriter, Cursor, Read, Write},
    path::Path,
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
static RE_USERSTUFF_START: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)<div[^>]*class="userstuff[^"]*"[^>]*>"#).unwrap()
});
static RE_TITLE_TAG: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)<h[1-6][^>]*class="fff_chapter_title"[^>]*>(.*?)</h[1-6]>"#).unwrap()
});
static RE_TITLE_FALLBACK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)<title>(.*?)</title>").unwrap());
static RE_A_TAG: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?is)<a\s[^>]*href="([^"]*)"[^>]*>(.*?)</a>"#).unwrap()
});
static RE_EM_TAG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)</?(?:em|i)>").unwrap());
static RE_STRONG_TAG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)</?(?:strong|b)>").unwrap());
static RE_STRIKE_TAG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)</?(?:s|strike|del)>").unwrap());
static RE_CODE_TAG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)</?code>").unwrap());
static RE_Q_OPEN_TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)<q>").unwrap());
static RE_Q_CLOSE_TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)</q>").unwrap());
static RE_TAG_PASS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(<br\s*/?>)|(<hr\s*/?>)|(</(?:p|div|h[1-6]|blockquote|li|ul|ol|section|article|pre|details|summary|figure|figcaption|center)\b[^>]*>)|(<(?:p|div|h[1-6]|blockquote|li|ul|ol|section|article|pre|details|summary|figure|figcaption|center)\b[^>]*>)|(<[^>]+>)")
        .unwrap()
});
static RE_SPACES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[ \t\r\f\v]+").unwrap());
static RE_MULTI_NL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\n{3,}").unwrap());
static RE_MD_HR_LIKE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^([*_=-]{3,})[ \t]*$").unwrap());

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

fn clean_html_to_text(html: &str) -> String {
    let text = html.replace("&lt;", "\u{2}").replace("&gt;", "\u{3}");
    let text = html_escape_decode(&text);
    let text = RE_A_TAG.replace_all(&text, |c: &regex::Captures| format!("[{}]({})", &c[2], &c[1]));
    let text = RE_EM_TAG.replace_all(&text, "*");
    let text = RE_STRONG_TAG.replace_all(&text, "**");
    let text = RE_STRIKE_TAG.replace_all(&text, "~~");
    let text = RE_CODE_TAG.replace_all(&text, "`");
    let text = RE_Q_OPEN_TAG.replace_all(&text, "\"");
    let text = RE_Q_CLOSE_TAG.replace_all(&text, "\"");
    let text = RE_TAG_PASS.replace_all(&text, |c: &regex::Captures| -> String {
        if c.get(1).is_some() {
            "\n".into()
        } else if c.get(2).is_some() {
            format!("\n\n\u{1}\n\n")
        } else if c.get(3).is_some() {
            "\n\n".into()
        } else if c.get(4).is_some() {
            String::new()
        } else if let Some(m) = c.get(5) {
            m.as_str().to_lowercase()
        } else {
            String::new()
        }
    });
    let text = RE_SPACES.replace_all(&text, " ");
    let text = RE_MD_HR_LIKE.replace_all(&text, |c: &regex::Captures| format!("\\{}", &c[1]));
    let text = text.replace('\u{1}', "---");
    let text = text.replace('\u{2}', "&lt;").replace('\u{3}', "&gt;");
    let text = RE_MULTI_NL.replace_all(&text, "\n\n");
    text.trim().to_string()
}

fn html_escape_decode(s: &str) -> String {
    let Some(first) = s.find('&') else { return s.to_string() };
    let mut out = String::with_capacity(s.len());
    out.push_str(&s[..first]);
    let mut rest = &s[first..];
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        let (rep, len) = if tail.starts_with("&amp;") {
            ("&", 5)
        } else if tail.starts_with("&lt;") {
            ("<", 4)
        } else if tail.starts_with("&gt;") {
            (">", 4)
        } else if tail.starts_with("&quot;") {
            ("\"", 6)
        } else if tail.starts_with("&#39;") {
            ("'", 5)
        } else if tail.starts_with("&apos;") {
            ("'", 6)
        } else if tail.starts_with("&nbsp;") {
            (" ", 6)
        } else {
            ("&", 1)
        };
        out.push_str(rep);
        rest = &rest[i + len..];
    }
    out.push_str(rest);
    out
}

fn cut_mid_tag(tail: &str, end: usize) -> usize {
    match tail[..end].rfind('<') {
        Some(lt) if lt > tail[..end].rfind('>').unwrap_or(0) => lt,
        _ => end,
    }
}

fn work_id_from_name(name: &str) -> u64 {
    RE_FILENAME_ID
        .captures(name)
        .and_then(|m| m[1].parse().ok())
        .unwrap_or(0)
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

        let ch_title = RE_TITLE_TAG
            .captures(html)
            .or_else(|| RE_TITLE_FALLBACK.captures(html))
            .map(|m| clean_html_to_text(&m[1]))
            .unwrap_or_default();

        let body_html = RE_USERSTUFF_START.find(html).map(|m| {
            let start = m.end();
            let tail = &html[start..];
            let end = match (tail.find("fff_chapter_notes"), tail.find("</body>")) {
                (Some(a), Some(b)) => a.min(b),
                (Some(a), None) => a,
                (None, Some(b)) => b,
                (None, None) => tail.len(),
            };
            &html[start..start + cut_mid_tag(tail, end)]
        }).unwrap_or(html);
        let body_text = clean_html_to_text(body_html);

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

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 3 {
        eprintln!("Usage: {} <input.zip> <output_dir>", args[0]);
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

    let output_dir = Path::new(&args[2]);
    fs::create_dir_all(output_dir).expect("failed to create output dir");

    let start = Instant::now();
    println!("Found {} EPUBs in {:.1}s.", with_commas(epubs.len() as u64), start.elapsed().as_secs_f64());

    let read_epub = |name: &str| -> Option<Vec<u8>> {
        let mut archive = bundle.lock().unwrap();
        archive.by_name(name).ok().and_then(|mut f| {
            let mut buf = Vec::new();
            f.read_to_end(&mut buf).ok().map(|_| buf)
        })
    };

    let scan_start = Instant::now();
    let scan_done = AtomicUsize::new(0);
    let mut keys: Vec<SortKey> = epubs
        .par_iter()
        .filter_map(|name| {
            let key = read_epub(name).and_then(|epub| scan_metadata(&epub, name));
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
                let work = read_epub(&key.name)
                    .and_then(|epub| extract_prose(&epub, key.work_id))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_author_literal_tags() {
        let html = r#"<p>&lt;barghest&gt; Hey Koji, guess what!</p>"#;
        assert_eq!(clean_html_to_text(html), "&lt;barghest&gt; Hey Koji, guess what!");
    }

    #[test]
    fn strips_escaped_markup() {
        let html = r#"<div class="userstuff"><p>He typed &lt;div class="chapter"&gt; aloud.</p><p>End &amp; done.</p></div>"#;
        let text = clean_html_to_text(html);
        assert_eq!(text, "He typed &lt;div class=\"chapter\"&gt; aloud.\n\nEnd & done.");
    }

    #[test]
    fn strips_real_markup() {
        let html = "<h2 class=\"fff_chapter_title\">Ch 1</h2><div class=\"userstuff\"><p>A<br/>B</p></div>";
        assert_eq!(clean_html_to_text(html), "Ch 1\n\nA\nB");
    }

    #[test]
    fn double_escape_survives() {
        assert_eq!(clean_html_to_text("<p>&amp;lt;div&amp;gt;</p>"), "&lt;div&gt;");
    }

    #[test]
    fn cut_backs_up_to_unclosed_tag() {
        let tail = r#"<p>Prose.</p>
<div class="fff_chapter_notes">notes</div>"#;
        let end = tail.find("fff_chapter_notes").unwrap();
        assert_eq!(cut_mid_tag(tail, end), tail.rfind("<div").unwrap());
        let text = clean_html_to_text(&tail[..cut_mid_tag(tail, end)]);
        assert!(!text.contains('<'), "leftover markup in: {text:?}");
        assert_eq!(text, "Prose.");
    }

    #[test]
    fn cut_keeps_clean_boundary() {
        let tail = "<p>Prose.</p><div>notes";
        let end = tail.find("notes").unwrap();
        assert_eq!(cut_mid_tag(tail, end), end);
    }

    #[test]
    fn keeps_inline_formatting() {
        assert_eq!(
            clean_html_to_text("<p>He was <em>very</em> <strong>bold</strong> and <i>italic</i>.</p>"),
            "He was *very* **bold** and *italic*."
        );
    }

    #[test]
    fn keeps_links() {
        assert_eq!(
            clean_html_to_text(r#"<p>See <a href="https://archiveofourown.org/works/1">this</a> now.</p>"#),
            "See [this](https://archiveofourown.org/works/1) now."
        );
    }

    #[test]
    fn keeps_strike_and_code() {
        assert_eq!(
            clean_html_to_text("<p><s>old</s> <strike>gone</strike> <code>x = 1</code></p>"),
            "~~old~~ ~~gone~~ `x = 1`"
        );
    }

    #[test]
    fn keeps_nested_formatting_in_links() {
        assert_eq!(
            clean_html_to_text(r#"<p><a href="/works/9"><em>Fic</em> here</a></p>"#),
            "[*Fic* here](/works/9)"
        );
    }

    #[test]
    fn keeps_sup_sub() {
        assert_eq!(
            clean_html_to_text("<p>x<sup>2</sup> and H<sub>2</sub>O</p>"),
            "x<sup>2</sup> and H<sub>2</sub>O"
        );
    }

    #[test]
    fn keeps_dl() {
        assert_eq!(
            clean_html_to_text("<dl><dt>Term</dt><dd>Definition</dd></dl>"),
            "<dl><dt>Term</dt><dd>Definition</dd></dl>"
        );
    }

    #[test]
    fn keeps_tables() {
        assert_eq!(
            clean_html_to_text("<table><tr><td>a</td><td><em>b</em></td></tr></table>"),
            "<table><tr><td>a</td><td>*b*</td></tr></table>"
        );
    }

    #[test]
    fn passes_through_misc_tags() {
        assert_eq!(
            clean_html_to_text("<p>Hi <SPAN Class='x'>there</SPAN>.</p>"),
            "Hi <span class='x'>there</span>."
        );
    }

    #[test]
    fn escapes_author_hr_like_lines() {
        assert_eq!(
            clean_html_to_text("<p>Before</p>\n<p>***</p>\n<p>After</p>"),
            "Before\n\n\\***\n\nAfter"
        );
    }

    #[test]
    fn real_hr_stays_hr() {
        assert_eq!(clean_html_to_text("<p>A</p><hr/><p>B</p>"), "A\n\n---\n\nB");
    }
}
