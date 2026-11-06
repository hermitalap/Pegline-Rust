# SPIKE-01：透明合成窗口（M0）

这是整个重写的**前置验证**。只做一个二进制 `src/bin/spike.rs`（约 300–500 行），
不接业务逻辑、不接托盘、不读截图文件。跑通并测出数字，才允许进入 M1。

> **M0 结果（2026-11-02）：通过。** 实测数字、截图与后续注意事项见
> `docs/SPIKE-01-RESULTS.md`。验收完成后 `src/bin/spike.rs` 已从构建中移除，
> 需要复查时从 git 历史取（提交 `8e7e420`）。

预计：1–2 个工作日。

---

## 1. 要回答的问题

1. `WS_EX_NOREDIRECTIONBITMAP` + DirectComposition + `CreateSwapChainForComposition`
   在 6K Intel 核显上，能不能正确地把预乘 alpha 合成到桌面（没有黑底、没有白边、没有拖影）？
2. 点击穿透开/关能否按需切换，并且不抢焦点？
3. 6016×420 与 2442×2344 两种尺寸的交换链，`Present(1,0)` 能否稳定 60 Hz？
4. 同样的绘制在 120 Hz 屏（或强制 `Present` 不节流）下单帧是否 ≤ 8 ms，能跑到 120 fps？
5. 私有工作集、显存/共享内存是多少？
6. DComp 的跨线程用法是否可行（UI 线程建 visual、渲染线程建交换链并 `SetContent`）？

---

## 2. 最小实现

一个窗口覆盖主屏工作区顶部 420 px，画：

- 一个圆角 16 DIP 的玻璃卡片（半透明白 + 1 px 渐变边框），
- 卡片里一张 480 px 的测试纹理（用代码生成的渐变位图即可），
- 卡片以弹簧在水平方向来回移动（位移 ±200 DIP，按真实 `dt` 积分，帧率跟随显示器），
- 卡片下方一个随位移变化的柔和阴影（预烘焙九宫格）。

### 2.1 初始化顺序

```text
CoInitializeEx(STA)
SetProcessDpiAwarenessContext(PER_MONITOR_AWARE_V2)
RegisterClassW
CreateWindowExW(WS_EX_NOREDIRECTIONBITMAP | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT, WS_POPUP)
D3D11CreateDevice(BGRA_SUPPORT)
D2D1CreateFactory → ID2D1DeviceContext
DCompositionCreateDevice(dxgiDevice) → IDCompositionDevice
DCompositionCreateDevice → CreateTargetForHwnd(hwnd, topmost=TRUE) → IDCompositionTarget
CreateVisual → SetContent(swapChain) → target.SetRoot(visual) → device.Commit()
CreateSwapChainForComposition(... FLIP_SEQUENTIAL, BGRA, PREMULTIPLIED, 2 buffers)
ShowWindow(SW_SHOWNOACTIVATE)
```

### 2.2 帧循环

```text
loop {
    snapshot = scene.load();
    device_context.BeginDraw();
    clear(transparent);
    draw_card(snapshot);
    device_context.EndDraw();
    swap_chain.Present(1, 0);   // vsync
    record frame time;
}
```

动画在渲染线程里积分，M0 不需要跨线程场景，先验证合成。

### 2.3 必须实现的开关（方便测）

- `--mode=static`：只画一张静止卡片（测内存基线）。
- `--mode=animate`：弹簧动画（测帧率）。
- `--mode=flight`：2442×2344 窗口 + 1600 px 纹理缩放（测飞行负载）。
- `--clickthrough=0|1`：切换 `WS_EX_TRANSPARENT`。
- `--seconds=N`：跑 N 秒后退出并打印统计。
- `--target=hwnd`：可选，把窗口挂到指定 HWND 下测子窗口行为（暂不需要）。

---

## 3. 测量项

每 1 秒采样一次，结束时打印：

| 指标 | 方法 |
|:--|:--|
| 显示器刷新率 | `IDXGIOutput1::FindClosestMatchingMode` 或 `DXGI_MODE_DESC.RefreshRate` |
| 帧间隔 p50 / p95 / max | `QueryPerformanceCounter` 前后帧差 |
| 掉帧率 | 间隔 > 1.5 × 刷新周期（16.67 ms @60，8.33 ms @120）的比例 |
| 平均每帧 CPU | `GetProcessTimes` 差值 / 帧数 |
| 私有工作集 | `GetProcessMemoryInfo` → `PrivateUsage` |
| 工作集 | `WorkingSetSize` |
| 交换链尺寸与数量 | 自己记录 |
| 设备丢失次数 | `Present` 返回 `DXGI_ERROR_DEVICE_REMOVED/RESET` |

输出格式（便于和 WPF 版的 `RESULT` 对比）：

```text
SPIKE mode=animate w=6016 h=420 hz=60 frames=3600 fps=60.0 p50=16.6 p95=17.1 max=24.0 jank=0.0% cpu_ms_frame=0.85 private=28MB ws=34MB
SPIKE mode=animate w=6016 h=420 hz=120 frames=7200 fps=119.8 p50=8.3 p95=8.9 max=12.0 jank=0.0% cpu_ms_frame=0.85 private=28MB ws=34MB
```

---

## 4. 验收标准

| # | 项 | 通过条件 |
|:--|:--|:--|
| 1 | 透明合成 | 窗口区域透出桌面，圆角外完全透明，无黑底/白边 |
| 2 | 点击穿透 | `clickthrough=1` 时点击落到下层应用；`=0` 时窗口收到 `WM_LBUTTONDOWN` |
| 3 | 不抢焦点 | 点击卡片后，前台应用（记事本）仍能继续输入 |
| 4 | 置顶 | 覆盖最大化的浏览器窗口 |
| 5 | 帧率 6016×420 @60 | ≥ 59.5 fps，p95 ≤ 17.5 ms，jank ≤ 1% |
| 6 | 帧率 2442×2344 @60 | ≥ 59.5 fps，p95 ≤ 17.5 ms |
| 7 | 120 Hz 能力 | 有 120 Hz 屏：≥ 118 fps，p95 ≤ 9 ms；无 120 Hz 屏：单帧绘制 ≤ 8 ms（`--nolimit` 测） |
| 8 | CPU | 60 Hz 动画时 ≤ 2 ms/帧，静止时 ≤ 0.1 ms/帧 |
| 9 | 内存 | 私有 ≤ 40 MB（static），≤ 80 MB（flight） |
| 10 | 稳定性 | 连续 5 分钟不掉帧、不失效、内存不增长 |
| 11 | 跨线程 | 若验证失败，记录并给出退化方案（DComp 调用集中在渲染线程） |

任何一条不达标，先定位；不要带着问题进入 M1。

---

## 5. 已知风险与备选

| 风险 | 备选 |
|:--|:--|
| `NOREDIRECTIONBITMAP` 在混合 DPI 下合成异常 | 退回 `WS_EX_LAYERED` + DComp 的 `IDCompositionVirtualSurface`（仍有 GPU 合成，但要自己管 alpha） |
| DComp 跨线程不安全 | 渲染与 DComp 调用放在同一线程；UI 线程通过消息通知 |
| 预乘 alpha 边缘发灰 | 检查 D2D 目标格式与 `D2D1_ALPHA_MODE_PREMULTIPLIED`，卡片边缘用 1 px 透明描边 |
| 6K 交换链显存过大 | 飞行窗口只覆盖路径包围盒（和 WPF 版一致），并池化复用 |
| 静态 CRT 与 `windows` 冲突 | 改用动态 CRT，随 exe 附带 `vcruntime140.dll`（最后手段） |

---

## 6. 交付物

1. `src/bin/spike.rs`（可运行，带上面的开关）。
2. `docs/SPIKE-01-RESULTS.md`：机器信息（GPU、屏幕、缩放）、每一项的实测数字、
   截图（透明效果、圆角边缘、点击穿透验证）、结论（继续 / 调整方案 / 放弃）。
3. 若跨线程方案有变，更新 `docs/ARCHITECTURE.md` §2 与 §12。

---

## 7. 下一步

通过后进入 M1（外壳）：托盘、设置、单实例、开机启动、快捷键、日志、单文件打包。
在 M1 结束前不要碰截图链路——先把“一个能常驻、能退出、能记住设置的空壳”跑稳。
