//! HLS stream downloads end to end, against a small playlist server:
//! master → best rendition → clear and AES-128 segments → one joined file.

mod common;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use aes::cipher::{block_padding::Pkcs7, BlockModeEncrypt, KeyIvInit};
use common::Fixture;
use parking_lot::Mutex;
use rdm_core::http::RequestSpec;
use rdm_core::testserver::fill;
use rdm_core::{NewDownload, Status};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const SEGMENT: usize = 96 * 1024;
const KEY: [u8; 16] = *b"0123456789abcdef";
const EXPLICIT_IV: [u8; 16] = [0xA5; 16];

struct Stream {
    segments: usize,
    /// Segments from this index on are AES-128 encrypted.
    encrypted_from: usize,
    /// Pause before each segment response, to make the download last.
    delay: Duration,
    /// Leave out `#EXT-X-ENDLIST` (a live stream).
    live: bool,
    hits: Mutex<HashMap<String, u32>>,
}

impl Stream {
    fn clear_segment(&self, i: usize) -> Vec<u8> {
        // Odd sizes on purpose: nothing may assume equal or block-aligned segments.
        let mut data = vec![0u8; SEGMENT + i * 37];
        fill(7, (i * SEGMENT * 2) as u64, &mut data);
        data
    }

    fn expected_file(&self) -> Vec<u8> {
        (0..self.segments).flat_map(|i| self.clear_segment(i)).collect()
    }

    fn iv(&self, i: usize) -> [u8; 16] {
        // Even segments name their IV; odd ones rely on the sequence number (starting at 100).
        if i % 2 == 0 {
            EXPLICIT_IV
        } else {
            let mut iv = [0u8; 16];
            iv[8..].copy_from_slice(&(100 + i as u64).to_be_bytes());
            iv
        }
    }

    fn respond(&self, path: &str) -> Option<(Vec<u8>, &'static str)> {
        *self.hits.lock().entry(path.to_string()).or_default() += 1;
        match path {
            "/show/master.m3u8" => Some((
                b"#EXTM3U\n\
                  #EXT-X-STREAM-INF:BANDWIDTH=400000,RESOLUTION=640x360\nlow/index.m3u8\n\
                  #EXT-X-STREAM-INF:BANDWIDTH=2500000,RESOLUTION=1280x720,CODECS=\"avc1.4d401f,mp4a.40.2\"\nhi/index.m3u8\n"
                    .to_vec(),
                "application/vnd.apple.mpegurl",
            )),
            "/show/hi/index.m3u8" => {
                let mut text = String::from("#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:100\n");
                for i in 0..self.segments {
                    if i == self.encrypted_from || (i > self.encrypted_from) {
                        if i % 2 == 0 {
                            text.push_str(&format!("#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\",IV=0x{}\n", hex::encode(EXPLICIT_IV)));
                        } else {
                            text.push_str("#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\"\n");
                        }
                    }
                    text.push_str(&format!("#EXTINF:4.0,\nseg{i}.ts\n"));
                }
                if !self.live {
                    text.push_str("#EXT-X-ENDLIST\n");
                }
                Some((text.into_bytes(), "application/vnd.apple.mpegurl"))
            }
            "/show/hi/key.bin" => Some((KEY.to_vec(), "application/octet-stream")),
            p => {
                let i: usize = p.strip_prefix("/show/hi/seg")?.strip_suffix(".ts")?.parse().ok()?;
                if i >= self.segments {
                    return None;
                }
                let clear = self.clear_segment(i);
                let body = if i >= self.encrypted_from {
                    cbc::Encryptor::<aes::Aes128>::new(&KEY.into(), &self.iv(i).into()).encrypt_padded_vec::<Pkcs7>(&clear)
                } else {
                    clear
                };
                Some((body, "video/mp2t"))
            }
        }
    }

    fn hits(&self, path: &str) -> u32 {
        self.hits.lock().get(path).copied().unwrap_or(0)
    }
}

async fn serve(stream: Arc<Stream>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else { break };
            let stream = stream.clone();
            tokio::spawn(async move {
                let mut carry = Vec::new();
                loop {
                    let head_end = loop {
                        if let Some(p) = carry.windows(4).position(|w| w == b"\r\n\r\n") {
                            break p;
                        }
                        let mut buf = [0u8; 4096];
                        match sock.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => carry.extend_from_slice(&buf[..n]),
                        }
                    };
                    let head = String::from_utf8_lossy(&carry[..head_end]).into_owned();
                    carry.drain(..head_end + 4);
                    let path = head.split_whitespace().nth(1).unwrap_or("/").to_string();
                    let reply = stream.respond(&path);
                    if path.contains("/seg") && !stream.delay.is_zero() {
                        tokio::time::sleep(stream.delay).await;
                    }
                    let out = match reply {
                        Some((body, ctype)) => {
                            let mut out = format!("HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\n\r\n", body.len()).into_bytes();
                            out.extend_from_slice(&body);
                            out
                        }
                        None => b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_vec(),
                    };
                    if sock.write_all(&out).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    format!("http://{addr}/show/master.m3u8")
}

fn stream(segments: usize, encrypted_from: usize, delay_ms: u64, live: bool) -> Arc<Stream> {
    Arc::new(Stream { segments, encrypted_from, delay: Duration::from_millis(delay_ms), live, hits: Mutex::new(HashMap::new()) })
}

#[test]
fn downloads_best_rendition_and_decrypts() {
    let fx = Fixture::new("hls");
    let s = stream(14, 5, 0, false);
    let url = fx.manager.runtime().block_on(serve(s.clone()));

    let id = fx.manager.add(NewDownload::new(RequestSpec::new(url)));
    fx.wait_status(id, Status::Complete, 30);

    let v = fx.manager.view(id).unwrap();
    // Named after the show, not after "master.m3u8", and filed as a video.
    assert_eq!(v.d.file_name, "show.ts");
    assert_eq!(v.d.category, "Video");
    assert_eq!(v.d.description, "Video stream 1280x720");
    let got = std::fs::read(v.d.path()).unwrap();
    let want = s.expected_file();
    assert_eq!(got.len(), want.len());
    assert!(got == want, "joined file differs from the clear segments");
    assert_eq!((v.d.size, v.d.downloaded), (Some(want.len() as u64), want.len() as u64));

    assert_eq!(s.hits("/show/hi/key.bin"), 1, "the key is fetched once");
    assert_eq!(s.hits("/show/low/index.m3u8"), 0, "the low rendition must not be touched");
    assert!((0..14).all(|i| s.hits(&format!("/show/hi/seg{i}.ts")) == 1));
    assert!(!rdm_core::filename::part_path(&v.d.path()).exists());
    assert!(!rdm_core::filename::state_path(&v.d.path()).exists());
}

#[test]
fn stream_download_pauses_and_resumes() {
    let fx = Fixture::new("hls-resume");
    let s = stream(48, 20, 120, false);
    let url = fx.manager.runtime().block_on(serve(s.clone()));

    let id = fx.manager.add(NewDownload::new(RequestSpec::new(url)));
    fx.wait_bytes(id, 8 * SEGMENT as u64, 30);
    fx.manager.pause(id);
    fx.wait_status(id, Status::Paused, 10);
    let kept = fx.manager.view(id).unwrap().d.downloaded;
    assert!(kept >= 4 * SEGMENT as u64, "only {kept} bytes kept");
    assert_eq!(std::fs::metadata(rdm_core::filename::part_path(&fx.manager.view(id).unwrap().d.path())).unwrap().len(), kept);

    fx.manager.start(id);
    fx.wait_status(id, Status::Complete, 40);
    let got = std::fs::read(fx.manager.view(id).unwrap().d.path()).unwrap();
    assert!(got == s.expected_file(), "resumed stream is not byte-identical");
    // What was already on disk is not fetched again.
    assert_eq!(s.hits("/show/hi/seg0.ts"), 1);
    assert_eq!(s.hits("/show/hi/seg1.ts"), 1);
}

#[test]
fn live_streams_are_refused_with_a_reason() {
    let fx = Fixture::new("hls-live");
    let s = stream(4, 99, 0, true);
    let url = fx.manager.runtime().block_on(serve(s));
    let id = fx.manager.add(NewDownload::new(RequestSpec::new(url)));
    fx.wait_status(id, Status::Failed, 15);
    assert_eq!(fx.manager.view(id).unwrap().d.error.as_deref(), Some("live streams cannot be downloaded"));
}
