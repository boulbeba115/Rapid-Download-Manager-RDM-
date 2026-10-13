//! HLS (`.m3u8`) video streams: fetch the playlist, download the media
//! segments in parallel, decrypt AES-128 if needed, and join them in order
//! into one file. DRM-protected and live streams are refused.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aes::cipher::{block_padding::Pkcs7, BlockModeDecrypt, KeyIvInit};
use futures_util::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::sync::watch;
use url::Url;

use crate::error::describe;
use crate::filename::{part_path, state_path};
use crate::http::{self, RequestSpec};
use crate::limiter::RateLimiter;
use crate::state::{load_json, save_json};
use crate::task::{Outcome, Progress};

const MAX_PLAYLIST: usize = 8 * 1024 * 1024;

pub fn looks_like_hls(url: &str, content_type: Option<&str>) -> bool {
    let by_path = Url::parse(url).map(|u| u.path().to_ascii_lowercase().ends_with(".m3u8")).unwrap_or(false);
    let by_type = content_type.map(|t| t.to_ascii_lowercase()).is_some_and(|t| t.contains("mpegurl"));
    by_path || by_type
}

#[derive(Debug, Clone, PartialEq)]
pub struct Variant {
    pub url: String,
    pub bandwidth: u64,
    pub resolution: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Key {
    None,
    Aes128 { uri: String, iv: Option<[u8; 16]> },
}

#[derive(Debug, Clone, PartialEq)]
pub struct MediaSegment {
    pub url: String,
    pub key: Key,
    /// Media sequence number; the default IV when the playlist gives none.
    pub sequence: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Playlist {
    Master(Vec<Variant>),
    Media { segments: Vec<MediaSegment>, init: Option<String>, ended: bool },
}

/// `KEY=value,KEY="quoted, value"` → map with upper-case keys.
fn attributes(list: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let mut rest = list;
    while !rest.is_empty() {
        let Some((key, after)) = rest.split_once('=') else { break };
        let (value, next) = if let Some(quoted) = after.strip_prefix('"') {
            match quoted.split_once('"') {
                Some((v, tail)) => (v, tail.trim_start_matches(',')),
                None => (quoted, ""),
            }
        } else {
            after.split_once(',').unwrap_or((after, ""))
        };
        out.insert(key.trim().to_ascii_uppercase(), value.trim().to_string());
        rest = next;
    }
    out
}

fn parse_iv(text: &str) -> Option<[u8; 16]> {
    let hex_part = text.trim().trim_start_matches("0x").trim_start_matches("0X");
    let bytes = hex::decode(format!("{hex_part:0>32}")).ok()?;
    bytes.try_into().ok()
}

pub fn parse_playlist(text: &str, base: &Url) -> Result<Playlist, String> {
    if !text.trim_start().starts_with("#EXTM3U") {
        return Err("not an HLS playlist".into());
    }
    let resolve = |uri: &str| base.join(uri.trim()).map(|u| u.to_string()).map_err(|_| format!("bad address in playlist: {uri}"));

    let mut variants = Vec::new();
    let mut segments = Vec::new();
    let mut pending_variant: Option<(u64, Option<String>)> = None;
    let mut key = Key::None;
    let mut init = None;
    let mut sequence = 0u64;
    let mut ended = false;
    let mut expect_segment = false;

    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if let Some(attrs) = line.strip_prefix("#EXT-X-STREAM-INF:") {
            let a = attributes(attrs);
            let bandwidth = a.get("BANDWIDTH").and_then(|b| b.parse().ok()).unwrap_or(0);
            pending_variant = Some((bandwidth, a.get("RESOLUTION").cloned()));
        } else if let Some(attrs) = line.strip_prefix("#EXT-X-KEY:") {
            let a = attributes(attrs);
            key = match a.get("METHOD").map(String::as_str) {
                None | Some("NONE") => Key::None,
                Some("AES-128") => {
                    let uri = a.get("URI").ok_or("encrypted stream without a key address")?;
                    Key::Aes128 { uri: resolve(uri)?, iv: a.get("IV").and_then(|v| parse_iv(v)) }
                }
                Some(other) => return Err(format!("this stream is protected ({other}) and cannot be downloaded")),
            };
        } else if let Some(attrs) = line.strip_prefix("#EXT-X-MAP:") {
            if let Some(uri) = attributes(attrs).get("URI") {
                init = Some(resolve(uri)?);
            }
        } else if let Some(n) = line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
            sequence = n.trim().parse().unwrap_or(0);
        } else if line.starts_with("#EXTINF:") {
            expect_segment = true;
        } else if line.starts_with("#EXT-X-ENDLIST") {
            ended = true;
        } else if !line.starts_with('#') {
            if let Some((bandwidth, resolution)) = pending_variant.take() {
                variants.push(Variant { url: resolve(line)?, bandwidth, resolution });
            } else if expect_segment {
                segments.push(MediaSegment { url: resolve(line)?, key: key.clone(), sequence });
                sequence += 1;
                expect_segment = false;
            }
        }
    }
    if !variants.is_empty() {
        Ok(Playlist::Master(variants))
    } else if !segments.is_empty() {
        Ok(Playlist::Media { segments, init, ended })
    } else {
        Err("the playlist contains no media".into())
    }
}

/// Everything needed to download one rendition.
#[derive(Debug, Clone)]
pub struct Plan {
    pub segments: Vec<MediaSegment>,
    pub init: Option<String>,
    /// Shown to the user, e.g. "1280x720".
    pub quality: Option<String>,
}

impl Plan {
    /// fMP4 streams carry an init segment and are playable as `.mp4`; the rest are MPEG-TS.
    pub fn extension(&self) -> &'static str {
        if self.init.is_some() {
            "mp4"
        } else {
            "ts"
        }
    }
}

async fn cancelled(rx: &mut watch::Receiver<bool>) {
    let _ = rx.wait_for(|c| *c).await;
}

async fn fetch_text(client: &Client, spec: &RequestSpec, url: &str, timeout: Duration) -> Result<(String, Url), String> {
    let fut = async {
        let mut resp = http::request(client, spec, url).send().await.map_err(|e| describe(&e))?;
        if !resp.status().is_success() {
            return Err(format!("server returned HTTP {}", resp.status().as_u16()));
        }
        let base = resp.url().clone();
        let mut body = Vec::new();
        while let Some(chunk) = resp.chunk().await.map_err(|e| describe(&e))? {
            body.extend_from_slice(&chunk);
            if body.len() > MAX_PLAYLIST {
                return Err("the playlist is implausibly large".to_string());
            }
        }
        Ok((String::from_utf8_lossy(&body).into_owned(), base))
    };
    tokio::time::timeout(timeout, fut).await.unwrap_or_else(|_| Err("timed out waiting for the server".into()))
}

/// Read the playlist(s) and decide what will be downloaded. The best
/// rendition (highest bandwidth) is chosen from a master playlist.
pub async fn prepare(client: &Client, spec: &RequestSpec, timeout: Duration, cancel: &mut watch::Receiver<bool>) -> Result<Plan, Outcome> {
    let work = async {
        let (text, base) = fetch_text(client, spec, &spec.url, timeout).await?;
        let (playlist, quality) = match parse_playlist(&text, &base)? {
            Playlist::Master(mut variants) => {
                variants.sort_by_key(|v| v.bandwidth);
                let best = variants.pop().expect("a master playlist has variants");
                let (text, base) = fetch_text(client, spec, &best.url, timeout).await?;
                (parse_playlist(&text, &base)?, best.resolution)
            }
            media => (media, None),
        };
        match playlist {
            Playlist::Media { ended: false, .. } => Err("live streams cannot be downloaded".to_string()),
            Playlist::Media { segments, init, .. } => Ok(Plan { segments, init, quality }),
            Playlist::Master(_) => Err("the playlist points to another list instead of media".to_string()),
        }
    };
    tokio::select! {
        biased;
        _ = cancelled(cancel) => Err(Outcome::Paused),
        r = work => r.map_err(Outcome::Failed),
    }
}

pub struct HlsConfig {
    pub spec: RequestSpec,
    pub final_path: PathBuf,
    /// Segments fetched at the same time.
    pub parallel: usize,
    pub max_retries: u32,
    pub timeout: Duration,
    pub limiter: Arc<RateLimiter>,
    pub global_limiter: Arc<RateLimiter>,
}

/// Resume file for a stream download (same `.rdmstate` path as a normal one,
/// told apart by the `hls` marker).
#[derive(Debug, Serialize, Deserialize)]
struct HlsState {
    hls: bool,
    url: String,
    segment_count: usize,
    segments_done: usize,
    bytes: u64,
}

/// Bytes on disk from an earlier run of this stream download, if it can be resumed.
pub fn resumable_bytes(final_path: &std::path::Path, url: &str) -> Option<u64> {
    let st: HlsState = load_json(&state_path(final_path))?;
    (st.hls && st.url == url).then_some(st.bytes)
}

struct Fetcher {
    client: Client,
    spec: RequestSpec,
    cfg_timeout: Duration,
    max_retries: u32,
    limiter: Arc<RateLimiter>,
    global_limiter: Arc<RateLimiter>,
    progress: Arc<Progress>,
    keys: tokio::sync::Mutex<HashMap<String, [u8; 16]>>,
}

impl Fetcher {
    /// One attempt at one URL; bytes are counted into the progress as they arrive.
    async fn get(&self, url: &str, counted: &mut u64) -> Result<Vec<u8>, String> {
        let mut resp = tokio::time::timeout(self.cfg_timeout, http::request(&self.client, &self.spec, url).send())
            .await
            .map_err(|_| "timed out waiting for the server".to_string())?
            .map_err(|e| describe(&e))?;
        if !resp.status().is_success() {
            return Err(format!("server returned HTTP {}", resp.status().as_u16()));
        }
        let mut body = Vec::with_capacity(resp.content_length().unwrap_or(0).min(64 << 20) as usize);
        loop {
            let chunk = tokio::time::timeout(self.cfg_timeout, resp.chunk())
                .await
                .map_err(|_| "connection stalled".to_string())?
                .map_err(|e| describe(&e))?;
            let Some(chunk) = chunk else { break };
            self.limiter.acquire(chunk.len()).await;
            self.global_limiter.acquire(chunk.len()).await;
            body.extend_from_slice(&chunk);
            *counted += chunk.len() as u64;
            self.progress.downloaded.fetch_add(chunk.len() as u64, Ordering::Relaxed);
        }
        Ok(body)
    }

    async fn get_with_retry(&self, url: &str) -> Result<Vec<u8>, String> {
        let mut attempt = 0u32;
        loop {
            let mut counted = 0;
            match self.get(url, &mut counted).await {
                Ok(body) => return Ok(body),
                Err(e) => {
                    // A failed attempt's bytes must not stay in the total.
                    self.progress.downloaded.fetch_sub(counted, Ordering::Relaxed);
                    attempt += 1;
                    if attempt > self.max_retries || e.contains("HTTP 4") {
                        return Err(e);
                    }
                    *self.progress.last_error.lock() = Some(e);
                    tokio::time::sleep(Duration::from_secs_f64((0.5 * 2f64.powi(attempt as i32 - 1)).min(15.0))).await;
                }
            }
        }
    }

    async fn key(&self, uri: &str) -> Result<[u8; 16], String> {
        let mut keys = self.keys.lock().await;
        if let Some(k) = keys.get(uri) {
            return Ok(*k);
        }
        let bytes = self.get_with_retry(uri).await?;
        // Key bytes are not part of the file.
        self.progress.downloaded.fetch_sub(bytes.len() as u64, Ordering::Relaxed);
        let key: [u8; 16] = bytes.try_into().map_err(|_| "the stream's decryption key has the wrong size".to_string())?;
        keys.insert(uri.to_string(), key);
        Ok(key)
    }

    /// Download one media segment and return its clear bytes plus how many bytes came over the wire.
    async fn segment(&self, seg: &MediaSegment) -> Result<(Vec<u8>, u64), String> {
        let key = match &seg.key {
            Key::None => None,
            Key::Aes128 { uri, iv } => {
                let iv = iv.unwrap_or_else(|| {
                    let mut iv = [0u8; 16];
                    iv[8..].copy_from_slice(&seg.sequence.to_be_bytes());
                    iv
                });
                Some((self.key(uri).await?, iv))
            }
        };
        let body = self.get_with_retry(&seg.url).await?;
        let wire = body.len() as u64;
        match key {
            None => Ok((body, wire)),
            Some((key, iv)) => {
                let clear = cbc::Decryptor::<aes::Aes128>::new(&key.into(), &iv.into())
                    .decrypt_padded_vec::<Pkcs7>(&body)
                    .map_err(|_| "a segment could not be decrypted (wrong key or damaged data)".to_string())?;
                Ok((clear, wire))
            }
        }
    }
}

/// Download `plan` into `cfg.final_path`. Pausing keeps what is on disk.
pub async fn run(client: Client, cfg: HlsConfig, plan: Plan, progress: Arc<Progress>, mut cancel: watch::Receiver<bool>) -> Outcome {
    let part = part_path(&cfg.final_path);
    let state_file = state_path(&cfg.final_path);
    let total = plan.segments.len();
    progress.resumable.store(true, Ordering::Relaxed);

    // Resume only if the state matches this stream and the data is really there.
    let saved: Option<HlsState> = load_json(&state_file);
    let on_disk = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
    let (mut done, mut bytes) = match saved {
        Some(st) if st.hls && st.url == cfg.spec.url && st.segment_count == total && st.segments_done <= total && on_disk >= st.bytes => {
            (st.segments_done, st.bytes)
        }
        _ => (0, 0),
    };

    if let Some(dir) = part.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let mut file = match tokio::fs::OpenOptions::new().create(true).write(true).truncate(false).open(&part).await {
        Ok(f) => f,
        Err(e) => return Outcome::Failed(format!("cannot create {}: {e}", part.display())),
    };
    // Drop anything past the last complete segment, then continue appending.
    if let Err(e) = file.set_len(bytes).await {
        return Outcome::Failed(format!("disk error: {e}"));
    }
    if let Err(e) = tokio::io::AsyncSeekExt::seek(&mut file, std::io::SeekFrom::Start(bytes)).await {
        return Outcome::Failed(format!("disk error: {e}"));
    }
    progress.downloaded.store(bytes, Ordering::Relaxed);

    let fetcher = Arc::new(Fetcher {
        client,
        spec: cfg.spec.clone(),
        cfg_timeout: cfg.timeout,
        max_retries: cfg.max_retries,
        limiter: cfg.limiter.clone(),
        global_limiter: cfg.global_limiter.clone(),
        progress: progress.clone(),
        keys: tokio::sync::Mutex::new(HashMap::new()),
    });

    let save = |done: usize, bytes: u64| {
        let st = HlsState { hls: true, url: cfg.spec.url.clone(), segment_count: total, segments_done: done, bytes };
        if let Err(e) = save_json(&state_file, &st) {
            log::error!("cannot save stream state: {e}");
        }
    };

    // The init segment (fMP4) goes first and is fetched once.
    if done == 0 && bytes == 0 {
        if let Some(init) = &plan.init {
            let fetched = tokio::select! {
                biased;
                _ = cancelled(&mut cancel) => return Outcome::Paused,
                r = fetcher.get_with_retry(init) => r,
            };
            match fetched {
                Ok(data) => {
                    if let Err(e) = file.write_all(&data).await {
                        return Outcome::Failed(format!("disk error: {e}"));
                    }
                    bytes += data.len() as u64;
                }
                Err(e) => return Outcome::Failed(e),
            }
        }
    }

    let parallel = cfg.parallel.clamp(1, 16);
    let mut stream = futures_util::stream::iter(plan.segments[done..].iter().cloned())
        .map(|seg| {
            let fetcher = fetcher.clone();
            async move { fetcher.segment(&seg).await }
        })
        .buffered(parallel);

    let mut tick = tokio::time::interval(Duration::from_millis(250));
    let mut last_sample = (Instant::now(), progress.downloaded());
    let mut ema = 0f64;
    let mut last_save = Instant::now();
    // Bytes counted into the progress for segments not yet written, so the
    // displayed total can be corrected to exact file bytes when writing.
    let outcome = loop {
        tokio::select! {
            biased;
            _ = cancelled(&mut cancel) => break Outcome::Paused,
            _ = tick.tick() => {
                let now = Instant::now();
                let got = progress.downloaded();
                let dt = now.duration_since(last_sample.0).as_secs_f64();
                if dt >= 0.2 {
                    let inst = got.saturating_sub(last_sample.1) as f64 / dt;
                    ema = if ema == 0.0 { inst } else { ema * 0.7 + inst * 0.3 };
                    progress.speed.store(ema as u64, Ordering::Relaxed);
                    last_sample = (now, got);
                }
                progress.connections.store(parallel.min(total - done) as u32, Ordering::Relaxed);
            }
            next = stream.next() => match next {
                None => break Outcome::Complete,
                Some(Err(e)) => break Outcome::Failed(e),
                Some(Ok((data, _wire))) => {
                    if let Err(e) = file.write_all(&data).await {
                        break Outcome::Failed(format!("disk error: {e}"));
                    }
                    bytes += data.len() as u64;
                    done += 1;
                    *progress.last_error.lock() = None;
                    // Estimate the final size from the average segment so far.
                    progress.size.store(bytes / done as u64 * total as u64, Ordering::Relaxed);
                    if last_save.elapsed() >= Duration::from_secs(3) {
                        let _ = file.sync_data().await;
                        save(done, bytes);
                        last_save = Instant::now();
                    }
                }
            },
        }
    };
    drop(stream);
    progress.connections.store(0, Ordering::Relaxed);
    progress.speed.store(0, Ordering::Relaxed);
    progress.downloaded.store(bytes, Ordering::Relaxed);

    let flushed = async {
        file.flush().await?;
        file.sync_data().await
    }
    .await;
    drop(file);
    if let Err(e) = flushed {
        return Outcome::Failed(format!("disk error: {e}"));
    }

    match outcome {
        Outcome::Complete => {
            progress.size.store(bytes, Ordering::Relaxed);
            if let Err(e) = std::fs::rename(&part, &cfg.final_path) {
                return Outcome::Failed(format!("cannot save {}: {e}", cfg.final_path.display()));
            }
            let _ = std::fs::remove_file(&state_file);
            crate::security::mark_of_the_web(&cfg.final_path, &cfg.spec.url);
            Outcome::Complete
        }
        other => {
            save(done, bytes);
            other
        }
    }
}

/// A sensible file stem for a stream: the playlist's own name unless it is one
/// of the generic ones every site uses, in which case the folder above it.
pub fn stream_name(url: &str) -> String {
    const GENERIC: [&str; 8] = ["index", "master", "playlist", "chunklist", "prog_index", "manifest", "video", "stream"];
    let segments: Vec<String> = Url::parse(url)
        .ok()
        .and_then(|u| u.path_segments().map(|s| s.filter(|p| !p.is_empty()).map(str::to_string).collect()))
        .unwrap_or_default();
    let stem = |s: &str| s.rsplit_once('.').map_or(s, |(a, _)| a).to_string();
    for seg in segments.iter().rev() {
        let name = stem(seg);
        if !name.is_empty() && !GENERIC.contains(&name.to_ascii_lowercase().as_str()) {
            return crate::filename::sanitize(&name);
        }
    }
    "video".into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Url {
        Url::parse("https://cdn.example/show/ep1/master.m3u8").unwrap()
    }

    #[test]
    fn master_playlist() {
        let text = "#EXTM3U\n\
            #EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360,CODECS=\"avc1.4d401e,mp4a.40.2\"\n\
            low/index.m3u8\n\
            #EXT-X-STREAM-INF:BANDWIDTH=2800000,RESOLUTION=1280x720\n\
            https://other.example/hi/index.m3u8\n";
        let Playlist::Master(v) = parse_playlist(text, &base()).unwrap() else { panic!() };
        assert_eq!(v.len(), 2);
        assert_eq!(v[0], Variant { url: "https://cdn.example/show/ep1/low/index.m3u8".into(), bandwidth: 800000, resolution: Some("640x360".into()) });
        assert_eq!(v[1].url, "https://other.example/hi/index.m3u8");
    }

    #[test]
    fn media_playlist_with_keys() {
        let text = "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-MEDIA-SEQUENCE:7\n\
            #EXTINF:4.0,\nseg7.ts\n\
            #EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\",IV=0x000102030405060708090A0B0C0D0E0F\n\
            #EXTINF:4.0,\nseg8.ts\n\
            #EXT-X-KEY:METHOD=AES-128,URI=\"https://k.example/k2\"\n\
            #EXTINF:4.0,\nseg9.ts\n\
            #EXT-X-KEY:METHOD=NONE\n\
            #EXTINF:2.5,\nseg10.ts\n#EXT-X-ENDLIST\n";
        let Playlist::Media { segments, init, ended } = parse_playlist(text, &base()).unwrap() else { panic!() };
        assert!(ended && init.is_none());
        assert_eq!(segments.len(), 4);
        assert_eq!((segments[0].sequence, &segments[0].key), (7, &Key::None));
        assert_eq!(
            segments[1].key,
            Key::Aes128 { uri: "https://cdn.example/show/ep1/key.bin".into(), iv: Some([0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]) }
        );
        assert_eq!(segments[2].key, Key::Aes128 { uri: "https://k.example/k2".into(), iv: None });
        assert_eq!((segments[3].sequence, &segments[3].key), (10, &Key::None));
        assert_eq!(segments[3].url, "https://cdn.example/show/ep1/seg10.ts");
    }

    #[test]
    fn refuses_what_it_cannot_do() {
        assert!(parse_playlist("<html>", &base()).is_err());
        assert!(parse_playlist("#EXTM3U\n", &base()).is_err());
        let drm = "#EXTM3U\n#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"skd://x\"\n#EXTINF:4,\na.ts\n";
        assert!(parse_playlist(drm, &base()).unwrap_err().contains("protected"));
        let live = "#EXTM3U\n#EXTINF:4,\na.ts\n";
        assert!(matches!(parse_playlist(live, &base()).unwrap(), Playlist::Media { ended: false, .. }));
    }

    #[test]
    fn fmp4_init_segment() {
        let text = "#EXTM3U\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:4,\na.m4s\n#EXT-X-ENDLIST\n";
        let Playlist::Media { init, .. } = parse_playlist(text, &base()).unwrap() else { panic!() };
        assert_eq!(init.as_deref(), Some("https://cdn.example/show/ep1/init.mp4"));
    }

    #[test]
    fn detection_and_naming() {
        assert!(looks_like_hls("https://h/a/b.M3U8?x=1", None));
        assert!(looks_like_hls("https://h/play", Some("application/vnd.apple.mpegurl")));
        assert!(!looks_like_hls("https://h/a.mp4", Some("video/mp4")));
        assert_eq!(stream_name("https://cdn.example/show/ep1/master.m3u8"), "ep1");
        assert_eq!(stream_name("https://cdn.example/trailer.m3u8"), "trailer");
        assert_eq!(stream_name("https://cdn.example/index.m3u8"), "video");
    }
}
