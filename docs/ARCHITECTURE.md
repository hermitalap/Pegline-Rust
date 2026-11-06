# 目标架构（Rust / Win32 / DirectComposition）

状态：**设计稿**。M0 spike 验证通过前，本文档中的跨线程与 DComp 细节都只能算“待验证假设”。

---

## 1. 要解决的是什么

WPF 版的瓶颈不在业务逻辑，而在渲染路径：

```
WPF 视觉树 → MILCore 软件光栅化整窗 → 32bpp 系统内存位图 → UpdateLayeredWindow → DWM
```

Rust 版要走的是：

```
场景快照 → D3D11 渲染到 flip-model 交换链 → DirectComposition 视觉树 → DWM 合成（GPU）
```

目标是在 6016×420 的透明条带上稳定 60 fps，一次全屏截图飞行动画不掉帧，
常驻私有内存 ≤ 40 MB，空闲 CPU ≤ 0.5%（单核百分比）。

---

## 2. 进程与线程

```
┌──────────────────────────────┐   ┌──────────────────────────────┐
│ UI 线程                       │   │ 渲染线程（帧时钟）             │
│  Win32 消息循环               │   │  弹簧积分、场景状态            │
│  线带/飞行/预览窗口的 HWND     │──▶│  D3D11 immediate context     │
│  命中测试、指针轮询、钩子       │   │  D2D1 device context         │
│  托盘、快捷键、剪贴板、监听     │◀──│  DWrite 布局缓存              │
│  向渲染线程发送意图             │   │  swap chain Present(1, 0)    │
└──────────────────────────────┘   └──────────────────────────────┘
              │                                   ▲
              │ 解码 / 编码 / 比较 / 回收站删除      │
              ▼                                   │
     ┌────────────────────────────────────────────────────┐
     │ worker 线程池（WIC 解码、PNG 编码、像素比较、IO）      │
     └────────────────────────────────────────────────────┘
```

- **UI 线程**只做轻活：消息、指针、状态机、窗口生命周期；把“加卡/删卡/悬停/放下/收起”
  这类意图写入命令队列，不直接改动画状态。
- **渲染线程**拥有帧时钟、场景和弹簧；每帧先排空命令队列，再按真实 `dt` 积分、绘制、
  `Present`。这样动画频率天然等于显示器刷新率（60/120/144），不依赖 UI 线程的定时器。
- 渲染线程每帧把“卡片命中框”等只读信息发布回 UI 线程（一个 `Arc` 快照），
  供点击穿透判断使用；延迟一帧（8–16 ms）对命中没有影响。
- 命令队列是 `Mutex<Vec<Command>>` 或三缓冲，每帧几条，开销可忽略。

为什么不是单线程：`Present(1, 0)` 会阻塞到垂直同步，放在 UI 线程会让消息循环停顿，
拖拽、悬停这类输入就会抖动。WPF 自己也是 UI + 渲染两条线程。

为什么动画不在 UI 线程：120 Hz 下每 8 ms 就要推进一次弹簧；让 UI 线程按显示器频率醒来
既浪费又容易和消息处理打架。帧时钟只应该有一个。

---

## 3. 窗口模型

| 窗口 | 生命周期 | 说明 |
|:--|:--|:--|
| 线带窗口 | 常驻（隐藏/显示） | 当前屏幕工作区顶部，宽度 = 工作区宽度，高 210 DIP |
| 飞行窗口 | 池化（最多 2 个） | 每次飞行的路径包围盒；结束后缩到 2×2 隐藏 |
| 预览窗口 | 按需创建 | 悬停大图；隐藏后延迟关闭，释放表面 |
| 拖拽预览 | 拖拽期间 | 跟随指针的小卡片 |
| 隐藏消息窗口 | 常驻 | 收 `WM_HOTKEY`、`WM_CLIPBOARDUPDATE` |

关键样式：

```text
WS_POPUP
WS_EX_NOREDIRECTIONBITMAP   // 不要 DWM 重定向表面，内容由 DComp 提供
WS_EX_TOOLWINDOW            // 不进任务栏 / Alt+Tab
WS_EX_NOACTIVATE            // 不抢焦点
WS_EX_TRANSPARENT           // 按需，点击穿透
WS_EX_LAYERED               // 按需，和上一行一起用于跨进程点击穿透（只作输入标志）
```

- `WM_MOUSEACTIVATE` 返回 `MA_NOACTIVATE`。
- 点击穿透：指针不在任何照片上时加 `WS_EX_TRANSPARENT | WS_EX_LAYERED`，在照片上时去掉；
  同时 `WM_NCHITTEST` 对非照片区域返回 `HTTRANSPARENT` 作为同线程双保险。
  M0 实测：只有 `WS_EX_TRANSPARENT` 时点击不会穿过到其他进程；`HTTRANSPARENT` 也只沿
  同一线程的窗口链传递。`WS_EX_LAYERED` 在这里不引入重定向位图，也不调用
  `UpdateLayeredWindow`，像素仍由 DComp 交换链提供。动态切换后需要
  `SetWindowPos(..., SWP_FRAMECHANGED)` 让命中测试生效。
- 置顶：显示时 `SetWindowPos(HWND_TOPMOST, SWP_NOACTIVATE)`；全屏应用切回后重试。
- DPI：清单声明 Per-Monitor V2；处理 `WM_DPICHANGED`，重建交换链尺寸，
  按新缩放重算 DIP→像素。

### 3.1 为什么用 `WS_EX_NOREDIRECTIONBITMAP`

普通分层窗口（`WS_EX_LAYERED` + `UpdateLayeredWindow`）必须由进程提供一张 CPU 位图；
`NOREDIRECTIONBITMAP` + DComp 则把 GPU 表面直接交给 DWM，没有整窗 CPU 光栅，也没有回读。
这是本次重写最核心的一条。

`WS_EX_LAYERED` 只在需要点击穿透时作为输入透明标志出现，不改变渲染路径：没有
`UpdateLayeredWindow`，没有 `SetLayeredWindowAttributes`，也没有整窗 CPU 位图。M0 实测
加上它之后 static 私有内存仍在 37.8–38.2 MB，和未加时一致。

---

## 4. 渲染

### 4.1 设备

- `D3D11CreateDevice`（BGRA 支持，`D3D11_CREATE_DEVICE_BGRA_SUPPORT`）。
- `ID2D1Factory1::CreateDevice` → `ID2D1DeviceContext`（所有 2D 内容）。
- `IDWriteFactory`（文字），`IWICImagingFactory`（解码）。
- DXGI：`CreateSwapChainForComposition`，`DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL`，
  `DXGI_FORMAT_B8G8R8A8_UNORM`，`DXGI_ALPHA_MODE_PREMULTIPLIED`，2 个缓冲。

### 4.2 每帧画什么

线带窗口一层画完，按顺序：

1. 绳子：D2D 几何（一条二次贝塞尔），三层描边（阴影/核心/高光）+ 两端渐变遮罩。
   几何在宽度变化时重建一次并缓存。
2. 每张卡片：
   - 预烘焙的相框阴影（九宫格纹理，按卡片尺寸缩放，不实时模糊）；
   - 玻璃底 + 渐变边框（D2D 圆角矩形 + 线性渐变画刷，画刷缓存）；
   - 照片纹理（480 px 缩略图，`D2D1_BITMAP_INTERPOLATION_MODE_LINEAR`）；
   - 夹子（缓存成一张小纹理，随卡片旋转）；
   - 悬停时：叉、按钮、提示文字。
3. 提示文字（空线带时）。

所有画刷、几何、文字布局在创建时缓存；**每帧不允许堆分配**（帧内用 `SmallVec`/栈数组）。

### 4.3 阴影

- 相框阴影：一次性渲染到 64×64 的模糊纹理，九宫格拉伸。
- 夹子/按钮阴影：同样预烘焙。
- 飞行中卡片 ≤ 300 DIP 才画阴影，和现在一致；飞行窗口里的阴影用同一张九宫格纹理。

### 4.4 文字

- `IDWriteTextLayout` 按（文本、字体、字号、宽度、对齐）缓存。
- 卡片按钮、提示、错误提示都是固定文本，启动时排好版。
- 便签文本每次颜色/内容变化时重排一次。

---

## 5. 场景与动画

### 5.1 场景

```rust
struct Scene {
    width_px: f32,          // 线带窗口宽（设备像素）
    scale: f32,             // 设备像素 / DIP
    revealed: bool,
    slide: f32,             // DIP，-222..0
    cards: Vec<Card>,
    hint_alpha: f32,
    rope: RopeGeometry,     // 宽度变化时重建
    hover: CardId,
    pressed: CardId,
    dragging: CardId,
    copied: CardId,
}

struct Card {
    id: CardId,
    photo: TextureId,       // 480px 缩略图
    size: (f32, f32),       // 相框尺寸，DIP
    tilt: f32,              // 固定 ±2.5
    x: Spring,              // 水平槽位
    drop: Spring,           // 吊挂下落
    swing: Spring,          // 摆动
    press: Spring,
    nudge: Option<Nudge>,   // 微风
    opacity: f32,
    cross_alpha: f32,
    actions_alpha: f32,
    arriving: bool,
}
```

### 5.2 弹簧

直接移植 `Spring`（`archive/wpf-v1/src/Support.cs`）：

- `response/damping` → `k = (2π/response)²`，`c = 4π*damping/response`；
- `stiffness/damping` 直接给；
- 每帧按 1/240 s 子步积分，保证任何帧率下稳定；
- 弹簧参数见 `docs/BEHAVIOR.md` §3.2。

### 5.3 帧循环与 120 Hz

- 渲染线程 `Present(1, 0)` 阻塞到垂直同步，帧率**自动等于显示器刷新率**：
  60 Hz 屏 60 fps，120 Hz 屏 120 fps。代码里不写死 16.6 ms。
- 每帧顺序：排空命令队列 → `dt = now - last`（`QueryPerformanceCounter`，上限 50 ms，
  避免休眠后跳变）→ 弹簧积分 → 绘制 → `Present`。
- 帧预算：60 Hz 16.6 ms，120 Hz **8.3 ms**。场景绘制按 8 ms 设计，
  `probe` 同时报告 p50 / p95 / max，p95 超过预算即视为不达标。
- 可选帧率上限：`fps=auto|60|120`（默认 `auto`）。`60` 用于省电或在 120 Hz 屏上
  保持和旧版一致的手感；实现方式是跳过 `Present` 并在下一帧合并 `dt`，
  **不要**用 `Sleep` 凑帧。
- 窗口隐藏时不进入帧循环（`WaitForSingleObject` 等命令或显示事件），空闲 CPU 归零。
- 垂直同步之外的同步手段：如果某些驱动上 `Present(1,0)` 的节流不稳定，
  退回 `Present(0, DXGI_PRESENT_DO_NOT_WAIT)` + `IDXGIOutput::WaitForVBlank`，
  在 M0 里对比两种方式的帧间隔分布再定。
- 可变刷新率（VRR）屏上跟随显示器即可；不主动限制到 60，避免和 VRR 打架。

---

## 6. 输入与命中测试

- 指针：30 Hz `GetCursorPos` 轮询（和现在一致；不用 `WM_MOUSEMOVE` 是因为窗口大部分时间点击穿透）。
- 命中：用场景里的同一套布局函数从指针反算卡片框，**不读回渲染结果**。
- 顶边点击：`WH_MOUSE_LL` 低层钩子，只看左/右键按下。
- 拖拽：OLE `DoDragDrop`（`IDropSource` + `IDataObject`），文件拖放列表；
  `GiveFeedback` 驱动预览窗口移动，另加 15 ms 定时器兜底。
- 菜单：`TrackPopupMenu`（或 `muda`），打开前 `SetForegroundWindow`。

---

## 7. 截图链路

- 文件监听：`ReadDirectoryChangesW`（异步或线程池阻塞），自带 250 ms 去抖；
  启动扫描用 `FindFirstFile`，创建时间早于进程启动的记为已知。
- 剪贴板：`AddClipboardFormatListener`；读的时候用 OLE `IDataObject`，
  取 `PNG` 格式流优先，其次 `CF_DIB`；写的时候自建 `IDataObject`（图片 + PNG + 文件列表）。
  `arboard` 不支持多格式，自己实现约 300 行。
- 解码：WIC `CreateDecoderFromFilename` + `IWICBitmapScaler`（按目标尺寸），
  直接产出 BGRA 缓冲上传纹理；不经过中间 `Vec<u8>` 的全尺寸拷贝。
- 编码：WIC PNG encoder 写临时文件再改名。
- 比较：两张图各解码到最长边 2048，逐行比较；允许 0.5% 像素、每通道 8 的差异。

---

## 8. 内存与体积预算

| 项 | MVP 门槛 | 优化目标 |
|:--|--:|--:|
| 进程私有（空闲，6 张卡） | ≤ 60 MB | ≤ 30 MB |
| 进程私有（截图飞行中） | ≤ 100 MB | ≤ 50 MB |
| exe 大小 | ≤ 15 MB | ≤ 10 MB |
| 线带交换链（6016×420，2 缓冲） | ~20 MB（显存/共享，不计入私有） | 同左 |
| 飞行交换链（2442×2344，2 缓冲） | ~46 MB（仅在飞行时） | 同左 |
| 缩略图纹理（12 × 480px） | ~6 MB | 同左 |
| 飞行纹理（1600px） | ~10 MB | 同左 |

策略：

- 解码只按目标尺寸（WIC scaler），不做全尺寸中间图。
- 上传纹理后立刻释放 CPU 位图。
- 纹理池化：卡片删除时纹理回池，不反复创建。
- 窗口隐藏即缩到 2×2（和 WPF 版最后一轮优化一致）。
- 像素比较分块，栈/小缓冲，不分配全图数组。
- 体积：静态 CRT + LTO + `strip` + `OPT:REF,ICF`；不引入 UI 框架和大依赖树。

---

## 9. 失败处理

| 情况 | 处理 |
|:--|:--|
| `DXGI_ERROR_DEVICE_REMOVED` / `DEVICE_RESET` | 丢弃设备与交换链，重建；场景快照保留 |
| 显卡驱动更新、休眠唤醒 | 同上 |
| `WM_DPICHANGED` | 重建交换链，重算 DIP |
| 显示器增删/分辨率变化 | 重新选屏、重新 `Place` |
| 解码失败（文件正在写） | 下个 tick 重试；连续失败则放弃 |
| 文件被删/被移走 | 1 s 定时检测，连续 2 次缺失 → 下绳 |
| 托盘/快捷键注册失败 | 记录，菜单里显示“被其他程序占用” |
| 未捕获 panic | `panic::set_hook` 写 `error.log`；UI 线程捕获后继续 |

---

## 10. 模块划分

下面的树是**当前实际布局**（2026-11-06 拆分后）。早先的设计稿把 `d2d.rs` / `shadow.rs` /
`texture.rs` / `gestures.rs` / `twins.rs` 都列为独立文件，但落地时没有拆成那么细：
画刷与阴影烘焙留在 `render/mod.rs`，手势在 `app/pointer.rs`，孪生合并在
`app/capture_flow.rs`。分层意图没变，粒度变了。

```
src/
  main.rs            进程入口、单实例、panic hook、消息循环
  lib.rs             模块清单
  app/               控制器：一条消息循环、一个状态机（UI 线程）
    mod.rs           消息循环、tick 编排、窗口过程
    pool.rs          WindowPool：全部窗口/句柄池 + 空闲释放
    book.rs          FlightBook：飞行中的卡片与交接截止时间
    line.rs          Line：线上的卡片（纯数据）
    state.rs         LineState：可见性/悬停/按压状态机
    capture_state.rs Capture：监视器、孪生窗口、剪贴板簿记
    sessions.rs      Sessions：两个就地编辑器的会话与交接
    reveal.rs        放下/收起/换屏
    pointer.rs       指针状态机：停留、悬停、按压、双击、起拖
    cards.rs         卡片几何、命中框、容量、pegged.txt
    capture_flow.rs  还原、监视、孪生合并、采集落地
    tray_menu.rs     托盘菜单与命令
    dev.rs           --*-demo 验收脚手架（普通启动一次比较即返回）
  platform
    win32.rs         DPI、屏幕、光标、全屏检测、窗口样式、高精度时钟
    window/
      mod.rs         窗口类注册、消息分发、COM 守卫、ModalGuard
      overlay.rs     线带窗口 + DComp 目标 + 交换链（渲染线程）
      flight/        飞行/掉落窗口（mod.rs 时间线 + paint.rs 绘制）
      preview.rs     悬停预览窗口
      ghost.rs       拖拽预览（自己跟随指针）
      shortcuts.rs   快捷键录制对话框（GDI）
  gfx/
    mod.rs           图形基础设施
    device.rs        D3D11 / D2D / DComp / DWrite / 交换链创建（唯一一份）
  render/
    mod.rs           Renderer：设备、交换链、帧提交、绘制原语、画刷/几何缓存
  scene/             线的动画模型
    mod.rs           Scene / Card / 弹簧 / apply / update / publish
    paint.rs         整个场景的绘制
  model & layout
    layout.rs        X、RopeY、PhotoSize、CardSize、命中框、预览位置
    types.rs         RectF / RectI / CardId / ImageData / FrameSnapshot
    spring.rs        帧率无关的弹簧积分
    preview.rs       预览几何（纯函数）
  editor/            标注编辑器
    model.rs         marks、撤销、命中几何
    layout.rs        卡片与药丸的布局
    window/          mod.rs 窗口/线程/状态机/IME + paint.rs 绘制
  note_editor/       便签 WYSIWYG 编辑器
    model.rs         文本、光标、选区、撤销
    display.rs       Markdown → 显示投影（纯函数）
    render/          mod.rs 状态机 + 排版缓存 + paint.rs 绘制
    window.rs        窗口、输入、IME、焦点
  capture/
    clipboard.rs     Win32 剪贴板管道
    dib.rs           DIB ↔ BGRA 编解码（纯算术）
    watcher.rs       ReadDirectoryChangesW
    wic.rs           WIC 解码/缩放/编码
    areas.rs         截取区域推断
  notes.rs           便签文件（原子写、归档、摘要）与 GDI 光栅化
  tray.rs            托盘图标
  dragdrop.rs        OLE 拖出
  sound.rs / settings.rs / i18n.rs / logging.rs / shortcut.rs / font.rs
  bin/probe.rs       帧率/内存测量（独立 bin）
```

---

## 11. 明确不采用的东西

| 方案 | 原因 |
|:--|:--|
| `winit` / `tao` | 透明窗口仍走分层窗口或额外抽象，拿不到 DComp 交换链 |
| `egui` / `iced` / `slint` | 自带 CPU 或 Skia 渲染层，和 DComp 合成重复，且难以做到逐像素透明 + 点击穿透 |
| WinUI 3 / Win2D | 运行时依赖、打包复杂；透明顶层窗口支持有限 |
| Electron / WebView | 内存基线比现在的 WPF 还高一个量级 |
| 自绘 CPU 光栅（tiny-skia） | 6K 面积 CPU 光栅正是要避开的东西 |

---

## 12. 需要 M0 验证的假设

1. `WS_EX_NOREDIRECTIONBITMAP` + `CreateSwapChainForComposition` + 预乘 alpha 在 Intel 6K 屏上能正确透出桌面。
2. 去掉 `WS_EX_TRANSPARENT` 时点击能命中窗口，加上后能穿透到下层应用。
   M0 实测：还需要 `WS_EX_LAYERED`；`HTTRANSPARENT` 只对同线程窗口有效。
3. `WS_EX_NOACTIVATE` + `MA_NOACTIVATE` 下，点击不抢焦点，前台应用继续接收键盘。
4. 6016×420 与 2442×2344 两种交换链在 60 Hz 下稳定 `Present(1,0)`，且在 120 Hz 屏上
   自动跑满 120（没有 120 Hz 屏时，至少验证单帧绘制 ≤ 8 ms）。
5. 私有工作集与显存占用达到 §8 的预算。
6. DComp 跨线程用法可行（否则退化为 DComp 调用集中在渲染线程、UI 线程只发命令）。

M0（2026-11-02）结果：1–6 全部验证通过，6 不需要退化方案。详细数字与截图见
`docs/SPIKE-01-RESULTS.md`。

任一条不成立，先解决再进入 M1；这些是重写的根基。
