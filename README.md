# Pegline-Rust

把截图挂在屏幕顶部的“晾衣绳”上。

[![CI](https://github.com/hermitalap/Pegline-Rust/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/hermitalap/Pegline-Rust/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/hermitalap/Pegline-Rust?sort=semver)](https://github.com/hermitalap/Pegline-Rust/releases/latest)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Platform](https://img.shields.io/badge/platform-Windows%2010%20%7C%2011%20x64-0078D4)](https://github.com/hermitalap/Pegline-Rust/releases/latest)

<p align="center">
  <picture>
    <source srcset="docs/demo/pegline-demo.webp" type="image/webp">
    <img src="docs/demo/pegline-demo.png" alt="Pegline-Rust 演示" width="1200">
  </picture>
</p>

日常写文档、对设计稿、整理素材时，截图很容易堆满桌面和下载文件夹。Pegline 把它们随手挂在屏幕顶端：鼠标顶一下屏幕上沿，绳子滑下来；移开鼠标，自动收起。需要参考哪张，抬头就能看见，不用在窗口之间反复切来切去。

这是 [Pegline](https://github.com/2829632740/Pegline)（原 C#/WPF 版）的 **Rust 重写版**，单文件约 1.8 MB，基于原生 Win32 + DirectComposition + Direct2D，体积更小、更省资源、动效更顺畅。

---

## 快速上手

### 下载运行
前往 [Releases 页面](https://github.com/hermitalap/Pegline-Rust/releases/latest) 下载 `pegline.exe`，双击即可运行。

- **纯绿色单文件**：约 1.8 MB，无需安装，不需要任何外部运行时（如 .NET 或 VC++）。
- **系统要求**：Windows 10 1809+ / Windows 11 (x64)。
- **数据存储**：配置与日志保存在 `%APPDATA%\Pegline`，便签在 `%LOCALAPPDATA%\Pegline`（与原版 WPF 配置完全兼容互通）。

### 常用操作

| 操作 | 对应行为 |
|:--|:--|
| **查看 / 收起挂绳** | 鼠标移到屏幕顶端触发滑出；移开后自动收起（也可在设置中锁定常驻） |
| **快捷键触发** | <kbd>Ctrl</kbd>+<kbd>Alt</kbd>+<kbd>T</kbd> 呼出/收起；左键点击托盘图标同理 |
| **单击卡片** | 复制截图（同时写入图片与文件，可直接粘贴进微信、文档或文件夹） |
| **双击卡片** | 用系统默认看图工具打开 |
| **长按卡片** | 用系统画图工具打开 |
| **悬停卡片** | 显示复制/打开快捷按钮，稍作停留展开大图预览 |
| **拖拽卡片** | 拖入软件发副本；拖入文件夹/桌面保存并取下；拖入回收站删除 |
| **拖动夹子** | 按住卡片顶部的夹子左右拖动：邻卡被挤压/让位，松手后顺序写入 `pegged.txt`；单击夹子不触发复制 |
| **右键卡片** | 打开菜单（复制、标注、打开、在资源管理器中定位、另存到桌面、删除等） |
| **截图入绳** | 用 Windows 自带的 <kbd>Win</kbd>+<kbd>Shift</kbd>+<kbd>S</kbd>（或 <kbd>PrtScn</kbd>）截图，截完自动挂到绳上；Pegline 不注册自己的截图热键，因此也没有可自定义的截图快捷键 |
| **Markdown 便签** | 按 <kbd>Ctrl</kbd>+<kbd>Alt</kbd>+<kbd>N</kbd> 将复制的文字新建便签卡片，一键挂绳 |

---

## 功能特性

- **点击穿透，不抢焦点**：只有卡片和绳子区域响应交互，多余区域完全透明穿透。悬停、预览、复制都不会打断当前正在输入的窗口。
- **截图智能接入**：
  - 使用 Windows 截图快捷键 <kbd>Win</kbd>+<kbd>Shift</kbd>+<kbd>S</kbd>：Pegline 复用系统自动存储的截图文件，不会产生临时文件。
  - 自动监听系统截图文件夹（<kbd>Win</kbd>+<kbd>PrtScn</kbd>）。
  - 支持监听微信QQ等截图功能在剪贴板中的截图，不用改变你的使用习惯，Pegline都支持。
- **内置轻量标注**：支持箭头、矩形、文字（完整中文输入法支持）、马赛克、橡皮擦。
- **桌面 Markdown 便签**：
  - 像写便利贴一样随手记，支持标题、清单代办 `[ ]`、引用、链接等 Markdown 语法。
  - 4 种便签纸配色，编辑保存后像卡片一样挂在绳子上。
  - 编辑器可以拖动右边缘、下边缘或右下角放大（460×360 ～ 820×640 DIP），被拖的边跟手、对边镜像，纸面围绕中心对称缩放；右下角是 24×24 DIP 的锚点，比 2 DIP 边带更容易点中；每个便签记住自己的大小。
- **原生质感与动效**：
  - 真实物理弹簧动画，按屏幕真实刷新率（60–120Hz）积分演算。
  - 仿 Mica 半透明磨砂材质。
  - 完善的 Per-Monitor V2 多显示器及混合 DPI 缩放支持。
  - 检测到游戏、全屏视频等全屏独占应用时自动避让退避。

---

## 性能表现

基于 Release 构建实测（单屏 200% DPI、60Hz）：

| 指标 | 实测表现 |
|:--|:--|
| **可执行文件大小** | **1.92 MB**（无外部依赖，开箱即用） |
| **后台静置内存** | **~66 MB** |
| **挂载 6 张卡片** | **~98 MB** |
| **冷启动耗时** | **~100 ms**（约 0.1 秒） |
| **动画帧率** | **稳定 60 fps**（绘制耗时 p50 约 0.7 ms） |

---

## 源码构建

环境要求：
- **Rust 1.85+**（stable，MSVC 工具链）
- Windows SDK（需要其中的 `rc.exe` 编译图标与清单资源）

```powershell
# 编译 Release 产物 (target\release\pegline.exe)
cargo build --release

# 运行测试
cargo test

# 代码检查与格式化
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

---

## 技术细节

- **无窗体开销渲染**：基于 DirectComposition + DXGI/D3D11 SwapChain + Direct2D，不依赖卡顿的 Win32 Layered Window，完全交给 DWM 硬件加速合成。
- **零逐帧 GC / 堆分配**：动画弹簧按实际 `dt` 积分；文字排版与阴影预先烘焙缓存，帧循环内无动态堆分配。
- **后台异步 I/O**：截图捕获、图片编解码、去重哈希对比均在独立工作线程池执行，彻底杜绝 UI 掉帧卡顿。

---

## 目录结构与文档

```text
src/              应用核心源码 (Win32, DirectComposition, 标注编辑器, 便签等)
assets/           图标、manifest 清单、矢量资源
docs/             设计规范与技术文档
tools/            性能测试与压测脚本
.github/          CI/CD 自动化流水线
```

- [docs/BEHAVIOR.md](docs/BEHAVIOR.md)：交互规范与细节定义（严格对标原版）
- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)：线程模型与渲染架构
- [docs/DECISIONS.md](docs/DECISIONS.md)：设计决策与优化记录
- [docs/RUST-STACK.md](docs/RUST-STACK.md)：依赖库与构建约定
- [CHANGELOG.md](CHANGELOG.md)：版本更新日志

---

## 致谢与许可

- 原项目：[Pegline (C#/WPF)](https://github.com/2829632740/Pegline)
- 最早灵感来源：macOS 平台的 [Tendedero](https://github.com/alejandrobujan/tendedero)（by Alejandro Buján）

本项目采用 [MIT 许可证](LICENSE) 开源。

---

**English Summary**: Pegline-Rust is a lightweight Windows utility that hangs your screenshots on a floating "clothesline" at the top of the screen. Built with Rust, Win32, and DirectComposition, it provides a smooth, native, and zero-dependency alternative to the original C#/WPF version. Portable, 1.8 MB single executable.
