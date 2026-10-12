//! Link grabber: fetch a page and list the files it links to.

use std::collections::HashSet;

use reqwest::Client;
use url::Url;

use crate::error::{Error, Result};
use crate::filename;
use crate::http::{self, RequestSpec};

/// Pages larger than this are not HTML worth scanning.
const MAX_PAGE: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub struct GrabbedLink {
    pub url: String,
    pub file_name: String,
    pub extension: String,
}

/// Every `href`/`src` target in `html`, resolved against `base`, http(s) only,
/// in document order without duplicates.
pub fn extract_links(html: &str, base: &Url) -> Vec<String> {
    let bytes = html.as_bytes();
    let lower = html.to_ascii_lowercase();
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    let mut at = 0;
    while at < lower.len() {
        // Next attribute that can hold a link.
        let next = ["href", "src"].iter().filter_map(|attr| lower[at..].find(attr).map(|p| (at + p, attr.len()))).min();
        let Some((pos, len)) = next else { break };
        at = pos + len;
        // Must be a whole attribute name: `data-src` and `hrefs` do not count.
        let boundary_ok = pos == 0 || matches!(bytes[pos - 1], b' ' | b'\t' | b'\n' | b'\r' | b'"' | b'\'' | b'/');
        if !boundary_ok {
            continue;
        }
        let mut i = at;
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() || bytes[i] != b'=' {
            continue;
        }
        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let (start, end) = match bytes[i] {
            quote @ (b'"' | b'\'') => {
                let start = i + 1;
                match html[start..].find(quote as char) {
                    Some(n) => (start, start + n),
                    None => break,
                }
            }
            _ => {
                let end = html[i..].find(|c: char| c.is_whitespace() || c == '>').map_or(html.len(), |n| i + n);
                (i, end)
            }
        };
        at = end;
        let raw = html[start..end].trim().replace("&amp;", "&");
        if raw.is_empty() || raw.starts_with('#') {
            continue;
        }
        let Ok(mut resolved) = base.join(&raw) else { continue };
        if !matches!(resolved.scheme(), "http" | "https") {
            continue;
        }
        resolved.set_fragment(None);
        let url = resolved.to_string();
        if seen.insert(url.clone()) {
            out.push(url);
        }
    }
    out
}

/// Keep the links whose file extension is in `extensions` (empty = any file with an extension).
pub fn filter_links(links: Vec<String>, extensions: &[String]) -> Vec<GrabbedLink> {
    links
        .into_iter()
        .filter_map(|url| {
            let file_name = filename::from_url(&url)?;
            let extension = filename::extension(&file_name)?;
            let wanted = extensions.is_empty() || extensions.iter().any(|e| e.eq_ignore_ascii_case(&extension));
            wanted.then_some(GrabbedLink { url, file_name, extension })
        })
        .collect()
}

/// Download the page at `spec.url` and return the files it links to.
pub async fn grab(client: &Client, spec: &RequestSpec, extensions: &[String]) -> Result<Vec<GrabbedLink>> {
    let mut resp = http::request(client, spec, &spec.url).send().await?;
    if !resp.status().is_success() {
        return Err(Error::Status(resp.status().as_u16()));
    }
    let base = resp.url().clone();
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        body.extend_from_slice(&chunk);
        if body.len() > MAX_PAGE {
            return Err(Error::Other("that address is a large file, not a web page".into()));
        }
    }
    let html = String::from_utf8_lossy(&body);
    Ok(filter_links(extract_links(&html, &base), extensions))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_and_resolves() {
        let base = Url::parse("https://site.example/downloads/index.html").unwrap();
        let html = r##"
            <a href="a.zip">A</a>
            <a HREF='/abs/b.pdf'>B</a>
            <a href = "https://cdn.example/c.mp4?token=1&amp;x=2">C</a>
            <img src=pic.png>
            <a href="#top">top</a> <a href="mailto:x@y.z">mail</a> <a href="javascript:void(0)">js</a>
            <a href="a.zip#again">dup</a>
            <div data-src="not-a-link.zip"></div>
            <video src="../media/clip.mkv"></video>
        "##;
        assert_eq!(
            extract_links(html, &base),
            vec![
                "https://site.example/downloads/a.zip",
                "https://site.example/abs/b.pdf",
                "https://cdn.example/c.mp4?token=1&x=2",
                "https://site.example/downloads/pic.png",
                "https://site.example/media/clip.mkv",
            ]
        );
    }

    #[test]
    fn filters_by_extension() {
        let links = vec![
            "https://h/a.zip".to_string(),
            "https://h/page".to_string(),
            "https://h/b.PDF".to_string(),
            "https://h/c.png".to_string(),
        ];
        let got = filter_links(links.clone(), &["zip".into(), "pdf".into()]);
        assert_eq!(got.iter().map(|l| l.file_name.as_str()).collect::<Vec<_>>(), vec!["a.zip", "b.PDF"]);
        assert_eq!(got[1].extension, "pdf");
        assert_eq!(filter_links(links, &[]).len(), 3);
    }

    #[test]
    fn survives_malformed_html() {
        let base = Url::parse("https://h/").unwrap();
        assert!(extract_links("<a href=", &base).is_empty());
        assert!(extract_links("<a href=\"unterminated", &base).is_empty());
        assert_eq!(extract_links("href=x.zip", &base), vec!["https://h/x.zip"]);
        assert!(extract_links("héllo href ünïcode src", &base).is_empty());
    }
}
