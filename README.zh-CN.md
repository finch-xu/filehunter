<p align="center">
  <img src="logo.png" alt="FileHunter" width="200">
</p>

<h1 align="center">FileHunter</h1>

<p align="center">
  <a href="README.md">English</a>
</p>

基于 Rust 构建的高性能多路径文件搜索 HTTP 服务器。

FileHunter 按优先级依次搜索多个配置目录，找到文件后通过分块流式传输返回，未找到则返回 404。适用于文件分布在不同存储路径、需要通过统一端点对外提供服务的场景。

## 亮点

- **多路径优先级搜索** — 配置多个根目录，支持顺序或并发两种搜索模式
- **按路径过滤文件类型** — 每个路径可独立限制允许的扩展名（图片、文档、视频等）
- **异步流式传输** — 基于 tokio + hyper 1.x，使用 `ReaderStream` 分块传输，内存占用极低
- **HTTP/1.1 & HTTP/2** — 通过 `hyper-util` 自动协商协议
- **安全加固** — 路径穿越防护、TOCTOU 缓解、空字节拒绝、隐藏文件屏蔽、`nosniff` 响应头
- **人性化配置** — TOML 格式，支持 `"10MB"`、`"64KB"` 等可读单位
- **极小体积** — 二进制约 3 MB（LTO + strip），空闲 RSS 约 41 MB

## 快速开始

### 源码编译

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

除 `bind` 和 `search.paths` 外，所有字段均为可选，带有合理的默认值。

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

[search]
# mode = "sequential"             # 或 "concurrent"

[[search.paths]]
root = "/data/images"
extensions = ["jpg", "jpeg", "png", "gif", "webp", "svg"]

[[search.paths]]
root = "/data/documents"
extensions = ["pdf", "docx", "xlsx", "txt", "csv"]

[[search.paths]]
root = "/data/general"
# 不限扩展名 — 作为兜底路径接受所有文件类型
```

### 搜索机制

以请求 `/report.pdf` 为例：

1. 检查 `/data/images` — 跳过（`.pdf` 不在允许的扩展名中）
2. 检查 `/data/documents` — 在 `/data/documents/report.pdf` **找到** → 返回文件
3. 若上一步未找到，继续检查 `/data/general`

### 搜索模式

通过 `search.mode` 控制多个根目录的搜索方式：

| 模式 | 行为 |
|---|---|
| `sequential`（默认） | 按配置顺序逐个检查根目录，第一个匹配即返回。行为确定 — 配置顺序决定优先级。 |
| `concurrent` | 同时探测所有符合条件的根目录，最快找到文件的立即响应。其余搜索任务立刻取消以释放资源。 |
| `latest_modified` | 检查所有根目录，返回**修改时间最新**的文件。每次请求都会遍历所有根目录，确保返回最新版本。 |

```toml
[search]
mode = "latest_modified"  # 或 "sequential"（默认）/ "concurrent"
```

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

请求路径可以包含任意深度的子目录。完整的相对路径会直接拼接到每个搜索根目录后面 — 服务器不会按文件名递归搜索。

**示例：** 假设使用上述配置，且文件位于 `/data/images/photos/2024/vacation/beach.jpg`：

```
GET /photos/2024/vacation/beach.jpg

1. 路径清理  → photos/2024/vacation/beach.jpg   （去掉前导 /，逐段校验）
2. 拼接根目录 → /data/images/photos/2024/vacation/beach.jpg
3. 安全校验  → 解析符号链接 + 确认路径仍在 /data/images 内
4. 返回文件  → 200 OK（分块流式传输）
```

如果在 `/data/images` 下未找到该路径，服务器会继续尝试下一个根目录（`/data/documents`、`/data/general` 等）。

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

## 运行时资源使用

> 测试环境：release 构建（LTO + strip），配置 4 条搜索路径（images / documents / videos / general），默认参数。

### 二进制与进程

| 指标 | 数值 |
|---|---|
| 二进制体积 | **~3 MB** |
| 空闲 VmSize | ~1,071 MB |
| 空闲 VmRSS | **~41 MB** |
| 空闲线程数 | 17 |
| 空闲打开文件描述符 | 10 |

### 内存（负载前后对比）

| 阶段 | VmRSS | 线程数 | 打开 FD |
|---|---|---|---|
| 空闲 | 41 MB | 17 | 10 |
| 200 并发混合请求后 | 59 MB | 25 | 10 |

> 内存在负载下保持平稳 — 异步流式传输设计避免将整个文件缓冲到内存中，高并发大文件传输后 RSS 仅增长约 18 MB，FD 数量不变。

### 单请求延迟

| 文件类型 | 大小 | 平均延迟 |
|---|---|---|
| 纯文本 | 11 B | ~1.8 ms |
| 图片（JPG） | 500 KB | ~2.8 ms |
| 文档（PDF） | 800 KB | ~3.5 ms |
| 视频（MP4） | 5 MB | ~13.9 ms |
| 二进制文件 | 5 MB | ~12.9 ms |

### 并发吞吐

| 场景 | 总耗时 |
|---|---|
| 50 并发 × 小文件 (11 B) | ~495 ms |
| 50 并发 × 图片 (500 KB) | ~451 ms |
| 10 并发 × 视频 (5 MB) | ~181 ms |
| 200 并发 × 混合文件 | ~1.8 s |

## 安全特性

- 路径穿越拦截（`.` / `..` / 符号链接逃逸）
- 空字节注入拒绝
- 隐藏文件和目录（dotfiles）屏蔽
- 所有响应添加 `X-Content-Type-Options: nosniff`
- 连接超时防护 slow-loris 攻击
- 请求大小限制（头部 + 正文）
- 文件大小限制，防止意外提供超大文件

## 许可证

[MIT](LICENSE)
