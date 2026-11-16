# Pegline 2.5.0 便签编辑器可调整大小 — 执行报告

- 版本：2.5.0
- 报告时间：2026-11-12 15:50:04
- 代码基线：`71cb85a`（2.4.2 之后）
- 状态：设计定稿，待实现
- 范围：便签编辑器的右边缘、右下角、下边缘拖拽缩放；悬停高亮；每张便签独立的尺寸记忆。
- 不在范围：移动编辑器窗口、左/上边缘缩放、键盘缩放、触控手势、悬停预览缩放。

---

## 0. 结论摘要

**可行，且不需要引入新 crate。** Win32 侧用 `SetCapture` + `SetWindowPos` 手写缩放，保留 `WS_POPUP`，不引入 `WS_THICKFRAME`，可以避开 DWM 在 Win10/11 上给可调窗口加的 1 px 边框、系统阴影和圆角，也避开模态 sizing loop 的重入问题。

真正的工作量不在“拖边框”，而在三处：

1. **去常量化的 15 处耦合**：纸张 460×360、内容宽 400、窗口 500×400 分散在 `display.rs`、`render/mod.rs`、`paint.rs`、`window.rs`、`app/cards.rs`、`app/mod.rs`。必须收敛成一个 `PaperSize`。
2. **阴影**：当前每次 `WM_SIZE` 都会在 CPU 上对整窗做 6 遍 box blur，460×360@200% 约 4.8 MB/8–15 ms，820×640@200% 约 9.4 MB/30–50 ms。直接拖拽会违反“不在 CPU 上栅格化整窗”和“无每帧堆分配”两条硬规则。必须改成 9-patch 一次性烘焙。
3. **点击命中与提交的优先级**：编辑器打开时，`WH_MOUSE_LL` 钩子会在窗口过程之前把“纸张外点击”解释为提交。右下 20 DIP 的阴影边距正好是未来的缩放带；不先改这条路径，第一次拖边框就会把便签提交掉。

按 200% DPI、820×640 纸张估算，编辑器打开内存从当前实测 158.11 MB 上升到约 177–183 MB（+19～25 MB），低于 190 MB 的验收线。该数字是估算，必须用 probe 实测后才能写进 `PLAN.md`。

预计工作量：**5–7 个工作日**（含视觉验收和 probe），其中约一半花在去常量化和阴影 9-patch 上。

---

## 1. 需求与已确认决策

### 1.1 用户已确认

| 项目 | 决定 |
|---|---|
| 最大纸张 | 820×640 DIP |
| 尺寸记忆 | 每张便签独立记忆 |
| 可发现性 | 悬停时高亮边框 |
| “再次点开查看” | 仅编辑器；悬停预览保持固定 408×264 DIP |

### 1.2 本报告采用的尺寸定义

“当前大小”指**可见纸张**，不是含阴影的外框：

| 对象 | 最小/默认 | 最大 |
|---|---:|---:|
| 纸张（paper） | 460×360 DIP | 820×640 DIP |
| 窗口（paper + 2×20 DIP 阴影边距） | 500×400 DIP | 860×680 DIP |
| 内容宽（paper - 30 - 30） | 400 DIP | 760 DIP |
| 可视高（paper - 26 - 30） | 304 DIP | 584 DIP |

本报告按文档和现有实现中的纸张 460×360 执行；窗口 500×400 是含 20 DIP 阴影边距的外框。

### 1.3 交互定义

- 只允许从**右边缘、下边缘、右下角**缩放；左、上边缘不动，左上角固定。
- 拖拽时窗口向右、向下生长；不改变窗口位置。
- 尺寸限制在纸张 DIP 上计算，实时预览用物理像素，释放时取整到 DIP。
- 不做等比锁定；宽高独立。
- 缩放不是文本编辑，不进入撤销历史，不改变便签的 dirty 状态。
- 缩放不改变字体大小、内边距、圆角、折角尺寸或卡片/预览尺寸；只改变可用空间和换行宽度。

---

## 2. 现状与差距

### 2.1 关键代码锚点

| 位置 | 现状 | 2.5.0 需要 |
|---|---|---|
| `src/note_editor/display.rs:17-31` | `PAPER_WIDTH=460`、`PAPER_HEIGHT=360`、`CONTENT_WIDTH=400` 编译期常量 | `PaperSize` 值类型；内容宽/可视高由纸张推导 |
| `src/note_editor/render/mod.rs:69-82` | `WINDOW_WIDTH_DIP=500`、`WINDOW_HEIGHT_DIP=400`、`MARGIN_DIP=20` | 窗口 = 纸张 + 2×margin，运行时推导 |
| `src/note_editor/render/mod.rs:1616-1619` | `paper_rect(scale)` 忽略 `scale`，返回常量 | 返回当前纸张矩形 |
| `src/note_editor/render/mod.rs:1651` | `paper_gradient` 用 `PAPER_HEIGHT` | 用当前纸张高度 |
| `src/note_editor/render/mod.rs:1671-1674` | `fold_geometries` 用常量 | 用当前纸张宽高 |
| `src/note_editor/render/mod.rs:1944-2005` | `ensure_paper` 按常量分配位图 | 按当前纸张×DPI 分配 |
| `src/note_editor/render/paint.rs:32` | `paint_paper_on` 全篇常量 | 接收 `PaperSize`，所有装饰/选区/文本跟随 |
| `src/note_editor/render/mod.rs:1056-1103` | 换行宽 `CONTENT_WIDTH - indent` | `paper.content_width() - indent` |
| `src/note_editor/render/mod.rs:419-447` | `Parsed::is_current` 不含宽度 | 宽度进入缓存键；宽度变化强制重建 |
| `src/note_editor/render/mod.rs:514` | `view_height` 初始化一次 | 每次 resize 更新并 clamp scroll |
| `src/note_editor/render/mod.rs:1298-1302` | 飞行图固定 460×360×scale | 当前纸张×scale；限制最长边 2048 px |
| `src/note_editor/render/mod.rs:1728-1770` | `bake_shadow` 整窗 CPU blur，每次 resize 调用 | 9-patch 图集，每 DPI 烘焙一次 |
| `src/note_editor/window.rs:981-1038` | 纸张外点击直接 `request_commit` | 先判缩放带，再判提交 |
| `src/note_editor/window.rs:1367-1395` | `paper_contains` 用常量 | 用当前纸张和圆角 |
| `src/note_editor/window.rs:1109-1165` | `WM_DPICHANGED` 重新居中到 500×400 | 保留记忆的纸张 DIP，clamp 后重新定位 |
| `src/app/pointer.rs:64-75` | LL 钩子纸张外点击直接 `commit()` | 共用 `hit_test`，缩放带不提交 |
| `src/app/cards.rs:304-333` | 飞行起终点按常量推算 | 从 HWND client rect 推导纸张 |
| `src/app/mod.rs:2759` | 丢弃新便签在 UI 线程 `render_paper(460,360)` | 用提交事件里的当前纸张图，或 worker |
| `src/app/mod.rs:3739` | 退出时 `settings.save()` | 关闭/退出前持久化尺寸 |

### 2.2 为什么不能直接加 `WS_THICKFRAME`

- Win10/11 会给 `WS_THICKFRAME` 窗口加 1 px 非客户区边框、系统阴影和圆角，和现有“无边框玻璃纸”外观冲突。
- 系统 sizing loop 会进入模态消息循环，`WM_ENTERSIZEMOVE`/`WM_EXITSIZEMOVE` 与现有 `WH_MOUSE_LL`、IME、提交逻辑交织，容易重入。
- `WS_THICKFRAME` 还要求处理 `WM_NCCALCSIZE`/`WM_NCHITTEST`，并可能在最大化/贴边时产生意外行为。
- 手写 `SetCapture` + `SetWindowPos` 只有一条消息路径，容易控制和测试；代价是光标、捕获丢失、焦点丢失要自己处理。

---

## 3. 方案设计

### 3.1 单一尺寸模型：`PaperSize`

在 `src/note_editor/display.rs` 增加：

```rust
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PaperSize {
    pub width: f32,  // DIP
    pub height: f32, // DIP
}

pub const MIN_PAPER: PaperSize = PaperSize { width: 460.0, height: 360.0 };
pub const MAX_PAPER: PaperSize = PaperSize { width: 820.0, height: 640.0 };
pub const DEFAULT_PAPER: PaperSize = MIN_PAPER;

impl PaperSize {
    pub fn content_width(self) -> f32 {
        (self.width - PAD_LEFT - PAD_RIGHT).max(1.0)
    }
    pub fn view_height(self) -> f32 {
        (self.height - PAD_TOP - PAD_BOTTOM).max(1.0)
    }
    pub fn window_size(self) -> (f32, f32) {
        (self.width + 2.0 * MARGIN_DIP, self.height + 2.0 * MARGIN_DIP)
    }
    pub fn clamp(self, min: PaperSize, max: PaperSize) -> PaperSize {
        // 小工作区例外时 max 可能小于 min；此时收敛到 max，不能强制 min。
        let max_w = max.width.max(1.0);
        let max_h = max.height.max(1.0);
        let min_w = min.width.min(max_w);
        let min_h = min.height.min(max_h);
        let width = if self.width.is_finite() { self.width } else { min_w };
        let height = if self.height.is_finite() { self.height } else { min_h };
        PaperSize {
            width: width.clamp(min_w, max_w),
            height: height.clamp(min_h, max_h),
        }
    }
}
```

约定：

- `MARGIN_DIP` 从 `render/mod.rs` 移到 `display.rs`（`render` 改为 `use display::MARGIN_DIP`），否则 `display` 会反向依赖 `render`。
- 默认尺寸比较用四舍五入后的整数 DIP，不直接比较 `f32`；`clamp` 入口先拒绝 NaN/∞。`max < min` 时收敛到 `max`（3.7 小工作区例外），不能反过来强制 `min`。

- **唯一真值源是 HWND 的 client rect**。窗口没有非客户区，所以 `paper_px = client_px - 2 * margin_px`。UI 线程和渲染线程都从同一个窗口尺寸推导，不新增跨线程尺寸命令。
- 当前**显示**尺寸的唯一真值源是 HWND client rect；`explicit_paper_dip` 只用于记忆/恢复，不参与命中测试。
- 渲染线程在 `EditorState::resize` 中更新 `paper`；UI 线程在 `hit_test`、`WM_DPICHANGED`、飞行起终点中用同一个推导。
- 默认尺寸下所有现有测试仍应得到 500×400 窗口、460×360 纸张；去常量化的第一阶段不改变任何可见行为。

### 3.2 几何与命中测试

缩放带定义（单位 DIP，窗口客户区坐标）：

- 纸张矩形：`left = 20`、`top = 20`、`right = 20 + paper.width`、`bottom = 20 + paper.height`。
- 右带：`x ∈ [paper.right - 2, client.right]`，且 `y ∈ [paper.top - 2, paper.bottom + 2]`。
- 下带：`y ∈ [paper.bottom - 2, client.bottom]`，且 `x ∈ [paper.left - 2, paper.right + 2]`。
- 右下角：右带与下带的交集。它覆盖 20 DIP 阴影边距和右下折角。
- 左/上边距、右上阴影角、左下阴影角仍然属于“提交区”。

2 DIP 的 `-2` 是为了让可见纸张边框本身可抓；它落在 30 DIP 内边距上，不会抢正文点击。

统一命中函数：

```rust
pub enum ResizeZone { Right, Bottom, Corner }

pub enum Hit {
    Paper,          // 可放光标/选择文本
    Resize(ResizeZone),
    Margin,         // 窗口内、纸张外、非缩放带 -> 提交
    Outside,        // 窗口外 -> 提交
}

impl NoteEditor {
    pub fn hit_test_screen(&self, x: i32, y: i32) -> Hit { /* ... */ }
    pub fn is_commit_point(&self, x: i32, y: i32) -> bool {
        matches!(self.hit_test_screen(x, y), Hit::Margin | Hit::Outside)
    }
}
```

优先级：`Resize` > `Paper` > `Margin` > `Outside`。`paper_contains` 仍然负责圆角，缩放带在它之前判断。

**提交路径必须共用这个函数**：

- `window.rs:991` 的 `WM_LBUTTONDOWN`：`Hit::Resize` 开始缩放；`Hit::Paper` 进入文本；`Hit::Margin/Outside` 提交。
- `app/pointer.rs:64-75` 的 LL 钩子：`is_commit_point()` 为真才 `editor.commit()`。缩放带和纸张内都返回 false。
- 不要在 LL 钩子里复制一套矩形计算；否则两处几何一旦不同步，就会出现“拖动时提交”或“纸张内点不到”的回归。

### 3.3 缩放状态机

```rust
struct ResizeState {
    zone: ResizeZone,
    start_cursor: POINT,       // screen px
    start_window: RECT,        // screen px
    start_paper_px: (i32, i32),
    moved: bool,               // 纸张 px 实际变化过
}
```

消息处理：

| 消息/事件 | 行为 |
|---|---|
| `WM_LBUTTONDOWN` 在缩放带 | `SetCapture`；记录 `ResizeState`；`pointer_down=false`；不发送 `PointerDown`；不提交。`SetCapture` 失败则记日志并放弃本次缩放。 |
| `WM_MOUSEMOVE` 且 `resizing` | 按 zone 计算 `dx/dy`；纸张 px 夹在 `[min_px, max_px]`；`SetWindowPos(SWP_NOMOVE|SWP_NOZORDER|SWP_NOACTIVATE)`；纸张 px 实际变化时 `moved=true`；不发送 `PointerMove`；重设尺寸光标。 |
| `WM_LBUTTONUP` | 先结束/取出 `ResizeState`，再 `ReleaseCapture`；若 `moved`，取整到 DIP、最后一次 `SetWindowPos`、写内存尺寸并持久化；若未移动，只结束缩放，不写键；`resizing=None`；不提交。 |
| `WM_CAPTURECHANGED` 且 `resizing` | 以最后一帧尺寸结束；仅 `moved` 时持久化；若焦点已丢失且未在停止，随后 `request_commit()`。 |
| `Esc` 且 `resizing` | 取消：恢复 `start_window` 和 `start_paper`；不提交。 |
| `Ctrl+S` 且 `resizing` | 先结束缩放，再走现有保存/提交。 |
| `WM_KILLFOCUS`/`WM_ACTIVATE(WA_INACTIVE)` 且 `resizing` | 先结束缩放并持久化，再提交；提交图用最终尺寸。 |
| `WM_CLOSE` 且 `resizing` | 先结束缩放，再走现有 `request_commit`。 |
| `WM_SETCURSOR` | `Resize` -> `IDC_SIZEWE`/`IDC_SIZENS`/`IDC_SIZENWSE`；`Paper` -> `IDC_IBEAM`；其余 -> `IDC_ARROW`。 |
| `WM_MOUSEMOVE` 且未缩放 | 命中变化时更新 hover；首次进入时 `TrackMouseEvent(TME_LEAVE)`。 |
| `WM_MOUSELEAVE` | 清除 hover；若未缩放则光标恢复箭头，缩放中保留尺寸光标。 |
| `WM_SIZE` | 忽略 0 宽高；发送 `Command::Resize{width_px,height_px,dpi}`；更新 UI 侧 `effective_paper`，不覆盖 `explicit_paper_dip`。 |
| `WM_DPICHANGED` | 先结束缩放（若有），用保存的 `explicit_paper_dip`，按新 DPI clamp，重新定位并 `SetWindowPos`；不要回到 500×400。 |
| `WM_GETMINMAXINFO` | 正常最小窗口 = 500×400×scale；3.7 例外模式下 min=max=工作区，不能 min > max；最大窗口 = 当前左上角到工作区右下角可容纳的尺寸；手写 clamp 仍是权威。 |
| `WM_DISPLAYCHANGE`/`WM_SETTINGCHANGE` | 先结束缩放（若有）；重算工作区；以 `explicit_paper_dip` 重新 clamp；必要时重新定位；发送 Resize。 |

夹取公式（物理像素）：

```text
max_w_px = min(MAX_PAPER.width * scale,
               work.right - window.left - 2 * margin_px)
max_h_px = min(MAX_PAPER.height * scale,
               work.bottom - window.top - 2 * margin_px)
```

`moved` 只在纸张 px 实际变化时置 true。纯点击缩放带、或在最大/最小处继续拖而不改变尺寸，都不写内存、不落盘；否则会把“小屏 clamp 后的 effective 尺寸”误写成新的显式尺寸。

结束缩放时先把 `ResizeState` 从 `resizing` 取出（或置 `finishing`），再 `ReleaseCapture`；否则 `WM_CAPTURECHANGED` 会重入，造成二次持久化/提交。

`min_px = MIN_PAPER * scale`。如果工作区连最小窗口都放不下，走 3.7 的“小工作区例外”。若 `max_px < min_px`（例外模式），不进入缩放；窗口固定为工作区，避免把逆序区间交给 `f32::clamp`。

**不做** `WM_ENTERSIZEMOVE` 模态循环；日志里用自定义 begin/end 记录一次拖拽，便于 probe 统计。

### 3.4 初始定位与“增长包络”

如果每次都把 500×400 居中，在 1366×768 这类屏幕上，窗口向下只能长到约 544 DIP 的纸张高，820×640 的最大值永远够不到。解决办法不是拖拽时移动窗口，而是**打开时预留增长空间**：

```text
desired_px = paper.window_size() * scale
growth_px  = min(MAX_PAPER.window_size() * scale, work.size_px())
left = min(center_left(desired_px), work.right_px - growth_px.width)
top  = min(center_top(desired_px),  work.bottom_px - growth_px.height)
left = max(left, work.left_px)
top  = max(top, work.top_px)
```

`growth` 是**窗口**的增长包络，不是纸张；窗口可以超出纸张 + 边距，最多到工作区（仍受最大纸张限制），纸张最大是 `min(MAX_PAPER, work.size() - 2*MARGIN)`。例如 1024×600 工作区：窗口最大 `min(860,1024) × min(680,600) = 860×600`，纸张最大 `min(820,984) × min(640,560) = 820×560`；不是把窗口也限制成 `min(860,984) × min(680,560) = 860×560`，也不是允许 1024×600 的窗口（纸张宽度上限 820）。

- 大屏幕上 `growth` 能容纳最大窗口，`left/top` 就是普通居中。
- 小屏幕上窗口会向左上偏移，直到从该位置能把最大尺寸长满；纸张仍然完整可见。
- 记住的尺寸接近最大时，`desired == growth`，自然居中。
- 拖拽期间左上角固定；只有打开、DPI 变化、工作区变化时才重新定位。

### 3.5 渲染与布局

`EditorState` 增加：

- `paper: PaperSize`
- `hover: Option<ResizeZone>`（由 `Shared` 原子量更新，只有变化时置 dirty）
- `layout_width: f32`（当前布局使用的内容宽）
- `shadow: ShadowAtlas`（9-patch）

`EditorState::resize(width_px, height_px, dpi)` 的顺序：

1. 更新 `width_px/height_px/dpi/scale/paper`。
2. `view_height = paper.view_height()`。
3. 如果 `content_width` 或 `dpi` 变了：
   - `parsed.ready = false`；
   - `layout_generation += 1`；
   - 清空按文本缓存的布局（避免旧宽度/旧字体度量布局被复用）；
   - 下一帧 `rebuild()` 重新换行。
4. 如果只是高度变了：不重排文本，只 `clamp_scroll()`。
5. 重建 swap chain/paper 位图；更新 `paper_rect`、gradient、fold、clip。
6. 更新 9-patch 的目标矩形；**不重新烘焙阴影**。
7. `dirty = true`。

渲染线程 drain 时合并连续的 `Command::Resize`，只保留最后一个；`Surface::resize` 在 client 尺寸未变时直接返回。每个窗口尺寸最多一次 `ResizeBuffers`/纸张位图重建，避免拖拽时命令队列把每帧放大成多次分配。

`paint_paper_on` 改为接收 `PaperSize`（或 `content_width`），替换：

- 纸张圆角/渐变 -> `paper.width/height`
- 代码块背景、引用条、分隔线、列表标记、选区、文本原点 -> `paper.content_width()`
- 折角 -> `paper.width/height`
- 选区/光标/IME 组合框 -> 当前内容宽

**换行缓存**：`Parsed::is_current` 必须包含 `content_width` 的 bits，或者宽度变化时无条件 `ready=false` 并清空布局缓存。只改 `LayoutInputs.width` 不够；现有快速路径会拿旧 `Parsed` 继续用。加一个单元测试：460 宽下 20 个 CJK 字符换行，820 宽下同一文本行数必须减少。

**滚动与光标**：

- resize 过程中只 `clamp_scroll()`，不自动 `ensure_caret_visible`，避免拖边框时正文跳动。
- 释放时如果编辑器有焦点，再 `ensure_caret_visible()`。
- 宽度变化后保留光标/选区偏移（字节偏移不变），只重新计算视觉位置。
- IME 组合期间允许缩放：不取消组合；重排后 post `WM_APP_IME_CARET` 重新锚定候选窗。用 Microsoft Pinyin 200% 验证。

### 3.6 阴影：9-patch 图集

当前 `bake_shadow` 每次 `WM_SIZE` 都分配 `1+1+4` 字节/像素并做 6 遍模糊。改成：

- 纸张圆角、阴影偏移、模糊半径固定；直边上的模糊截面与边长无关，只有四个角是特殊的。
- 按当前 DPI 烘焙一张小图集：四个角 + 四条边 + 中心（中心可以是 1×1 或空），总尺寸约 `(2*corner + 2)²`。
- `corner` 取 `MARGIN_DIP + RADIUS_DIP + 少量余量`，约 44–48 DIP；200% 下约 96 px，3×3 图集约 `288×288×4 ≈ 0.33 MB`，留余量按 0.3–0.6 MB 估算。
- 每次 resize 只更新 9 个目标矩形（`DrawBitmap` 的 destination），不碰像素。
- 图集只在初始化、DPI 变化时重烘；阴影颜色固定，不需要每帧/每次拖拽重烘。
- 目标矩形按设备像素对齐，避免 1 px 接缝；边条用线性插值拉伸。
- 备选方案“释放时烘一次、拖拽时拉伸旧阴影”会让阴影在拖拽中变形 2–3 倍，不推荐。

### 3.7 小工作区例外

正常情况最小纸张 460×360。若工作区连 500×400 窗口都放不下：

- `available = work.size() - 2*MARGIN`。
- 若 `available >= MIN_PAPER`，正常 clamp。
- 若 `available < MIN_PAPER`，进入例外模式：纸张取 `available.max(1)`，窗口填满工作区，允许内部滚动；阴影边距可能被裁掉。
- 例外模式下先把窗口左上角放到工作区左上角，再把 `WM_GETMINMAXINFO` 的 min/max track 都设为工作区大小（min 不能大于 max）；否则 USER32 会阻止窗口收到目标尺寸。
- 这是极端环境兜底，不改变正常情况下的“460×360 是最小值”。日志记录一次 `note resize: small work area`，不弹窗、不加 i18n 字符串。

### 3.8 悬停高亮与光标

- 命中 `Resize(Right)`：只高亮右边缘（含右下圆角），2 DIP，系统高亮色（`COLOR_HIGHLIGHT`，取不到时用 `#4A90E2`）。
- 命中 `Resize(Bottom)`：只高亮下边缘。
- 命中 `Resize(Corner)`：高亮右+下边缘和右下圆角。
- 高亮画在窗口层，不画进纸张位图；不改变 ClearType，也不会进入提交图/缩略图。
- 高亮即时出现，不做动画；光标已经足够表达可拖拽。
- 光标在捕获期间不会被 `WM_SETCURSOR` 自动刷新，所以 `WM_MOUSEMOVE` 里必须重新 `SetCursor`。
- 不要永久显示拖拽把手；用户已选择“悬停高亮”。

### 3.9 飞行、卡片与丢弃

- `note_editor_paper_px()` 改为：读编辑器 HWND 的 client rect，减去 `2*MARGIN`；`note_editor_card_px()` 再按 `PHOTO_INSET` 外扩。两者不再用 460/360 常量。
- 打开飞行：`take_flight_image()` 已是按恢复尺寸渲染的图；入场目标用当前纸张矩形。
- 提交飞行：`NoteEditorEvent::Commit{image}` 的图必须按**当前**纸张尺寸渲染；返回目标用当前纸张矩形。渲染线程按命令顺序处理“最后一次 Resize -> Commit”，不要在 UI 线程重算图。
- 丢弃新便签：不要再在 UI 线程 `render_paper(460,360)`。`on_note_editor_event` 已经把提交图存进 `note_return_image`；`discard_new_note` 直接用它（当前尺寸、当前纸张）。若图为空，走 worker 或使用缓存的空白纸图，绝不在 UI 线程分配/栅格化 820×640 的整纸。
- 飞行图限制最长边 **2048 px**：820×640@200% 为 1640×1280，不降采样；250% 时 2050×1600 会轻微裁到 2048，属于可接受折中。该图只用于飞行，卡片缩略图仍由 `notes::render` 按固定卡片比例重绘。
- 悬停预览 `render_preview` 固定 408×264，不受编辑器尺寸影响。
- 如果自由宽高导致返回飞行图与 136×88 卡片比例差异明显，先按现有缩放行为验收；若视觉上不可接受，再在飞行目标上做 cover-crop，不改变保存的缩略图。

---

## 4. 尺寸记忆与生命周期

### 4.1 键格式

沿用 `noteColor:<file name>` 的先例：

```ini
noteSize:<file name>=460x360
noteSize:<file name>=820x640
```

- 值：两个整数 DIP，`WxH`，小写 `x`；解析时兼容大写 `X`。
- 只存整数 DIP；不存物理像素，不存浮点。
- 文件名为 `notes::create` 生成的 `*.txt` 文件名，不含路径。Windows 文件名不含 `:`，所以键分隔安全。
- 与 `noteColor:` 一样，未知键保留；旧版本会忽略 `noteSize:`，降级安全。
- 默认尺寸不写键；恢复到默认时删除键，避免无意义行。

`src/settings.rs` 增加：

```rust
const NOTE_SIZE_PREFIX: &str = "noteSize:";

pub fn note_size(&self, file_name: &str) -> Option<PaperSize> {
    let raw = self.string(&format!("{NOTE_SIZE_PREFIX}{file_name}"))?;
    parse_note_size(raw)
}

pub fn set_note_size(&mut self, file_name: &str, size: PaperSize) {
    let width = size.width.round() as i32;
    let height = size.height.round() as i32;
    if width == DEFAULT_PAPER.width.round() as i32
        && height == DEFAULT_PAPER.height.round() as i32
    {
        self.remove(&format!("{NOTE_SIZE_PREFIX}{file_name}"));
    } else {
        self.set_string(
            &format!("{NOTE_SIZE_PREFIX}{file_name}"),
            &format!("{width}x{height}"),
        );
    }
}

pub fn remove_note_size(&mut self, file_name: &str) {
    self.remove(&format!("{NOTE_SIZE_PREFIX}{file_name}"));
}
```

解析防御：

- 缺失、空串、非数字、负号、`0x0`、多于一个 `x`、超出 `u32` -> `None`，用默认尺寸，记 debug 日志。
- 合法但超过 820×640 -> clamp，不报错。
- 合法但低于 460×360 -> clamp。
- 读取后再按当前工作区 `effective_max` clamp；磁盘里保留用户原来的显式尺寸，不因为一次小屏打开就覆盖。

### 4.2 持久化时机

**在鼠标释放时写内存并持久化**，不在 `WM_SIZE` 里写：

- 一次拖拽只产生一次 `settings.save()`；与现有 `set_note_color` 的即时保存路径一致。
- 不是每帧 IO；拖拽过程中只改窗口和渲染状态。
- 新建便签在 `notes::create` 成功、拿到文件名之后写键；丢弃的新便签没有文件名，不写键。
- 关闭编辑器不再额外写一次；退出时 `settings.save()` 仍然兜底。
- 拖拽中只更新 `effective_paper`；释放时写 `explicit_paper_dip`。退出兜底时若 `moved` 为真，用当前 effective 作为 explicit；否则保留原显式值，避免小屏 clamp 被误写。

这条路径沿用 `set_note_color` 的同步 `save()`；它与 AGENTS 的“UI 线程不阻塞文件 IO”有张力。2.5.0 先测量；若 p95 > 2 ms 或出现掉帧，阶段 4 必须改为单一后台 writer（快照 + 唯一临时文件），不能以“文件很小”为由长期保留。

### 4.3 生命周期矩阵

| 场景 | 内存尺寸 | 磁盘键 | 说明 |
|---|---|---|---|
| 新建便签，未缩放 | 460×360 | 无 | 默认即最小 |
| 新建便签，缩放后保存 | 当前尺寸 | `noteSize:<new file>` | `notes::create` 成功后写；保存失败则编辑器保持打开，键不写 |
| 新建便签，缩放后丢弃 | 当前尺寸 | 无 | 空便签走 tumble；无文件可关联 |
| 已有便签打开 | 读键 -> clamp | 保持 | 无键则默认；小屏 clamp 不覆盖磁盘值 |
| 已有便签缩放 | 当前尺寸 | 写入/更新 | 释放即写 |
| 已有便签缩放回默认 | 460×360 | 删除 | 不保留冗余默认键 |
| 已有便签缩放后保存文本 | 当前尺寸 | 保持 | 文本写入与尺寸键独立 |
| 已有便签缩放后无文本修改 | 当前尺寸 | 保持/更新 | 仍走 `fly_note_back_unchanged`，尺寸照样记忆 |
| 归档成功 | — | 删除 | `on_note_archived` 成功后移除并 `settings.save()`（best effort） |
| 归档失败 | 当前尺寸 | 保持 | 编辑器关闭/报错，键不丢 |
| 外部删除便签文件 | — | 可能残留 | 孤儿键无害；下次同名文件会继承旧尺寸，属于已知边缘情况 |
| 设置值损坏 | 默认 | 保留原值 | 解析失败用默认；下次缩放覆盖 |
| DPI/显示器变化 | 当前 DIP | 不主动改写 | 只为当前工作区 clamp；用户再次拖拽才覆盖 |
| 拖拽中崩溃 | 最后一次释放的值 | 最后一次释放的值 | 拖拽中的实时尺寸不落盘，可接受 |
| 拖拽中退出应用 | 若 moved 则当前尺寸，否则原显式值 | 兜底保存 | 退出前结束缩放并 `settings.save()` |
| 第二个实例 | 各自内存 | 最后写入者胜 | `PEGLINE_ALLOW_SECOND_INSTANCE=1` 的测试用逃生口；不新增写入者 |

孤儿键不做启动扫描：扫描 `Notes` 目录并逐个 `stat` 会把启动 IO 带进来，收益极低。键一行约 40–80 字节，可忽略；归档路径已经负责正常清理。

### 4.4 与现有 `noteColor` 的关系

- 两者都是 per-note 元数据，键前缀不同，互不影响。
- 颜色仍然即时保存（现有行为）；尺寸在释放时保存。两者都只写同一个小 INI，不引入新文件。
- 新建便签保存时，尺寸键以新文件名写入 `on_note_saved`；颜色沿用现有逻辑（默认 0 可以不写键，用户改色时再由 `set_note_color` 写入）。尺寸键必须在 `close_note_editor` 之前完成；颜色键可以在卡片出现后再写。
- 归档成功时同时移除 `noteColor:` 和 `noteSize:`？现有代码是否移除颜色键需要实现时确认；2.5.0 至少移除尺寸键，避免归档文件名的颜色/尺寸残留。若现有颜色键未清理，单独记录，不在本版本顺手改行为。

---

## 5. DPI、显示器与小工作区

### 5.1 尺寸以 DIP 存储

- 所有用户可见尺寸、设置值、最小/最大限制都用 DIP。
- `NoteEditor` 保存两个值：`explicit_paper_dip`（用户显式尺寸，写盘用）和由当前 client rect 推导的 `effective_paper`（显示用）。`WM_SIZE` 只更新 `effective_paper`，小屏 clamp 不会覆盖显式值。
- 物理像素只在 `SetWindowPos`、`ResizeBuffers`、位图分配、命中测试的窗口坐标中出现。
- `scale = GetDpiForWindow(hwnd) / 96.0`。
- 释放时 `explicit_paper_dip = round(paper_px / scale)`，再 clamp；避免 125%/150% 下累计漂移。

### 5.2 `WM_DPICHANGED`

当前实现会 `centered_rect(work, dpi)` 回到 500×400；2.5.0 必须：

1. 读取 UI 侧保存的 `explicit_paper_dip`（最后一次打开/拖拽释放的显式尺寸），不是 `WM_SIZE` 的 effective 值。
2. 用新 DPI 计算 `max_paper` 和窗口像素。
3. clamp，按“增长包络”重新定位。
4. `SetWindowPos`；`WM_SIZE` 会把新尺寸/DPI 送给渲染线程。
5. 磁盘键不改写；只有用户再次拖拽才更新。

如果新工作区放不下，按 3.7 例外或 clamp；不要静默回到默认尺寸。

### 5.3 多显示器

- 每张便签只存一个 DIP 尺寸，不按显示器分别存。
- 在 A 显示器调到 820×640，拿到 B 显示器（工作区更小）打开：显示 clamp 后的尺寸，但磁盘仍是 820×640；回到 A 再打开恢复 820×640。
- 这是“每便签独立记忆”与“按当前工作区可用”的折中；不引入 per-monitor 键，避免键爆炸。
- 编辑器窗口当前不跨屏移动；显示器变化主要由 DPI/工作区消息触发。

### 5.4 工作区变化

- 任务栏移动/自动隐藏、分辨率变化：`WM_SETTINGCHANGE`/`WM_DISPLAYCHANGE` 以 `explicit_paper_dip` 重新 clamp 到新工作区，再 `SetWindowPos`；小屏只缩小显示，不覆盖显式值。
- 如果窗口已经超出新工作区，优先缩小到最大可用；不移动左上角，除非窗口完全不可见、当前位置连最小窗口都放不下，或进入 3.7 例外模式。若当前位置放不下最小窗口但工作区本身放得下，按 3.4 增长包络重新定位，不要误入例外。
- 这些路径只改显示，不改磁盘键（除非用户拖拽）。

---

## 6. 性能与内存预算

### 6.1 200% DPI 估算

按现有实测基线：编辑器打开 158.11 MB（默认 460×360），其中约 29 MB 是 D2D/DWrite/窗口等固定开销，约 17 MB 是纸张/交换链/飞行图/阴影。

| 项目 | 默认 460×360（当前 2.4.2） | 最大 820×640（2.5 + 9-patch） | 说明 |
|---|---:|---:|---|
| 纸张位图 | 920×720×4 ≈ 2.6 MB | 1640×1280×4 ≈ 8.4 MB | 不透明，ClearType |
| 交换链 2 缓冲 | 1000×800×4×2 ≈ 6.4 MB | 1720×1360×4×2 ≈ 18.7 MB | 窗口尺寸 |
| 飞行图 | 920×720×4 ≈ 2.6 MB | 1640×1280×4 ≈ 8.4 MB | 最长边 2048 上限，200% 不触发 |
| 阴影 | 整窗 3.2 MB（烘焙峰值 4.8 MB） | 9-patch ≈ 0.3–0.6 MB | 9-patch 同时降低默认占用 |
| 小计 | ≈ 14.8 MB | ≈ 35.8–36.1 MB | 不含固定开销 |
| 编辑器打开估算 | ≈ 44 MB 增量 | ≈ 65 MB 增量 | 固定开销按 29 MB 估算 |
| 应用私有内存估算 | 158.11 MB（实测） | ≈ 177–183 MB | 待 probe 实测 |

默认列用当前 2.4.2 的全窗阴影对齐 158.11 MB 实测；2.5 默认也会换成 9-patch，默认绝对内存会比 158.11 MB 低约 2.6–2.9 MB。最大列是 2.5 的估算，绝对上限按 158.11 - 46 + 65 ≈ 177 MB 起算，留余量到 183 MB。

结论：

- 最大尺寸比默认多约 **19–25 MB**；验收线定 **≤ 190 MB**（200%，6016×3384）。
- 9-patch 把阴影从“每次拖拽的整窗 CPU 光栅”变成“每 DPI 一次的小图集”，是本次内存/帧时间的关键。
- 飞行图 2048 上限在 200% 最大尺寸下不降采样；250% 时轻微降采样，避免 13 MB+ 的临时图。
- 丢弃新便签改用提交图后，UI 线程不再分配 820×640 整纸，避免 8.4 MB/15–30 ms 的卡顿。

### 6.2 帧时间

- 拖拽时每帧最多一次 `ResizeBuffers`，由渲染线程合并 `Command::Resize`：只保留最新尺寸，帧开始应用。
- 阴影每 DPI 只烘焙一次；拖拽期间阴影目标矩形更新是 O(9)。
- 文本重排只在宽度变化时发生；高度变化不重排。渲染线程每帧最多重排一次。
- 目标：60 Hz 下 frame p95 ≤ 16.7 ms，draw p95 ≤ 4 ms；120 Hz 下 frame p95 ≤ 8.3 ms。120 Hz 硬件未验证前不写“通过”。
- 若长文本在最大宽度下重排超过 4 ms，启用降级：拖拽期间按 8 px 宽度步进重排，释放时做最终重排；不改变最终布局。该降级必须先有测量数据再启用。

### 6.3 稳定性

- 连续 20 次“打开 -> 拖到最大 -> 拖回默认 -> 关闭”，私有内存漂移 ≤ 2 MB。
- 每次拖拽阴影烘焙次数 ≤ 1（正常应为 0，只有首次/DPI 变化为 1）。
- 每个稳定尺寸 `ResizeBuffers` ≤ 1 次；失败 0 次。
- `SetWindowPos`/`ResizeBuffers` 失败只记日志并保持上一尺寸，不 panic、不丢文本。

---

## 7. 边界情况清单

| 场景 | 预期行为 |
|---|---|
| 拖右边缘 | 只改宽；高不变；左上角固定 |
| 拖下边缘 | 只改高；宽不变；左上角固定 |
| 拖右下角 | 宽高同时改；折角跟随 |
| 拖到最小 | 停在 460×360 纸张 / 500×400 窗口；不出现 0 或负尺寸 |
| 拖到最大 | 停在 820×640 纸张 / 860×680 窗口；受工作区进一步限制 |
| 拖到屏幕右/下边缘 | 停在工作区边缘；不把纸张拖出屏幕 |
| 小工作区 | 按 3.7 clamp；纸张完整可见；内部滚动 |
| 点击纸张内 | 正常放光标/选择，不缩放、不提交 |
| 点击纸张边框 | 2 DIP 重叠带优先缩放；不会误放光标 |
| 点击左/上边距 | 仍按旧行为提交（用户没有要求左/上缩放） |
| 点击右/下边距 | 开始缩放，不提交 |
| 点击窗口外 | LL 钩子提交；不缩放 |
| 拖拽中按 Esc | 取消并恢复起始尺寸；不提交 |
| 拖拽中按 Ctrl+S | 先结束缩放，再保存/提交 |
| 拖拽中 Alt-Tab/失焦 | 以最后尺寸结束并持久化，再提交；不提交半帧尺寸 |
| 拖拽中捕获丢失 | 以最后尺寸结束；必要时提交 |
| 拖拽中关闭窗口 | 先结束缩放，再走现有提交 |
| 双击缩放带 | 不选词、不最大化；按普通缩放按下处理 |
| 右键缩放带 | 不进入缩放；交给现有右键/上下文行为 |
| 触摸/笔 | 走系统合成鼠标；不额外支持手势 |
| 拖拽中 IME 组合 | 不取消组合；重排后重锚候选窗 |
| 拖拽中文本选区 | 选区偏移不变；视觉重排；不丢失 |
| 拖拽中滚动 | 只 clamp；释放时有焦点再保证光标可见 |
| 长文本缩窄 | 内容变高；滚动范围更新；不丢尾部 |
| 长文本放宽 | 内容变矮；滚动 clamp；不出现空白跳动 |
| 便签文本未改 | 尺寸仍持久化；返回飞行仍走 unchanged 路径 |
| 新建空便签缩放后丢弃 | 无尺寸键；tumble 用当前提交图 |
| 保存失败 | 编辑器保持打开；不写尺寸键；文本不丢 |
| 归档成功 | 删除尺寸键；文件移动；不残留 |
| 归档失败 | 保留尺寸键；报错路径不变 |
| 设置值损坏 | 默认尺寸；日志；不崩溃 |
| 设置值超大 | clamp 到 820×640；不触发 DXGI 上限 |
| 设置文件缺失 | 默认尺寸 |
| 设置文件只读 | `save()` 返回 false；记日志；本次运行仍记忆，下次可能丢失 |
| 第二个实例 | 最后写入者胜；已知限制，不在本版本修 |
| 外部重命名便签 | 新文件名无键 -> 默认；旧键成孤儿 |
| 外部删除便签 | 键成孤儿；无害；不启动扫描 |
| 恢复归档便签（未来） | 默认尺寸；本版本不提供恢复 UI |
| 100% DPI | 纸张 460×360 像素；窗口 500×400 |
| 150% DPI | 尺寸按 DIP 换算；取整误差 ≤ 1 px |
| 200% DPI | 纸张 920×720；窗口 1000×800；最大 1640×1280 / 1720×1360 |
| DPI 切换 | 保留 DIP；clamp；重新定位；不回到默认 |
| 任务栏变化 | 重算工作区；clamp；必要时移动 |
| 最小化/恢复 | 忽略 0 尺寸；恢复后按当前纸张重绘 |
| 关闭再打开 | 从键恢复；无键默认；窗口居中/增长包络 |

---

## 8. 实施阶段

### 阶段 0：尺寸模型与设置（0.5–1 天）

- 新增 `PaperSize`、`MIN_PAPER`、`MAX_PAPER`、`DEFAULT_PAPER`。
- 把 `MARGIN_DIP` 从 `render/mod.rs` 移到 `display.rs`；把 `display.rs` 的常量改为默认值/方法；新增 `content_width()`、`view_height()`、`window_size()`。
- 新增 `settings::parse_note_size`、`format_note_size`、`note_size`、`set_note_size`、`remove_note_size`。
- 单元测试：clamp、解析、格式化、默认删除、损坏值。
- 本阶段不改变 UI 行为；所有现有测试仍按 460×360/500×400 通过。

退出标准：`cargo test` 全绿；新类型有单元测试；没有 UI 可见变化。

### 阶段 1：动态纸张渲染（1–1.5 天）

- `EditorState.paper`；`resize` 更新 `view_height`、`paper`、`layout_width`。
- `paint_paper_on`、`paper_rect`、`paper_gradient`、`fold_geometries`、`ensure_paper` 全部运行时化。
- `Parsed::is_current` 加宽度；宽度变化清布局缓存；CJK/英文换行测试。
- `render_flight_image` 用当前纸张；2048 上限。
- `ShadowAtlas` 9-patch；`rebuild_scenery` 不再整窗烘焙；阴影烘焙计数日志。
- 视觉验证：默认尺寸与 2.4.2 像素级一致；820×640 纸张、折角、阴影、文本、选区正常。

退出标准：默认尺寸无回归；手动把 `paper` 设为 820×640 能正确渲染；拖拽前不要求输入。

### 阶段 2：Win32 缩放与悬停（1–1.5 天）

- `Hit`/`ResizeZone`/`hit_test_screen`/`is_commit_point`。
- `NoteEditor.explicit_paper_dip`/`effective_paper`；打开时读取显式值并 clamp 到当前工作区。
- `ResizeState`、`SetCapture`、`SetWindowPos`、`WM_GETMINMAXINFO`、`WM_DPICHANGED`、`WM_DISPLAYCHANGE`/`WM_SETTINGCHANGE`。
- 光标枚举与捕获期间重设；`TrackMouseEvent` 悬停/离开。
- 悬停高亮（右/下/角）画在窗口层。
- 改 `window.rs` 与 `app/pointer.rs` 的提交判定，共用 hit-test。
- `--note-resize-demo`：脚本化右、下、角、最大、最小、Esc、Alt-Tab。

退出标准：手动拖拽三方向正确；点击优先级正确；LL 钩子不再误提交；100%/200% 光标/高亮正确。

### 阶段 3：应用集成与飞行（0.5–1 天）

- `note_editor_paper_px`/`note_editor_card_px` 从 client rect 推导。
- 打开时读取尺寸并定位；提交图/返回飞行用当前尺寸。
- `discard_new_note` 改用 `note_return_image`，移除 UI 线程整纸渲染。
- 归档/保存/关闭/退出路径接入尺寸键。
- 验证：打开飞行、提交返回、无修改返回、空便签 tumble、卡片缩略图、悬停预览不变。

退出标准：视觉链路完整；无 UI 线程大图分配；尺寸在 reopen 后恢复。

### 阶段 4：性能、视觉与文档（1 天）

- 跑 probe（新增 `--mode=note` 或 `--note-resize-demo` 采样），记录默认/最大/拖拽的 p50/p95、私有内存、ResizeBuffers/阴影次数。
- 20 次开关/缩放循环泄漏测试。
- 100%/200% 截图；长文本、CJK、IME、选区、滚动。
- 更新文档；写 release notes。
- 把实测数字补进 `PLAN.md`（不要写估算值）。

退出标准：验收清单全过；clippy/fmt/test 全绿；文档更新；未测项明确标注。

---

## 9. 测试计划

### 9.1 单元测试

- `PaperSize::clamp`：默认、最小、最大、反向 min/max、NaN/inf 防护（若允许 f32 输入）。
- `parse_note_size`：`460x360`、`820X640`、` 460x360 `、`0x0`、`-1x2`、`abc`、`1x2x3`、`99999x99999`、空串。
- `format_note_size`：取整、默认删除。
- `Settings::note_size`/`set_note_size`/`remove_note_size`：未知键保留、默认删除、大小写。
- `hit_test_screen`：纸张、右带、下带、角、左/上边距、窗口外、圆角外。
- `resize_clamp`：三个方向、最小、最大、工作区限制。
- `frame_rect_for_paper`：大屏居中、小屏增长包络、工作区不足。
- `PaperSize::content_width`/`view_height`：默认 400/304，最大 760/584。
- `resize` 只改高不触发重排；改宽触发重排。
- `render_flight_image` 尺寸：460×360@100%=460×360，@200%=920×720；820×640@200%=1640×1280。
- `ShadowAtlas` 目标矩形：默认/最大无重叠、无 1 px 缝、DPI 变化重烘。

### 9.2 集成/手动

- `--note-resize-demo` 脚本：
  1. 打开已有便签，默认尺寸。
  2. 右边缘 +200 DIP；检查宽高、纸张、文本换行。
  3. 下边缘 +200 DIP；检查高、滚动。
  4. 右下角拖到最大；检查 clamp 和工作区。
  5. 拖回最小；检查不越界。
  6. 释放 -> 关闭 -> 重开；检查恢复。
- 新便签：默认 -> 缩放 -> 输入 -> 保存 -> 重开恢复；空便签缩放 -> 丢弃 -> 无键。
- 已有便签：缩放 -> 不改文本 -> 关闭 -> 重开恢复；缩放 -> 归档 -> 键删除。
- 点击优先级：纸张内、边框、右/下边距、左/上边距、窗口外；LL 钩子日志确认只提交一次。
- 拖拽中 Esc/Ctrl+S/Alt-Tab/捕获丢失/关闭。
- DPI 100%/200% 切换；任务栏移动；小工作区。
- 损坏/缺失/只读 settings.ini。
- 长文本（数百行）、CJK、英文混排、代码块、引用、列表、选区、滚动、IME 组合。
- 回归：复制/粘贴/剪切、Ctrl+C、Ctrl+S、Esc、双击选词、右键、便签颜色、悬停预览、标注编辑器、Mica、托盘。

### 9.3 视觉验收

截图存入 `docs/visual/`（命名按现有习惯）：

- `note-resize-default-100.png` / `note-resize-default-200.png`
- `note-resize-max-100.png` / `note-resize-max-200.png`
- `note-resize-hover-right.png`
- `note-resize-hover-bottom.png`
- `note-resize-hover-corner.png`
- `note-resize-long-text.png`
- `note-resize-ime.png`

检查：

- 纸张、阴影、折角、边框在缩放中不裂缝、不拉伸。
- 文本左右内边距保持 30 DIP，顶部 26，底部 30。
- 折角始终在右下，尺寸不变。
- 高亮只出现在可拖边，不遮挡文本。
- 光标形状正确，离开后恢复箭头。
- 100%/200% 下没有 1 px 错位或模糊。

### 9.4 Probe / 性能

建议新增：

```powershell
cargo run --bin probe -- --mode=note --paper=460x360
cargo run --bin probe -- --mode=note --paper=820x640
cargo run --bin probe -- --mode=note --resize-demo
```

记录：

- 私有内存：默认/最大/20 次循环后。
- frame p50/p95、draw p50/p95。
- `ResizeBuffers` 次数/失败次数。
- 阴影烘焙次数（每 DPI 应为 1；拖拽中 0）。
- UI 线程最长阻塞（settings.save、文件 IO、丢弃渲染）。

阈值：

- 200% 最大尺寸编辑器打开 ≤ 190 MB。
- 20 次循环漂移 ≤ 2 MB。
- 60 Hz frame p95 ≤ 16.7 ms；draw p95 ≤ 4 ms。
- 120 Hz 只在硬件可用时测；未测就写“未测”，不写“通过”。
- 每个稳定尺寸 `ResizeBuffers` ≤ 1；失败 0。

---

## 10. 风险登记

| 优先级 | 风险 | 触发/后果 | 缓解 | 验证 |
|---|---|---|---|---|
| P0 | 每次 `WM_SIZE` 整窗烘焙阴影 | 拖拽卡顿、每帧堆分配、违反硬规则 | 9-patch，每 DPI 一次；目标矩形 O(9) | 烘焙计数、frame p95、内存 |
| P0 | 窗口变大但纸张/文本仍 460×360 | 右侧/底部空白，文本不换行 | `PaperSize` 全链路；`content_width` 进缓存键 | 最大尺寸截图、换行单测 |
| P0 | 缩放带被 LL 钩子当成提交 | 第一次拖边框就关闭便签 | 共用 `hit_test`/`is_commit_point`；两条路径都测 | 日志、点击矩阵 |
| P1 | `Parsed::is_current` 不含宽度 | 旧换行被复用，文本溢出 | 宽度 bits 或强制 `ready=false` + 清缓存 | CJK 换行单测 |
| P1 | `view_height` 不更新 | 滚动范围/光标可见性错误 | resize 更新 + clamp；释放时 ensure caret | 长文本滚动测试 |
| P1 | 飞行图/丢弃图仍固定 460×360 | 返回飞行尺寸错误、UI 线程大图卡顿 | 用当前纸张图；丢弃用提交图；2048 上限 | 视觉 + UI 阻塞日志 |
| P1 | `WM_DPICHANGED` 回到 500×400 | 记忆尺寸丢失、跨 DPI 跳变 | 保存 `explicit_paper_dip`，按新 DPI clamp/定位 | 100%/200% 切换 |
| P1 | 拖拽中失焦提交半帧尺寸 | 便签以未完成的尺寸关闭 | 先结束缩放/持久化，再提交 | Alt-Tab 测试 |
| P1 | 捕获期间光标不刷新 | 光标停在箭头/I-beam | `WM_MOUSEMOVE` 重设；光标枚举 | 三方向拖拽 |
| P1 | 悬停高亮不消失 | 光标离开后边框仍亮 | `TrackMouseEvent(TME_LEAVE)` | 移入/移出 |
| P1 | IME 组合在缩放中错位 | 候选窗位置错误 | 重排后 post `WM_APP_IME_CARET`；不取消组合 | Microsoft Pinyin 200% |
| P2 | 小工作区下最大值不可达 | 用户以为最大失效 | 增长包络定位；工作区 clamp；文档说明 | 1366×768/1024×600 |
| P1 | settings 同步写阻塞 UI | 释放时 1–2 ms 卡顿，违反“UI 线程不阻塞文件 IO”的字面要求 | 只在释放写一次并测量；p95 > 2 ms 或掉帧则迁移到单一后台 writer（快照 + 唯一 tmp） | 写耗时日志、拖拽 p95 |
| P2 | 第二实例 settings 竞争 | 最后写入者胜/临时文件冲突 | 不新增写入者；已知限制 | 双实例测试 |
| P2 | 孤儿 `noteSize:` 键 | 设置文件缓慢增长 | 归档删除；不启动扫描；文档说明 | 归档测试 |
| P2 | 自由宽高导致返回飞行比例失真 | 飞行图被拉伸 | 先按现有缩放验收；必要时 cover-crop | 820×360 视觉测试 |
| P2 | 长文本拖拽重排超预算 | 拖拽掉帧 | 每帧一次重排；超 4 ms 时按 8 px 步进/释放时最终重排 | probe p95 |

---

## 11. 需要更新的文档

- `docs/BEHAVIOR.md` §10/§10.1：缩放范围、最小/最大、命中带、悬停、光标、记忆、生命周期。
- `docs/NOTE-EDITOR.md` §5.1/§5.2/§5.8/§13：移除“固定 460×360”，写 `PaperSize`、9-patch、增长包络、DPI、设置键。
- `docs/DECISIONS.md`：新增决策条目（手写缩放 vs `WS_THICKFRAME`、9-patch、每便签尺寸、释放时持久化、2048 飞行图上限）。
- `PLAN.md`：新增 2.5.0 小节或链接到本报告；实现后把实测内存/帧时间写进 §9/新 §13，不写估算值。
- `docs/PARITY.md`：加入 2.5.0 验收项。
- `docs/VISUAL-TESTS.md`：截图与 100%/200% 结果。
- `docs/RELEASE-2.5.0.md`：发布说明、已知限制（小工作区最大值、无键盘缩放、孤儿键、第二实例）。
- `AGENTS.md`：状态段落更新到 2.5.0（实现完成后）。

---

## 12. 验收清单

- [ ] 默认打开：纸张 460×360 DIP，窗口 500×400 DIP（100%）；200% 为 920×720 / 1000×800。
- [ ] 右边缘只改宽；下边缘只改高；右下角同时改；左上角固定。
- [ ] 最小/最大限制生效；最大 820×640 纸张 / 860×680 窗口（工作区允许时）。
- [ ] 拖拽实时跟手；纸张、阴影、折角、文本、选区、滚动同步。
- [ ] 悬停右/下/角显示对应高亮；离开消失；光标正确；拖拽中光标不闪回。
- [ ] 点击纸张放光标；点击左/上边距提交；点击右/下边距缩放；点击窗口外提交；LL 钩子不误提交。
- [ ] 尺寸记忆：已有便签 reopen 恢复；新便签保存后恢复；未改文本也恢复；归档删除键；丢弃无键；损坏值默认。
- [ ] DPI 100%/200% 切换保留 DIP 尺寸；小工作区正确 clamp；任务栏变化不越界。
- [ ] 长文本/CJK/英文/代码块/引用/列表在最大/最小宽度下换行正确；高度变化不重排。
- [ ] IME 组合、选区、复制/粘贴、Ctrl+S、Esc、双击选词、右键无回归。
- [ ] 打开飞行/提交返回/无修改返回/空便签 tumble 视觉正确；卡片缩略图和悬停预览仍固定。
- [ ] 丢弃新便签不在 UI 线程做大图渲染；提交图按当前尺寸。
- [ ] 20 次开关/缩放无泄漏；阴影每 DPI 只烘一次；`ResizeBuffers` 每稳定尺寸 ≤ 1。
- [ ] 200% 最大尺寸编辑器打开 ≤ 190 MB；60 Hz frame p95 ≤ 16.7 ms；draw p95 ≤ 4 ms。
- [ ] `cargo build --release`、`cargo test`、`cargo clippy --all-targets -- -D warnings`、`cargo fmt --check` 全绿。
- [ ] 文档、截图、release notes 更新；未测项（120 Hz、第二显示器、睡眠唤醒）明确标注。

---

## 13. 开放问题与不做的事

1. **“当前大小”指纸张还是窗口外框？** 本报告按纸张 460×360 执行；若指窗口 500×400，则纸张最小应变 420×320。实现前用一句话复核。
2. **每显示器独立尺寸？** 不做。每便签一个 DIP 尺寸，按当前工作区 clamp；不做 per-monitor 键。
3. **移动窗口？** 不做。左上角固定，只有右/下缩放；移动仍是 v1 的非目标。
4. **键盘/触控缩放？** 不做。MVP 只做鼠标拖拽 + 光标 + 悬停高亮。
5. **缩放动画？** 不做。实时跟手，不加弹簧；编辑器是工具窗口，动画会拖慢反馈。
6. **撤销缩放？** 不做。尺寸不是文本编辑，不进入 undo 历史。
7. **尺寸写入便签文件？** 不做。`noteSize:` 是本地设置，不进入 `.txt`，复制/导出便签不携带尺寸。
8. **归档清理颜色键？** 现有 `noteColor:` 是否在归档时移除需实现时确认；2.5.0 至少移除 `noteSize:`，不顺手改变颜色行为。
9. **孤儿键清理？** 不做启动扫描；归档删除已覆盖正常路径，孤儿键无害。
10. **第二实例 settings 竞争？** 不在本版本修；只保证不新增第二个写入者。
11. **120 Hz / 第二显示器 / 睡眠唤醒？** 仍属于硬件相关未测项；没有测量就不写通过。

---

## 14. 代码草图

### 14.1 `PaperSize` 与窗口推导

```rust
// src/note_editor/display.rs
pub const MIN_PAPER: PaperSize = PaperSize { width: 460.0, height: 360.0 };
pub const MAX_PAPER: PaperSize = PaperSize { width: 820.0, height: 640.0 };

// src/app/cards.rs
pub fn note_editor_paper_px(editor: &NoteEditor) -> Option<Rect> {
    let client = editor.client_size()?;
    let dpi = unsafe { GetDpiForWindow(editor.hwnd()) };
    let margin = (MARGIN_DIP * dpi as f32 / 96.0).round() as i32;
    Some(Rect {
        left: margin,
        top: margin,
        right: client.width - margin,
        bottom: client.height - margin,
    })
}
```

### 14.2 命中测试

```rust
pub fn hit_test(&self, x: i32, y: i32) -> Hit {
    let (w, h) = self.client_size_px();
    let dpi = self.dpi();
    let s = dpi as f32 / 96.0;
    let margin = (MARGIN_DIP * s).round() as i32;
    let paper = PaperSize {
        width: (w - 2 * margin) as f32 / s,
        height: (h - 2 * margin) as f32 / s,
    };
    let left = margin;
    let top = margin;
    let right = w - margin;
    let bottom = h - margin;
    let slop = (2.0 * s).round() as i32;
    let in_right = x >= right - slop && y >= top - slop && y <= bottom + slop;
    let in_bottom = y >= bottom - slop && x >= left - slop && x <= right + slop;
    if in_right && in_bottom { return Hit::Resize(ResizeZone::Corner); }
    if in_right { return Hit::Resize(ResizeZone::Right); }
    if in_bottom { return Hit::Resize(ResizeZone::Bottom); }
    if paper_contains(x, y, paper, s) { return Hit::Paper; }
    if x < 0 || y < 0 || x >= w || y >= h { return Hit::Outside; }
    Hit::Margin
}
```

### 14.3 缩放夹取

```rust
fn resize_target(start: (i32, i32), zone: ResizeZone, dx: i32, dy: i32,
                 scale: f32, max_paper_px: (i32, i32)) -> (i32, i32) {
    let mut w = start.0;
    let mut h = start.1;
    if matches!(zone, ResizeZone::Right | ResizeZone::Corner) {
        w = (start.0 + dx).clamp((MIN_PAPER.width * scale) as i32, max_paper_px.0);
    }
    if matches!(zone, ResizeZone::Bottom | ResizeZone::Corner) {
        h = (start.1 + dy).clamp((MIN_PAPER.height * scale) as i32, max_paper_px.1);
    }
    (w, h)
}
```

### 14.4 9-patch 阴影

```rust
struct ShadowAtlas {
    atlas: Bitmap,
    src: [Rect; 9],
    dst: [Rect; 9],
    dpi: u32,
    corner_px: i32,
}

impl ShadowAtlas {
    fn bake(dpi: u32) -> Self { /* 小图集，CPU 一次 */ }
    fn layout(&mut self, window_px: (i32, i32), margin_px: i32) {
        // 四个角保持 corner_px；边条拉伸；中心拉伸；目标矩形像素对齐
    }
    fn draw(&self, ctx: &DeviceContext) {
        for i in 0..9 { ctx.DrawBitmap(&self.atlas, &self.dst[i], 1.0, LINEAR, &self.src[i]); }
    }
}
```

---

## 15. 智能体 brainstorm 输入摘要

四路并行分析（Win32、布局/渲染/内存、UX/生命周期、风险/测试）的结论已并入本报告：

- **Win32**：手写 `SetCapture` + `SetWindowPos` 优于 `WS_THICKFRAME`；缩放带与 LL 钩子提交冲突是最高优先级；`WM_DPICHANGED` 必须保留 DIP。
- **布局/渲染**：15 处常量耦合；`Parsed::is_current` 缺宽度是隐藏的换行 bug；`bake_shadow` 是唯一不可接受的每帧成本；9-patch 是正确解。
- **UX/生命周期**：每便签键 `noteSize:<file>`；释放时更新内存；归档删除；新建保存后写；丢弃不写；悬停高亮 + 尺寸光标；悬停预览不缩放。
- **风险/测试**：P0 是阴影、纸张不生长、提交回归；P1 是 `view_height`、飞行图、DPI、失焦、光标；验收必须包含 100%/200% 截图、20 次循环、probe 帧时间。

---

*本报告只描述设计与执行计划，不包含代码改动。实现完成后，实测数字与偏差应回写到 `PLAN.md` 和 `docs/RELEASE-2.5.0.md`。*
