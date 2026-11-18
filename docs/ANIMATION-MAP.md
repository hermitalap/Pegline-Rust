# Pegline-Rust 动画代码地图

面向审计者的入口文档。目的：把"这个项目里所有会动的东西"按**时钟 → 状态 → 绘制**三段定位到具体 `文件:行`，并列出可疑点，便于下一轮只读审计直接按图索骥。

- 审计对象的规格是 `docs/BEHAVIOR.md`，取舍依据是 `docs/DECISIONS.md`，硬规则是 `AGENTS.md`。
- 本文只描述**代码在哪、怎么动、用什么参数**，不重复产品行为。
- 行号基于 2.6.2 之后的 HEAD（2026-11-17）。若与当前文件不符，以符号名为准再重新定位。
- 动画是产品本身（`AGENTS.md` 工作流第 3 条），所以"看起来能简化"不等于"可以删"；请对照 `BEHAVIOR.md` 判断。

---

## 0. 一句话概括

项目里没有统一动画系统：**一个 186 行的弹簧内核**（`src/spring.rs`）被三套互不相干的"时间线"驱动——线带场景（渲染线程按真实 `dt` 积分）、飞行窗口（渲染线程里手写的分段 `elapsed` 时间轴）、便签悬停（UI 线程 15 ms 定时器 + 自己的 240 Hz 子步进）。三者通过 `Command`/`FlightEvent` 消息和 `now_seconds()` 这一个时钟源对齐。

---

## 1. 全局运行模型

### 1.1 线程与时钟

| 角色 | 位置 | 说明 |
| --- | --- | --- |
| 时基 | `src/win32.rs:576-601` `win32::now_seconds()` | QPC 秒；全项目唯一时钟源 |
| UI 线程主定时器 | `src/app/mod.rs:80` `TIMER_TICK` / `:81` `TICK_MS = 33`，`:582` `SetTimer` | 33 ms 一拍：reveal/hide 延迟、事件泵、`update_note_entry` |
| UI 线程便签定时器 | `src/app/mod.rs:85` `NOTE_FOLLOW_MS = 15`，`:1836` `SetTimer`，`:1880` `KillTimer` | 只在便签悬停期间存在 |
| 消息分发 | `src/app/mod.rs:861` `on_message`，`:869` `WM_TIMER`，`:923` `tick` | |
| 线带渲染线程 | `src/window/overlay.rs:245` `render_thread` | 拥有 D3D/D2D 设备、swap chain、`Scene`、帧循环 |
| 飞行窗口线程 | `src/window/flight/mod.rs:609` `flight_thread` | 拥有自己的 swap chain 与时间轴 |
| 幽灵窗口线程 | `src/window/ghost.rs:172` `ghost_thread` | 15 ms 轮询跟随指针 |
| 预览/便签纸线程 | `src/window/preview.rs:286` `preview_thread` | 只有倾斜变化时重绘 |
| 编辑器线程 | `src/note_editor/window.rs`、`src/editor/window/mod.rs` | 各自 `render_thread`，无连续动画（见 §8） |

### 1.2 帧循环（唯一真正的动画循环）

`src/window/overlay.rs:297-417`：

1. `:301-340` 不显示或本帧无事可做时阻塞等命令：可见时用 `scene.next_wake_in(now)` 算下一次唤醒（阵风），隐藏时 `rx.recv()` 无限等。**这是 idle CPU ≈ 0 的实现**。
2. `:342-353` 排空命令队列，逐条 `handle`（`:461`，`SetFrameCap` 在 `:492`）。
3. `:370-375` 可选帧率上限：`thread::sleep(frame_cap - since)`；帧率档位见 `src/settings.rs:33-58`（`FpsCap::{Auto, Hz60, Hz120}`，`period()`），由 `src/app/mod.rs:847` 和 `src/app/reveal.rs:173` 下发。
4. `:377-379` `now`、`dt = (now - last).clamp(0.0, 0.05)`——**所有场景动画的唯一 dt 来源**，上限 50 ms。
5. `:381-394` 夹子拖拽在此每帧采样一次 `client_cursor`（`GetCursorPos`），并用 `GetAsyncKeyState(VK_LBUTTON)` 兜底松手。
6. `:396` `scene.update(dt, now)`；`:398-416` `begin`/`draw`/`end`/`present(true)`；`:417` `needs_frame = animating`。
7. 呈现：`src/render/mod.rs:572` → `src/gfx/device.rs:498-516` `Present(sync as u32, 0)`，三种结果 `Present::{Ok, Retry, DeviceLost}`（`src/render/mod.rs:56-66`）。`Retry`（`DXGI_ERROR_WAS_STILL_DRAWING`）在 `:409` 被直接忽略并立刻重画，无退避。

### 1.3 数据流

UI 线程只改"意图"，不改像素：

```
App (src/app/mod.rs)
  ├─ Command  ───────────────► 线带渲染线程  Scene::apply  src/scene/mod.rs:576
  ├─ FlightRequest ──────────► 飞行窗口线程  Flight::fly* src/window/flight/mod.rs:288/319/357
  └─ FrameSnapshot ◄─────────  Scene::publish src/scene/mod.rs:1225（命中框/卡片位置回投给 UI）
FlightEvent {Started, Landed, Finished} ─► App::on_flight_* src/app/mod.rs:1148/1175/1235
```

`Command` 全集在 `src/scene/mod.rs:91-148`，每个变体都直接对应一段动画语义（见 §3.2 表）。

---

## 2. 弹簧内核 `src/spring.rs`（186 行）

| 符号 | 行 | 说明 |
| --- | --- | --- |
| `pub struct Spring { value, velocity, target, stiffness, damping }` | 9-15 | `value` 单位随用途：DIP、度、比例 |
| `Spring::new(response, damping_ratio, value)` | 19-29 | 仿 SwiftUI `spring(response:dampingFraction:)` |
| `set_response` | 32-36 | `stiffness = (TAU/response)^2`；`damping = 4π·ratio/response` |
| `set_stiffness(stiffness, damping)` | 39-42 | 仿 SwiftUI `interpolatingSpring`，**第二条参数化路径** |
| `REST_DISTANCE = 0.01` / `REST_SPEED = 0.05` | 49-51 | 静止阈值，注释说明 6K 下 x≈1600 DIP 的 f32 ulp 才 0.0001 |
| `step(dt)` | 54-75 | 半隐式欧拉；`steps = ceil(dt*240)`，子步 `h = dt/steps`（`:67-74`） |
| 静止即吸附 | 58-66 | 每次 `step` **开头**判静止，命中就 `value = target; velocity = 0` 并返回 |
| `settled()` | 78-81 | 与上面同阈值，供 `animating` 判定 |
| `snap(value)` | 85-89 | 同时改 `value` 与 `target`，清速度 |
| `mod ease` | 93-124 | `in_out_cubic`、`smooth(x,a,b)`（平滑阶跃，clamp）、`out_quad`、`carry`/`carry_rate`（F3）、`smoothstep`（F4）、`standard`（CSS ease，F11） |

测试（`#[cfg(test)]`，`113-185`）：`converges_to_target`、`frame_rate_independent_after_one_second`（60 vs 120 Hz 同落点）、`settled_reports_when_at_rest`、`snap_clears_velocity`、`does_not_creep_below_float_precision`（x=1591 无爬行）。

**审计要点**：`step` 的"先判静止再积分"意味着跨过阈值的那一帧仍会带残速多积分一步，下一帧才吸附；数值上无害，但如果有人假设"settled() 为真 ⇒ 本帧没动"就会错。

---

## 3. 线带场景 `src/scene/mod.rs`（1986 行）

### 3.1 常量（`19-76`）

| 常量 | 值 | 用途 |
| --- | --- | --- |
| `COPIED_SECONDS` | 1.2 | "Copied" 气泡停留 |
| `FADE_SECONDS` | 0.16 | 卡片淡入：`opacity += (1-opacity)*(dt/0.16*2).min(1)`（等效 τ≈0.08 s，另有按 `drop` 的线性覆盖，`scene/mod.rs:1173-1176`） |
| `HIDE_SECONDS` | 0.22 | 收线二次缓动时长 |
| `ROW_RESPONSE` / `ROW_DAMPING` | 0.55 / 0.78 | 行内水平弹簧 |
| `DRAG_STEP_RESPONSE` / `_DAMPING` | 0.24 / 1.0 | 拖拽时邻卡让位（临界阻尼） |
| `RELEASE_RESPONSE` / `_DAMPING` | 0.30 / 1.0 | 松手落位（临界阻尼、不过冲） |
| `GRAB_DRAG_DIP` | 4.0 | 判定"真的在拖"的位移 |
| `ORDER_DEADBAND_DIP` | 12.0 | 越过邻槽中点 12 DIP 换序，反向 12 DIP 才换回（合计 24 DIP 迟滞） |
| `DRAG_LEAN_MAX_DEG` / `DRAG_LEAN_SPEED` / `_RESPONSE` / `_DAMPING` | 4.0 / 1800 / 0.14 / 1.0 | 拖拽时随速度倾倒 |
| `DRAG_LIFT_SCALE` | 1.04 | 拖拽放大 |
| `RUBBER_FREE_DIP` / `RUBBER_SLOPE` / `RUBBER_MAX_DIP` | 18 / 0.22 / 26 | 向下橡皮筋，`rubber_y()` 在 `79-87` |
| `PIN_HOVER_SCALE/GLOW` | 1.12 / 0.22 | 夹子悬停 |
| `PIN_PRESS_SCALE/GLOW` | 0.94 / 0.26 | 夹子按下 |
| `PIN_DRAG_SCALE/GLOW` | 1.08 / 0.20 | 夹子拖拽 |

几何常量在 `src/layout.rs`：`HIDDEN_SLIDE = -(210+12)`（`:44`，线与窗口上边的隐藏位移）、`NOTE_PEEK_ANGLE_DEG = 18` / `NOTE_PEEK_VISIBLE_DIP = 12`（`:59-61`）、`sag()` 绳垂度（`:79`，静态几何，不是动画）、`slot_x`（`:93`）、`note_hotspot` / 便签"露角"位姿（`:201` / `:228`）。卡片顶边 `slot_top` 不在这里算：`publish` 里由 `rope_y(x) - PIN_ABOVE + drop + slide` 得出（`src/scene/mod.rs:1251`），绘制侧同式（`src/scene/paint.rs:114`）。

### 3.2 `Card` 的弹簧表（`src/scene/mod.rs:177-265`）

| 字段 | 初值 / 参数 | 驱动事件 |
| --- | --- | --- |
| `x` | `Spring::new(ROW_RESPONSE, ROW_DAMPING, x)` | 布局、加卡、移除、换序、`SetFlying` |
| `drop` | `Spring::new(0.42, 0.72, -46)` | 新卡降落、橡皮筋、落地 |
| `swing` | `Spring::new(1.0, 1.0)`，新卡置 `value = 16.0` 且 `set_stiffness(46.0, 2.6)` | 落地摆动、阵风、`nudge` |
| `press` | `Spring::new(0.3, 0.6)` | 按下缩放（`target = 0.95` 或拖拽 1.035，见 `:1111-1125`） |
| `lean` | `Spring::new(0.14, 1.0)` | 拖拽倾倒（度） |
| `pin_scale` / `pin_glow` / `pin_hot` | 0.16/0.85, 0.16/0.85, 0.18/0.90 | 夹子视觉；拖拽时改成 `0.12/1.0`（`:1130-1155`） |
| 普通 `f32` | `opacity`、`cross_alpha`、`actions_alpha`、`copied_alpha` | 一阶指数逼近（`dt*12`、`dt*14`） |

`Card::new`（`218-265`）：`arriving` 时 `drop.snap(0)`、`opacity = 1`、`nudge(2.2)`；否则 `swing.value = 16.0`、`set_stiffness(46.0, 2.6)`、`drop.target = 0`。
`nudge`（`267`）设置 `nudge_from/to`、`nudge_t = 0`；`schedule_gust(deg, delay)`（`276`）做 0–0.35 s 错峰；`step_swing`（`303-330`）里阵风是**位置 + 速度缓动**（`nudge_t += dt/0.3`，`smoothstep` + `nudge_v0·0.3·carry`，F4），结束后 `swing.value = nudge_to; velocity = 0; set_stiffness(38.0, 2.4)`。
`swing_at_rest`（`293`）：`|value| < 0.05°` 且 `|velocity| < 0.3°/s`（F7，= 0.17 DIP / 0.35 物理像素），注释解释为何比通用阈值宽。

### 3.3 命令的动画语义（`apply`，`:576-783`）

| 命令 | 行 | 效果 |
| --- | --- | --- |
| `Layout` | 578 | `set_layout` 重算槽位，目标变化由弹簧追 |
| `SetVisible` | 579 | `set_visible`（隐藏时停帧） |
| `Reveal(bool)` | 580-598 | 关闭时 `end_grab()`、`hide_t = -1`、`note_peek.snap(0)`；打开时若 `now >= next_gust` 重置阵风时钟（`:591-595`） |
| `AddCard` | 599+ | 上传位图、`Card::new`、`relayout_cards` |
| `SetFlying` | 699 | 卡片 `x.snap(landing_x)`、`opacity = 0`、`swing/lean = 0`；`flying = false` 时 `opacity = 1`、`nudge(2.2)`、重排 |
| `SetRehang` | `744` / `959` | 不重排、隐藏卡片；落地时 `nudge(2.2)` |
| `NotePeek(bool)` | 745-751 | `note_peek.target = 1/0`（弹簧 0.22 临界） |
| `HideNotePeek` | 752-754 | `note_peek.snap(0)`（"整张便签接管，不能留残影"） |
| `Copied` | 756 | `nudge(3.0)`、`copied_until = now + 1.2` |
| `Grab` | app 侧发，`begin_grab` `784` | 见 §3.4 |
| `Gust` | 764 | `gust(now)`（探针手动触发；正常由内部时钟） |

### 3.4 夹子拖拽手势（2.6.2 的"直接操纵"）

- `begin_grab`（`784`）：拒绝 `flying`/`rehang`/未落地；把所有卡 `x.set_response(DRAG_STEP_RESPONSE, 1.0)`；被拖卡 `press.set_response(0.12, 1.0)`。
- `set_pointer`（`531`）：`dt > 1e-4` 时用位移估水平速度，超过 `GRAB_DRAG_DIP` 置 `moved`。
- `track_grab`（`858`）：被拖卡 `x.snap(pointer)`、`drop.snap(rubber_y(...))`——**1:1 跟手，无弹簧无延迟**；`lean.target = speed * (4/1800)` 限幅 ±4°；水平位置钳在 `[半卡宽, 宽-半卡宽]`。
- `reorder_grab`（`884`）：每次调用最多走一步，条件是 `x > 槽+SPACING/2+12` 或 `< 槽-SPACING/2-12`，交换后 `relayout_cards`。
- `end_grab`（`826`）：他人回 `ROW`；本卡回 `RELEASE`（0.30/1.0）、`x.velocity = grab.speed` 限幅 ±600、`drop.target = 0`、`lean.target = 0`、`release_spring = true`；`update` 在 `:1097` 检测落定后把本卡换回 `ROW` 参数。
- 渲染线程侧每帧调用 `set_pointer` 并兜底松手：`src/window/overlay.rs:381-394`。

### 3.5 一帧的积分顺序（`update`，`:995-1213`）

```
note_peek 弹簧 (:1004)                       ← 便签露角
note_peek_x 仅可见时积分，否则 snap (:1009-1021)
slide：开=弹簧；关=0.22 s 二次缓动，每帧 velocity=0 (:1023-1042)
完全收起 → publish 一次后 return false (:1044-1051)
hint_alpha 指数逼近，阈值 0.001 (:1053-1059)
copied 到期清理 (:1061-1064)
阵风时钟 (:1066-1068)
夹子拖拽：track + reorder (:1073-1090)
逐卡：x.step / release_spring 归位 (:1092-1109)
      drop.step、step_swing、press、pin、lean (:1111-1161)
      opacity / cross_alpha / copied_alpha / actions_alpha (:1163-1185)
      animating 汇总 (:1187-1206)
animating || dirty → publish (:1209-1212)
```

`publish`（`:1225`）把 `slide`、卡片位置/角度、命中框写进 `FrameSnapshot`；`slot_top = rope_y(x) - PIN_ABOVE + drop + slide`，`pin = (slot_center, slot_top)` 就是旋转原点——绘制和命中测试共用这一份数据（`AGENTS.md` 禁止读像素）。

### 3.6 线带绘制 `src/scene/paint.rs`（267 行）

| 位置 | 内容 |
| --- | --- |
| `:14` `draw` | 整体平移 `(0, slide.value)` → 绳 → 露角便签 → 空提示 → 卡片 |
| `:22` / `:69` `draw_note_peek` | `reveal = note_peek.value`；`cy = base_cy - (1-reveal)*NOTE_PEEK_VISIBLE_DIP`，先转 `NOTE_PEEK_ANGLE_DEG` 再平移（`:79-82`） |
| `:43-58` | 按顺序画，被拖拽的卡最后画（永远在最上层） |
| `:102` `draw_card` | `angle = swing + tilt + lean`（`:116`）；变换 = `scale_around(press*lift)` × `rotation_around(angle, (CARD_WIDTH/2, 0))` × `translation(slot_left, slot_top+slide)`（`:122-136`） |
| `:151` | `push_opacity(bounds, card.opacity)`，组透明度 |
| `:155-165` | 阴影按抬起/悬停/静止取 0.30 / 0.26 / 0.18（阴影本身是预烘焙九宫格，不逐帧模糊） |
| `:177` | 夹子用 `pin_scale` / `pin_glow` / `pin_hot` |
| `:206-260` | "Copied" 气泡与按钮行随 `copied_alpha`/`actions_alpha` 淡入，按钮行上浮 `-4*(1-actions_alpha)` |

---

## 4. 飞行窗口 `src/window/flight/`（1523 + 197 行）

### 4.1 常量（`mod.rs:41-79`）

| 常量 | 值 | 含义 |
| --- | --- | --- |
| `FLIGHT_SECONDS` | 0.65 | 飞行段 |
| `LAND_FADE_SECONDS` | 0.16 | 落地后淡出（真卡在下面） |
| `DROP_SECONDS` | 0.32 | 掉落（取卡） |
| `DROP_DISTANCE` | 200 | 掉落位移，`t^3` 缓动 |
| `TUMBLE_SHRINK_SECONDS` / `TUMBLE_SHRINK` | 0.30 / 0.45 | 丢弃空便签的缩小 |
| `TUMBLE_GRAVITY` | 2600 | 重力 DIP/s² |
| `TUMBLE_SECONDS` | 2.0 | 兜底结束 |
| `TUMBLE_HANDOFF_SECONDS` | 0.05 | 丢弃便签先保持起始姿态多久，再开始下落（`mod.rs:70`） |
| `ARC_DIP` | 30 | 飞行弧线高度，`-sin(π·k)·30` |
| `CHROME_FADE_START/END` | 0.10 / 0.55 | 玻璃框淡入的时间窗（占 0.65 s 的比例），只用于截图飞行 |
| `NOTE_CHROME_LEAVE_END` | 1/3 | 便签去编辑器时边框淡出结束的**路径**进度（`k`，不是时间） |
| `NOTE_CHROME_ARRIVE_START` | 2/3 | 便签回卡片时边框淡入开始的路径进度 |
| `ENTRY_CROSSFADE_SECONDS` | 0.12 | 卡纸 → 编辑器纸面的溶解 |
| `ENTRY_SETTLE_RESPONSE/_DAMPING` | 0.16 / 0.9 | 进编辑器前把卡扶正的角弹簧 |
| `PAD_DIP` / `PAD_FRACTION` | 60 / 0.05 | 飞行窗口比路径包围盒多留的边 |
| `MAX_FRAME_DT` | 0.05 | 窗口内 dt 上限（与 overlay 重复定义，见 §12） |

### 4.2 三条时间线

| 时间线 | 入口 | 推进 | 结束条件 |
| --- | --- | --- | --- |
| 飞行 | `Flight::fly` `:288`、`fly_decorated` `:319`、`fly_entry` `:357` → `start_flight` `:386` | `FlightPlan::step(elapsed, frame_dt)` `:929` | `landed && elapsed >= 0.65 + 0.16`（`:1030` 附近） |
| 掉落 | `drop_card` `:438` → `run_drop` `:1159` | `drop_pose(t)` `paint.rs:119` | `t >= 1`（0.32 s） |
| 抛丢 | `tumble` `:542` → `run_tumble` `:1423` | 手写重力积分（`Tumble::fall` `:1197`） | 纸面中心完全出窗 或 2.0 s（都从交接结束的时刻算起） |

`FlightPlan::step` 关键点：

- `t = elapsed/0.65`，`k = ease::standard(t)`（CSS `cubic-bezier(0.25, 0.1, 0.25, 1)`，F11）；尺寸/中心线性插值，`cy -= sin(π·k)·30`（`:954-955`）。
- 落地后叠 `LandingFollow`：`x` 偏移 + `swing`（`:936-965`）。
- 有起始位图时用 0.16/0.9 角弹簧把 `angle` 收到 0（`:968-980`），否则 `angle = tilt*k + swing`。
- `chrome` 由 `ChromeMode` 决定（`:1044`）：`FadeIn` = `smoothstep(t, 0.10, 0.55)`（截图飞行，不变）、`Full` = 1（标注编辑器往返，不变）、`Leave` = `1 - smooth(k, 0, 1/3)`、`Arrive` = `smooth(k, 2/3, 1)`。便签两条按**路径**进度 `k` 而不是时间：`k = 1/3` 约在 `t = 0.215`（0.14 s），`k = 2/3` 约在 `t = 0.39`（0.25 s）。
- `inset` 与 `radius` 也从 `ChromeMode` 来（`:1056`、`:1065`）：`FadeIn` 仍是 `PHOTO_INSET * chrome` / `PHOTO_RADIUS`；`Full`/`Leave`/`Arrive` 的 `inset` 恒为 `PHOTO_INSET`，`radius` 在卡片的 12 DIP 与编辑器的 22 DIP（`note_editor::display::RADIUS_DIP`）之间按 `k` 过渡（`Leave` 12→22，`Arrive` 22→12）。
- `blend` 时 `crossfade = elapsed/0.12`（`:1024`）；`alpha = 1 - smooth((elapsed-0.65)/0.16)`（`:998-1006`）。
- 旋转仍绕"绳端"：`card.top - CARD_OFFSET_BELOW_TOP`（`:1011-1017`）。
- `pre-roll` 段（`step_pre_roll` `:1036`）：先原地扶正、把卡纸溶解成编辑器纸面，`ready = settle.settled() && crossfade >= 1`。

### 4.3 落点镜像（防止"两张图分开"）

`LandingFollow`（`:569-607`）：

```rust
x: Spring::new(0.55, 0.78),      // 与 Scene ROW_RESPONSE/ROW_DAMPING 同值
swing: Spring::new(1.0, 1.0),    // 落地 nudge 2.2°（0.3 s）后 set_stiffness(38.0, 2.4)
```

这两个数**必须**与 `Scene::Card` 的 `ROW_*`、`nudge(2.2)`、`step_swing` 的 `(38.0, 2.4)` 一致，否则淡出中的飞行副本会和下面的真卡错位——2.1.1 修的就是这个。

### 4.4 绘制 `paint.rs`

| 位置 | 内容 |
| --- | --- |
| `:20` | `SHADOW_MAX_CARD = 300`：宽度 ≤300 DIP 才画卡片阴影 |
| `:23` `paint` | 设变换 → `draw_flight_card` → `paint_start_image` → 复位 |
| `:50` `paint_start_image` | `crossfade >= 1` 直接返回（`:56`）；`start_alpha = 1 - smooth(crossfade, 0, 1)`（`:66`）；inset 与圆角取 `pose.inset` / `pose.radius` |
| `:76` `paint_pre_roll` | 起飞前那一帧：卡还在槽位、正在扶正、卡纸与纸面叠加（`chrome = 1`、`inset = PHOTO_INSET`、`radius = PHOTO_RADIUS`） |
| `:119` `drop_pose` | `e = t^3`，旋转绕自身顶心**先于**平移（注释记录了"绕窗口点转会甩出去"的旧 bug） |
| `:134` `draw_flight_card` | 照片 inset 与圆角由调用方传入（不再从 `chrome` 推导）；玻璃框/描边乘 `chrome`；夹子画在 `card.top - CARD_OFFSET_BELOW_TOP`，透明度乘 `pin` |

### 4.5 事件与中断

`FlightEvent`（`:142`）`Started` / `Landed` / `Finished`；`run_flight`（`:1260`）在**画第一帧之前**报 `Landed`（`:1096-1156` 的顺序有注释说明原因），`Started` 在首帧上屏后报，`Finished` 最后报。`FlightCommand`（`:160`）与"代"（generation）计数让新截图可以抢占正在飞的窗口而不误报旧事件。

三条 `run_*` 在离开循环前都要 `clear_frame`（`:1183`，`begin` + `end` + `present`，一帧全透明），正常结束和被打断两条路径都要走。窗口是复用的，隐藏时不动交换链；不擦的话下一段飞行在 UI 线程换好窗口位置、渲染线程还没画之前，DWM 会把上一段的最后一帧按新尺寸拉伸露出一块（丢弃翻滚的末帧是不透明的）。

`run_tumble` 的 `Started` 与另外两条不同：先 `DwmFlush()`（`:1529`），确认第一帧已被合成，再报事件，因为编辑窗口一收到就隐藏自己的纸面；`Present` 只是排队，默认延迟三帧。失败只记日志。它同时把起始姿态保持 `TUMBLE_HANDOFF_SECONDS`，所以编辑窗口早或晚一帧消失都被同一张纸盖住。

---

## 5. 交接状态机 `src/app/mod.rs` + `src/app/book.rs`

这是审计中最容易出"一帧闪空/幽灵卡片"的地方。

| 位置 | 行为 |
| --- | --- |
| `book.rs:17-34` `FlightBook` | `flying_card`、`flying_rehang`、`annotate_show_at`、`annotate_close_at`、`note_show_at` |
| `app/mod.rs:1148-1171` `on_flight_started` | 首帧上屏后才隐藏被接管的窗口：`TUMBLE` → 隐藏便签编辑器；`NOTE` → `hide_note_card()`；`ANNOTATE_OPEN` → 隐藏被标注的卡；`ANNOTATE_CLOSE` → 隐藏编辑器 |
| `:1175-1232` `on_flight_landed` | `NOTE`/`ANNOTATE_OPEN` → 显示编辑器并把飞行窗置顶；`ANNOTATE_CLOSE` → `SetRehang{flying:false}`；普通落地 → `SetFlying`/`SetRehang{flying:false}`（`rehang` 决定是否保持行不重排） |
| `:1235-1279` `on_flight_finished` | 飞行窗口回池、`flight_idle_at`；各分支清 bookkeeping |
| 看门狗 | `:2383` 便签 `note_show_at = now + 1.5`；`:3082` 标注打开 `annotate_show_at = now + 1.5`；`:3250` 标注关闭 `annotate_close_at = now + 2.0`；到期在 `tick`（`:956-1035`）兜底 |
| 打开标注 | `:3011-3095` `open_editor`：先 `SetRehang{flying:true}` 把卡藏在线里，再用 `fly_entry(ANNOTATE_OPEN_TOKEN, …, angle)` 从卡位飞到编辑器卡位 |
| 返回标注 | `:3222-3254` `fly_picture_back` → `fly_card_back`（`:2684`）→ `fly_decorated(ANNOTATE_CLOSE_TOKEN, …)` |
| 便签返回 | `:2716` `fly_note_back`；`:2812` `hide_note_card`；`:2853` `discard_new_note`（`flight.tumble`） |
| 进编辑器 | `:2319` `open_note_editor_with`：`fly_entry(NOTE_FLIGHT_TOKEN, FlightImage::Ready(...))`，起飞无 pin（新便签还没"挂上"） |

---

## 6. 便签悬停入口（唯一跑在 UI 定时器上的动画）

`src/app/mod.rs:1538-1719` `update_note_entry`，由 33 ms `tick` 和 15 ms `TIMER_NOTE` 共同驱动（`:1836` 开始，`:1880` 结束）。

| 位置 | 内容 |
| --- | --- |
| `:1538-1557` | 前置条件：线不可见 / 编辑器打开 / 菜单打开 / 正在拖拽 → 取消并复位 |
| `:1563` | `pin_press` 时收起露角并取消悬停 |
| `:1720` `note_entry_pointer` | 真实光标，或 `PEGLINE_NOTE_DEMO_POINTER` 指定的演示点 |
| `:1751` `note_entry_zones` | 顶部整列热点 + 距边 24 DIP 的判定 |
| `:1598-1611` `In` | `t = (now-started)/(0.22*slow)`，`in_out_cubic`，每拍发布导数到 `hover.vel`（F13）；中心从露角位姿插值到 home，角度 `NOTE_PEEK_ANGLE_DEG*(1-k)` |
| `:1631` | `t > 0.15` 后指针离开 → 立刻转 `Out` |
| `:1650` `Hover` | 每拍 `step_hover_physics` |
| `:1653-1667` `Out` | `t/(0.42*slow)` 缓动 + `step_hover_spin(hover, 0.0, dt)` 把纸扶正，`t>=1` 结束 |
| `:1698-1707` | 每拍用 `preview.move_to` 移动窗口、`preview.tilt_note` 下发倾斜 |

物理函数：

| 位置 | 内容 |
| --- | --- |
| `:3975-3983` | `dt = (now - hover.last).clamp(0, 0.05)`——**dt 来自 UI 定时器间隔，不是渲染帧** |
| `:3988-3993` | `stiffness = ω²`、`damping = 2·0.6·ω`，`ω = TAU/NOTE_HOME_RESPONSE`；`bob = sin((now-float_start)·TAU·0.22)·3·scale` |
| `:3999-4022` | 240 Hz 子步进；指针拉力只在 `0.5 < distance < 260*scale` 时生效，`desired = min(distance*0.22, 30*scale)`，`force = stiffness*desired/distance` |
| `:4024-4047` | 夹在工作区内：只丢掉朝外的速度，保留朝内的 |
| `:4066` `hover_tilt_target` | 对角项 `2·dx·dy/dist²`，`reach = min(dist/120, 1)`，最多 5° |
| `:4083` `step_hover_spin` | 同 response/damping 的角弹簧 |
| `:4096` `tilted_card_hits` | 命中测试反解倾斜 |
| `:4114` `clamp_center` | 中心夹紧 |

配套：`src/app/mod.rs:161` 起 `NOTE_HOME_RESPONSE 0.56`、`NOTE_HOME_DAMPING 0.6`、`NOTE_PULL_RATIO 0.22`、`NOTE_PULL_RANGE_DIP 260`、`NOTE_PULL_MAX_DIP 30`、`NOTE_BOB_DIP 3`、`NOTE_BOB_HZ 0.22`、`NOTE_TILT_MAX_DEG 5`、`NOTE_TILT_RANGE_DIP 120`、`NOTE_FLY_IN_SECONDS 0.22`、`NOTE_FLY_OUT_SECONDS 0.42`。

预览窗口侧：`src/window/preview.rs:56` `NOTE_TILT_STEP_DEG = 0.05`（倾斜变化小于此值不重绘，让纸片能"睡着"）、`:238-253` `tilt_note`、`:207` `move_to`、`:310/368/415` 渲染线程记录"当前已画的倾斜"。

---

## 7. 悬停大图预览（不是弹簧，是延迟）

| 位置 | 内容 |
| --- | --- |
| `src/preview.rs:23` `HOVER_DELAY = 0.35` | 停留 0.35 s 才请求预览 |
| `src/preview.rs:164-200` `HoverState::observe` | 换卡/换图会重启 dwell 并递增 generation，丢弃过期解码 |
| `src/app/mod.rs:1959` `update_preview` | 便签悬停或夹子按住时隐藏预览（`:1960-1972`） |
| 测试 | `src/preview.rs:346-396`：`hover_waits_before_requesting_one_image`、`switching_cards_cancels_…`、`leaving_cancels_…`、`edited_images_replace_…` |

预览窗口本身是 `SetWindowPos` 放置，没有淡入淡出；出现/消失是硬切（视觉上由卡片按钮行的淡入掩盖）。

---

## 8. 编辑器内部几乎没有动画

这是刻意的：两个编辑器的"动画"全部由飞行窗口 + `app` 编排，窗口自身只在状态变化时重绘。

| 编辑器 | 位置 | 实际存在的时间性内容 |
| --- | --- | --- |
| 便签编辑器 | `src/note_editor/render/mod.rs` | 光标闪烁 `BLINK_SECONDS = 0.53`（`:85`），`draw`（`:1318`）在 `:1320-1322` 翻转 `caret_on`，`tick_in`（`:1345`）给出下一次唤醒；除此之外只在文本/选区/滚动变化时重建 |
| 便签编辑器窗口 | `src/note_editor/window.rs` | 拖拽/缩放由 `WM_MOUSEMOVE` 直接 `SetWindowPos`（`:1273`），无平滑；每批指针移动合并（`BATCH_LIMIT 512`、`MOVE_RING 16384`），`DragStats`（`:593-660`）只做统计 |
| 标注编辑器 | `src/editor/window/mod.rs` | 悬停只点亮按钮：`apply`（`:1557`）里 `HoverClear` 分支（`:1572-1576`）注释写明"只重绘 chrome，卡位图复用"；`draw`（`:1969`）按 dirty 标志决定；无时间轴 |
| 标注编辑器绘制 | `src/editor/window/paint.rs` | `:193-330` 的 `hovered`/`selected` 直接选色，无过渡；`paint_button`（`:510`） |

便签编辑器的纸面尺寸/最小尺寸、标注编辑器的卡位几何在 `src/note_editor/display.rs`、`src/editor/layout.rs`，属于布局而非动画。

---

## 9. 拖拽幽灵 `src/window/ghost.rs`

| 位置 | 内容 |
| --- | --- |
| `:37` `GHOST_SCALE = 0.9` | 卡片的 90% |
| `:39` `FOLLOW_DIP = 12` | 挂在指针下方 12 DIP（`follow_rect` `:334`） |
| `:42` `FOLLOW_MS = 15` | 渲染线程 `try_recv` + 高精度 `sleep(15 ms)` 轮询指针（`:203-224`，F15），**无弹簧、无预测** |
| `:44` `GHOST_OPACITY = 0.85` | 整卡（玻璃+照片+描边）作为一组透明度 |

注意：它与夹子拖拽是**两套**机制。OLE 拖动期间 `tick` 直接 return（`src/app/mod.rs:931-935`），只有幽灵的 15 ms 定时器在跑；夹子拖拽则完全在渲染线程按帧采样。

---

## 10. 帧率、节能与测量

| 位置 | 内容 |
| --- | --- |
| `src/scene/mod.rs:507` `next_wake_in` | 拖拽中 → `Some(0.0)`；空行/拖拽 → `None`（无限等）；否则 `next_gust - now` |
| `src/scene/mod.rs:1044-1051` | 完全收起：publish 一次后 `return false`，渲染线程阻塞 |
| `src/window/overlay.rs:407-409` | `present(true)` 总是 vsync；`Retry` 立即重画 |
| `src/settings.rs:33-58` | `fps=auto|60|120`，`period()` 返回最小帧间隔 |
| `src/bin/probe.rs` | `--mode=line|idle|static|clear|prim|atlas|flight|timer|duo|note`；`line` 模式每帧手动 `Command::Gust`（`:296`）制造持续动画，**不代表真实 idle**；输出 `fps/p50/p95/max/draw_p50/draw_p95/private`（`:357`、`:459`、`:778`） |
| 阵风 | 每 7–16 s 一次（`scene/mod.rs:982` 里 `next_gust = now + rng.range(7.0, 16.0)`），每卡错峰 0–0.35 s，幅度 1.6–3.4°，落地后再摆 ≈4 s |

---

## 11. 复现与测量命令

```powershell
cargo build --release
cargo test
cargo clippy --all-targets -- -D warnings

# 线带：持续阵风下的帧率/绘制耗时/私有内存
cargo run --release --bin probe -- --mode=line --seconds=20 --cards=6
# 飞行：重复 10 次起落
cargo run --release --bin probe -- --mode=flight --repeat=10
# 便签编辑器开销
cargo run --release --bin probe -- --mode=note

# 手工看动画（演示开关都在 src/app/dev.rs:89-140）
cargo run --release --bin pegline -- --note-hover-demo
cargo run --release --bin pegline -- --note-editor-demo --note-hold
cargo run --release --bin pegline -- --note-discard-demo
cargo run --release --bin pegline -- --annotate-demo
cargo run --release --bin pegline -- --editor-demo --editor-hold
cargo run --release --bin pegline -- --note-resize-demo
```

相关环境变量：`PEGLINE_NOTE_DEMO_POINTER`、`PEGLINE_NOTE_PEEK_POINTER`、`PEGLINE_NOTE_SLOW`（悬停慢放）、`PEGLINE_NOTE_RESIZE_PAPER`、`PEGLINE_NOTE_RESIZE_HOVER`、`PEGLINE_NOTE_STORAGE_PAPER`、`PEGLINE_ALLOW_SECOND_INSTANCE`。

---

## 12. 审计重点（按优先级）

> 每条都给"为什么值得看"，不是"我觉得可以改"。判定基准是 `docs/BEHAVIOR.md` 与 `docs/DECISIONS.md`。

**A. 常量双写，漂移风险**

1. `src/scene/mod.rs` 的 `ROW_RESPONSE/ROW_DAMPING`（`:26-27`）与 `src/window/flight/mod.rs:578` 的 `Spring::new(0.55, 0.78)` 是同一组物理量，两处独立维护；`nudge(2.2)` / `set_stiffness(38.0, 2.4)` 同样在 `scene/mod.rs:218-265,322` 和 `flight/mod.rs:590-607` 各写一遍。2.1.1 的修复依赖它们相等。检查：是否有测试锁住这份相等；改一处会不会静默错位。
2. `dt` 上限 `0.05` 至少四处各自定义：`src/window/overlay.rs:378`、`src/window/flight/mod.rs:79`（`MAX_FRAME_DT`）、`src/app/mod.rs:3983`、以及各编辑器渲染线程。检查是否应统一。
3. `DRAG_STEP_RESPONSE`（0.24/1.0）与 `RELEASE_RESPONSE`（0.30/1.0）在 `scene/mod.rs` 内被 `begin_grab`/`end_grab`/`update:1097` 三处引用，存在"换回 ROW 参数"的时序；检查是否存在某帧用错参数导致速度突变。

**B. 时钟不一致**

4. 便签悬停跑在 15 ms UI 定时器上（`app/mod.rs:1836, 3975-3983`），纸片位置由 `SetWindowPos` 直接摆放（`preview.rs:207`），而倾斜由另一个线程按 0.05° 阈值重绘（`preview.rs:56, 238-253`）。66.7 Hz 的采样对上 60 Hz 的合成会产生 beat；且 UI 线程一旦被模态循环/OLE 拖拽阻塞，`dt` clamp 0.05 会让纸片"跳"而非追平。检查 `BEHAVIOR.md` 是否允许这种非渲染帧驱动的运动。
5. `reveal` 不对称：开是弹簧 0.42/0.82（`scene/mod.rs:1027`），关是 0.22 s 二次缓动（`:1029-1041`）。**已修（F5）**：收起现在用 `hide_v0` 承接起点速度，`velocity` 不再每帧清零；中途放下不会硬停。
6. `HIDE_DELAY 0.4`（`app/mod.rs:106`）后才 `overlay.hide()`（`:1287-1294`），而 `HIDE_SECONDS 0.22`；如果某帧 dt 被 clamp 或渲染线程卡顿，收起动画可能被硬切。检查余量是否足够（0.4 vs 0.22）。

**C. 未测试/难测试的路径**

7. `step_hover_physics` / `hover_tilt_target` / `step_hover_spin`（`app/mod.rs:3975-4114`）在 `App` 上，**没有单元测试**；`probe` 也没有 note-hover 模式。检查是否有等价的手测记录（`docs/VISUAL-TESTS.md`）。
8. 飞行窗口的 `Started`→`Landed`→`Finished` 次序、`tumble` 的结束条件（`flight/mod.rs:1281-1296`）、看门狗 1.5 s/2.0 s（`app/mod.rs:2383, 3082, 3250`）只有手测与演示开关覆盖。检查这些"兜底"是否真的可达，还是死防御。
9. `Present::Retry`（`overlay.rs:409`）直接重画、无退避；`DXGI_ERROR_WAS_STILL_DRAWING` 下会连续 `BeginDraw`。检查是否会在低端 GPU 上忙转。

**D. 手势与状态泄漏**

10. `begin_grab` 会把**所有**卡的 `x` 换成 0.24/1.0（`scene/mod.rs:784`），`end_grab` 只把本卡换到 0.30/1.0、其余在 `update:1097` 落定后逐个换回 ROW。检查拖拽中途被 `Reveal(false)`（`:580-598` 调用 `end_grab`）或新截图打断时，是否所有卡都回到 ROW。
11. 夹子拖拽的松手速度来自 `set_pointer` 的位移估计（`scene/mod.rs:531`），只在 `dt > 1e-4` 时更新；`end_grab` 用它设 `x.velocity`（限幅 ±600）。检查松手前一帧没有移动时，是否继承了过期的速度（可能造成"甩飞"）。
12. `Command::Dragging`（OLE，`:118`）与 `Command::Grab`（夹子，`:121`）是两套拖拽语义；`tick` 在 OLE 期间直接 return（`app/mod.rs:931-935`），但渲染线程仍在跑 `Grab`。检查二者能否同时成立。

**E. 慢衰减与节能**

13. `swing_at_rest` 阈值 0.05°/0.3°/s（`scene/mod.rs:293`，F7）配上 `set_stiffness(46, 2.6)`（ζ≈0.19）或 `(38, 2.4)`（ζ≈0.19），从 16°/2.2° 衰减到 0.02° 需要数秒（量级：3–5 s）。每次落地或阵风都会让 60 fps 循环多跑几秒；阵风每 7–16 s 一次（`:982`）。检查 `PLAN.md` 里有没有对应的功耗/帧循环实测，还是只测了 fps。
14. `Scene::update` 的 `animating` 汇总（`:1187-1206`）用 0.002/0.001 混用阈值，且对 `cross_alpha` 等改用了"与目标比较"（注释 `:1193-1195` 解释过原因）。检查是否有新的 alpha 字段漏进这个列表（漏了 → 动画中途停帧）。

**F. 结构重复**

15. 飞行窗口的 `drop_pose`（`paint.rs:119`）与 `run_drop`（`mod.rs:1159`）各算一次进度；`chrome`/`crossfade`/`alpha` 三条淡入淡出曲线在 `FlightPlan::step` 里手动拼装（`:987-1030`），没有共享的"时间轴"抽象。检查是否已经出现三处不一致（例如 `CHROME_FADE_*` 用比例、`LAND_FADE_SECONDS` 用绝对秒）。
16. `src/app/dev.rs`（666 行）把 11 个演示开关和约 20 个环境变量编进发布版；审计时注意区分"演示专用路径"和真实动画路径，别把演示分支当成冗余删掉（`AGENTS.md` 允许它们存在）。

> **2026-11-17 修复状态**（分支 `anim/261117-fix`，DECISIONS D19）：item 5 已由 F5 修复（收起交出速度）；item 13 的阈值抬到 0.05°/0.3°/s（F7），缓慢衰减本身保留；item 15 的 `drop_pose` 现签名为 `(t, left, top, width, angle, scale)` 且 `e = t²`（F9/F10）；§4 的 `k` 换成 `ease::standard`；§6 的 `In`/`Out` 都交出速度（F13）；幽灵改为 `try_recv` + 高精度 sleep（F15）。其余条目维持原判。

---

## 13. 明确不属于动画（避免审计跑偏）

- 绳子的垂度 `layout::sag()`（`src/layout.rs:79`）：静态几何，绳的几何体在 DPI 或宽度变化时重建（`render/mod.rs:1037`）。
- 阴影：预烘焙九宫格 + 每帧九次 blit（`src/render/mod.rs:1269` 起），`SHADOW_MAX_CARD`（`flight/paint.rs:20`）只是"小卡才画"的开关。
- Mica/玻璃材质：`src/mica.rs`、`Renderer::draw_glass`（`src/render/mod.rs:588`），随壁纸/显示变化重建，不参与动画。
- 命中测试：全部由 `publish` 的同一份布局数学得出（`scene/mod.rs:1225`、`app/mod.rs:4096` 的 `tilted_card_hits`），不读像素——这是硬规则，不是待优化点。

---

## 14. 测试覆盖地图

| 领域 | 测试 | 位置 |
| --- | --- | --- |
| 弹簧内核 | 5 个（含 60/120 Hz 等价、无爬行） | `src/spring.rs:117-185` |
| 场景状态机 | `a_landing_is_opaque_under_the_flight_and_glides`、`a_removed_card_glides_the_entry_…`、`a_hidden_entry_snaps_…`、`a_rehang_keeps_the_row_…`、`a_flying_card_waits_…` | `src/scene/mod.rs:1386-1637` |
| 阵风 | `a_scheduled_gust_waits_its_delay_then_releases_the_spring`、`a_gust_eases_out_swings_past_zero_once_and_settles` | `src/scene/mod.rs:1637-1728` |
| 拖拽 | `the_rubber_band_…`、`a_grab_during_the_reveal_…`、`the_card_in_hand_is_placed_on_the_pointer_not_sprung`、`crossing_the_midpoint_reorders_once_…`、`the_release_keeps_the_pointer_velocity_and_lands_without_overshooting`、`a_plain_pin_click_does_not_reorder`、`the_lean_follows_the_pointer_speed_and_is_capped` | `src/scene/mod.rs:1796-1980` |
| 飞行时间轴 | `a_falling_card_only_moves_down`、`the_pin_stays_at_the_rope_end`、`the_travel_ends_on_the_destination_rect`、`the_settle_spring_advances_by_time_not_by_frames`、`note_frames_fade_on_the_editor_side_of_the_path`、`a_capture_still_fades_its_frame_in_over_the_clock`、`a_note_paper_carries_the_editor_radius_across_the_path` | `src/window/flight/mod.rs:1393-1740` |
| 悬停 dwell | 4 个 | `src/preview.rs:346-396` |
| 曝光缺口 | 便签悬停物理（`app` 层）、`Started/Landed` 次序、`tumble` 边界、`Retry` 路径、看门狗竞态 | 无单测 |

---

## 15. 版本背景（避免把已修的 bug 当现状）

- 2.6.2 删除了整块"纸张摆动"物理（`src/scene/pendulum.rs`、`src/scene/trace.rs`、`PEGLINE_PAPER_*` 及 `tools/paper-*`），夹子拖拽改为直接操纵 + 三个弹簧 + 两个阈值（`docs/DECISIONS.md` D18，取代 D16/D17）。若在 `docs/` 或旧 release notes 里看到 `pendulum`，那是历史。
- 2.7.0 之后（2026-11-18，`docs/DECISIONS.md` D20）便签飞行的玻璃边框改为按路径进度淡出/淡入：`chrome_full: bool` 变成 `ChromeMode { FadeIn, Full, Leave, Arrive }`，`FlightPose` 多了 `inset`/`radius`。若看到"便签往返全程完整边框"，那是 2.7.0 及更早的行为。
- 2.6.1 修的是"夹子按住时底部预览仍弹出"（`src/app/mod.rs:1964-1972` 现在是显式拦截）。
- 2.1.1 修的是"落点镜像"（`LandingFollow`，见 §4.3）。
- 2.4.0 把玻璃换成 Mica（`src/mica.rs`），代码量和绘制面都变了，但与动画时序无关。

---

## 16. 一句话给审计者

先读 §3.5 的 `update` 顺序、§4.2 的三条时间线、§5 的交接状态机——90% 的动画 bug 只会出现在这三处的边界上；其余部分（编辑器内部、绘制、命中测试）基本是"状态到了就画一帧"，不值得花时间。
