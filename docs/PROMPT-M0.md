# 开发 Prompt — M0：合成 spike

把下面整段复制给下一个 session。目标是**只做 M0**，验证 GPU 合成这条路走不走得通。

---

你是 Pegline Rust 重写的实施者。仓库根目录已经完成设计阶段，**没有任何 Rust 代码**。
本次任务只做 **M0：透明合成 spike**，不要写业务功能。

## 先读（按顺序，别跳）

1. `AGENTS.md` — 工作约定与硬规则
2. `PLAN.md` — 目标（小/轻/快）、MVP 策略、M0 出口条件
3. `docs/ARCHITECTURE.md` — §2 线程、§3 窗口、§4 渲染、§5 帧循环
4. `docs/RUST-STACK.md` — 依赖、构建、打包
5. `docs/SPIKE-01.md` — 本次任务的完整规格与验收
6. 需要理解产品行为时查 `docs/BEHAVIOR.md`；旧实现的只读参考在 `archive/wpf-v1/`

## 交付物

1. 可 `cargo build --release` 的工程骨架：
   - `Cargo.toml`（`windows` 0.62、`tray-icon`/`muda` 可以等 M1 再加，spike 阶段不引）、
   - `build.rs` + `assets/pegline.manifest`（Per-Monitor V2）+ 占位图标，
   - `src/main.rs`（空壳，`#![windows_subsystem = "windows"]`），
   - `src/bin/spike.rs`。
2. spike 按 `docs/SPIKE-01.md` §2 实现：`WS_EX_NOREDIRECTIONBITMAP` 窗口、
   `CreateSwapChainForComposition`、DComp 视觉树、D2D 画玻璃卡片 + 纹理 + 预烘焙九宫格阴影、
   弹簧动画（按真实 dt 积分）、点击穿透开关。
3. 跑出 `SPIKE-01.md` §3 的全部指标：`mode=static|animate|flight`，60 Hz 与 120 Hz 分开记录。
4. `docs/SPIKE-01-RESULTS.md`：机器信息（GPU/屏幕/缩放/刷新率）、每条验收的实测数字、
   截图（透明合成、圆角边缘、点击穿透验证）、结论（继续 / 调整 / 放弃）。
5. 更新 `PLAN.md` 中 M0 的状态与实测数字。

## 验收

`docs/SPIKE-01.md` §4 的 11 条全部通过。任何一条不通过：记录现象与数据，给出 §5 的备选方案，
**不要进入 M1**。

## 硬约束

- 单文件、静态 CRT（`+crt-static`）；MVP exe ≤ 15 MB。
- 私有内存：static ≤ 40 MB，flight ≤ 80 MB。
- 60 Hz ≥ 59.5 fps，p95 ≤ 17.5 ms；120 Hz ≥ 118 fps，p95 ≤ 9 ms（没有 120 Hz 屏时验证单帧 ≤ 8 ms）。
- 渲染只用 D3D11 + D2D1 + DWrite + WIC + DirectComposition；**不引入**
  `winit`/`egui`/`iced`/`slint`/WinUI/`tokio`/`image`。
- 帧循环跟随显示器刷新率，代码里不出现 16.6 ms 的假设。
- 每帧不分配堆内存；阴影预烘焙；UI 线程不阻塞；不读回 GPU 表面。
- 代码 UTF-8 无 BOM、CRLF；注释与标识符英文。

## 流程

- 一次只加一层：先出一个透明窗口，再加 DComp，再加 D2D 内容，再加动画，最后加点击穿透开关。
- 每完成一块跑 `cargo build` + `cargo test` + `cargo clippy --all-targets -- -D warnings`。
- 测量用同一台机器、同一次开机；先 `--mode=static` 记基线，再 `animate`，再 `flight`。
- 结束时提交：`feat: add the DirectComposition spike`。

## 不要做

- 不要实现托盘、设置、截图链路、卡片业务、菜单（这些是 M1+）。
- 不要为“以后可能用到”加抽象层。
- 不要动 `archive/wpf-v1/`；它是冻结的参考实现。
- 不要在没有实测数字的情况下宣称“性能达标”。

## 环境

- Windows 11，Rust stable `x86_64-pc-windows-msvc`（本机 1.96.1，`cargo` 已在 PATH）。
- 开发机屏幕 6016×3384 @200%，60 Hz；120 Hz 验证可能需要外接显示器或 `--nolimit` 计时。
- 旧版可随时构建对照：`archive\wpf-v1\build.cmd`（不要在根目录跑旧脚本）。
