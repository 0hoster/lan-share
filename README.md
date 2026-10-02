# lan-share · 局域网文件互传 + 直播录屏

一个用 Rust 写的局域网互传服务：在一台设备上启动程序，同一 WiFi / 局域网内的其他设备用浏览器打开地址，即可互相上传、下载文件，也可以**把屏幕或摄像头实时直播给其他人看**。前端资源编译进二进制，单文件即可运行，支持 **Windows / macOS / Linux**。

## 特性

- **并行分片上传**：浏览器把文件切成 8 MiB 分片，多路并发 PUT，服务端按偏移量各写各的，最后统一校验合并；单个 TCP 连接跑不满带宽时效果最明显。
- **并行 Range 下载**：下载接口支持 HTTP `Range`，浏览器开多路请求并行拉取区间并在本地重组，同时兼容 IDM、`curl -C -` 等断点续传工具。
- **流式落盘**：请求体经有界队列流式写入文件，内存占用恒定，超大文件也不会把内存吃满。
- **可续传的接口**：上传会话记录每个分片状态，`GET /api/upload` 可查进度，重复上传同一分片是幂等的。
- **实时协同**：WebSocket 广播文件增删事件，多个浏览器之间自动同步列表。
- **简洁 Web UI**：拖拽上传、进度/速度/剩余时间、搜索、直链复制，自适应深色模式与手机屏幕。
- **可选访问令牌**：`--token` 开启后，接口与 WebSocket 都要求携带令牌。
- **直播录屏**（v0.2）：浏览器共享屏幕/摄像头即可开播，其他人点开就能看；服务端可同时把直播录成 WebM 文件，结束后自动出现在共享文件列表里。
- **单文件部署**：`cargo build --release` 产物约 3 MB，无运行时依赖。

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

## 直播录屏

切到页面顶部的「直播录屏」标签即可使用：

1. **开播**：填写标题（可选），选择是否把录像保存到服务器，点「共享屏幕开播」或「摄像头开播」，在浏览器弹窗里选中要共享的屏幕 / 窗口 / 标签页。勾选「同时采集麦克风」可以把麦克风与系统声音混在一起推出去。
2. **观看**：任何人打开同一个地址，在「正在直播」列表里点「观看」即可，延迟通常在 1~3 秒。播放器下方会显示已直播时长、落后直播点的秒数、已接收的数据量和观看人数。
3. **结束**：点「结束直播」（或直接在浏览器自带的共享提示条上点「停止共享」）。如果勾选了保存录像，服务端会把整段直播写成 `录屏-<标题>-<时间>.webm` 放进共享文件列表，其他人可以直接下载或在线预览。

几点说明：

- 直播走的是 **WebSocket + MediaSource**，不需要额外安装插件，也不需要 WebRTC 信令服务器；代价是延迟比 WebRTC 高一些（分片间隔 1 秒，实测端到端约 1~3 秒）。
- 采集需要 HTTPS 或 localhost 这两个安全上下文。局域网里用 `http://192.168.x.x:8080` 打开时，Chrome 仍然允许共享屏幕（getDisplayMedia 在局域网 IP 上可用），但如果浏览器拒绝，可以改用 `localhost` 或给服务套一层 HTTPS 反代。
- 观看端依赖 MSE 播放 WebM：Chrome / Edge / Firefox 支持，**Safari 目前无法直接观看**（可以下载录像文件，Safari 能播 WebM 文件）。用 Chrome/Edge 观看体验最好。
- 同一时间最多 8 路直播、每路最多 32 个观众；主播掉线（90 秒没有新数据）会被自动结束并保存已录部分。

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

直播链路（v0.2）：

  │ ① POST /api/live/start          ──►  建房间，返回房间号与主播密钥
  │ ② PUT  /api/live/{id}/chunk     ──►  MediaRecorder 每秒一片，顺序推送
  │                                      服务端：留存首个「初始化分片」+ 最近分片环缓冲
  │                                              （可选）同时写录像文件
  │ ③ WS   /api/live/{id}/ws        ◄──  观众先收 info + 初始化分片 + 最近分片，
  │                                      之后实时收新分片，交给 MediaSource 播放
  │ ④ POST /api/live/{id}/stop      ──►  结束直播，录像转正并进入文件列表
```

数据目录结构：

```
lan-share-data/
├── files/   已完成文件（<uuid>__<原始文件名>，方便直接到目录里取用）
├── meta/    每个文件一份 JSON 元数据，启动时用于恢复索引
└── tmp/     上传中的临时文件与直播录像中间文件，异常退出后启动时自动清理
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
| `GET` | `/api/live/rooms` | 正在进行的直播列表 |
| `POST` | `/api/live/start` | 开播：`{title, mime, record}` → `{room_id, key}` |
| `PUT` | `/api/live/{id}/chunk` | 主播推送分片（裸字节，需 `x-live-key` 头） |
| `POST` | `/api/live/{id}/stop` | 结束直播；开启录制时返回生成的录像文件 |
| `GET` | `/api/live/{id}/ws` | 观众连接：先收一条 `info` 文本，随后是二进制分片 |

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
# 端到端接口测试（49 项：上传/下载/Range/416/乱序分片/删除/鉴权/WebSocket/直播推流与录像）
python3 scripts/e2e_test.py http://127.0.0.1:8080

# 吞吐量基准
python3 scripts/bench.py http://127.0.0.1:8080 256

# 真实浏览器测试（无头 Chrome：拖拽上传、并行下载、真实 MediaRecorder 开播 + MSE 观看）
python3 scripts/browser_test.py http://127.0.0.1:8080 /tmp/ui.png

# 代码检查
cargo clippy --all-targets && cargo fmt --check
```

## 路线图

- **v0.1** 文件互传：并行分片上传、Range 并行下载、实时列表同步。
- **v0.2（当前）** 直播录屏：屏幕/摄像头开播、WebSocket + MediaSource 转发、服务端视频录像。
- **v0.3** 体验增强：上传断点自动续传（刷新页面后继续）、多文件打包下载、扫码访问、直播画质/码率选择、WebRTC 低延迟模式。

## 许可

MIT
