# PLAN.md — 起飞动画卡顿优化

状态：**进行中**（逐项修改，每项都要编译 + 重启 + 人工确认）

## 背景

用户反馈：截图飞向晾衣绳时明显掉帧。诊断结论：**不是硬件问题**，而是 WPF 透明窗口
（`AllowsTransparency=true`）走 CPU 合成，而 `Flight` 在这个窗口里每帧重绘大尺寸照片 +
每帧重算 20px 阴影 + 高等级重采样。此外截图链路上有大量 UI 线程同步解码/编码。

完整诊断过程与数据见 `build/patches/all-6-items.patch` 对应的提交前状态，以及本文件下方基线表。

## 基线（仓库默认状态，本机 6016×3384 @200%，60 Hz）

| 场景 | fps | 单帧 | 备注 |
|:--|--:|--:|:--|
| 线带 6016×420（透明窗口，小卡片移动） | 60.0 | 16.6 ms | 窗口大小本身不是问题 |
| 全屏透明窗口，只动小卡片 | 58.8 | 16.7 ms | 同上 |
| 全屏透明 + 飞行内容（3000px 图 + blur20 + HighQuality） | **8.7** | 101.7 ms | 灾难 |
| 同上，改成不透明窗口 | 25.4 | 40.1 ms | 透明窗口 CPU 约 7 倍 |
| 全屏透明，去掉阴影 | 19.1 | 46.4 ms | 阴影约占一半 |
| 2400×1350 飞行，有阴影 / 无阴影 | 33.9 / 55.4 | — | |
| 1200×700 飞行 | 56.5 | — | |
| **真实 Flight**：全屏截图 | **8.4** | 117–225 ms | |
| **真实 Flight**：2266×2392 截图 | **33.2** | 前 13 帧 29–88 ms，之后 16–17 ms | 用户实际感受 |
| **真实 Flight**：1141×695 截图 | 53.9 | — | |

UI 线程同步开销（同一台机器实测）：

| 操作 | 耗时 |
|:--|--:|
| PNG 编码 6016×3384（剪贴板接管） | ~215 ms |
| PNG 编码 2266×2392 | ~55 ms |
| `Thumbnails.Load(480)`，5733×2972 的 PNG | ~155 ms |
| `Thumbnails.Load(3000)`，同上 | ~170 ms |
| `SamePicture`（整幅像素比较） | ~380–430 ms |

## 改动清单（按收益从大到小，逐项实施）

每项都要：改 → `build.cmd` → `test.ps1` → 重启 → 人工确认 → 回来更新本表。

| # | 项目 | 涉及文件 | 预期收益 | 状态 |
|:--|:--|:--|:--|:--|
| 1 | **飞行阶段不画阴影**：起飞时不给 `container` 挂 `DropShadowEffect`，等卡片缩到 300 点以下再淡入（常量 `ShadowBelowDip`，设 0 = 全程无阴影）。落地后由 `CardView` 的静态阴影接管 | `src/Flight.cs` | 3.2MP 时 33.9 → 55.4 fps；全屏 8.7 → 19.1 | ✅ 已完成（见下） |
| 2 | **起飞尺寸上限 + 预缩放**：卡片起飞时最长边不超过 1600 设备像素；`App.Fly` 按这个尺寸解码（不再解 3000px），缩放模式改 `Linear` | `src/Flight.cs`, `src/App.cs` | 5.4MP 场景约 42 → 55+ fps；解码 ~170 → ~90 ms | ✅ 已完成（见下） |
| 3 | **窗口复用 + 轨迹包围盒**：`Flight` 窗口池化复用，窗口只覆盖「起飞点 → 落点」的包围盒，不再每次新建全屏窗口 | `src/Flight.cs` | 全屏场景 57 → 61 fps；消除首次 50–70 ms 建窗卡顿 | ✅ 已完成（见下） |
| 4 | **去掉逐帧布局与分配**：复用 `RectangleGeometry`（照片 Clip）、复用 `RotateTransform`（夹子）、布局在 `Show()` 之前完成 | `src/Flight.cs` | 每帧少一次布局 + 若干分配 | ✅ 已完成（见下） |
| 5 | **重活移出 UI 线程**：PNG 编码（`Watchers.cs`）、截图解码 + 缩略图派生、重复图判重（`App.cs`）进线程池；`recent` 加锁；异常写 `error.log` | `src/Support.cs`, `src/App.cs`, `src/Watchers.cs`, `src/Line.cs` | 去掉 0.3–0.7 s 的截图冻结 | ✅ 已完成（见下） |
| 6 | **卡片/线带逐帧分配**：`CardView` 位置改用复用的 `TranslateTransform`（不再每帧 `SetLeft/SetTop`），`cross/actions/copiedLabel` 的变换复用；`LineWindow` 的提示文字只 `Measure` 一次 | `src/CardView.cs`, `src/LineWindow.cs` | 空闲 CPU 2.6% → 更低；减少 GC 抖动 | ⚠️ 部分完成（`CardView` 已回退，见下） |

## 每项的实施记录

### 修复 · 文件路径的截图没有起飞动画（2026-11-02）

人工确认第 4/5 项时报出"起飞动画消失了"。排查结论：**与 4/5 项无关，是既有缺陷**。

- 用户的截图全部来自 `Pictures\Screenshots`（Win+Shift+S / Win+PrtScn 存文件的那条路），
  而不是剪贴板路径。
- `StartWatcher` 调用 `CaptureArea.Guess(w, h, foreground, fromPointer: false)`：区域截图
  既不等于屏幕尺寸、也不等于前台窗口尺寸，于是返回 `null` → `HangCapture` 直接挂卡、
  不飞。只有"整屏"或"正好是某个窗口大小"的截图才会飞。
- 复现：往 `Pictures\Screenshots` 写入一个区域尺寸的新 PNG，全程无飞行窗口（卡片直接出现）。

修复（`src/App.cs`）：

1. 文件监听路径改为**允许用指针推断起点**，但加 3 秒新鲜度判断：
   ```csharp
   bool fresh = (DateTime.UtcNow - Pegged.SafeWriteTime(path)).TotalSeconds < 3;
   WatchCapture(path, CaptureArea.Guess(w, h, Native.GetForegroundWindow(), fresh));
   ```
   Win+Shift+S 框选结束时指针正好停在选区右下角，`Guess` 的指针分支正好还原截取区域；
   过期文件仍退回"直接落地"，避免从鼠标当前位置莫名其妙地飞。
2. `HangDecoded` 在起飞前显式确保线带已显示：
   ```csharp
   wanted = true; Refresh(); if (isPresent) Reveal(false, 2.5);
   ```
   这样即使线带已满（`LiveCount` 不增长、`ItemsChanged` 的 `live > lastLiveCount` 分支不执行），
   新截图依然会显示并起飞。

验证：写入新鲜的区域尺寸 PNG 后，飞行窗口在 ~160 ms 出现（`2623,-23 1832×2251`），
卡片正常飞向绳上；期间用户同时截的图也各自起飞，无重复卡片、`error.log` 无记录。

注意：`Copy-Item` 会保留源文件时间戳，测试时必须用 `[System.IO.File]::WriteAllBytes` 之类的写法生成"新鲜"文件，否则会被新鲜度判断挡掉。

### 第 6 项 · 逐帧分配（2026-11-02，部分完成）

保留的改动（`src/LineWindow.cs`）：

- `hint` 的 `Measure` 只做一次（`hintMeasured` 标志，`PlaceOnScreen` 时重置）；
- `Canvas.SetLeft/SetTop(hint, …)` 只在计算出的位置变化时才调用。

**回退的改动**（`src/CardView.cs`，`git checkout -- src/CardView.cs`）：

原本把卡片定位从 `Canvas.SetLeft/SetTop` 换成复用的 `TranslateTransform`（`TransformGroup{place, rotate}`），
并把 `cross`/`actions`/`copiedLabel` 的变换改为复用实例。改完后人工确认时**整条绳带的卡片位置乱了**，
表现为卡片高低不一、互相重叠。

排查记录（都排除了）：

- 探针 `mode=line`（同样的 `CardView` 代码 + 真实 12 张图）渲染正常，几何数值与 `CardFrame` 一致；
- `mode=dpi` 验证了隐藏/显示/隐藏后 `Place()` 的 `LayoutTransform` 始终是 1.0，不存在整条内容被 2× 缩放；
- 应用无 `error.log`、无残留飞行窗口；
- 回退 `CardView.cs` 后，1:1 截图确认绳带恢复正常（卡片挂在绳上，中间因绳子下垂略低，倾斜是设计内的随机角）。

结论：这次定位方式改动在真实应用里的行为与探针不一致，原因未查明；该项收益本来就小（只是少一点
GC 抖动和一次布局），不值得继续冒险，故回退并记录。第 6 项的"降空闲 CPU"目标只完成了 `LineWindow`
那一半。

实测空闲 CPU（12 张卡片、绳带收起、鼠标不在顶部）：单核 **4.2%**（0.25 s / 6 s）。
注意这与最初的 2.6% 不可直接比较——那次测的是空绳带（0 张卡片）。

### 第 1 项 · 飞行阶段不画阴影（2026-11-02 完成）

改动：`src/Flight.cs`（+28 / −1）

- 新增 `public const double ShadowBelowDip = 300;`
- `DropShadowEffect` 从构造函数里挪到字段，构造时**不**挂到 `container`
- `Place(w, h, ...)` 里按卡片宽度开关：`w <= ShadowBelowDip` 才挂，否则 `container.Effect = null`

实测（`mode=flight`，同一台机器）：

| 场景 | 改前 | 改后 |
|:--|--:|--:|
| 2266×2392 截图 | 33.2 fps，jank 39% | **40.5 fps，jank 14%** |
| 全屏 6016×3384 | 8.4 fps，单帧 117–225 ms | **27.5 fps，单帧 24–81 ms** |

`test.ps1` 通过，程序已重启。人工确认：起飞瞬间无阴影、接近落定时阴影淡入、落地无跳变。

**追加调整（人工确认后）**：原版相框透明度是 `Ease.Smooth(k, 0.35, 1)`（按缓动进度，约飞到
44% 才开始淡入）。去掉阴影后前段只剩一个光秃秃的矩形，观感突兀。改为按**飞行时间**淡入：

```csharp
const double ChromeFrom = 0.10, ChromeTo = 0.55;
double chrome = Ease.Smooth(raw, ChromeFrom, ChromeTo);
```

即 10% 处开始、55% 处完全显示。探针抓帧确认：240 ms 时白框与夹子已完整、仍无阴影。
想更早/更晚出现，只调这两个常量即可。

### 第 2 项 · 起飞尺寸上限 + 预缩放（2026-11-02 完成）

改动：`src/Flight.cs`（+68 / −5 累计）、`src/App.cs`（+4 / −1）

- 新增 `public const int MaxStartPixels = 1600;` 与 `Limit(Rect)`：起飞矩形按自身中心等比缩小到最长边 ≤ 1600 设备像素
- 构造函数里 `this.from = Local(Limit(from))`（落点 `to` 不变）
- `RenderOptions.SetBitmapScalingMode(photo, BitmapScalingMode.Linear)`（原为 `HighQuality`）
- `App.Fly` 的解码尺寸从 `min(3000, …)` 改为 `min(Flight.MaxStartPixels, …)`

实测（`mode=flight`）：

| 场景 | 第 1 项后 | 第 2 项后 |
|:--|--:|--:|
| 2266×2392 | 40.5 fps | **47.7 fps**，jank 12.5%，p50 16.5 ms |
| 全屏 6016×3384 | 27.5 fps | **46.2 fps**，jank 12.9%，p50 16.7 ms |
| 1141×695 | — | 49.7 fps，p50 16.7 ms |

p50 已是 16.5–16.7 ms（满帧），平均值被开头几帧的**建窗**成本拖低 —— 第 3 项解决。
副作用：全屏截图起飞时按 1600px 居中显示（不再铺满屏幕），磁盘上的原图不受影响。

### 第 3 项 · 窗口复用 + 轨迹包围盒（2026-11-02 完成）

改动：`src/Flight.cs`（+156 / −23 累计）

- 构造函数改为无参，只搭视觉树和窗口样式；每次飞行走 `Begin(image, fromPx, toPx, tilt, on, falling, completion)`
- 新增 `static readonly List<Flight> idle` + `Take()`：飞行结束后 `Hide()` 并放回池子，下次直接复用
- 新增 `Bounds(Rect a, Rect b)`：窗口只覆盖「起飞矩形 ∪ 落点矩形」再各边外扩 `60 + 5%`（给相框、倾斜和阴影留余量），不再是 `Place(on.Bounds)` 整屏
- `Finish()` 由 `Close()` 改为 `Hide()` + 回收
- 布局在 `Show()` 之前完成，复用窗口不会闪上一张图

实测（`mode=flight ... repeat=2`，同一进程内先冷后热）：

| 场景 | 第 2 项后 | 第 3 项 · 新建窗口 | 第 3 项 · 复用窗口 |
|:--|--:|--:|--:|
| 2266×2392 | 47.7 fps | 51.0 fps | **58.6 fps**，jank 0%，p50 16.5 ms，max 28.9 |
| 全屏 6016×3384 | 46.2 fps | 55.8 fps | **61.7 fps**，jank 0%，p50 16.4 ms，max 22.7 |

仍有一次性的冷启动成本：程序启动后**第一次**飞行要建窗口、分配透明表面，首帧约 58 ms。
之后每次复用窗口，全程满帧。

### 第 4 项 · 去掉 Flight 的逐帧布局与分配（2026-11-02 完成）

改动：`src/Flight.cs`

- 新增 `readonly RectangleGeometry photoClip`，构造时 `photo.Clip = photoClip`；`SetChrome` 里改为
  修改 `Rect` / `RadiusX` / `RadiusY`，不再每帧 `new RectangleGeometry(...)`
- 新增 `readonly RotateTransform clipRotate(0, 4.5, 14)`，构造时挂给夹子；`Place()` 里只改 `Angle`，
  不再每帧 `new RotateTransform(...)`
- `Update()` 里每帧分配的 `Func<double,double,double> lerp` 闭包改为直接算式
- 布局仍在 `Show()` 之前完成（第 3 项）

实测：与第 3 项基本持平（复用窗口 58.3 fps / 全屏 60.7 fps，p50 16.6 / 16.3 ms）。
这一项是"少一点抖动"，不改变上限；收益体现在 GC 压力和渲染树脏标记上。

### 第 5 项 · 重活移出 UI 线程（2026-11-02 完成）

改动：`src/Support.cs`（+27）、`src/App.cs`、`src/Watchers.cs`、`src/Line.cs`

- `Support.cs` 新增 `Background.Run(Func<object> work, Action<object> done)`：线程池执行，回调
  marshal 回 `Application.Current.Dispatcher`；worker 抛异常时写 `error.log`（`Program.Log` 改为 public）
- `Watchers.cs`：剪贴板截图先 `Freeze()`，PNG 编码与落盘移到 worker；无法冻结时退回同步保存
- `App.cs`：`HangCapture` 里的「判重 + 解码 + 生成缩略图」整段移到 worker，回来后 `HangDecoded`
  在 UI 线程挂卡片；`Fly` 直接使用 worker 解码好的图，不再在 UI 线程 `Thumbnails.Load`
- `Line.cs`：新增 `Hang(path, thumb, quietly, flying)` 重载与 `Thumbnails.Scaled()`（在 worker 上
  一次性物化成 `WriteableBitmap`，避免懒加载把重采样留到 UI 线程）
- `recent` 列表加 `recentLock`；判重改为「先登记再比较」，避免两张孪生截图并发时互相看不见

实测（真实链路，838×733 剪贴板截图）：剪贴板置入后 324 ms 飞行窗口出现，窗口为轨迹包围盒
（`3823,-21 1488×2411`），约 0.9 s 后消失，`error.log` 无记录；抓帧确认飞行卡片正常渲染。
UI 线程不再被编码/解码/判重阻塞（原来单次可达 0.3–0.7 s）。


回滚前那版「六项一起改」的完整改动保存在：

- `build/patches/all-6-items.patch` —— 相对仓库默认状态的完整 diff
- `build/patches/final/src/*.cs` —— 改完后的源码副本（可直接对照）

（`build/` 被 git 忽略，不会污染仓库。）

## 诊断时发现、但与本轮优化无关的既有行为

这几条在**仓库默认状态**下就存在，记录备用，暂不处理：

1. **线带满员时新截图不落地也不飞**：`Hang` 里超过 `MaxItems`（本机 12 张）会挤掉最旧的一张，
   `LiveCount` 不变，`ItemsChanged` 的 `live > lastLiveCount` 分支不执行 → 线带不显示、`Fly`
   的 `isRevealed` 守卫失败 → 卡片直接 `Land`，看不到起飞动画。
2. **前台是全屏窗口时不显示线带**（`FullScreen.IsActive`），此时截图同样不会飞。设计如此，
   但用户可能误以为"动画坏了"。
3. 线带窗口内容偶尔出现整体错位，需在逐项验证时确认是否与本轮改动有关（诊断时用
   `mode=line` 探针测得的几何是正确的，怀疑与"线带未显示时卡片不 Step"有关）。

## 验证方式

- 帧率：`build\probe\Probe.exe mode=flight spanw=2266 spanh=2392 repeat=2`
  （`repeat=2` 分别测"新建窗口"和"复用窗口"）
- 视觉：`mode=flight ... shots=1` 会在 `build\probe\shot*.png` 留下飞行过程帧
- 功能：真实截图一次（Win+PrtScn 或 Ctrl+Alt+S），确认卡片挂上、动画顺滑、落地无跳变

## 内存占用分析（2026-11-02）

用户反馈托盘常驻时内存"百兆级"。实测（本机 6016×3384 @200%，6 张卡片）：

| 状态 | private | working set |
|:--|--:|--:|
| 刚启动、线带收起 | 120–129 MB | 156 MB |
| 线带放下、6 张卡片 | 216 MB | 267 MB |
| 老版本进程（已跑 45 分钟） | 343 MB | 342 MB |
| 一次 6K 截图过程中的峰值 | 355 MB | 505 MB |
| 其中托管堆 | **4.6 MB** | — |

结论：托管堆只有几 MB，内存几乎全是 WPF 的原生表面（D3D9/分层窗口缓冲），不是泄漏。

归因（探针 `build\probe\MemProbe.exe`、`LadderProbe.exe`、`DecodePeak.exe`，均在 build 下不入库）：

- **线带窗口本身 ≈ +185–205 MB**：6016×420 的分层透明窗口 + 6 张卡片。空的不透明窗口在硬件
  渲染下也要 +74 MB（其中约 43 MB 是第一个窗口触发的 D3D 设备固定开销），分层再 +29 MB，
  卡片与动画再 +100 MB。24 个 `DropShadowEffect` 约占 28 MB；动画 10 秒内存稳定不增长，
  是表面池的上限，不是泄漏。
- **飞行窗口 ≈ +50–180 MB**：窗口覆盖整条飞行路径（实测 3008×3264 量级），结束后隐藏但
  表面仍被池化保留。这是老进程比新启动进程多出上百 MB 的主要原因。
- **软件渲染**（`RenderOptions.ProcessRenderMode = SoftwareOnly`）：线带降到 91 MB、飞行
  135 MB，但真实 Flight 从 60 fps 掉到 36–45 fps，代价太大，未采用。
- **一次性峰值**：判重 `SamePicture` 旧实现把两张 6K 图各解成全尺寸 `byte[]`，6K 自比较峰值
  +237 MB；剪贴板 `Opaque`/`Hash` 同样各拷一份全尺寸位图。

本轮改动（`build.cmd` + `test.ps1` + 重启验证）：

1. `Thumbnails.SamePicture`：两边都按最长边 2048 解码（`ComparePixels`），再按 64 行一条带
   比较。6K 自比较 294 ms / 峰值 +237 MB → **90 ms / +23 MB**，判重结果不变
   （自比较 true、不同图 false）。
2. `ClipboardCatcher.Opaque`/`Hash`：按条带扫描，不透明图在第一个像素就返回；哈希只取每 4 行。
   不再有 80 MB 级 `byte[]`，剪贴板通知返回更快。
3. `Flight.Finish`：隐藏后把池化窗口缩到 2×2 交还表面，下次飞行重新 `Place`。探针：飞行结束
   514 MB → **461 MB**，复用窗口 58.6 → 59.6 fps，无回归。
4. `EditCapture` 的 `Original` 只解到 2048（判重本来也只比到 2048），编辑期间少留 81 MB。

仍可探索（未做）：

- 飞行窗口改为**跟着卡片移动**（每帧 `SetWindowPos`，`SWP_NOSIZE`），不再一次覆盖整条路径：
  窗口面积可从 5–7 MP 降到 1 MP 以下，软件渲染下的 CPU 也会大降；风险是逐帧移窗的观感。
- `HoverPreview` 的 `ImagePreviewWindow` 在隐藏后 `Close()`（约 4 MB 表面 + 渲染缓存），下次
  悬停重建；350 ms 的悬停延迟足够掩盖建窗成本。
- 线带窗口是最大头（约 200 MB），但属于 WPF 分层窗口的固有成本。应用层唯一的大杠杆是整体
  切软件渲染，代价是起飞动画掉到 36–45 fps，不建议默认开启。
