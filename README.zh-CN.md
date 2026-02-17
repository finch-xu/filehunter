<p align="center">
  <img src="logo.png" alt="FileHunter" width="200">
</p>

<h1 align="center">FileHunter</h1>

<p align="center">
  <a href="https://github.com/finch-xu/filehunter/releases/latest"><img src="https://img.shields.io/github/v/release/finch-xu/filehunter" alt="Release"></a>
  <a href="https://github.com/finch-xu/filehunter/blob/main/LICENSE"><img src="https://img.shields.io/github/license/finch-xu/filehunter" alt="License"></a>
  <a href="https://www.rust-lang.org"><img src="https://img.shields.io/badge/rust-1.93%2B-orange?logo=rust" alt="Rust 1.93+"></a>
  <a href="https://github.com/finch-xu/filehunter/pkgs/container/filehunter"><img src="https://img.shields.io/badge/ghcr.io-filehunter-blue?logo=docker" alt="Docker"></a>
  <a href="https://github.com/finch-xu/filehunter/actions/workflows/github-code-scanning/codeql"><img src="https://github.com/finch-xu/filehunter/actions/workflows/github-code-scanning/codeql/badge.svg" alt="CodeQL"></a>
</p>

<p align="center">
  <a href="README.md">English</a> · <a href="https://github.com/finch-xu/filehunter/wiki">📖 Wiki</a>
</p>

<p align="center">
  <img src="Logic-diagram.jpg" alt="FileHunter 逻辑示意图">
</p>

基于 Rust 构建的高性能多路径文件搜索 HTTP 服务器。

FileHunter 按 URL 前缀将请求路由到不同的搜索目录组，找到文件后通过分块流式传输返回，未找到则返回 404。适用于文件分布在不同存储路径、需要通过多个 URL 端点对外提供服务的场景。

## 亮点

- **多前缀 URL 路由** — 每个 `[[locations]]` 将一个 URL 前缀映射到独立的搜索路径和搜索模式
- **按路径过滤文件类型** — 每个路径可独立限制允许的扩展名（图片、文档、视频等）
- **三种搜索模式** — sequential（优先级顺序）、concurrent（最快优先）、latest_modified（最新修改时间优先）— 可按 location 独立配置
- **异步流式传输** — 基于 tokio + hyper 1.x，使用 `ReaderStream` 分块传输，内存占用极低
- **HTTP/1.1 & HTTP/2** — 通过 `hyper-util` 自动协商协议
- **安全加固** — 路径穿越防护、TOCTOU 缓解、空字节拒绝、隐藏文件屏蔽、前缀段边界检查、`nosniff` 响应头
- **人性化配置** — TOML 格式，支持 `"10MB"`、`"64KB"` 等可读单位
- **极小体积** — 二进制约 3 MB（LTO + strip）

## 快速开始

### 源码编译

> **环境要求：** Rust 1.93+（Edition 2024）

```bash
cargo build --release
./target/release/filehunter --config config.toml
```

### Docker 部署

```bash
docker compose up -d
```

或手动构建：

```bash
docker build -t filehunter .
docker run -p 8080:8080 \
  -v ./config.toml:/etc/filehunter/config.toml:ro \
  -v /data:/data:ro \
  filehunter
```

## 配置说明

除 `bind` 和 `locations` 外，所有字段均为可选，带有合理的默认值。

```toml
[server]
bind = "0.0.0.0:8080"

# keepalive = true
# connection_timeout = 300        # 秒，0 = 不限制
# max_header_size = "8KB"
# max_headers = 64
# max_body_size = "1MB"
# http2_max_streams = 128
# max_file_size = "10MB"          # 0 = 不限制
# stream_buffer_size = "64KB"

[[locations]]
prefix = "/imgs"
mode = "sequential"

[[locations.paths]]
root = "/data/images"
extensions = ["jpg", "jpeg", "png", "gif", "webp", "svg"]

[[locations]]
prefix = "/docs"
mode = "concurrent"

[[locations.paths]]
root = "/data/documents"
extensions = ["pdf", "docx", "xlsx", "txt", "csv"]

[[locations.paths]]
root = "/data/archive"
extensions = ["pdf", "docx", "xlsx", "txt", "csv"]

[[locations]]
prefix = "/"

[[locations.paths]]
root = "/data/general"
# 不限扩展名 — 作为兜底路径接受所有文件类型
```

### 路由机制

每个 `[[locations]]` 块将一个 URL 前缀映射到一组搜索路径。请求到达时，FileHunter 匹配最长前缀，剥离前缀后在该 location 的路径中搜索文件。

**示例**（使用上述配置）：

| 请求 | 匹配的 location | 搜索内容 | 结果 |
|---|---|---|---|
| `GET /imgs/photo.jpg` | `prefix="/imgs"` | 在 `/data/images` 中搜索 `photo.jpg` | 顺序搜索 |
| `GET /docs/report.pdf` | `prefix="/docs"` | 在 `/data/documents`、`/data/archive` 中搜索 `report.pdf` | 并发搜索 |
| `GET /other/file.txt` | `prefix="/"` | 在 `/data/general` 中搜索 `other/file.txt` | 兜底匹配 |
| `GET /imgs` | `prefix="/imgs"` | `/` → 路径校验拒绝 → 404 | 无文件可服务 |

**前缀匹配规则：**

- 最长前缀优先：若同时配置了 `/api/v1` 和 `/api`，请求 `/api/v1/data` 匹配 `/api/v1`
- 段边界匹配：`/imgs` 不会匹配 `/imgs-extra/file.jpg`（剥离后剩余部分必须以 `/` 开头）
- `prefix="/"` 作为兜底，匹配所有未被其他前缀捕获的请求
- 未匹配任何前缀的请求返回 404

### 不同路由配置下的行为

#### 仅配置根路由（`prefix="/"`）

只配置 `prefix="/"` 时，所有请求由同一个 location 处理 — 等同于最简单的扁平搜索：

```toml
[[locations]]
prefix = "/"

[[locations.paths]]
root = "/data/files"
```

| 请求 | 匹配 | 剥离后路径 | 行为 |
|---|---|---|---|
| `GET /photo.jpg` | `"/"` | `/photo.jpg` | 在 `/data/files` 中搜索 `photo.jpg` |
| `GET /sub/dir/file.txt` | `"/"` | `/sub/dir/file.txt` | 在 `/data/files` 中搜索 `sub/dir/file.txt` |
| `GET /` | `"/"` | `/` | 路径清理拒绝空路径 → **404** |
| `GET /../etc/passwd` | `"/"` | `/../etc/passwd` | 路径清理拒绝 `..` → **404** |
| `GET /.env` | `"/"` | `/.env` | 路径清理拒绝隐藏文件 → **404** |

#### 配置多前缀路由

配置多个前缀时，每个请求被路由到最佳匹配的 location：

```toml
[[locations]]
prefix = "/imgs"

[[locations.paths]]
root = "/data/images"
extensions = ["jpg", "png"]

[[locations]]
prefix = "/api/v1"

[[locations.paths]]
root = "/data/api-v1"

[[locations]]
prefix = "/api"

[[locations.paths]]
root = "/data/api-legacy"
```

| 请求 | 匹配 | 剥离后路径 | 行为 |
|---|---|---|---|
| `GET /imgs/photo.jpg` | `"/imgs"` | `/photo.jpg` | 在 `/data/images` 中搜索 `photo.jpg` |
| `GET /imgs/sub/pic.png` | `"/imgs"` | `/sub/pic.png` | 在 `/data/images` 中搜索 `sub/pic.png` |
| `GET /imgs` | `"/imgs"` | `/` | 路径清理拒绝空路径 → **404** |
| `GET /imgs/` | `"/imgs"` | `/` | 路径清理拒绝空路径 → **404** |
| `GET /imgs-hd/photo.jpg` | — | — | 非段边界，无前缀匹配 → **404** |
| `GET /api/v1/users.json` | `"/api/v1"` | `/users.json` | 最长前缀优先 → 在 `/data/api-v1` 中搜索 |
| `GET /api/v2/data.json` | `"/api"` | `/v2/data.json` | 回退到 `/api` → 在 `/data/api-legacy` 中搜索 |
| `GET /api` | `"/api"` | `/` | 路径清理拒绝空路径 → **404** |
| `GET /other/file.txt` | — | — | 无前缀匹配，无兜底路由 → **404** |
| `GET /%69mgs/photo.jpg` | — | — | 原始路径不匹配 `/imgs` → **404** |

> **注意：** 未配置 `prefix="/"` 兜底路由时，任何不匹配已配置前缀的请求直接返回 404。

### 搜索模式

每个 location 通过 `mode` 字段独立配置搜索模式：

| 模式 | 行为 |
|---|---|
| `sequential`（默认） | 按配置顺序逐个检查根目录，第一个匹配即返回。行为确定 — 配置顺序决定优先级。 |
| `concurrent` | 同时探测所有符合条件的根目录，最快找到文件的立即响应。其余搜索任务立刻取消以释放资源。 |
| `latest_modified` | 检查所有根目录，返回**修改时间最新**的文件。每次请求都会遍历所有根目录，确保返回最新版本。 |

**模式对比**（N = 符合条件的根目录数量）：

| | `sequential` | `concurrent` | `latest_modified` |
|---|---|---|---|
| **返回哪个文件** | 按配置顺序，第一个命中 | I/O 响应最快的 | 修改时间最新的 |
| **每次请求 I/O（最优）** | 1 个 root | 1 个 root（并行） | N 个 root（全部） |
| **每次请求 I/O（最差）** | N 个 root | N 个 root（并行） | N 个 root（全部） |
| **可提前退出** | 是，找到即停 | 是，最快即停 | 否，必须检查全部 |
| **本地磁盘性能** | 最优 | 较慢（spawn 开销） | 略慢于 sequential |
| **NFS / 对象存储性能** | 高延迟逐个叠加 | 最优（并行 I/O） | 并行有帮助但未使用 |
| **结果确定性** | 由配置顺序决定 | 不确定（取决于 I/O 速度） | 确定（由 mtime 决定） |
| **适用场景** | 通用，优先级控制 | 高延迟网络挂载 | 镜像存储 / 分级存储 |

### 子目录支持

请求路径可以包含任意深度的子目录。前缀剥离后的相对路径会直接拼接到每个搜索根目录后面 — 服务器不会按文件名递归搜索。

**示例：** 假设配置了 `prefix="/imgs"`，根目录为 `/data/images`，文件位于 `/data/images/photos/2024/vacation/beach.jpg`：

```
GET /imgs/photos/2024/vacation/beach.jpg

1. 前缀匹配  → prefix="/imgs"，剥离 → /photos/2024/vacation/beach.jpg
2. 路径清理  → photos/2024/vacation/beach.jpg   （去掉前导 /，逐段校验）
3. 拼接根目录 → /data/images/photos/2024/vacation/beach.jpg
4. 安全校验  → 解析符号链接 + 确认路径仍在 /data/images 内
5. 返回文件  → 200 OK（分块流式传输）
```

**要点：**

- 必须提供包含所有子目录的**精确相对路径** — 服务器不会跨目录搜索文件名。
- 路径中的每个分段都会被校验：`..`、`.`、隐藏文件（`.git`、`.env`）、空字节均会被拒绝。
- 符号链接会被解析；如果真实路径逃逸出根目录，请求将被拦截。

### 大小单位

大小字段支持整数（`65536`）或可读字符串（`"64KB"`、`"10MB"`、`"2GB"`）。

## 日志

FileHunter 使用 `tracing`，支持环境变量控制日志级别：

```bash
# 默认：info 级别
./filehunter --config config.toml

# debug 级别
RUST_LOG=filehunter=debug ./filehunter --config config.toml
```

## 安全特性

- 前缀段边界匹配（防止 `/imgs` 误匹配 `/imgs-extra/`）
- 前缀在 percent-decode 之前剥离（编码路径无法绕过前缀匹配）
- 路径穿越拦截（`.` / `..` / 符号链接逃逸）
- 空字节注入拒绝
- 隐藏文件和目录（dotfiles）屏蔽
- 所有响应添加 `X-Content-Type-Options: nosniff`
- 连接超时防护 slow-loris 攻击
- 请求大小限制（头部 + 正文）
- 文件大小限制，防止意外提供超大文件
- 配置校验：规范化后重复前缀被拒绝

## 许可证

[MIT](LICENSE)
