# lan-share · 局域网文件互传

一个用 Rust 写的局域网文件传输服务：在一台设备上启动程序，同一 WiFi / 局域网内的其他设备用浏览器打开地址，即可互相上传、下载文件。前端资源编译进二进制，单文件即可运行，支持 **Windows / macOS / Linux**。

第一阶段（本版本）实现文件传输；视频流共享已在架构上预留（见路线图）。

## 特性

- **并行分片上传**：浏览器把文件切成 8 MiB 分片，多路并发 PUT，服务端按偏移量各写各的，最后统一校验合并；单个 TCP 连接跑不满带宽时效果最明显。
- **并行 Range 下载**：下载接口支持 HTTP `Range`，浏览器开多路请求并行拉取区间并在本地重组，同时兼容 IDM、`curl -C -` 等断点续传工具。
- **流式落盘**：请求体经有界队列流式写入文件，内存占用恒定，超大文件也不会把内存吃满。
- **可续传的接口**：上传会话记录每个分片状态，`GET /api/upload` 可查进度，重复上传同一分片是幂等的。
- **实时协同**：WebSocket 广播文件增删事件，多个浏览器之间自动同步列表。
- **简洁 Web UI**：拖拽上传、进度/速度/剩余时间、搜索、直链复制，自适应深色模式与手机屏幕。
- **可选访问令牌**：`--token` 开启后，接口与 WebSocket 都要求携带令牌。
- **单文件部署**：`cargo build --release` 产物约 2.9 MB，无运行时依赖。

## 快速开始

```bash
# 需要 Rust 1.75+（https://rustup.rs）
cargo run --release
```

启动后终端会打印访问地址：

```
  lan-share · 局域网文件互传
  ─────────────────────────────────────────────
  本机访问    http://127.0.0.1:8080/
  局域网访问  http://192.168.1.5:8080/  ← 把这个地址发给同事
  数据目录    lan-share-data
  分片大小    8 MiB
  已有文件    0 个
  按 Ctrl+C 停止服务
```

把「局域网访问」那一行的地址发给同一网络下的其他设备，用浏览器打开即可互传文件。

### 常用参数

| 参数 | 默认值 | 说明 |
| --- | --- | --- |
| `-p, --port` | `8080` | 监听端口 |
| `--host` | `0.0.0.0` | 监听地址；只想本机访问可设为 `127.0.0.1` |
| `-d, --data-dir` | `lan-share-data` | 文件与元数据保存目录 |
| `--chunk-mib` | `8` | 分片大小（MiB，1~64）；大文件用更大分片，弱网用更小分片 |
| `--token [值]` | 不启用 | 开启访问令牌；留空自动生成随机令牌 |
| `-v, --verbose` | 关 | 打印请求级调试日志 |

```bash
# 换端口 + 开启随机令牌 + 16 MiB 分片
cargo run --release -- --port 9000 --token --chunk-mib 16

# 指定数据目录
cargo run --release -- --data-dir ~/Downloads/lan-share
```

### 打包发布

```bash
cargo build --release          # 产物在 target/release/lan-share(.exe)
```

| 平台 | 产物 | 备注 |
| --- | --- | --- |
| Linux | `target/release/lan-share` | 体积约 2.9 MB |
| macOS | `target/release/lan-share` | 首次运行可能需在「安全性与隐私」放行 |
| Windows | `target\release\lan-share.exe` | 首次运行如遇防火墙提示，请允许「专用网络」 |

### 交叉编译（在任意一台机器上产出三平台二进制）

不需要 macOS 机器、不需要 Apple SDK，也不需要 root：工具链全部装在用户目录下
（默认 `~/.local/share/lan-share-cross`），用 **zig** 作为跨平台链接器，由
`cargo-zigbuild` 调用。

```bash
./scripts/setup-cross.sh      # 一次性安装：rustup + 各目标 std + zig + cargo-zigbuild
./scripts/build-cross.sh      # 构建全部目标，产物在 dist/
```

实用参数：

```bash
./scripts/build-cross.sh --list                    # 查看支持的目标
./scripts/build-cross.sh x86_64-pc-windows-gnu     # 只构建某一个目标
PROFILE=dev ./scripts/build-cross.sh               # 调试构建（不打包）
DIST=... CROSS_ROOT=/opt/toolchain ./scripts/build-cross.sh
```

默认目标（已在 Linux x86_64 上实测全部构建通过）：

| 目标 | 产物 | 说明 |
| --- | --- | --- |
| `x86_64-unknown-linux-gnu` | 2.9 MB | 动态链接，适配常见发行版 |
| `x86_64-unknown-linux-musl` | 2.8 MB | **静态链接**，任意发行版/容器直接跑 |
| `aarch64-unknown-linux-musl` | 2.5 MB | ARM64 服务器、树莓派 |
| `x86_64-pc-windows-gnu` | 2.7 MB | Windows x64（MinGW ABI，无需 MSVC） |
| `x86_64-apple-darwin` | 2.5 MB | macOS Intel |
| `aarch64-apple-darwin` | 2.3 MB | macOS Apple Silicon |

产物同时生成 `tar.gz`（Windows 为 `zip`）与 `dist/SHA256SUMS.txt`。

几点说明：

- 构建 macOS 目标时 rustc 会尝试 `xcrun --show-sdk-path` 并打印一条 SDK 警告，
  这是无害的：符号解析由 zig 自带的 stub 完成，产物实测为正确的 Mach-O。
- 交叉编译出的 macOS 二进制**未签名**，对方首次打开需右键「打开」或在
  「系统设置 → 隐私与安全性」放行。
- Windows 走 GNU ABI 以便在 Linux 上直接产出；如需 MSVC 版请在 Windows 上
  `cargo build --release`（CI 的 native job 会自动做这件事）。
- 国内网络可加镜像：`RUSTUP_DIST_SERVER=https://mirrors.aliyun.com/rustup ./scripts/setup-cross.sh`。

## 使用说明

1. 打开页面，把文件拖进上传区（或点击选择，支持多选）。
2. 「传输队列」显示每个文件的进度、实时速度和总进度。
3. 「共享文件」列表里可以对任意文件操作：
   - **下载**：多连接并行下载并自动重组保存；
   - **直链**：新标签页直接打开（音视频、图片可在线预览）；
   - **复制**：复制直链，可粘贴到下载工具里多线程下载；
   - **删除**：从服务端彻底删除。
4. 右上角「上传并发 / 下载并发」可按网络情况调整，默认 4：
   - 千兆有线：6~8 更合适；
   - WiFi / 弱网：2~4 更稳，过高反而容易因丢包重传变慢。

> 浏览器单文件并行下载需要把数据在内存中重组，因此超过 1.5 GB 会自动回退为浏览器原生下载（同样支持断点续传）。

## 工作原理

```
浏览器                                    lan-share (Rust / axum)
  │ ① POST /api/upload/init  ──────────►  创建会话，预分配临时文件
  │ ② PUT  /api/upload/{id}/chunk/{n} ─►  多路并发，按偏移量各写各的
  │ ③ POST /api/upload/{id}/complete ──►  校验长度 → 移入 files/ → 写 meta/
  │
  │ ④ HEAD /api/files/{id}/download ───►  返回大小与 Accept-Ranges
  │ ⑤ GET  .../download  (Range)   ────►  206 Partial Content，多路并发拉取
  │
  │ ⑥ WS   /ws                        ◄── 文件增删事件广播（实时同步列表）
```

数据目录结构：

```
lan-share-data/
├── files/   已完成文件（<uuid>__<原始文件名>，方便直接到目录里取用）
├── meta/    每个文件一份 JSON 元数据，启动时用于恢复索引
└── tmp/     上传中的临时文件，异常退出后启动时自动清理
```

### HTTP API

| 方法 | 路径 | 说明 |
| --- | --- | --- |
| `GET` | `/api/files` | 文件列表 |
| `GET` | `/api/files/{id}/download` | 下载，支持 `Range`；`?inline=1` 返回内联 |
| `HEAD` | `/api/files/{id}/download` | 取文件大小与 `Accept-Ranges` |
| `DELETE` | `/api/files/{id}` | 删除文件 |
| `POST` | `/api/upload/init` | 创建上传会话 `{name,size}` |
| `PUT` | `/api/upload/{id}/chunk/{n}` | 上传第 n 个分片（请求体为裸字节） |
| `POST` | `/api/upload/{id}/complete` | 校验并完成上传 |
| `DELETE` | `/api/upload/{id}` | 取消上传并清理临时文件 |
| `GET` | `/api/upload` | 进行中的上传及其已收分片 |
| `GET` | `/api/stats` | 文件数、总字节数、运行时长 |
| `GET` | `/ws` | WebSocket 事件流 |

开启令牌后，上述接口都需要 `?token=xxx` 或 `Authorization: Bearer xxx`；静态页面本身不鉴权，令牌通过 URL 传入后会保存在浏览器 localStorage 中。

## 性能说明

本机回环实测 256 MiB 文件（回环无带宽瓶颈，CPU 与客户端脚本是瓶颈）：

| 并发 | 上传 | 下载 |
| --- | --- | --- |
| 1 | 703 MiB/s | 590 MiB/s |
| 4 | 730 MiB/s | 644 MiB/s |
| 8 | 733 MiB/s | 612 MiB/s |

回环下并行收益不明显，**并行真正的价值在真实局域网**：WiFi 下单个 TCP 连接常因 RTT、丢包和拥塞窗口跑不满带宽，多连接并行能明显提高实际吞吐。调整 `--chunk-mib` 与页面上的并发数即可找到本网络的最优组合。

服务端侧的性能设计：

- 上传时多个分片并发写入同一文件的不同区间，每个请求只占用一个有界缓冲，不随文件大小增长；
- 采用 `spawn_blocking` + 有界 channel，磁盘写入不阻塞异步运行时，也不会无限堆积内存；
- 下载使用 `ReaderStream` 流式发送，缓冲区 256 KiB；
- 元数据写入采用「临时文件 + rename」，保证原子性。

## 安全提示

默认**不启用**鉴权，方便家里或办公室随手传输。如果所在网络不完全可信：

```bash
cargo run --release -- --token            # 自动生成随机令牌
cargo run --release -- --token mySecret   # 指定令牌
```

另外建议：传输敏感文件时使用自己的热点或可信 WiFi；服务只应监听局域网，不要直接暴露到公网（如需公网访问，请放在 VPN 或反向代理 + TLS 之后）。

## 测试

```bash
# 端到端接口测试（32 项：上传/下载/Range/416/乱序分片/删除/鉴权/WebSocket）
python3 scripts/e2e_test.py http://127.0.0.1:8080

# 吞吐量基准
python3 scripts/bench.py http://127.0.0.1:8080 256

# 真实浏览器测试（无头 Chrome 模拟拖拽上传 + 并行下载，可输出截图）
python3 scripts/browser_test.py http://127.0.0.1:8080 /tmp/ui.png

# 代码检查
cargo clippy --all-targets && cargo fmt --check
```

## 路线图

- **v1（当前）** 文件互传：并行分片上传、Range 并行下载、实时列表同步。
- **v2** 视频流共享（类直播）：浏览器 `getUserMedia` 采集 → WebRTC / WebSocket 分发 → 其他设备用 `<video>` 播放；对不支持 WebRTC 的场景提供 fMP4 分片 + `MediaSource` 的降级方案。
- **v3** 体验增强：上传断点自动续传（刷新页面后继续）、多文件打包下载、扫码访问、目录级分享。

## 许可

MIT
