use std::collections::HashSet;
use std::convert::Infallible;
use std::ffi::OsStr;
use std::net::IpAddr;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use bytes::Bytes;
use futures_util::TryStreamExt;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Empty, Full, StreamBody};
use hyper::body::Frame;
use hyper::{Method, Request, Response, StatusCode};
use tokio::fs::File;
use tokio_util::io::ReaderStream;
use tracing::{debug, info, warn};

use governor::clock::Clock;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;

use crate::config::{normalize_prefix, BasicAuthConfig, Config, LocationConfig, SearchMode};
use crate::ratelimit::KeyedLimiter;

pub type ResponseBody = BoxBody<Bytes, std::io::Error>;

type SearchResult = (PathBuf, File, u64, SystemTime);

struct SearchRoot {
    path: PathBuf,
    /// `None` = allow all file types; `Some(set)` = only listed extensions.
    extensions: Option<HashSet<String>>,
}

impl SearchRoot {
    fn accepts(&self, ext: &str) -> bool {
        match &self.extensions {
            None => true,
            Some(set) => set.contains(&ext.to_ascii_lowercase()),
        }
    }
}

struct Location {
    prefix: String,
    roots: Vec<SearchRoot>,
    search_mode: SearchMode,
    max_file_size: u64,
}

impl Location {
    fn from_config(loc: &LocationConfig, server_max_file_size: u64) -> Self {
        let prefix = normalize_prefix(&loc.prefix);

        let max_file_size = loc
            .max_file_size
            .map(|bs| bs.as_u64())
            .unwrap_or(server_max_file_size);

        let roots: Vec<SearchRoot> = loc
            .paths
            .iter()
            .filter_map(|entry| match entry.root.canonicalize() {
                Ok(canonical) if canonical.is_dir() => {
                    let ext_set = entry.extension_set();
                    info!(
                        prefix = %prefix,
                        path = %canonical.display(),
                        extensions = %ext_set.as_ref().map_or("*".into(), |s| {
                            let mut v: Vec<_> = s.iter().map(String::as_str).collect();
                            v.sort_unstable();
                            v.join(", ")
                        }),
                        "search path registered"
                    );
                    Some(SearchRoot { path: canonical, extensions: ext_set })
                }
                Ok(_) => {
                    warn!(path = %entry.root.display(), "not a directory, skipping");
                    None
                }
                Err(e) => {
                    warn!(path = %entry.root.display(), error = %e, "cannot resolve path, skipping");
                    None
                }
            })
            .collect();

        if roots.is_empty() {
            warn!(prefix = %prefix, "no valid search paths for location");
        }

        info!(
            prefix = %prefix, mode = ?loc.mode, roots = roots.len(),
            max_file_size = %crate::config::ByteSize(max_file_size),
            "location configured"
        );

        Self {
            prefix,
            roots,
            search_mode: loc.mode,
            max_file_size,
        }
    }

    /// Search across this location's roots using its configured search mode.
    async fn search(&self, request_path: &str) -> Option<SearchResult> {
        match self.search_mode {
            SearchMode::Sequential => self.search_sequential(request_path).await,
            SearchMode::Concurrent => self.search_concurrent(request_path).await,
            SearchMode::LatestModified => self.search_latest(request_path).await,
        }
    }

    async fn search_sequential(&self, request_path: &str) -> Option<SearchResult> {
        let relative = sanitize_path(request_path)?;

        let ext = relative
            .extension()
            .and_then(OsStr::to_str)
            .unwrap_or("");

        for root in &self.roots {
            match try_root(root, &relative, ext, self.max_file_size, request_path).await {
                Ok(Some(result)) => return Some(result),
                Ok(None) => continue,
                Err(()) => return None,
            }
        }

        None
    }

    async fn search_concurrent(&self, request_path: &str) -> Option<SearchResult> {
        let relative = sanitize_path(request_path)?;

        let ext = relative
            .extension()
            .and_then(OsStr::to_str)
            .unwrap_or("")
            .to_owned();

        let mut handles = Vec::new();

        for root in &self.roots {
            if !root.accepts(&ext) {
                debug!(
                    request_path, root = %root.path.display(), ext,
                    "skipped (extension not allowed)"
                );
                continue;
            }

            let root_path = root.path.clone();
            let candidate = root.path.join(&relative);
            let max_file_size = self.max_file_size;
            let req_path = request_path.to_owned();

            handles.push(tokio::spawn(
                probe_root(root_path, candidate, max_file_size, req_path),
            ));
        }

        race_handles(handles).await
    }

    async fn search_latest(&self, request_path: &str) -> Option<SearchResult> {
        let relative = sanitize_path(request_path)?;

        let ext = relative
            .extension()
            .and_then(OsStr::to_str)
            .unwrap_or("");

        let mut best: Option<SearchResult> = None;

        for root in &self.roots {
            match try_root(root, &relative, ext, self.max_file_size, request_path).await {
                Ok(Some(found)) => {
                    let dominated = best.as_ref().is_none_or(|b| found.3 > b.3);
                    if dominated {
                        if let Some(ref prev) = best {
                            debug!(
                                request_path,
                                superseded = %prev.0.display(),
                                by = %found.0.display(),
                                "newer file found, replacing previous candidate"
                            );
                        }
                        best = Some(found);
                    }
                }
                Ok(None) => continue,
                Err(()) => return None,
            }
        }

        best
    }
}

pub struct FileSearcher {
    locations: Vec<Location>,
    max_body_size: u64,
    stream_buffer_size: usize,
}

impl FileSearcher {
    pub fn new(config: &Config) -> Self {
        let server_max_file_size = config.server.max_file_size.as_u64();

        let mut locations: Vec<Location> = config
            .locations
            .iter()
            .map(|loc| Location::from_config(loc, server_max_file_size))
            .collect();

        // Sort by prefix length descending (longest match first).
        locations.sort_by(|a, b| b.prefix.len().cmp(&a.prefix.len()));

        Self {
            locations,
            max_body_size: config.server.max_body_size.as_u64(),
            stream_buffer_size: config.server.stream_buffer_size.as_usize(),
        }
    }

    /// Match a request path to a location, returning the location and the
    /// remaining path after stripping the prefix.
    fn match_location<'a>(&'a self, path: &'a str) -> Option<(&'a Location, &'a str)> {
        for loc in &self.locations {
            if loc.prefix == "/" {
                return Some((loc, path));
            }
            if path == loc.prefix {
                return Some((loc, "/"));
            }
            if let Some(rest) = path.strip_prefix(&loc.prefix)
                && rest.starts_with('/')
            {
                return Some((loc, rest));
            }
        }
        None
    }

    async fn search(&self, request_path: &str) -> Option<SearchResult> {
        let (location, stripped_path) = self.match_location(request_path)?;
        location.search(stripped_path).await
    }
}

// ---------------------------------------------------------------------------
// Shared search helpers
// ---------------------------------------------------------------------------

/// Core file probe: canonicalize, open, check metadata and size.
///
/// Returns:
/// - `Ok(Some(...))` — file found
/// - `Ok(None)` — not found or not a regular file
/// - `Err(())` — path traversal detected (canonical path escaped root)
async fn probe_candidate(
    root_path: &Path,
    candidate: PathBuf,
    max_file_size: u64,
    request_path: &str,
) -> Result<Option<SearchResult>, ()> {
    let canonical = match tokio::fs::canonicalize(&candidate).await {
        Ok(c) if c.starts_with(root_path) => c,
        Ok(_) => {
            warn!(request_path, "path traversal blocked");
            return Err(());
        }
        Err(_) => return Ok(None),
    };

    let file = match File::open(&canonical).await {
        Ok(f) => f,
        Err(_) => return Ok(None),
    };
    let meta = match file.metadata().await {
        Ok(m) if m.is_file() => m,
        _ => return Ok(None),
    };

    if max_file_size > 0 && meta.len() > max_file_size {
        debug!(
            request_path, resolved = %canonical.display(),
            size = meta.len(), limit = max_file_size,
            "skipped (file too large)"
        );
        return Ok(None);
    }

    let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);

    Ok(Some((canonical, file, meta.len(), modified)))
}

/// Attempt to find the file under a single search root (with extension filter).
async fn try_root(
    root: &SearchRoot,
    relative: &Path,
    ext: &str,
    max_file_size: u64,
    request_path: &str,
) -> Result<Option<SearchResult>, ()> {
    if !root.accepts(ext) {
        debug!(
            request_path, root = %root.path.display(), ext,
            "skipped (extension not allowed)"
        );
        return Ok(None);
    }
    probe_candidate(&root.path, root.path.join(relative), max_file_size, request_path).await
}

/// Wait for the first `JoinHandle` that returns `Some`, then abort all
/// remaining handles to free resources.
async fn race_handles(
    mut handles: Vec<tokio::task::JoinHandle<Option<SearchResult>>>,
) -> Option<SearchResult> {
    let mut result = None;

    while !handles.is_empty() {
        let (finished, _index, remaining) = futures_util::future::select_all(handles).await;

        match finished {
            Ok(Some(found)) => {
                result = Some(found);
                for h in remaining {
                    h.abort();
                }
                break;
            }
            _ => {
                handles = remaining;
            }
        }
    }

    result
}

/// Spawnable probe for a single root — owns all data for `tokio::spawn`.
/// Extension filtering must be done before calling this.
async fn probe_root(
    root_path: PathBuf,
    candidate: PathBuf,
    max_file_size: u64,
    request_path: String,
) -> Option<SearchResult> {
    probe_candidate(&root_path, candidate, max_file_size, &request_path)
        .await
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Path sanitization
// ---------------------------------------------------------------------------

/// Convert a raw URL path into a safe relative filesystem path.
///
/// Rejects: null bytes, `..`, `.`, dotfiles, and any non-normal component.
fn sanitize_path(raw: &str) -> Option<PathBuf> {
    let decoded = percent_encoding::percent_decode_str(raw)
        .decode_utf8()
        .ok()?;

    // Null bytes could truncate the path at the OS level.
    if decoded.contains('\0') {
        return None;
    }

    let mut clean = PathBuf::new();
    for component in Path::new(decoded.as_ref()).components() {
        match component {
            Component::Normal(seg) => {
                // Block hidden files / directories (e.g. .env, .git).
                if seg.as_encoded_bytes().first() == Some(&b'.') {
                    return None;
                }
                clean.push(seg);
            }
            Component::RootDir => {}
            _ => return None, // reject "..", prefix, etc.
        }
    }

    if clean.as_os_str().is_empty() {
        return None;
    }
    Some(clean)
}

// ---------------------------------------------------------------------------
// HTTP handler
// ---------------------------------------------------------------------------

/// Validate Basic Auth credentials from the Authorization header.
///
/// Uses constant-time comparison (XOR-fold) to prevent timing attacks.
fn check_basic_auth(headers: &hyper::HeaderMap, auth: &BasicAuthConfig) -> bool {
    let Some(header_val) = headers.get(hyper::header::AUTHORIZATION) else {
        return false;
    };
    let Ok(val) = header_val.to_str() else {
        return false;
    };
    let Some(encoded) = val.strip_prefix("Basic ") else {
        return false;
    };
    let Ok(decoded_bytes) = BASE64_STANDARD.decode(encoded.trim()) else {
        return false;
    };
    let Ok(decoded) = std::str::from_utf8(&decoded_bytes) else {
        return false;
    };
    let Some((user, pass)) = decoded.split_once(':') else {
        return false;
    };

    // Constant-time comparison: always compare all bytes to prevent timing attacks.
    let user_bytes = user.as_bytes();
    let pass_bytes = pass.as_bytes();
    let expected_user = auth.username.as_bytes();
    let expected_pass = auth.password.as_bytes();

    let user_len_match = user_bytes.len() == expected_user.len();
    let pass_len_match = pass_bytes.len() == expected_pass.len();

    let mut acc: u8 = 0;
    for (a, b) in user_bytes.iter().zip(expected_user.iter()) {
        acc |= a ^ b;
    }
    for (a, b) in pass_bytes.iter().zip(expected_pass.iter()) {
        acc |= a ^ b;
    }

    user_len_match && pass_len_match && acc == 0
}

pub async fn handle_request(
    req: Request<impl hyper::body::Body + Send + 'static>,
    searcher: Arc<FileSearcher>,
    limiter: Option<Arc<KeyedLimiter>>,
    auth: Option<Arc<BasicAuthConfig>>,
    client_ip: IpAddr,
) -> Result<Response<ResponseBody>, Infallible> {
    let path = req.uri().path();

    // Health check endpoints — exempt from rate limiting, priority over file routing.
    if path == "/health" || path == "/ready" {
        if req.method() != Method::GET && req.method() != Method::HEAD {
            return Ok(text_response(
                StatusCode::METHOD_NOT_ALLOWED,
                "Method Not Allowed",
            ));
        }
        let msg = if path == "/health" { "OK" } else { "READY" };
        let body = if req.method() == Method::HEAD {
            empty_body()
        } else {
            full_body(msg)
        };
        return Ok(Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "text/plain; charset=utf-8")
            .header("Content-Length", msg.len())
            .body(body)
            .unwrap());
    }

    // Basic Auth (checked after health endpoints, before rate limiting).
    if let Some(ref auth_cfg) = auth {
        if !check_basic_auth(req.headers(), auth_cfg) {
            debug!(status = 401, path, "request handled (unauthorized)");
            return Ok(Response::builder()
                .status(StatusCode::UNAUTHORIZED)
                .header(
                    "WWW-Authenticate",
                    format!("Basic realm=\"{}\"", auth_cfg.realm),
                )
                .header("Content-Type", "text/plain; charset=utf-8")
                .header("X-Content-Type-Options", "nosniff")
                .body(full_body("Unauthorized"))
                .unwrap());
        }
    }

    // Per-IP rate limiting (checked before anything else except health).
    if let Some(ref lim) = limiter
        && let Err(not_until) = lim.check_key(&client_ip)
    {
        let wait = not_until.wait_time_from(governor::clock::DefaultClock::default().now());
        let retry_after = wait.as_secs().max(1);
        debug!(
            status = 429, %client_ip, retry_after,
            "request handled (rate limited)"
        );
        return Ok(Response::builder()
            .status(StatusCode::TOO_MANY_REQUESTS)
            .header("Retry-After", retry_after)
            .header("Content-Type", "text/plain; charset=utf-8")
            .header("X-Content-Type-Options", "nosniff")
            .body(full_body("Too Many Requests"))
            .unwrap());
    }

    if req.method() != Method::GET && req.method() != Method::HEAD {
        debug!(status = 405, method = %req.method(), "request handled");
        return Ok(text_response(
            StatusCode::METHOD_NOT_ALLOWED,
            "Method Not Allowed",
        ));
    }

    // Reject requests with an oversized or malformed Content-Length.
    if let Some(cl) = req.headers().get(hyper::header::CONTENT_LENGTH) {
        let len: u64 = cl
            .to_str()
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(u64::MAX); // treat unparseable as oversized → 413
        if len > searcher.max_body_size {
            debug!(status = 413, path, "request handled");
            return Ok(text_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                "Payload Too Large",
            ));
        }
    }

    let is_head = req.method() == Method::HEAD;

    match searcher.search(path).await {
        Some((file_path, file, size, mtime)) => {
            let mime = mime_guess::from_path(&file_path).first_or_octet_stream();
            let etag = generate_etag(mtime, size);
            let last_modified = format_http_date(mtime);

            // 304 Not Modified check
            if is_not_modified(req.headers(), &etag, mtime) {
                debug!(status = 304, path, "request handled (not modified)");
                return Ok(Response::builder()
                    .status(StatusCode::NOT_MODIFIED)
                    .header("ETag", &etag)
                    .header("Last-Modified", &last_modified)
                    .body(empty_body())
                    .unwrap());
            }

            // Range request check
            if let Some(range_val) = req.headers().get(hyper::header::RANGE) {
                if let Ok(range_str) = range_val.to_str() {
                    if should_honor_range(req.headers(), &etag, mtime) {
                        if let Some(byte_range) = parse_range_header(range_str) {
                            match byte_range.resolve(size) {
                                Some((start, end)) => {
                                    let content_length = end - start + 1;
                                    let content_range =
                                        format!("bytes {start}-{end}/{size}");

                                    debug!(
                                        status = 206, path,
                                        resolved = %file_path.display(),
                                        range = %content_range,
                                        "request handled (partial content)"
                                    );

                                    let body = if is_head {
                                        empty_body()
                                    } else {
                                        use tokio::io::{AsyncReadExt, AsyncSeekExt};
                                        let mut file = file;
                                        if file
                                            .seek(std::io::SeekFrom::Start(start))
                                            .await
                                            .is_err()
                                        {
                                            return Ok(text_response(
                                                StatusCode::INTERNAL_SERVER_ERROR,
                                                "Internal Server Error",
                                            ));
                                        }
                                        let limited = file.take(content_length);
                                        let stream = ReaderStream::with_capacity(
                                            limited,
                                            searcher.stream_buffer_size,
                                        );
                                        StreamBody::new(stream.map_ok(Frame::data))
                                            .boxed()
                                    };

                                    return Ok(Response::builder()
                                        .status(StatusCode::PARTIAL_CONTENT)
                                        .header("Content-Type", mime.as_ref())
                                        .header("Content-Length", content_length)
                                        .header("Content-Range", content_range)
                                        .header("Accept-Ranges", "bytes")
                                        .header("ETag", &etag)
                                        .header("Last-Modified", &last_modified)
                                        .header("X-Content-Type-Options", "nosniff")
                                        .body(body)
                                        .unwrap());
                                }
                                None => {
                                    debug!(
                                        status = 416, path,
                                        "request handled (range not satisfiable)"
                                    );
                                    return Ok(Response::builder()
                                        .status(StatusCode::RANGE_NOT_SATISFIABLE)
                                        .header(
                                            "Content-Range",
                                            format!("bytes */{size}"),
                                        )
                                        .header("Content-Type", "text/plain; charset=utf-8")
                                        .header("X-Content-Type-Options", "nosniff")
                                        .body(full_body("Range Not Satisfiable"))
                                        .unwrap());
                                }
                            }
                        }
                    }
                    // If should_honor_range is false or parse failed, fall through to full 200
                }
            }

            // Full 200 response
            debug!(
                status = 200, path,
                resolved = %file_path.display(), size,
                "request handled"
            );

            let body = if is_head {
                empty_body()
            } else {
                stream_body(file, searcher.stream_buffer_size)
            };

            Ok(Response::builder()
                .status(StatusCode::OK)
                .header("Content-Type", mime.as_ref())
                .header("Content-Length", size)
                .header("Accept-Ranges", "bytes")
                .header("ETag", &etag)
                .header("Last-Modified", &last_modified)
                .header("X-Content-Type-Options", "nosniff")
                .body(body)
                .unwrap())
        }
        None => {
            debug!(status = 404, path, "request handled");
            Ok(text_response(StatusCode::NOT_FOUND, "Not Found"))
        }
    }
}

// ---------------------------------------------------------------------------
// Conditional request helpers (ETag / Last-Modified / 304)
// ---------------------------------------------------------------------------

fn format_http_date(time: SystemTime) -> String {
    httpdate::fmt_http_date(time)
}

fn generate_etag(mtime: SystemTime, size: u64) -> String {
    let secs = mtime
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("W/\"{secs}-{size}\"")
}

fn is_not_modified(headers: &hyper::HeaderMap, etag: &str, mtime: SystemTime) -> bool {
    // If-None-Match takes priority over If-Modified-Since (RFC 9110 §13.1.2)
    if let Some(inm) = headers.get(hyper::header::IF_NONE_MATCH) {
        if let Ok(val) = inm.to_str() {
            if val.trim() == "*" {
                return true;
            }
            return val.split(',').any(|tag| tag.trim() == etag);
        }
    }

    if let Some(ims) = headers.get(hyper::header::IF_MODIFIED_SINCE) {
        if let Ok(val) = ims.to_str() {
            if let Ok(since) = httpdate::parse_http_date(val) {
                let mtime_secs = mtime
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                let since_secs = since
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                return mtime_secs <= since_secs;
            }
        }
    }

    false
}

// ---------------------------------------------------------------------------
// Range request helpers (206 Partial Content)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ByteRange {
    FromTo(u64, u64),
    From(u64),
    Suffix(u64),
}

impl ByteRange {
    fn resolve(self, file_size: u64) -> Option<(u64, u64)> {
        if file_size == 0 {
            return None;
        }
        match self {
            ByteRange::FromTo(start, end) => {
                if start >= file_size || start > end {
                    return None;
                }
                Some((start, end.min(file_size - 1)))
            }
            ByteRange::From(start) => {
                if start >= file_size {
                    return None;
                }
                Some((start, file_size - 1))
            }
            ByteRange::Suffix(n) => {
                if n == 0 {
                    return None;
                }
                let start = file_size.saturating_sub(n);
                Some((start, file_size - 1))
            }
        }
    }
}

fn parse_range_header(value: &str) -> Option<ByteRange> {
    let value = value.strip_prefix("bytes=")?;
    // Multi-range not supported — fall back to full response
    if value.contains(',') {
        return None;
    }
    let value = value.trim();
    if let Some(suffix) = value.strip_prefix('-') {
        let n: u64 = suffix.trim().parse().ok()?;
        return Some(ByteRange::Suffix(n));
    }
    let (start_str, end_str) = value.split_once('-')?;
    let start: u64 = start_str.trim().parse().ok()?;
    if end_str.trim().is_empty() {
        return Some(ByteRange::From(start));
    }
    let end: u64 = end_str.trim().parse().ok()?;
    Some(ByteRange::FromTo(start, end))
}

fn should_honor_range(headers: &hyper::HeaderMap, etag: &str, mtime: SystemTime) -> bool {
    let Some(if_range) = headers.get(hyper::header::IF_RANGE) else {
        return true; // No If-Range → always honor range
    };
    let Ok(val) = if_range.to_str() else {
        return false;
    };
    let val = val.trim();
    // ETag comparison
    if val.starts_with('"') || val.starts_with("W/\"") {
        return val == etag;
    }
    // HTTP-date comparison
    if let Ok(date) = httpdate::parse_http_date(val) {
        let mtime_secs = mtime
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let date_secs = date
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        return mtime_secs == date_secs;
    }
    false
}

// ---------------------------------------------------------------------------
// Body helpers
// ---------------------------------------------------------------------------

fn empty_body() -> ResponseBody {
    Empty::<Bytes>::new()
        .map_err(|never| match never {})
        .boxed()
}

fn full_body(data: &'static str) -> ResponseBody {
    Full::new(Bytes::from(data))
        .map_err(|never| match never {})
        .boxed()
}

fn stream_body(file: File, buffer_size: usize) -> ResponseBody {
    let stream = ReaderStream::with_capacity(file, buffer_size);
    StreamBody::new(stream.map_ok(Frame::data)).boxed()
}

fn text_response(status: StatusCode, message: &'static str) -> Response<ResponseBody> {
    Response::builder()
        .status(status)
        .header("Content-Type", "text/plain; charset=utf-8")
        .header("X-Content-Type-Options", "nosniff")
        .body(full_body(message))
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{normalize_prefix, SearchMode};

    // -----------------------------------------------------------------------
    // sanitize_path — security-critical (10 tests)
    // -----------------------------------------------------------------------

    #[test]
    fn sanitize_normal_path() {
        let p = sanitize_path("/foo/bar.txt").unwrap();
        assert_eq!(p, PathBuf::from("foo/bar.txt"));
    }

    #[test]
    fn sanitize_nested_path() {
        let p = sanitize_path("/a/b/c/d.png").unwrap();
        assert_eq!(p, PathBuf::from("a/b/c/d.png"));
    }

    #[test]
    fn sanitize_single_file() {
        let p = sanitize_path("/readme.md").unwrap();
        assert_eq!(p, PathBuf::from("readme.md"));
    }

    #[test]
    fn sanitize_rejects_null_byte() {
        assert!(sanitize_path("/foo\0bar").is_none());
    }

    #[test]
    fn sanitize_rejects_dotdot() {
        assert!(sanitize_path("/foo/../etc/passwd").is_none());
    }

    #[test]
    fn sanitize_rejects_dotfile() {
        assert!(sanitize_path("/.env").is_none());
    }

    #[test]
    fn sanitize_rejects_hidden_dir() {
        assert!(sanitize_path("/.git/config").is_none());
    }

    #[test]
    fn sanitize_rejects_empty() {
        assert!(sanitize_path("/").is_none());
    }

    #[test]
    fn sanitize_url_encoded_space() {
        let p = sanitize_path("/foo%20bar.txt").unwrap();
        assert_eq!(p, PathBuf::from("foo bar.txt"));
    }

    #[test]
    fn sanitize_url_encoded_dotdot() {
        assert!(sanitize_path("/%2e%2e/etc/passwd").is_none());
    }

    // -----------------------------------------------------------------------
    // SearchRoot::accepts (3 tests)
    // -----------------------------------------------------------------------

    #[test]
    fn accepts_all_when_none() {
        let root = SearchRoot {
            path: PathBuf::from("/tmp"),
            extensions: None,
        };
        assert!(root.accepts("gif"));
    }

    #[test]
    fn accepts_matching_case_insensitive() {
        let set = HashSet::from(["jpg".to_string(), "png".to_string()]);
        let root = SearchRoot {
            path: PathBuf::from("/tmp"),
            extensions: Some(set),
        };
        assert!(root.accepts("JPG"));
    }

    #[test]
    fn rejects_non_matching() {
        let set = HashSet::from(["jpg".to_string()]);
        let root = SearchRoot {
            path: PathBuf::from("/tmp"),
            extensions: Some(set),
        };
        assert!(!root.accepts("gif"));
    }

    // -----------------------------------------------------------------------
    // FileSearcher::match_location (6 tests)
    // -----------------------------------------------------------------------

    /// Build a FileSearcher with prefix-only locations (no real filesystem roots).
    fn searcher_with_prefixes(prefixes: &[&str]) -> FileSearcher {
        let mut locations: Vec<Location> = prefixes
            .iter()
            .map(|p| Location {
                prefix: normalize_prefix(p),
                roots: vec![],
                search_mode: SearchMode::Sequential,
                max_file_size: 0,
            })
            .collect();
        locations.sort_by(|a, b| b.prefix.len().cmp(&a.prefix.len()));
        FileSearcher {
            locations,
            max_body_size: 1_048_576,
            stream_buffer_size: 65536,
        }
    }

    #[test]
    fn match_exact() {
        let s = searcher_with_prefixes(&["/imgs"]);
        let (loc, rest) = s.match_location("/imgs").unwrap();
        assert_eq!(loc.prefix, "/imgs");
        assert_eq!(rest, "/");
    }

    #[test]
    fn match_prefix_with_rest() {
        let s = searcher_with_prefixes(&["/imgs"]);
        let (loc, rest) = s.match_location("/imgs/a.jpg").unwrap();
        assert_eq!(loc.prefix, "/imgs");
        assert_eq!(rest, "/a.jpg");
    }

    #[test]
    fn match_root_catchall() {
        let s = searcher_with_prefixes(&["/imgs", "/"]);
        let (loc, rest) = s.match_location("/other/x").unwrap();
        assert_eq!(loc.prefix, "/");
        assert_eq!(rest, "/other/x");
    }

    #[test]
    fn match_longest_prefix_wins() {
        let s = searcher_with_prefixes(&["/img", "/img/photos"]);
        let (loc, rest) = s.match_location("/img/photos/a.jpg").unwrap();
        assert_eq!(loc.prefix, "/img/photos");
        assert_eq!(rest, "/a.jpg");
    }

    #[test]
    fn match_no_false_partial() {
        let s = searcher_with_prefixes(&["/img"]);
        assert!(s.match_location("/image/x.jpg").is_none());
    }

    #[test]
    fn match_no_match() {
        let s = searcher_with_prefixes(&["/imgs"]);
        assert!(s.match_location("/videos/x").is_none());
    }

    // -----------------------------------------------------------------------
    // generate_etag (2 tests)
    // -----------------------------------------------------------------------

    #[test]
    fn etag_format() {
        let mtime = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let etag = generate_etag(mtime, 524288);
        assert_eq!(etag, "W/\"1700000000-524288\"");
    }

    #[test]
    fn etag_deterministic() {
        let mtime = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(100);
        assert_eq!(generate_etag(mtime, 42), generate_etag(mtime, 42));
    }

    // -----------------------------------------------------------------------
    // is_not_modified (3 tests)
    // -----------------------------------------------------------------------

    #[test]
    fn not_modified_if_none_match() {
        let mut headers = hyper::HeaderMap::new();
        headers.insert(
            hyper::header::IF_NONE_MATCH,
            "W/\"100-42\"".parse().unwrap(),
        );
        let mtime = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(100);
        assert!(is_not_modified(&headers, "W/\"100-42\"", mtime));
    }

    #[test]
    fn not_modified_if_modified_since() {
        let mtime = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let mut headers = hyper::HeaderMap::new();
        headers.insert(
            hyper::header::IF_MODIFIED_SINCE,
            format_http_date(mtime).parse().unwrap(),
        );
        assert!(is_not_modified(&headers, "W/\"wrong\"", mtime));
    }

    #[test]
    fn not_modified_if_none_match_priority() {
        // If-None-Match doesn't match → not 304, even if If-Modified-Since matches
        let mtime = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let mut headers = hyper::HeaderMap::new();
        headers.insert(
            hyper::header::IF_NONE_MATCH,
            "W/\"wrong\"".parse().unwrap(),
        );
        headers.insert(
            hyper::header::IF_MODIFIED_SINCE,
            format_http_date(mtime).parse().unwrap(),
        );
        assert!(!is_not_modified(&headers, "W/\"100-42\"", mtime));
    }

    // -----------------------------------------------------------------------
    // parse_range_header (7 tests)
    // -----------------------------------------------------------------------

    #[test]
    fn range_from_to() {
        assert_eq!(
            parse_range_header("bytes=0-99"),
            Some(ByteRange::FromTo(0, 99))
        );
    }

    #[test]
    fn range_from_open() {
        assert_eq!(
            parse_range_header("bytes=100-"),
            Some(ByteRange::From(100))
        );
    }

    #[test]
    fn range_suffix() {
        assert_eq!(
            parse_range_header("bytes=-100"),
            Some(ByteRange::Suffix(100))
        );
    }

    #[test]
    fn range_multi_unsupported() {
        assert_eq!(parse_range_header("bytes=0-10,20-30"), None);
    }

    #[test]
    fn range_no_prefix() {
        assert_eq!(parse_range_header("items=0-10"), None);
    }

    #[test]
    fn range_garbage() {
        assert_eq!(parse_range_header("bytes=abc"), None);
    }

    #[test]
    fn range_empty_suffix() {
        // "bytes=-" is invalid — no number after -
        assert_eq!(parse_range_header("bytes=-"), None);
    }

    // -----------------------------------------------------------------------
    // ByteRange::resolve (5 tests)
    // -----------------------------------------------------------------------

    #[test]
    fn resolve_from_to_normal() {
        assert_eq!(ByteRange::FromTo(0, 99).resolve(200), Some((0, 99)));
    }

    #[test]
    fn resolve_from_to_clamped() {
        // end exceeds file size → clamp to file_size - 1
        assert_eq!(ByteRange::FromTo(0, 999).resolve(100), Some((0, 99)));
    }

    #[test]
    fn resolve_from_beyond_eof() {
        assert_eq!(ByteRange::From(100).resolve(50), None);
    }

    #[test]
    fn resolve_suffix_zero() {
        assert_eq!(ByteRange::Suffix(0).resolve(100), None);
    }

    #[test]
    fn resolve_empty_file() {
        assert_eq!(ByteRange::From(0).resolve(0), None);
        assert_eq!(ByteRange::Suffix(10).resolve(0), None);
    }

    // -----------------------------------------------------------------------
    // should_honor_range (3 tests)
    // -----------------------------------------------------------------------

    #[test]
    fn honor_range_no_if_range() {
        let headers = hyper::HeaderMap::new();
        let mtime = SystemTime::UNIX_EPOCH;
        assert!(should_honor_range(&headers, "W/\"0-0\"", mtime));
    }

    #[test]
    fn honor_range_etag_match() {
        let mut headers = hyper::HeaderMap::new();
        headers.insert(hyper::header::IF_RANGE, "W/\"100-42\"".parse().unwrap());
        let mtime = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(100);
        assert!(should_honor_range(&headers, "W/\"100-42\"", mtime));
    }

    #[test]
    fn honor_range_etag_mismatch() {
        let mut headers = hyper::HeaderMap::new();
        headers.insert(hyper::header::IF_RANGE, "W/\"999-42\"".parse().unwrap());
        let mtime = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(100);
        assert!(!should_honor_range(&headers, "W/\"100-42\"", mtime));
    }

    // -----------------------------------------------------------------------
    // check_basic_auth (7 tests)
    // -----------------------------------------------------------------------

    fn test_auth_config() -> BasicAuthConfig {
        BasicAuthConfig {
            enabled: true,
            username: "admin".into(),
            password: "secret".into(),
            realm: "filehunter".into(),
        }
    }

    #[test]
    fn basic_auth_missing_header_fails() {
        let headers = hyper::HeaderMap::new();
        assert!(!check_basic_auth(&headers, &test_auth_config()));
    }

    #[test]
    fn basic_auth_valid_credentials_passes() {
        let mut headers = hyper::HeaderMap::new();
        // "admin:secret" → base64 "YWRtaW46c2VjcmV0"
        headers.insert(
            hyper::header::AUTHORIZATION,
            "Basic YWRtaW46c2VjcmV0".parse().unwrap(),
        );
        assert!(check_basic_auth(&headers, &test_auth_config()));
    }

    #[test]
    fn basic_auth_wrong_password_fails() {
        let mut headers = hyper::HeaderMap::new();
        // "admin:wrong" → base64 "YWRtaW46d3Jvbmc="
        headers.insert(
            hyper::header::AUTHORIZATION,
            "Basic YWRtaW46d3Jvbmc=".parse().unwrap(),
        );
        assert!(!check_basic_auth(&headers, &test_auth_config()));
    }

    #[test]
    fn basic_auth_non_basic_scheme_fails() {
        let mut headers = hyper::HeaderMap::new();
        headers.insert(
            hyper::header::AUTHORIZATION,
            "Bearer some-token".parse().unwrap(),
        );
        assert!(!check_basic_auth(&headers, &test_auth_config()));
    }

    #[test]
    fn basic_auth_malformed_base64_fails() {
        let mut headers = hyper::HeaderMap::new();
        headers.insert(
            hyper::header::AUTHORIZATION,
            "Basic !!!not-base64!!!".parse().unwrap(),
        );
        assert!(!check_basic_auth(&headers, &test_auth_config()));
    }

    #[test]
    fn basic_auth_no_colon_separator_fails() {
        let mut headers = hyper::HeaderMap::new();
        // "adminonly" (no colon) → base64 "YWRtaW5vbmx5"
        headers.insert(
            hyper::header::AUTHORIZATION,
            "Basic YWRtaW5vbmx5".parse().unwrap(),
        );
        assert!(!check_basic_auth(&headers, &test_auth_config()));
    }

    #[test]
    fn basic_auth_password_with_colon_passes() {
        let auth = BasicAuthConfig {
            enabled: true,
            username: "admin".into(),
            password: "pass:word".into(),
            realm: "filehunter".into(),
        };
        let mut headers = hyper::HeaderMap::new();
        // "admin:pass:word" → base64 "YWRtaW46cGFzczp3b3Jk"
        headers.insert(
            hyper::header::AUTHORIZATION,
            "Basic YWRtaW46cGFzczp3b3Jk".parse().unwrap(),
        );
        assert!(check_basic_auth(&headers, &auth));
    }
}
