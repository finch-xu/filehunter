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

use crate::config::{Config, SearchMode};

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

pub struct FileSearcher {
    roots: Vec<SearchRoot>,
    search_mode: SearchMode,
    max_body_size: u64,
    max_file_size: u64,
    stream_buffer_size: usize,
}

impl FileSearcher {
    pub fn new(config: &Config) -> Self {
        let roots: Vec<SearchRoot> = config
            .search
            .paths
            .iter()
            .filter_map(|entry| match entry.root.canonicalize() {
                Ok(canonical) if canonical.is_dir() => {
                    let ext_set = entry.extension_set();
                    info!(
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
            warn!("no valid search paths configured");
        }

        info!(search_mode = ?config.search.mode, "search mode configured");

        Self {
            roots,
            search_mode: config.search.mode,
            max_body_size: config.server.max_body_size.as_u64(),
            max_file_size: config.server.max_file_size.as_u64(),
            stream_buffer_size: config.server.stream_buffer_size.as_usize(),
        }
    }

    /// Search across roots using the configured search mode.
    ///
    /// Returns (canonical_path, open File handle, file size).
    async fn search(&self, request_path: &str) -> Option<(PathBuf, File, u64)> {
        match self.search_mode {
            SearchMode::Sequential => self.search_sequential(request_path).await,
            SearchMode::Concurrent => self.search_concurrent(request_path).await,
            SearchMode::LatestModified => self.search_latest(request_path).await,
        }
    }

    /// Sequential search: check each root in config order; first match wins.
    async fn search_sequential(&self, request_path: &str) -> Option<(PathBuf, File, u64)> {
        let relative = sanitize_path(request_path)?;

        let ext = relative
            .extension()
            .and_then(OsStr::to_str)
            .unwrap_or("");

        for root in &self.roots {
            match Self::try_root(root, &relative, ext, self.max_file_size, request_path).await {
                Ok(Some((path, file, size, _mtime))) => return Some((path, file, size)),
                Ok(None) => continue,
                Err(()) => return None, // path traversal — abort
            }
        }

        None
    }

    /// Concurrent search: probe all eligible roots at the same time.
    /// The first root to find a valid file wins; all other tasks are cancelled
    /// immediately (tokio task drops = automatic cancellation).
    async fn search_concurrent(&self, request_path: &str) -> Option<(PathBuf, File, u64)> {
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

        // Wait for the first successful result; abort remaining tasks.
        let result = Self::race_handles(handles).await;
        result.map(|(path, file, size, _mtime)| (path, file, size))
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
                    // Abort all remaining tasks immediately.
                    for h in remaining {
                        h.abort();
                    }
                    break;
                }
                _ => {
                    // This task returned None or panicked; keep waiting on the rest.
                    handles = remaining;
                }
            }
        }

        result
    }

    /// Latest-modified search: check all eligible roots and return the file
    /// with the most recent modification time.
    async fn search_latest(&self, request_path: &str) -> Option<(PathBuf, File, u64)> {
        let relative = sanitize_path(request_path)?;

        let ext = relative
            .extension()
            .and_then(OsStr::to_str)
            .unwrap_or("");

        let mut best: Option<(PathBuf, File, u64, SystemTime)> = None;

        for root in &self.roots {
            match Self::try_root(root, &relative, ext, self.max_file_size, request_path).await {
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
                Err(()) => return None, // path traversal — abort
            }
        }

        best.map(|(path, file, size, _mtime)| (path, file, size))
    }

    /// Attempt to find the file under a single search root.
    ///
    /// Returns:
    /// - `Ok(Some(...))` — file found
    /// - `Ok(None)` — not found in this root, safe to continue
    /// - `Err(())` — path traversal detected, abort entire search
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

        let candidate = root.path.join(relative);

        // Resolve symlinks and verify the real path is still inside the root.
        let canonical = match tokio::fs::canonicalize(&candidate).await {
            Ok(c) if c.starts_with(&root.path) => c,
            Ok(_) => {
                warn!(request_path, "path traversal blocked");
                return Err(()); // security violation — stop all search
            }
            Err(_) => return Ok(None),
        };

        // Open file, then obtain metadata from the fd — avoids TOCTOU between
        // a separate stat() and open().
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

        debug!(
            request_path, resolved = %canonical.display(),
            size = meta.len(), "file found"
        );
        Ok(Some((canonical, file, meta.len(), modified)))
    }
}

// ---------------------------------------------------------------------------
// Standalone root probe — owns all data, suitable for tokio::spawn
// ---------------------------------------------------------------------------

/// Probe a single root directory for a file. Returns `None` on miss or
/// security violation. Designed to be spawned as an independent task.
async fn probe_root(
    root_path: PathBuf,
    candidate: PathBuf,
    max_file_size: u64,
    request_path: String,
) -> Option<(PathBuf, File, u64, SystemTime)> {
    let canonical = match tokio::fs::canonicalize(&candidate).await {
        Ok(c) if c.starts_with(&root_path) => c,
        Ok(_) => {
            warn!(request_path, "path traversal blocked");
            return None;
        }
        Err(_) => return None,
    };

    let file = match File::open(&canonical).await {
        Ok(f) => f,
        Err(_) => return None,
    };
    let meta = match file.metadata().await {
        Ok(m) if m.is_file() => m,
        _ => return None,
    };

    if max_file_size > 0 && meta.len() > max_file_size {
        debug!(
            request_path, resolved = %canonical.display(),
            size = meta.len(), limit = max_file_size,
            "skipped (file too large)"
        );
        return None;
    }

    let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);

    debug!(
        request_path, resolved = %canonical.display(),
        size = meta.len(), "file found (concurrent)"
    );
    Some((canonical, file, meta.len(), modified))
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
        None => Ok(text_response(StatusCode::NOT_FOUND, "Not Found")),
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
