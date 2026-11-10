# Pegline-Rust

**把截图挂在屏幕顶部的晾衣绳上。** · Screenshots, strung on a clothesline at the top of your screen.

[![CI](https://github.com/hermitalap/Pegline-Rust/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/hermitalap/Pegline-Rust/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/hermitalap/Pegline-Rust?sort=semver)](https://github.com/hermitalap/Pegline-Rust/releases/latest)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Platform](https://img.shields.io/badge/platform-Windows%2010%20%7C%2011%20x64-0078D4)](https://github.com/hermitalap/Pegline-Rust/releases/latest)

<p align="center">
  <picture>
    <source srcset="docs/demo/pegline-demo.webp" type="image/webp">
    <img src="docs/demo/pegline-demo.png" alt="Pegline-Rust 演示：挂绳上的截图卡片、大图预览、复制提示、Markdown 便签编辑器和标注编辑器" width="1200">
  </picture>
</p>

<p align="center"><sub>23 秒看完主要流程：预览 → 复制 → Markdown 便签 → 标注 → 保存回挂。<br>演示中的截图与便签均为合成素材，不含私人内容。</sub></p>

## 截图，不该被窗口挡住

写文档、对设计稿、整理素材时，截图常常散落在桌面、下载文件夹和聊天窗口里。Pegline 把它们挂到屏幕顶部的一条晾衣绳上：指针碰一下屏幕上沿，绳子滑下来；移开，绳子收回去。需要哪张，抬眼就能看到，不用在窗口之间来回切换。

Pegline-Rust 是 [Pegline](https://github.com/2829632740/Pegline) 的 **Rust 重写版**，本仓库是它的公开 fork。行为保持一致，渲染、截图、标注、便签整条链路改为原生实现，因此更小、更轻，动画也更稳。

- **不抢焦点**：只有卡片区域接收鼠标，其余点击穿透；复制、预览、标注都不会把你正在写的文档切走。
- **一直在手边**：置顶显示，支持多显示器和逐显示器 DPI；全屏应用会自动让位。
- **动画是产品的一部分**：弹簧按真实帧间隔积分，帧循环跟随显示器刷新率，目标 60–120 Hz。

## 能做什么

- **挂绳**：贴屏幕顶部、始终置顶；指针靠近才出现，离开自动收回，也可以设为常驻。
- **截图**：托盘一键截图（调用 Win+Shift+S）、监听截图文件夹、粘贴剪贴板图片；重复截图自动合并。
- **卡片**：单击复制（图片 + 文件）、双击打开、右键菜单；悬停显示大图预览；可拖进应用、文件夹或回收站。
- **标注**：箭头、矩形、文字（支持中文输入法）、马赛克、橡皮擦；6 种颜色、3 档线宽，撤销 / 重做。
- **Markdown 便签**：在纸面上直接写标题、列表、任务框、引用、行内代码、代码块、链接和分隔线，保存后按 Markdown 渲染；四种纸色，悬停预览，编辑后飞回绳子。
- **托盘与快捷键**：截图 / 便签快捷键可录制；开机启动、音效、挂绳常驻、清空截图；en / es / zh 三语。
- **观感**：从壁纸取色的半透明玻璃、预烘焙阴影、逐显示器 DPI、跟随刷新率的帧循环。

便签里可以直接写：

```markdown
# Launch checklist
- [x] Freeze the build
- [ ] Record the demo
> Small, light, fast.
```

## 小、轻、快

| | 实测 |
|:--|:--|
| 单文件 exe | **1.8 MB**，无安装器，不需要 .NET 或 VC++ 运行时 |
| 静置内存 | **约 66 MB** 私有内存 |
| 挂六张截图 | **约 97 MB** 私有内存 |
| 冷启动 | **约 0.1 秒**（2.2.0 测得 105–107 ms） |
| 挂绳动画 | **约 60 fps**，绘制 p50 1.6 ms |
| 连续截图 | 200 张后内存 **+3.7%** |

> 数据来自本机 release 构建和内置测量工具 `probe`；测试机为单屏、200% DPI、60 Hz。120 Hz 屏幕、第二显示器和睡眠/唤醒仍在硬件测试清单上，这里不写没有测过的数字。

## 下载

[**最新版本**](https://github.com/hermitalap/Pegline-Rust/releases/latest) · 下载 `pegline.exe`，双击运行。

- 单文件、绿色版：约 1.8 MB，无需安装，也不需要 .NET 或 VC++ 运行时。
- 系统要求：Windows 10 1809+ / Windows 11，x64。
- 数据位置：设置与日志在 `%APPDATA%\Pegline`，便签在 `%LOCALAPPDATA%\Pegline`；与 WPF 版共用，可以直接互换。

## 从源码构建

需要 **Rust 1.85+**（stable、MSVC 工具链）和 Windows SDK（`rc.exe` 用来嵌入图标与 manifest）。

```powershell
cargo build --release          # target\release\pegline.exe
cargo test                     # 纯逻辑测试
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

发布由 GitHub Actions 完成：推送 `v*` 标签后，流水线校验标签与 `Cargo.toml`、exe 版本资源一致，构建并直接发布 `pegline.exe`。

## 怎么做的

- **渲染**：DirectComposition + D3D11 swap chain + Direct2D；overlay 由 DWM 合成，不用 layered window，也不整窗 CPU 光栅化。
- **动画**：弹簧按真实 `dt` 积分，帧循环跟随显示器刷新率；阴影只烘焙一次，文字和几何缓存复用，渲染循环里没有逐帧堆分配。
- **截图与 IO**：解码、编码、比较和文件操作放在 worker pool，UI 线程不阻塞。
- **材质**：Mica 从壁纸采样，壁纸或显示器变化时刷新；壁纸或透明效果不可用时回退到半透明填充。
- **标注**：源图最多按 2048 px 解码，图片与标记合成到不透明卡片位图以保证 ClearType；窗口使用预乘 swap chain。
- **便签**：纸面与 Markdown 投影共用同一套绘制代码，编辑时是 WYSIWYG；文件保持 `.txt` + BOM + CRLF，保存采用原子写入并归档旧版本。

## 仓库结构

| 路径 | 内容 |
|:--|:--|
| `src/` | 应用（`main.rs`、`app/`、`window/`、`editor/`、`note_editor/`、…）与测量工具 `probe` |
| `assets/` | 图标、DPI manifest、资源脚本、Lucide 工具图标 |
| `docs/` | 行为规范、架构、实现决策、技术栈与截图 |
| `docs/demo/` | README 的 WebP 演示与 PNG 回退（合成素材） |
| `tools/` | 压测与可视化检查脚本 |
| `.github/workflows/` | CI 与发布流水线 |

## CI / CD

- **CI**（[ci.yml](.github/workflows/ci.yml)）：每次 push 到 `main` 和每个 PR，在 `windows-latest` 上跑 `cargo fmt --check`、`clippy -D warnings`、`cargo test` 和 release 构建，校验 exe 版本并上传 artifact。
- **Release**（[release.yml](.github/workflows/release.yml)）：推送 `v*` 标签时校验标签与 `Cargo.toml`、exe 版本资源一致，构建后用 `gh` 直接发布 `pegline.exe`，说明取自 `CHANGELOG.md` 对应段落。

## 文档

| 文档 | 内容 |
|:--|:--|
| [docs/BEHAVIOR.md](docs/BEHAVIOR.md) | 完整的用户可见行为规范（数值来自 WPF 原版） |
| [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) | 线程模型与 Win32 + DirectComposition 架构 |
| [docs/DECISIONS.md](docs/DECISIONS.md) | 实现决策、被否决的优化与被修正的行为 |
| [docs/RUST-STACK.md](docs/RUST-STACK.md) | 工具链、crate、构建与发布约定 |
| [CHANGELOG.md](CHANGELOG.md) | 版本变更记录 |

> 文档目前为中文；行为规范是 Rust 版与 WPF 版逐条对照的依据。

## 上游与许可

Pegline 是 [Tendedero](https://github.com/alejandrobujan/tendedero)（macOS，作者 Alejandro Buján）的 Windows 移植；Pegline-Rust 是 Pegline 的 Rust 重写，本仓库是它的公开 fork。原 WPF 实现不在本仓库中，仍由[上游项目](https://github.com/2829632740/Pegline)维护。

MIT，见 [LICENSE](LICENSE)。Pegline 的名字和图标不在 Tendedero 的授权范围内，因此 Windows 移植使用了自己的名字和图标；本仓库沿用 Pegline 的名字与图标。

---

**English** — Pegline-Rust is a public fork of [Pegline](https://github.com/2829632740/Pegline): a Windows tray app that hangs screenshots on a clothesline at the top of the screen. It is a Rust rewrite (Win32 + DirectComposition) of the original C#/WPF app, shipped as a single portable executable, MIT licensed. The demo above was recorded from a release build with synthetic screenshots; see the [docs](docs/) for behaviour, architecture and decisions.
