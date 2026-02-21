use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use hyper::body::Incoming;
use hyper::{Request, Response};
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder as AutoBuilder;
use hyper_util::service::TowerToHyperService;
use tokio::net::TcpListener;
use http_body_util::BodyExt as _;
use tower::util::BoxCloneService;
use tower::ServiceBuilder;
use tower_http::compression::predicate::{DefaultPredicate, Predicate as _, SizeAbove};
use tower_http::compression::{CompressionBody, CompressionLayer};
use tower_http::cors::{AllowHeaders, AllowMethods, AllowOrigin, CorsLayer, ExposeHeaders};
use tracing::{debug, info};

use filehunter::config::{CompressionConfig, Config, CorsConfig};
use filehunter::ratelimit::{self, KeyedLimiter};
use filehunter::server::{handle_request, FileSearcher, ResponseBody};

#[derive(Parser)]
#[command(
    name = "filehunter",
    about = "High-performance multi-path file search HTTP server"
)]
struct Args {
    /// Path to the TOML configuration file
    #[arg(short, long, default_value = "config.toml")]
    config: String,
}

/// Build a `CorsLayer` from config.
fn build_cors_layer(cfg: &CorsConfig) -> CorsLayer {
    let origin = if cfg.allow_origins.iter().any(|o| o == "*") {
        AllowOrigin::any()
    } else {
        AllowOrigin::list(
            cfg.allow_origins
                .iter()
                .filter_map(|o| o.parse().ok()),
        )
    };

    let methods = if cfg.allow_methods.iter().any(|m| m == "*") {
        AllowMethods::any()
    } else {
        AllowMethods::list(
            cfg.allow_methods
                .iter()
                .filter_map(|m| m.parse().ok()),
        )
    };

    let headers = if cfg.allow_headers.iter().any(|h| h == "*") {
        AllowHeaders::any()
    } else {
        AllowHeaders::list(
            cfg.allow_headers
                .iter()
                .filter_map(|h| h.parse().ok()),
        )
    };

    let expose = if cfg.expose_headers.iter().any(|h| h == "*") {
        ExposeHeaders::any()
    } else {
        ExposeHeaders::list(
            cfg.expose_headers
                .iter()
                .filter_map(|h| h.parse().ok()),
        )
    };

    let mut layer = CorsLayer::new()
        .allow_origin(origin)
        .allow_methods(methods)
        .allow_headers(headers)
        .expose_headers(expose)
        .max_age(Duration::from_secs(cfg.max_age));

    if cfg.allow_credentials {
        layer = layer.allow_credentials(true);
    }

    layer
}

/// Predicate: respect `DefaultPredicate` (skip images, tiny responses) + user `min_size`.
type CompPredicate =
    tower_http::compression::predicate::And<DefaultPredicate, SizeAbove>;

/// Build a `CompressionLayer` from config.
///
/// Algorithm selection (`no_*`) must happen before `compress_when()` because
/// the disabler methods are only available on `CompressionLayer<DefaultPredicate>`.
fn build_compression_layer(cfg: &CompressionConfig) -> CompressionLayer<CompPredicate> {
    let mut layer = CompressionLayer::new();

    let algos = &cfg.algorithms;
    if !algos.iter().any(|a| a == "gzip") {
        layer = layer.no_gzip();
    }
    if !algos.iter().any(|a| a == "br") {
        layer = layer.no_br();
    }
    if !algos.iter().any(|a| a == "deflate") {
        layer = layer.no_deflate();
    }
    if !algos.iter().any(|a| a == "zstd") {
        layer = layer.no_zstd();
    }

    let min_size = cfg.min_size.as_u64().min(u16::MAX as u64) as u16;
    let predicate = DefaultPredicate::new().and(SizeAbove::new(min_size));

    layer.compress_when(predicate)
}

/// Re-box the compressed response body back into our erased `ResponseBody` type.
///
/// `CompressionBody` unifies errors into `BoxError`; we wrap it back into
/// `std::io::Error` via `Error::other()` to match our `ResponseBody` alias.
fn rebox_response(
    resp: Response<CompressionBody<ResponseBody>>,
) -> Response<ResponseBody> {
    resp.map(|body| body.map_err(std::io::Error::other).boxed())
}

type ErasedService =
    BoxCloneService<Request<Incoming>, Response<ResponseBody>, Infallible>;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "filehunter=info".parse().unwrap()),
        )
        .init();

    let args = Args::parse();

    let config = Config::load(&args.config)?;
    let addr: SocketAddr = config.server.bind.parse()?;
    let searcher = Arc::new(FileSearcher::new(&config));

    // Connection timeout (0 = unlimited).
    let conn_timeout = match config.server.connection_timeout {
        0 => None,
        secs => Some(Duration::from_secs(secs)),
    };

    let mut builder = AutoBuilder::new(TokioExecutor::new());

    builder
        .http1()
        .keep_alive(config.server.keepalive)
        .max_buf_size(config.server.max_header_size.as_usize())
        .max_headers(config.server.max_headers);
    builder
        .http2()
        .max_header_list_size(config.server.max_header_size.as_u32())
        .max_concurrent_streams(config.server.http2_max_streams);

    // CORS layer (optional).
    let cors_layer = if config.server.cors.enabled {
        Some(build_cors_layer(&config.server.cors))
    } else {
        None
    };

    // Compression layer (optional, default off).
    let compression_layer = if config.server.compression.enabled {
        Some(build_compression_layer(&config.server.compression))
    } else {
        None
    };

    // Per-IP rate limiter (optional).
    let limiter: Option<Arc<KeyedLimiter>> = if config.server.rate_limit.enabled {
        let lim = ratelimit::build_limiter(&config.server.rate_limit);
        ratelimit::spawn_cleanup(lim.clone(), config.server.rate_limit.cleanup_interval);
        Some(lim)
    } else {
        None
    };

    let listener = TcpListener::bind(addr).await?;
    info!(
        %addr,
        locations = config.locations.len(),
        keepalive = config.server.keepalive,
        connection_timeout = config.server.connection_timeout,
        max_header_size = %config.server.max_header_size,
        max_headers = config.server.max_headers,
        max_body_size = %config.server.max_body_size,
        http2_max_streams = config.server.http2_max_streams,
        max_file_size = %config.server.max_file_size,
        stream_buffer_size = %config.server.stream_buffer_size,
        cors_enabled = config.server.cors.enabled,
        rate_limit_enabled = config.server.rate_limit.enabled,
        rate_limit_rps = config.server.rate_limit.requests_per_second,
        rate_limit_burst = config.server.rate_limit.burst_size,
        compression_enabled = config.server.compression.enabled,
        "server listening"
    );

    loop {
        tokio::select! {
            result = listener.accept() => {
                let (stream, remote_addr) = result?;
                let searcher = searcher.clone();
                let builder = builder.clone();
                let cors_layer = cors_layer.clone();
                let compression_layer = compression_layer.clone();
                let limiter = limiter.clone();
                let client_ip = remote_addr.ip();

                tokio::spawn(async move {
                    let io = TokioIo::new(stream);

                    let inner = tower::service_fn(move |req: Request<Incoming>| {
                        let searcher = searcher.clone();
                        let limiter = limiter.clone();
                        async move {
                            handle_request(req, searcher, limiter, client_ip).await
                        }
                    });

                    let erased: ErasedService = match (&cors_layer, &compression_layer) {
                        (Some(cors), Some(comp)) => BoxCloneService::new(
                            ServiceBuilder::new()
                                .map_response(rebox_response)
                                .layer(cors.clone())
                                .layer(comp.clone())
                                .service(inner),
                        ),
                        (None, Some(comp)) => BoxCloneService::new(
                            ServiceBuilder::new()
                                .map_response(rebox_response)
                                .layer(comp.clone())
                                .service(inner),
                        ),
                        (Some(cors), None) => BoxCloneService::new(
                            ServiceBuilder::new()
                                .layer(cors.clone())
                                .service(inner),
                        ),
                        (None, None) => BoxCloneService::new(inner),
                    };

                    let hyper_svc = TowerToHyperService::new(erased);
                    let serve = builder.serve_connection(io, hyper_svc);

                    let result = if let Some(d) = conn_timeout {
                        match tokio::time::timeout(d, serve).await {
                            Ok(r) => r,
                            Err(_) => {
                                debug!(%remote_addr, "connection timed out");
                                return;
                            }
                        }
                    } else {
                        serve.await
                    };

                    if let Err(e) = result {
                        debug!(%remote_addr, error = %e, "connection ended");
                    }
                });
            }
            _ = tokio::signal::ctrl_c() => {
                info!("shutting down");
                break;
            }
        }
    }

    Ok(())
}
