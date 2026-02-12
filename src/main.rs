use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder as AutoBuilder;
use tokio::net::TcpListener;
use tracing::{debug, info};

mod config;
mod server;

use config::Config;
use server::{handle_request, FileSearcher};

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

    let listener = TcpListener::bind(addr).await?;
    info!(
        %addr,
        keepalive = config.server.keepalive,
        connection_timeout = config.server.connection_timeout,
        max_header_size = %config.server.max_header_size,
        max_headers = config.server.max_headers,
        max_body_size = %config.server.max_body_size,
        http2_max_streams = config.server.http2_max_streams,
        max_file_size = %config.server.max_file_size,
        stream_buffer_size = %config.server.stream_buffer_size,
        "server listening"
    );

    loop {
        tokio::select! {
            result = listener.accept() => {
                let (stream, remote_addr) = result?;
                let searcher = searcher.clone();
                let builder = builder.clone();

                tokio::spawn(async move {
                    let io = TokioIo::new(stream);
                    let service = service_fn(move |req| {
                        let searcher = searcher.clone();
                        async move { handle_request(req, searcher).await }
                    });

                    let serve = builder.serve_connection(io, service);

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
