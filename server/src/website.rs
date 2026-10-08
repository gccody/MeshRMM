//! The website: the pages and assets `npm run build` writes to
//! `dashboard/dist`, built into the binary.
//!
//! Every page is prerendered: `/` is `index.html`, `/<page>` is
//! `<page>/index.html`, and any other path gets `404.html` with a 404 status.
//! The browser then loads the account and data from `/v1`. Files under
//! `assets/` have content hashes in their names, so browsers keep them for a
//! year; everything else is revalidated with its ETag.
use std::{collections::HashMap, sync::Arc};

use axum::{
    body::Body,
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use rust_embed::Embed;
use sha2::{Digest, Sha256};

/// A server built before the website was gets an empty one, and says so
/// at startup. Debug builds read the files from disk when the server starts.
#[derive(Embed)]
// Relative to this crate.
#[folder = "../dashboard/dist"]
#[allow_missing = true]
struct Build;

const IMMUTABLE: &str = "public, max-age=31536000, immutable";
const REVALIDATE: &str = "no-cache";
const NOT_FOUND_PAGE: &str = "404.html";

#[derive(Debug, Clone, Default)]
pub struct Website {
    files: Arc<HashMap<String, File>>,
}

#[derive(Debug)]
struct File {
    bytes: Bytes,
    content_type: HeaderValue,
    etag: HeaderValue,
    cache_control: HeaderValue,
}

impl Website {
    /// The website built into this binary.
    pub fn embedded() -> Self {
        Self::from_files(Build::iter().filter_map(|path| {
            let file = Build::get(&path)?;
            Some((path.into_owned(), file.data.into_owned()))
        }))
    }

    /// A website of `(path, contents)` pairs, with paths relative to its root.
    pub fn from_files(files: impl IntoIterator<Item = (String, Vec<u8>)>) -> Self {
        let files = files
            .into_iter()
            .map(|(path, bytes)| {
                let file = File {
                    content_type: HeaderValue::from_static(content_type(&path)),
                    etag: etag(&bytes),
                    cache_control: HeaderValue::from_static(if path.starts_with("assets/") {
                        IMMUTABLE
                    } else {
                        REVALIDATE
                    }),
                    bytes: Bytes::from(bytes),
                };
                (path, file)
            })
            .collect();
        Self {
            files: Arc::new(files),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// The response to a request for `path` outside the API, or `None` when
    /// the website has no page for it and isn't built.
    pub fn respond(&self, method: &Method, path: &str, headers: &HeaderMap) -> Option<Response> {
        let (file, status) = match self.find(path) {
            Some(file) => (file, StatusCode::OK),
            None => (self.files.get(NOT_FOUND_PAGE)?, StatusCode::NOT_FOUND),
        };
        if method != Method::GET && method != Method::HEAD {
            return Some(
                (
                    StatusCode::METHOD_NOT_ALLOWED,
                    [(header::ALLOW, HeaderValue::from_static("GET, HEAD"))],
                )
                    .into_response(),
            );
        }
        let unchanged = status == StatusCode::OK
            && headers
                .get(header::IF_NONE_MATCH)
                .is_some_and(|tags| etag_matches(tags, &file.etag));
        let mut response = if unchanged {
            StatusCode::NOT_MODIFIED.into_response()
        } else {
            (status, Body::from(file.bytes.clone())).into_response()
        };
        let response_headers = response.headers_mut();
        if !unchanged {
            response_headers.insert(header::CONTENT_TYPE, file.content_type.clone());
        }
        response_headers.insert(header::ETAG, file.etag.clone());
        response_headers.insert(header::CACHE_CONTROL, file.cache_control.clone());
        Some(response)
    }

    /// A file by its own path, or a page by its route.
    fn find(&self, path: &str) -> Option<&File> {
        let relative = path.trim_start_matches('/');
        if relative.is_empty() {
            return self.files.get("index.html");
        }
        if let Some(file) = self.files.get(relative)
            && relative != NOT_FOUND_PAGE
        {
            return Some(file);
        }
        let route = relative.trim_end_matches('/');
        self.files.get(&format!("{route}/index.html"))
    }
}

fn content_type(path: &str) -> &'static str {
    match path.rsplit_once('.').map(|(_, extension)| extension) {
        Some("html") => "text/html; charset=utf-8",
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json" | "map") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        Some("woff2") => "font/woff2",
        Some("woff") => "font/woff",
        Some("txt" | "sh") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

fn etag(bytes: &[u8]) -> HeaderValue {
    let digest = Sha256::digest(bytes);
    let hex: String = digest[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    HeaderValue::from_str(&format!("\"{hex}\"")).expect("a quoted hex string is a header value")
}

/// Whether an `If-None-Match` list names `etag`, ignoring weak markers.
fn etag_matches(header: &HeaderValue, etag: &HeaderValue) -> bool {
    let Ok(tags) = header.to_str() else {
        return false;
    };
    let wanted = etag.to_str().unwrap_or_default();
    tags.split(',')
        .map(|tag| tag.trim().trim_start_matches("W/"))
        .any(|tag| tag == "*" || tag == wanted)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn website() -> Website {
        Website::from_files([
            ("index.html".to_owned(), b"<p>devices</p>".to_vec()),
            ("toolbox/index.html".to_owned(), b"<p>toolbox</p>".to_vec()),
            ("404.html".to_owned(), b"<p>missing</p>".to_vec()),
            ("assets/index-abc.js".to_owned(), b"console.log(1)".to_vec()),
            ("favicon.svg".to_owned(), b"<svg/>".to_vec()),
        ])
    }

    fn found(website: &Website, path: &str) -> Option<String> {
        website
            .find(path)
            .map(|file| String::from_utf8(file.bytes.to_vec()).unwrap())
    }

    /// A wrong folder path builds an empty website without complaint, so
    /// a built website must be found.
    #[test]
    fn a_built_website_is_embedded() {
        let built =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../dashboard/dist/index.html");
        if built.exists() {
            let website = Website::embedded();
            assert!(website.find("/").is_some());
            assert!(website.find("/login").is_some());
            assert!(website.files.contains_key(NOT_FOUND_PAGE));
        }
    }

    #[test]
    fn routes_map_to_their_pages_and_files_to_themselves() {
        let website = website();
        assert_eq!(found(&website, "/").as_deref(), Some("<p>devices</p>"));
        assert_eq!(
            found(&website, "/toolbox").as_deref(),
            Some("<p>toolbox</p>")
        );
        assert_eq!(
            found(&website, "/toolbox/").as_deref(),
            Some("<p>toolbox</p>")
        );
        assert_eq!(found(&website, "/favicon.svg").as_deref(), Some("<svg/>"));
        assert_eq!(found(&website, "/users"), None);
        assert_eq!(found(&website, "/404.html"), None);
        assert_eq!(found(&website, "/../index.html"), None);
    }

    #[test]
    fn hashed_assets_are_immutable_and_pages_revalidate() {
        let website = website();
        assert_eq!(
            website.files["assets/index-abc.js"].cache_control,
            IMMUTABLE
        );
        assert_eq!(website.files["index.html"].cache_control, REVALIDATE);
        assert_eq!(website.files["favicon.svg"].cache_control, REVALIDATE);
        assert_eq!(
            website.files["assets/index-abc.js"].content_type,
            "text/javascript; charset=utf-8"
        );
    }

    #[test]
    fn if_none_match_lists_and_weak_tags_match() {
        let etag = HeaderValue::from_static("\"abc\"");
        for header in ["\"abc\"", "W/\"abc\"", "\"x\", \"abc\"", "*"] {
            assert!(
                etag_matches(&HeaderValue::from_static(header), &etag),
                "{header}"
            );
        }
        assert!(!etag_matches(&HeaderValue::from_static("\"abd\""), &etag));
    }
}
