use std::collections::HashSet;
use std::fmt;
use std::path::PathBuf;

use serde::de;
use serde::Deserialize;

// ---------------------------------------------------------------------------
// ByteSize — human-friendly byte size with serde support
// ---------------------------------------------------------------------------

/// A byte size that deserializes from either an integer (`65536`) or a
/// human-friendly string (`"64KB"`, `"1MB"`, `"2GB"`).
///
/// Display always picks the most natural unit: `64KB`, `1MB`, `1024B`, etc.
#[derive(Debug, Clone, Copy)]
pub struct ByteSize(pub u64);

impl ByteSize {
    pub fn as_u64(self) -> u64 {
        self.0
    }

    pub fn as_usize(self) -> usize {
        self.0 as usize
    }

    pub fn as_u32(self) -> u32 {
        self.0 as u32
    }
}

impl fmt::Display for ByteSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const KB: u64 = 1024;
        const MB: u64 = 1024 * 1024;
        const GB: u64 = 1024 * 1024 * 1024;

        let b = self.0;
        if b == 0 {
            write!(f, "0")
        } else if b.is_multiple_of(GB) {
            write!(f, "{}GB", b / GB)
        } else if b.is_multiple_of(MB) {
            write!(f, "{}MB", b / MB)
        } else if b.is_multiple_of(KB) {
            write!(f, "{}KB", b / KB)
        } else {
            write!(f, "{}B", b)
        }
    }
}

impl<'de> Deserialize<'de> for ByteSize {
    fn deserialize<D: de::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;

        impl<'de> de::Visitor<'de> for Visitor {
            type Value = ByteSize;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                write!(f, "a byte size: integer or string like \"8KB\", \"1MB\", \"2GB\"")
            }

            fn visit_u64<E: de::Error>(self, v: u64) -> Result<ByteSize, E> {
                Ok(ByteSize(v))
            }

            fn visit_i64<E: de::Error>(self, v: i64) -> Result<ByteSize, E> {
                if v < 0 {
                    return Err(E::custom("byte size cannot be negative"));
                }
                Ok(ByteSize(v as u64))
            }

            fn visit_str<E: de::Error>(self, s: &str) -> Result<ByteSize, E> {
                parse_byte_size(s).map_err(E::custom)
            }
        }

        deserializer.deserialize_any(Visitor)
    }
}

fn parse_byte_size(s: &str) -> Result<ByteSize, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty byte size".into());
    }

    let num_end = s
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(s.len());
    let (num_str, unit_str) = s.split_at(num_end);
    let unit_str = unit_str.trim();

    let number: u64 = num_str
        .parse()
        .map_err(|_| format!("invalid number in byte size: {s}"))?;

    let multiplier: u64 = match unit_str.to_ascii_uppercase().as_str() {
        "" | "B" => 1,
        "K" | "KB" => 1024,
        "M" | "MB" => 1024 * 1024,
        "G" | "GB" => 1024 * 1024 * 1024,
        _ => return Err(format!("unknown unit: {unit_str} (use B, KB, MB, or GB)")),
    };

    number
        .checked_mul(multiplier)
        .map(ByteSize)
        .ok_or_else(|| format!("byte size overflow: {s}"))
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub server: ServerConfig,
    pub locations: Vec<LocationConfig>,
}

// ---------------------------------------------------------------------------
// CORS & Rate Limit configuration
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct CorsConfig {
    pub enabled: bool,
    pub allow_origins: Vec<String>,
    pub allow_methods: Vec<String>,
    pub allow_headers: Vec<String>,
    pub expose_headers: Vec<String>,
    pub max_age: u64,
    pub allow_credentials: bool,
}

impl Default for CorsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            allow_origins: vec!["*".into()],
            allow_methods: vec!["GET".into(), "HEAD".into(), "OPTIONS".into()],
            allow_headers: vec!["*".into()],
            expose_headers: vec!["Content-Length".into(), "Content-Type".into()],
            max_age: 86400,
            allow_credentials: false,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct RateLimitConfig {
    pub enabled: bool,
    pub requests_per_second: u32,
    pub burst_size: u32,
    pub cleanup_interval: u64,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            requests_per_second: 10,
            burst_size: 30,
            cleanup_interval: 600,
        }
    }
}

/// All fields except `bind` have sensible defaults — existing configs keep working.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    /// Bind address, e.g. "0.0.0.0:8080".
    pub bind: String,

    /// Enable HTTP/1.1 keep-alive.
    pub keepalive: bool,

    /// Maximum connection lifetime in seconds (0 = unlimited).
    pub connection_timeout: u64,

    /// Maximum size for the request line + headers. e.g. "8KB"
    pub max_header_size: ByteSize,

    /// Maximum number of request headers.
    pub max_headers: usize,

    /// Maximum allowed Content-Length. e.g. "1MB"
    pub max_body_size: ByteSize,

    /// HTTP/2 maximum concurrent streams per connection.
    pub http2_max_streams: u32,

    /// Maximum file size that can be served. e.g. "10MB"
    /// Files exceeding this are skipped during search.
    pub max_file_size: ByteSize,

    /// Response streaming buffer size. e.g. "64KB"
    pub stream_buffer_size: ByteSize,

    /// CORS configuration.
    pub cors: CorsConfig,

    /// Per-IP rate limiting configuration.
    pub rate_limit: RateLimitConfig,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: "0.0.0.0:8080".into(),
            keepalive: true,
            connection_timeout: 300,
            max_header_size: ByteSize(8192),
            max_headers: 64,
            max_body_size: ByteSize(1_048_576),
            http2_max_streams: 128,
            max_file_size: ByteSize(10 * 1024 * 1024),
            stream_buffer_size: ByteSize(65536),
            cors: CorsConfig::default(),
            rate_limit: RateLimitConfig::default(),
        }
    }
}

/// Controls how multiple search roots are probed.
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SearchMode {
    /// Check each root sequentially in config order; first match wins.
    /// Deterministic: config order defines priority.
    #[default]
    Sequential,
    /// Probe all eligible roots concurrently; the fastest match wins.
    /// Remaining searches are cancelled as soon as a result is found.
    Concurrent,
    /// Check all eligible roots and return the file with the most recent
    /// modification time. Useful when the same filename exists in multiple
    /// roots and the latest version should always be served.
    LatestModified,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LocationConfig {
    /// URL prefix for this location, e.g. "/imgs1".
    pub prefix: String,

    /// Search strategy. Default: `"sequential"`.
    #[serde(default)]
    pub mode: SearchMode,

    /// Per-location maximum file size override.
    /// If omitted, falls back to `[server].max_file_size`.
    pub max_file_size: Option<ByteSize>,

    /// Search paths for this location.
    pub paths: Vec<SearchPath>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SearchPath {
    /// Root directory for this search entry.
    pub root: PathBuf,

    /// Allowed file extensions (without leading dot), e.g. ["jpg", "jpeg", "png"].
    /// If omitted or empty, all file types are allowed.
    #[serde(default)]
    pub extensions: Vec<String>,
}

impl SearchPath {
    /// Pre-compute a normalized `HashSet` of lowercase extensions for fast lookup.
    pub fn extension_set(&self) -> Option<HashSet<String>> {
        if self.extensions.is_empty() {
            return None; // None means "allow all"
        }
        Some(
            self.extensions
                .iter()
                .map(|e| e.trim_start_matches('.').to_ascii_lowercase())
                .collect(),
        )
    }
}

/// Normalize a location prefix: ensure it starts with `/` and has no trailing `/`.
pub fn normalize_prefix(raw: &str) -> String {
    let mut p = raw.to_string();
    if !p.starts_with('/') {
        p.insert(0, '/');
    }
    while p.len() > 1 && p.ends_with('/') {
        p.pop();
    }
    p
}

/// Minimum value hyper accepts for HTTP/1.1 read buffer size.
const MIN_HEADER_SIZE: u64 = 8192;

impl Config {
    pub fn load(path: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let content = std::fs::read_to_string(path)?;
        let config: Config = toml::from_str(&content)?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), String> {
        if self.server.max_header_size.0 < MIN_HEADER_SIZE {
            return Err(format!(
                "max_header_size must be >= {} (got {})",
                ByteSize(MIN_HEADER_SIZE),
                self.server.max_header_size,
            ));
        }
        if self.server.stream_buffer_size.0 == 0 {
            return Err("stream_buffer_size must be > 0".into());
        }
        if self.locations.is_empty() {
            return Err("at least one [[locations]] must be configured".into());
        }

        if self.server.cors.enabled
            && self.server.cors.allow_credentials
            && self.server.cors.allow_origins.iter().any(|o| o == "*")
        {
            return Err(
                "CORS: allow_credentials=true is incompatible with allow_origins=[\"*\"]".into(),
            );
        }

        if self.server.rate_limit.enabled {
            if self.server.rate_limit.requests_per_second == 0 {
                return Err("rate_limit.requests_per_second must be > 0".into());
            }
            if self.server.rate_limit.burst_size == 0 {
                return Err("rate_limit.burst_size must be > 0".into());
            }
        }

        let mut seen_prefixes = HashSet::new();
        for loc in &self.locations {
            if loc.paths.is_empty() {
                return Err(format!(
                    "location prefix={:?} must have at least one path",
                    loc.prefix,
                ));
            }
            if loc.prefix.contains('\0') || loc.prefix.contains("..") {
                return Err(format!(
                    "location prefix={:?} contains forbidden characters",
                    loc.prefix,
                ));
            }
            let normalized = normalize_prefix(&loc.prefix);
            if !seen_prefixes.insert(normalized) {
                return Err(format!(
                    "duplicate location prefix={:?}",
                    loc.prefix,
                ));
            }
        }
        Ok(())
    }
}
