<p align="center">
  <img src="logo.png" alt="FileHunter" width="200">
</p>

<h1 align="center">FileHunter</h1>

<p align="center">
  <a href="README.zh-CN.md">中文</a>
</p>

A high-performance, multi-path file search HTTP server built with Rust.

FileHunter searches across multiple configured directories in priority order, serves the first match via chunked streaming, and returns 404 if nothing is found. Designed for scenarios where files are spread across different storage paths and need to be served through a single endpoint.

## Highlights

- **Multi-path priority search** — configure multiple root directories; first match wins (sequential or concurrent mode)
- **Per-path extension filtering** — restrict each path to specific file types (images, documents, videos, etc.)
- **Async streaming** — built on tokio + hyper 1.x with chunked `ReaderStream` for low memory usage
- **HTTP/1.1 & HTTP/2** — automatic protocol negotiation via `hyper-util`
- **Security hardened** — path traversal protection, TOCTOU mitigation, null byte rejection, dotfile blocking, `nosniff` headers
- **Human-friendly config** — TOML format with size values like `"10MB"`, `"64KB"`
- **Tiny footprint** — ~3 MB binary (LTO + strip), ~41 MB RSS at idle

## Quick Start

### From Source

```bash
cargo build --release
./target/release/filehunter --config config.toml
```

### Docker

```bash
docker compose up -d
```

Or build manually:

```bash
docker build -t filehunter .
docker run -p 8080:8080 \
  -v ./config.toml:/etc/filehunter/config.toml:ro \
  -v /data:/data:ro \
  filehunter
```

## Configuration

All fields except `bind` and `search.paths` are optional with sensible defaults.

```toml
[server]
bind = "0.0.0.0:8080"

# keepalive = true
# connection_timeout = 300        # seconds, 0 = unlimited
# max_header_size = "8KB"
# max_headers = 64
# max_body_size = "1MB"
# http2_max_streams = 128
# max_file_size = "10MB"          # 0 = no limit
# stream_buffer_size = "64KB"

[search]
# mode = "sequential"             # or "concurrent"

[[search.paths]]
root = "/data/images"
extensions = ["jpg", "jpeg", "png", "gif", "webp", "svg"]

[[search.paths]]
root = "/data/documents"
extensions = ["pdf", "docx", "xlsx", "txt", "csv"]

[[search.paths]]
root = "/data/general"
# No extensions — accepts any file type as a catch-all.
```

### How Search Works

Given a request for `/report.pdf`:

1. Check `/data/images` — skipped (`.pdf` not in allowed extensions)
2. Check `/data/documents` — **found** at `/data/documents/report.pdf` → serve it
3. `/data/general` would be checked next if not found above

### Search Mode

Control how multiple roots are probed via `search.mode`:

| Mode | Behavior |
|---|---|
| `sequential` (default) | Check each root one-by-one in config order. First match wins. Deterministic — config order defines priority. |
| `concurrent` | Probe all eligible roots at the same time. The fastest match wins. Remaining searches are cancelled immediately to free resources. |
| `latest_modified` | Check all roots and return the file with the **most recent modification time**. All roots are always checked so the newest version wins. |

```toml
[search]
mode = "latest_modified"  # or "sequential" (default) / "concurrent"
```

**Mode comparison** (N = number of eligible roots):

| | `sequential` | `concurrent` | `latest_modified` |
|---|---|---|---|
| **Which file is returned** | First match by config order | Fastest I/O response | Most recently modified |
| **I/O per request (best)** | 1 root | 1 root (parallel) | N roots (all) |
| **I/O per request (worst)** | N roots | N roots (parallel) | N roots (all) |
| **Can exit early** | Yes, on first hit | Yes, on first hit | No, must check all |
| **Local disk perf** | Optimal | Slower (spawn overhead) | Slightly slower than sequential |
| **NFS / object storage perf** | High latency stacks up | Optimal (parallel I/O) | Parallel would help but not used |
| **Result determinism** | Config order | Non-deterministic | Deterministic (by mtime) |
| **Best for** | General use, priority control | High-latency network mounts | Mirrored / staged storage |

### Subdirectory Support

Request paths can contain subdirectories of any depth. The full relative path is joined directly to each search root — there is no recursive filename search.

**Example:** given the config above and a file at `/data/images/photos/2024/vacation/beach.jpg`:

```
GET /photos/2024/vacation/beach.jpg

1. sanitize  → photos/2024/vacation/beach.jpg   (strip leading /, validate each segment)
2. root join → /data/images/photos/2024/vacation/beach.jpg
3. security  → canonicalize + verify path is still inside /data/images
4. serve     → 200 OK (chunked stream)
```

If the same path is not found under `/data/images`, the server continues to the next root (`/data/documents`, then `/data/general`, etc.).

**Key points:**

- You must provide the **exact relative path** including all subdirectories — the server will not search for the filename across directories.
- Each path segment is validated: `..`, `.`, dotfiles (`.git`, `.env`), and null bytes are all rejected.
- Symlinks are resolved; if the real path escapes the root directory, the request is blocked.

### Size Values

Size fields accept integers (`65536`) or human-friendly strings (`"64KB"`, `"10MB"`, `"2GB"`).

## Logging

FileHunter uses `tracing` with env-filter support:

```bash
# Default: info level
./filehunter --config config.toml

# Debug level
RUST_LOG=filehunter=debug ./filehunter --config config.toml
```

## Runtime Resource Usage

> Test environment: release build (LTO + strip), 4 search paths configured (images / documents / videos / general), default parameters.

### Binary & Process

| Metric | Value |
|---|---|
| Binary size | **~3 MB** |
| Idle VmSize | ~1,071 MB |
| Idle VmRSS | **~41 MB** |
| Idle threads | 17 |
| Idle open file descriptors | 10 |

### Memory (before / after load)

| Phase | VmRSS | Threads | Open FDs |
|---|---|---|---|
| Idle | 41 MB | 17 | 10 |
| After 200 concurrent mixed requests | 59 MB | 25 | 10 |

> Memory stays flat under load — the async streaming design avoids buffering entire files into memory. RSS grows only ~18 MB after heavy concurrent large-file transfers, and the FD count remains unchanged.

### Single-Request Latency

| File type | Size | Avg latency |
|---|---|---|
| Plain text | 11 B | ~1.8 ms |
| Image (JPG) | 500 KB | ~2.8 ms |
| Document (PDF) | 800 KB | ~3.5 ms |
| Video (MP4) | 5 MB | ~13.9 ms |
| Binary | 5 MB | ~12.9 ms |

### Concurrent Throughput

| Scenario | Total time |
|---|---|
| 50 concurrent × small file (11 B) | ~495 ms |
| 50 concurrent × image (500 KB) | ~451 ms |
| 10 concurrent × video (5 MB) | ~181 ms |
| 200 concurrent × mixed files | ~1.8 s |

## Security

- Path traversal blocked (`.` / `..` / symlink escape)
- Null bytes rejected
- Hidden files and directories (dotfiles) blocked
- `X-Content-Type-Options: nosniff` on all responses
- Connection timeout protection against slow-loris attacks
- Request size limits (headers + body)
- File size limit to prevent serving unexpectedly large files

## License

[MIT](LICENSE)
