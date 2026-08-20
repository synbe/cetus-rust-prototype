# Rust rewrite design and migration plan

直接回答：重写 Cetus 为 Rust 能带来更低的延迟、更高的并发吞吐、更细粒度的内存控制和更好的零拷贝管道。下面给出具体的改进思路、技术选型、架构与实现要点、权衡以及一个分阶段迁移计划（含性能/体积优化建议），你可以按此做原型或完整重写。

## 一、总体目标（为什么用 Rust）
- 更低的运行时开销：更高效的内存管理（无垃圾回收停顿）、更少的分配开销与更可控的内存复用（buffer pools、mmap）。  
- 更高吞吐与并发：零成本抽象 + 高度并行的 CPU 工作（像图像解码、颜色空间转换）用 rayon / threads 更高效。  
- 更强的安全性：编译时内存安全（减少内存错误、UAF、缓冲区溢出）。  
- 更灵活的二进制打包：编译器优化（LTO、strip、panic=abort）可做体积/性能折中。

## 二、核心改进点（与现有 Go 实现对比）
1. 减少 PNG 编码/解码开销
   - 现状：chromedp/CDP 返回 PNG（base64），程序把 PNG bytes 直接写入 ffmpeg image2pipe（png），PNG IO 与压缩开销大。  
   - 改进：
     - 优先使用 Chrome 的 screencast/jpeg 输出（更小的图片）并直接喂给 ffmpeg 的 mjpeg image2pipe（-f image2pipe -vcodec mjpeg -i pipe:0），避免在进程内进行 PNG 编/解码；或者
     - 若必须解码为原始像素，使用 libjpeg-turbo 的 Rust 绑定（tj）或 libwebp 的高速解码，然后把 rawvideo（RGBA 或 RGB24）直接写入 ffmpeg（-f rawvideo -pixel_format rgba -video_size WxH -framerate N -i pipe:0）。rawvideo 路线在 CPU 上更友好（避免 PNG 压缩/解压的高开销），且 ffmpeg 自身更擅长做色彩转换/编码。
2. 减少内存拷贝与分配
   - 使用 Bytes / bytes::BytesMut、mmap（memmap2）或预分配的 buffer pool 来复用帧缓冲区，减少 GC/堆分配。  
   - 对管道写入采用非阻塞或专用写线程，使用零拷贝 where possible（避免在每帧上分配新的 Vec<u8>）。
3. 更优的并发模型
   - 使用 tokio (async) + blocking threads 或直接使用 sync + rayon：
     - 网络/WS/CDP/IO 用 async tokio，便于同时管理多个浏览器连接或下载资源。
     - CPU密集型任务（JPEG 解码、YUV 转换）用 rayon 或 dedicated thread pool，这样不会阻塞 async reactor。
4. 更高效的 worker/任务划分
   - 采用工作队列 + 固定线程池 + buffer pool，避免频繁构建/销毁浏览器实例；或基于 Chrome 实例池复用 target/contexts。
   - 对长序列帧使用分段并行：若 chrome 无法多实例一致性渲染，可使用多个独立 headless instances 各自负责帧范围（现有 Go 的 workers 思路在 Rust 中更高效）。
5. 可选的零磁盘流（stream-first）与高效率缓存
   - 支持直接管道（browser -> process memory -> ffmpeg）以减少 IO。  
   - 对于需要 resume 的场景，使用压缩帧缓存格式（比如以 webp 或 zstd 压缩的文件）而不是 PNG 序列以节省磁盘与 IO。
6. 与 ffmpeg 的交互
   - 继续使用外部 ffmpeg（二进制），因为实现编码器代价高且复杂。以 process::Command 管道写入或用 ffmpeg libs（libav*) 以内嵌方式（存在绑定复杂度与跨平台问题）。
7. 保证确定性
   - 固定 Chromium / ffmpeg 版本（捆绑或明确下载），在代码中强制使用特定 flags。所有随机/系统依赖点设为可配置且记录在 manifest。

## 三、Rust 具体技术栈建议
- CLI 和配置：clap（或 argh）  
- CDP / Chrome 控制：
  - chromiumoxide（async, tokio-based, 基于 CDP，维护较好） 或 headless_chrome（同步实现）  
  - 直接 WebSocket + cdp-json via async-tungstenite + serde_json（若要自定义实现）
- 图片压缩/解码：
  - libjpeg-turbo bindings: tj-rs / turbojpeg-sys（高性能 JPEG 解码）  
  - libwebp bindings 或 webp crate（WebP 支持）  
  - image crate（通用，但纯 Rust 实现可能慢于 libjpeg-turbo）
- 视频编码：
  - 方式A（首选）：spawn ffmpeg binary (std::process::Command) + write to stdin (image2pipe 或 rawvideo)  
  - 方式B（内嵌）：ffmpeg-sys / ffmpeg-next crates（复杂，建议仅在需要超低延迟或更细控制时考虑）
- 并发/异步：tokio (async) + rayon（CPU-bound）  
- Buffer 管理：bytes crate、memmap2、pooling via crossbeam or custom pools  
- 日志/追踪：tracing + tracing-subscriber  
- 序列化：serde / serde_json  
- 测试工具：assert_cmd、tempfile、insta（snapshot tests）

## 四、关键实现策略（更具体）
1. Capture pipeline（browser 控制）
   - 使用 chromiumoxide 建立一个 WebSocket CDP 会话；选择使用 Page.startScreencast(format="jpeg", quality=N) 或 HeadlessExperimental.beginFrame（若需要非常精确），并实现 frame-seek script（与现有 JS 脚本功能一致）。  
   - 如果使用 screencast/jpeg：浏览器会 push 每帧 base64 jpeg 事件 -> Rust worker 取出 raw jpeg bytes -> 直接写入 ffmpeg stdin（image2pipe + vcodec mjpeg）。优点：极少的 CPU 解码开销（全部交给 ffmpeg，如果使用 mjpeg input，ffmpeg 内部解码），网络/IPC payload 更小。  
   - 如果需要 feed rawvideo：使用 libjpeg-turbo 在 Rust 内部快速解码 jpeg -> color convert (RGBA->YUV420) -> write rawvideo to ffmpeg stdin. 代价是额外的 CPU；但可获得更一致的像素格式控制。  
2. Encoder pipeline（ffmpeg）
   - Prefer: ffmpeg args like:
     - For jpeg stream: ffmpeg -f image2pipe -vcodec mjpeg -r {fps} -i pipe:0 ... (then audio/subs/filters as in Go impl)
     - For rawvideo: ffmpeg -f rawvideo -pixel_format rgba -video_size {WxH} -framerate {fps} -i pipe:0 ...
   - Build filter_complex like现有实现（scale, subtitles, audio filters）。  
   - Spawn ffmpeg and stream frames asynchronously; monitor stderr and exit status.
3. Frame cache / resume
   - For caching on disk, store frames in compressed webp or zstd stream to reduce disk space. Keep a JSON manifest like config.cetus. Use atomic write + rename for each frame file.  
   - Alternatively, store frames in a single ARC file (custom archive) to reduce number of files (faster FS operations).
4. Determinism
   - Pin Chrome and ffmpeg versions; set environment variables and ffmpeg flags to deterministic encoding (CRF and no variable metadata). Use reproducible build flags.
5. Binary size优化
   - Release build with LTO, codegen-units = 1, strip symbols (strip binary), panic = 'abort', remove debug prints. If desired, produce two builds: debug (dev) and tiny-release (optimized).
   - Avoid heavy dependencies (e.g., avoid linking libav statically unless necessary); prefer dynamic linking to shrink static binary, or use musl if static is required (but musl can increase size for some crates).

## 五、性能/资源优化清单（细节）
- 用 image2pipe + mjpeg to ffmpeg 路线可减少 CPU 与内存压力（如果 Chrome 可以输出 JPEG）。PNG 压缩/解压最耗 CPU，避免 PNG。
- 使用 frame buffer pool（Vec<u8> reuse）减少堆分配。
- CPU-bound工作交由 rayon，IO-bound用 async tokio；避免把 CPU 工作挂在 tokio 单线程 reactor。
- 对 disk cache 使用 fewer files（pack frames into chunked archives）以避免 inode 开销与 slow directory listing。
- 使用 memory mapped files for reading/serving static assets（fonts/images）给 Chrome 时，减少 copying。
- 对跨-platform，针对 Windows 特性（process pipes）做适配，测试在 Linux/macOS/Windows。

## 六、架构/模块布局（Rust 项目建议目录）
- src/
  - bin/cetus.rs (CLI entry, clap)
  - crate-compose/ (解析 composition、timeline)
  - crate-browser/ (cdp client wrapper, session pool, seek script injector)
  - crate-capture/ (frame pipeline, worker queue, buffer pool)
  - crate-encoder/ (ffmpeg args builder + encoder process wrapper)
  - crate-cache/ (frame cache formats, manifest read/write)
  - crate-utils/ (logging, metrics, config)
- tests/ (integration tests using examples/smoke.html)

## 七、迁移/迭代计划（分阶段）
阶段 0 — 验证性原型（2-4 周）
- 目标：实现最小端到端路径：Chrome（screencast jpeg） -> Rust 程序 -> ffmpeg (mjpeg) -> out.mp4。  
- 库：chromiumoxide 或直接用 async-tungstenite + minimal CDP call; spawn ffmpeg; no frame cache。  
- 成果：能用 examples/smoke.html 生成可播放的 mp4。测基线吞吐与延迟。

阶段 1 — 性能优化（2-4 周）
- 添加 buffer pool、预分配、并发 workers，并比较 PNG vs JPEG vs rawvideo 路线的 CPU/IO/内存。  
- 添加音频 / subtitles 支持（复用 Go 的 ffmpeg 参数构造逻辑）。

阶段 2 — 稳定与特性匹配（2-4 周）
- 实现 frame cache + manifest + resume、BeginFrame 路线的实现支持、更多 CLI 参数与检测版本 pin。  
- 集成 LTO、panic=abort、strip，做二进制 size 测试。

阶段 3 — 替换与扩展（2-6 周）
- 扩展 pkg/cetus 风格的 library API（供其它 Rust 程序调用）。  
- 运行完整回归测试，对比输出哈希（确定性测试）。

## 八、风险与权衡
- Chromium API 兼容性：不同 Chrome 版本对 beginFrame、screencast 等支持度不同，需 pin 版本并测试。  
- 二进制体积：Rust 可能引入大静态二进制（尤其链接 libjpeg/libwebp 等本地库）；用 LTO+strip 可以显著减少，但与 Go 的静态/动态差别要测试。  
- 实现复杂度：实现高效的解码/颜色转换管道需要良好工程（native bindings、线程调度），初期比 Go 版本实现成本更高。  
- 平台支持（Windows）：某些低级优化（mmap、pipe behavior）在 Windows 需要额外处理。

## 九、示例策略（具体实现建议）
- 优先尝试：Chrome -> screencast JPEG -> Rust -> spawn ffmpeg with mjpeg input. 这通常是最快能达到“更快+更小内存”的优化路径。  
- 如果需要更低延迟或更好的像素控制，再做 jpeg-decode->rawvideo pipeline（使用 libjpeg-turbo + rayon），然后 feed rawvideo。  
- 保留可选 PNG 支持以兼容现有行为，但把它设为次要首选。

## 十、度量指标（用于验证“更快/更小”）
- 单帧延迟（从 seek 到帧写入 ffmpeg stdin 的时间分布）  
- 帧吞吐（frames/sec）在单实例与多 worker 下  
- CPU 使用率 / 内存占用 / 磁盘 IO  
- 最终二进制大小（strip + LTO）  
- 确定性检验（相同 inputs 多次生成文件的字节差异或解码后的像素哈希）

结尾与下一步
- 我可以帮你做下列任一件事（选一个）：
  1. 写一个 Rust 原型：Chrome screencast (jpeg) -> Rust -> ffmpeg(mjpeg)（包含示例代码、Cargo.toml、简单 benchmark）。  
  2. 根据你的优先级，生成 encoder args builder 的 Rust 版本（与现有 Go 功能等价）。  
  3. 给出 BeginFrame + rawvideo 路线的参考实现（带 libjpeg-turbo 解码与 rawvideo 管道的示例）。  

你希望先做哪个原型或具体部分？
