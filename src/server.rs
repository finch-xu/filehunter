use std::collections::HashSet;
use std::convert::Infallible;
use std::ffi::OsStr;
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

use crate::config::{normalize_prefix, Config, LocationConfig, SearchMode};

type ResponseBody = BoxBody<Bytes, std::io::Error>;

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
}

impl Location {
    fn from_config(loc: &LocationConfig) -> Self {
        let prefix = normalize_prefix(&loc.prefix);

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

        info!(prefix = %prefix, mode = ?loc.mode, roots = roots.len(), "location configured");

        Self {
            prefix,
            roots,
            search_mode: loc.mode,
        }
    }

    /// Search across this location's roots using its configured search mode.
    async fn search(&self, request_path: &str, max_file_size: u64) -> Option<(PathBuf, File, u64)> {
        match self.search_mode {
            SearchMode::Sequential => self.search_sequential(request_path, max_file_size).await,
            SearchMode::Concurrent => self.search_concurrent(request_path, max_file_size).await,
            SearchMode::LatestModified => self.search_latest(request_path, max_file_size).await,
        }
    }

    async fn search_sequential(&self, request_path: &str, max_file_size: u64) -> Option<(PathBuf, File, u64)> {
        let relative = sanitize_path(request_path)?;

        let ext = relative
            .extension()
            .and_then(OsStr::to_str)
            .unwrap_or("");

        for root in &self.roots {
            match try_root(root, &relative, ext, max_file_size, request_path).await {
                Ok(Some((path, file, size, _mtime))) => return Some((path, file, size)),
                Ok(None) => continue,
                Err(()) => return None,
            }
        }

        None
    }

    async fn search_concurrent(&self, request_path: &str, max_file_size: u64) -> Option<(PathBuf, File, u64)> {
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
            let req_path = request_path.to_owned();

            handles.push(tokio::spawn(
                probe_root(root_path, candidate, max_file_size, req_path),
            ));
        }

        let result = race_handles(handles).await;
        result.map(|(path, file, size, _mtime)| (path, file, size))
    }

    async fn search_latest(&self, request_path: &str, max_file_size: u64) -> Option<(PathBuf, File, u64)> {
        let relative = sanitize_path(request_path)?;

        let ext = relative
            .extension()
            .and_then(OsStr::to_str)
            .unwrap_or("");

        let mut best: Option<(PathBuf, File, u64, SystemTime)> = None;

        for root in &self.roots {
            match try_root(root, &relative, ext, max_file_size, request_path).await {
                Ok(Some(found)) => {
                    let dominated = best.as_ref().map_or(true, |b| found.3 > b.3);
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

        best.map(|(path, file, size, _mtime)| (path, file, size))
    }
}

pub struct FileSearcher {
    locations: Vec<Location>,
    max_body_size: u64,
    max_file_size: u64,
    stream_buffer_size: usize,
}

impl FileSearcher {
    pub fn new(config: &Config) -> Self {
        let mut locations: Vec<Location> = config
            .locations
            .iter()
            .map(Location::from_config)
            .collect();

        // Sort by prefix length descending (longest match first).
        locations.sort_by(|a, b| b.prefix.len().cmp(&a.prefix.len()));

        Self {
            locations,
            max_body_size: config.server.max_body_size.as_u64(),
            max_file_size: config.server.max_file_size.as_u64(),
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
            if let Some(rest) = path.strip_prefix(&loc.prefix) {
                if rest.starts_with('/') {
                    return Some((loc, rest));
                }
            }
        }
        None
    }

    async fn search(&self, request_path: &str) -> Option<(PathBuf, File, u64)> {
        let (location, stripped_path) = self.match_location(request_path)?;
        location.search(stripped_path, self.max_file_size).await
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
) -> Result<Option<(PathBuf, File, u64, SystemTime)>, ()> {
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
) -> Result<Option<(PathBuf, File, u64, SystemTime)>, ()> {
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
    mut handles: Vec<tokio::task::JoinHandle<Option<(PathBuf, File, u64, SystemTime)>>>,
) -> Option<(PathBuf, File, u64, SystemTime)> {
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
) -> Option<(PathBuf, File, u64, SystemTime)> {
    match probe_candidate(&root_path, candidate, max_file_size, &request_path).await {
        Ok(found) => found,
        Err(()) => None,
    }
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

pub async fn handle_request(
    req: Request<hyper::body::Incoming>,
    searcher: Arc<FileSearcher>,
) -> Result<Response<ResponseBody>, Infallible> {
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
            debug!(status = 413, path = %req.uri().path(), "request handled");
            return Ok(text_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                "Payload Too Large",
            ));
        }
    }

    let path = req.uri().path();
    let is_head = req.method() == Method::HEAD;

    match searcher.search(path).await {
        Some((file_path, file, size)) => {
            debug!(
                status = 200, path,
                resolved = %file_path.display(), size,
                "request handled"
            );
            let mime = mime_guess::from_path(&file_path).first_or_octet_stream();

            let body = if is_head {
                empty_body()
            } else {
                stream_body(file, searcher.stream_buffer_size)
            };

            Ok(Response::builder()
                .status(StatusCode::OK)
                .header("Content-Type", mime.as_ref())
                .header("Content-Length", size)
                .header("Accept-Ranges", "none")
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
