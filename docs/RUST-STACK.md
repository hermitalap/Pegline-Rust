# Rust 技术栈与工程约定

状态：**选型已定，尚未落盘**。版本号是编写本文时的 crates.io 最新稳定版，创建 `Cargo.toml`
时以 `cargo add` 的结果为准，并提交 `Cargo.lock`。

---

## 1. 工具链

| | |
|:--|:--|
| 工具链 | stable `x86_64-pc-windows-msvc`（本机已装 1.96.1） |
| Edition | 2024 |
| 最低版本 | 1.85（Edition 2024 起点） |
| 目标平台 | Windows 10 1809+ / Windows 11，x64（ARM64 暂不做） |
| 链接 | 静态 CRT：`-C target-feature=+crt-static` |

`+crt-static` 很重要：目标是**单文件、零运行时依赖**，和现在的 WPF 版一样拷过去就能跑。
`windows` crate 与静态 CRT 兼容，但 M0 要验证一遍链接结果。

---

## 2. 依赖

| crate | 版本 | 用途 | 备注 |
|:--|--:|:--|:--|
| `windows` | 0.62 | 全部 Win32 / COM API | 只依赖这一个，`windows-core` 由它再导出 |
| `tray-icon` | 0.26 | 托盘图标 | 底层就是 `Shell_NotifyIcon`；也可自己写 |
| `muda` | 0.21 | 托盘菜单 / 右键菜单 | 若不想要依赖，可退化为 `TrackPopupMenu` |
| `embed-resource` | 2.x | 构建时嵌入 `.ico` 与清单 | build-dependency |
| `anyhow` | 1.x | 应用层错误 | 只在边界用，不进入热路径 |

**不引入**：`winit`、`egui`、`iced`、`image`（用 WIC）、`serde`、`tokio`、
`log`/`env_logger`（自己写 30 行文件日志）。依赖越少，单文件越小、审计越容易。

文件监听用 `ReadDirectoryChangesW` 直接实现（约 150 行），不引入 `notify`：
`notify` 当前最新是 9.0.0-rc，稳定线 8.x 也能用，但我们要的“文件还在写就下次重试”
逻辑自己控制更直接。

### 2.1 `windows` crate 需要开启的 feature

```toml
[dependencies.windows]
version = "0.62"
features = [
  "Win32_Foundation",
  "Win32_Graphics_Direct3D",
  "Win32_Graphics_Direct3D11",
  "Win32_Graphics_Dxgi",
  "Win32_Graphics_Dxgi_Common",
  "Win32_Graphics_Direct2D",
  "Win32_Graphics_Direct2D_Common",
  "Win32_Graphics_DirectWrite",
  "Win32_Graphics_DirectComposition",
  "Win32_Graphics_Imaging",
  "Win32_Graphics_Gdi",
  "Win32_UI_Shell",
  "Win32_UI_WindowsAndMessaging",
  "Win32_UI_Input_KeyboardAndMouse",
  "Win32_System_Com",
  "Win32_System_Com_StructuredStorage",
  "Win32_System_DataExchange",
  "Win32_System_Memory",
  "Win32_System_Ole",
  "Win32_System_Threading",
  "Win32_System_LibraryLoader",
  "Win32_System_Registry",
  "Win32_System_SystemInformation",
  "Win32_System_SystemServices",
  "Win32_System_ProcessStatus",
  "Win32_Security",
]
```

（feature 名以 `cargo doc` 里 `windows` crate 的实际模块为准；M0 时用编译器报错逐个补。）

---

## 3. 工程结构

```toml
[package]
name = "pegline"
version = "2.0.0"
edition = "2024"
rust-version = "1.85"
description = "Screenshots hanging on a clothesline at the top of the screen"

[[bin]]
name = "pegline"
path = "src/main.rs"
# #![windows_subsystem = "windows"] in main.rs

[[bin]]
name = "probe"
path = "src/probe.rs"

[profile.release]
opt-level = 3
lto = "thin"
codegen-units = 1
panic = "unwind"      # UI 线程需要 catch_unwind 兜底
strip = "symbols"

[target.x86_64-pc-windows-msvc]
rustflags = ["-C", "target-feature=+crt-static"]
```

`build.rs` 用 `embed-resource` 嵌入：

- `assets/pegline.manifest`（Per-Monitor V2 DPI 感知、`longPathAware`、`supportedOS` 全列）；
- `assets/pegline.ico`（16–256 多尺寸，PNG 压缩条目）。

图标由 `tools/make-icon`（Rust bin，D2D 或 GDI+ 绘制）在构建前生成，
和 WPF 版“代码画图标”保持一致；生成物放 `target/` 或 `assets/generated/`，不入库。

---

## 4. 构建与运行

```powershell
cargo build --release
target\release\pegline.exe
```

开发期：

```powershell
cargo run --bin probe -- --mode=flight
cargo clippy --all-targets -- -D warnings
cargo fmt --check
cargo test
```

单文件检查：`dumpbin /dependents target\release\pegline.exe` 不应出现
`vcruntime140.dll` / `msvcp140.dll`（静态 CRT 的目标）。

---

## 5. 测试策略

| 层 | 方式 |
|:--|:--|
| 纯逻辑 | `#[test]`：`RopeY`、`X`、`PhotoSize`、`CardSize`、命中框、预览位置、截取区域推断、快捷键解析、设置解析、弹簧积分 |
| 图像 | 便签渲染、图标渲染输出 PNG，与基准图逐像素/感知哈希比较 |
| 渲染 | `probe` bin：真实窗口、真实动画，输出 fps / p50 / p95 / CPU / 私有内存 |
| 端到端 | 交互式：`docs/PARITY.md` 的清单，人工跑一遍 |
| 稳定性 | 设备丢失、DPI 切换、休眠唤醒、连续 200 次截图（看内存曲线） |

CI（GitHub Actions `windows-latest`）：`fmt`、`clippy -D warnings`、`test`、`build --release`。
渲染与交互测试不能进 CI，只能本地跑。

---

## 6. COM 与 unsafe 约定

- 每个线程进入 COM 前显式 `CoInitializeEx`：UI 线程 STA，渲染/worker 线程 MTA。
- `windows` crate 的对象已实现 `Drop`，不要手写 `Release`。
- `unsafe` 只出现在三处：Win32 调用封装、COM 接口调用、D3D 资源创建；
  每个 `unsafe` 块上方必须有一行注释说明前置条件。
- 不在 `Drop` 里调用会失败的外部操作（托盘、文件）；显式 `shutdown()`。
- 消息循环里 `catch_unwind` 包住 `wndproc` 外的分发，单次异常不能让托盘图标消失。

---

## 7. 日志

- `%APPDATA%\Pegline\error.log`：panic、HRESULT 失败、IO 失败。
- `%APPDATA%\Pegline\trace.log`：仅当环境变量 `PEGLINE_TRACE=1` 或
  `%APPDATA%\Pegline\trace.on` 存在时记录（截图、拖拽、双胞胎、飞行）。
- 日志写入用 `std::fs::OpenOptions::append`，失败静默；不要因为日志失败影响主流程。
- 格式：`2026-11-02T17:20:01.123+08:00 LEVEL message`。

---

## 8. 打包与体积

目标：**单文件、免安装、3–15 MB**。Rust 静态链接 + `windows` crate 的基线通常在 3–6 MB，
余量充足；如果超了，按顺序做：

1. `strip = "symbols"`、`lto = "thin"`、`codegen-units = 1`（已在 profile 里）。
2. 链接参数 `/OPT:REF /OPT:ICF`。
3. 检查是否误引入了重型依赖（`image`、`regex`、`serde` 全量、`tokio`）。
4. 资源压缩：图标多尺寸用 PNG 条目；多语言字符串用 `&'static str` 表，不进资源 DLL。

- 不做安装包；开机启动写 `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`。
- 版本号写入 exe 资源（`VERSIONINFO`），托盘提示显示 `Pegline <version>`。
- 发布页附 SHA-256；可选代码签名（暂不做，签名证书成本高于收益）。

## 8.1 帧率与 120 Hz

- 渲染循环用 `Present(1, 0)` 跟随显示器；不要写 `Sleep(16)` 之类。
- 帧预算按 8.3 ms（120 Hz）设计，`probe` 报告 p50/p95/max。
- 设置项 `fps=auto|60|120` 默认 `auto`；`60` 用“跳过 Present + 合并 dt”实现，不用 Sleep。
- 120 Hz 屏上弹簧与淡入淡出必须按真实 `dt` 积分，不能按帧数计数。

---

## 9. 迁移兼容

Rust 版必须直接复用 WPF 版的数据：

- `%APPDATA%\Pegline\settings.ini`、`pegged.txt`（格式见 `docs/BEHAVIOR.md` §12.3）；
- `%LOCALAPPDATA%\Pegline\Screenshots`、`Notes`；
- 注册表 Run 键同名 `Pegline`，两个版本会互相覆盖——首次启动检测到旧值指向
  `Pegline.exe` 时提示一次，由用户决定是否接管。

这样用户可以先用 Rust 版跑一周，不满意再切回 WPF 版，线带上的图不会丢。

---

## 10. 许可与命名

- 代码 MIT，沿用仓库根目录的 `LICENSE`。
- 上游 Tendedero 的许可要求修改版**换名、换图标**；`Pegline` 这个名字和图标已经满足，
  重写时继续使用，不要改回 `Tendedero`。
