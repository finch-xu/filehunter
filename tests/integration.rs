use std::fs;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use http_body_util::{BodyExt, Empty};
use hyper::{Request, StatusCode};
use tempfile::TempDir;

use filehunter::config::*;
use filehunter::server::{handle_request, FileSearcher, ResponseBody};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_request(method: &str, uri: &str) -> Request<Empty<Bytes>> {
    Request::builder()
        .method(method)
        .uri(uri)
        .body(Empty::new())
        .unwrap()
}

fn localhost() -> IpAddr {
    "127.0.0.1".parse().unwrap()
}

async fn body_string(resp: hyper::Response<ResponseBody>) -> String {
    let collected = resp.into_body().collect().await.unwrap();
    String::from_utf8(collected.to_bytes().to_vec()).unwrap()
}

/// Create a temp directory with files, return (TempDir, FileSearcher).
fn setup_single_root(
    files: &[(&str, &[u8])],
    extensions: Vec<String>,
) -> (TempDir, Arc<FileSearcher>) {
    let dir = tempfile::tempdir().unwrap();
    for (name, content) in files {
        let path = dir.path().join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, content).unwrap();
    }

    let config = Config {
        server: ServerConfig::default(),
        locations: vec![LocationConfig {
            prefix: "/".into(),
            mode: SearchMode::Sequential,
            max_file_size: None,
            paths: vec![SearchPath {
                root: dir.path().to_path_buf(),
                extensions,
            }],
        }],
    };
    let searcher = Arc::new(FileSearcher::new(&config));
    (dir, searcher)
}

// ---------------------------------------------------------------------------
// HTTP method & status code (5 tests)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn get_existing_returns_200() {
    let (_dir, searcher) = setup_single_root(&[("test.txt", b"hello")], vec![]);
    let req = make_request("GET", "/test.txt");
    let resp = handle_request(req, searcher, None, localhost()).await.unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get("Content-Length")
            .unwrap()
            .to_str()
            .unwrap(),
        "5"
    );
    let body = body_string(resp).await;
    assert_eq!(body, "hello");
}

#[tokio::test]
async fn get_missing_returns_404() {
    let (_dir, searcher) = setup_single_root(&[("test.txt", b"hello")], vec![]);
    let req = make_request("GET", "/nope.txt");
    let resp = handle_request(req, searcher, None, localhost()).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn head_returns_200_empty_body() {
    let (_dir, searcher) = setup_single_root(&[("test.txt", b"hello")], vec![]);
    let req = make_request("HEAD", "/test.txt");
    let resp = handle_request(req, searcher, None, localhost()).await.unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get("Content-Length")
            .unwrap()
            .to_str()
            .unwrap(),
        "5"
    );
    let body = body_string(resp).await;
    assert!(body.is_empty(), "HEAD body should be empty, got: {body:?}");
}

#[tokio::test]
async fn post_returns_405() {
    let (_dir, searcher) = setup_single_root(&[("test.txt", b"hello")], vec![]);
    let req = make_request("POST", "/test.txt");
    let resp = handle_request(req, searcher, None, localhost()).await.unwrap();
    assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn oversized_content_length_413() {
    let (_dir, searcher) = setup_single_root(&[("x", b"tiny")], vec![]);
    let req = Request::builder()
        .method("GET")
        .uri("/x")
        .header("Content-Length", "999999999")
        .body(Empty::<Bytes>::new())
        .unwrap();
    let resp = handle_request(req, searcher, None, localhost()).await.unwrap();
    assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

// ---------------------------------------------------------------------------
// MIME types (2 tests)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn mime_jpg() {
    let (_dir, searcher) = setup_single_root(&[("photo.jpg", b"\xFF\xD8")], vec![]);
    let req = make_request("GET", "/photo.jpg");
    let resp = handle_request(req, searcher, None, localhost()).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let ct = resp.headers().get("Content-Type").unwrap().to_str().unwrap();
    assert_eq!(ct, "image/jpeg");
}

#[tokio::test]
async fn mime_html() {
    let (_dir, searcher) = setup_single_root(&[("page.html", b"<html></html>")], vec![]);
    let req = make_request("GET", "/page.html");
    let resp = handle_request(req, searcher, None, localhost()).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let ct = resp.headers().get("Content-Type").unwrap().to_str().unwrap();
    assert_eq!(ct, "text/html");
}

// ---------------------------------------------------------------------------
// Extension filtering (2 tests)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn filter_blocks_disallowed() {
    let (_dir, searcher) =
        setup_single_root(&[("file.exe", b"binary")], vec!["jpg".into()]);
    let req = make_request("GET", "/file.exe");
    let resp = handle_request(req, searcher, None, localhost()).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn filter_allows_matching() {
    let (_dir, searcher) =
        setup_single_root(&[("file.jpg", b"\xFF\xD8")], vec!["jpg".into()]);
    let req = make_request("GET", "/file.jpg");
    let resp = handle_request(req, searcher, None, localhost()).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// ---------------------------------------------------------------------------
// Search modes (2 tests)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sequential_returns_first_root() {
    let dir1 = tempfile::tempdir().unwrap();
    let dir2 = tempfile::tempdir().unwrap();
    fs::write(dir1.path().join("data.txt"), b"first").unwrap();
    fs::write(dir2.path().join("data.txt"), b"second").unwrap();

    let config = Config {
        server: ServerConfig::default(),
        locations: vec![LocationConfig {
            prefix: "/".into(),
            mode: SearchMode::Sequential,
            max_file_size: None,
            paths: vec![
                SearchPath {
                    root: dir1.path().to_path_buf(),
                    extensions: vec![],
                },
                SearchPath {
                    root: dir2.path().to_path_buf(),
                    extensions: vec![],
                },
            ],
        }],
    };
    let searcher = Arc::new(FileSearcher::new(&config));

    let req = make_request("GET", "/data.txt");
    let resp = handle_request(req, searcher, None, localhost()).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_string(resp).await;
    assert_eq!(body, "first");
}

#[tokio::test]
async fn latest_modified_returns_newer() {
    let dir1 = tempfile::tempdir().unwrap();
    let dir2 = tempfile::tempdir().unwrap();

    fs::write(dir1.path().join("data.txt"), b"old").unwrap();
    fs::write(dir2.path().join("data.txt"), b"new").unwrap();

    // Make dir1's file 1 hour old.
    let old_time = SystemTime::now() - Duration::from_secs(3600);
    let f = fs::File::options()
        .write(true)
        .open(dir1.path().join("data.txt"))
        .unwrap();
    f.set_times(fs::FileTimes::new().set_modified(old_time))
        .unwrap();

    let config = Config {
        server: ServerConfig::default(),
        locations: vec![LocationConfig {
            prefix: "/".into(),
            mode: SearchMode::LatestModified,
            max_file_size: None,
            paths: vec![
                SearchPath {
                    root: dir1.path().to_path_buf(),
                    extensions: vec![],
                },
                SearchPath {
                    root: dir2.path().to_path_buf(),
                    extensions: vec![],
                },
            ],
        }],
    };
    let searcher = Arc::new(FileSearcher::new(&config));

    let req = make_request("GET", "/data.txt");
    let resp = handle_request(req, searcher, None, localhost()).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_string(resp).await;
    assert_eq!(body, "new");
}

// ---------------------------------------------------------------------------
// Routing integration (1 test)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn longest_prefix_routing() {
    let img_dir = tempfile::tempdir().unwrap();
    let root_dir = tempfile::tempdir().unwrap();

    fs::write(img_dir.path().join("photo.jpg"), b"img-content").unwrap();
    fs::write(root_dir.path().join("photo.jpg"), b"root-content").unwrap();

    let config = Config {
        server: ServerConfig::default(),
        locations: vec![
            LocationConfig {
                prefix: "/img".into(),
                mode: SearchMode::Sequential,
                max_file_size: None,
                paths: vec![SearchPath {
                    root: img_dir.path().to_path_buf(),
                    extensions: vec![],
                }],
            },
            LocationConfig {
                prefix: "/".into(),
                mode: SearchMode::Sequential,
                max_file_size: None,
                paths: vec![SearchPath {
                    root: root_dir.path().to_path_buf(),
                    extensions: vec![],
                }],
            },
        ],
    };
    let searcher = Arc::new(FileSearcher::new(&config));

    let req = make_request("GET", "/img/photo.jpg");
    let resp = handle_request(req, searcher, None, localhost()).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_string(resp).await;
    assert_eq!(body, "img-content");
}

// ---------------------------------------------------------------------------
// Rate limiting (1 test)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rate_limited_returns_429() {
    let (_dir, searcher) = setup_single_root(&[("test.txt", b"hello")], vec![]);

    let limiter_config = RateLimitConfig {
        enabled: true,
        requests_per_second: 1,
        burst_size: 1,
        cleanup_interval: 600,
    };
    let limiter = filehunter::ratelimit::build_limiter(&limiter_config);

    // First request should succeed (consumes the single burst token).
    let req = make_request("GET", "/test.txt");
    let resp = handle_request(req, searcher.clone(), Some(limiter.clone()), localhost())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Second request should be rate-limited.
    let req = make_request("GET", "/test.txt");
    let resp = handle_request(req, searcher, Some(limiter), localhost())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(resp.headers().contains_key("Retry-After"));
}
