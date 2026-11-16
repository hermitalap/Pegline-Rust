# Pegline 2.6.0 夹子拖拽排序 — 执行报告

- **版本**：2.6.0（设计稿，未实现）
- **报告时间**：2026-11-12 23:38
- **代码基线**：`bc30655` `fix: keep IME selection offsets on UTF-8 boundaries`
- **状态**：设计完成，待拍板与实现。文中所有数值均为计划值，尚未实测。
- **范围**：夹子（pin）悬停/按下/拖拽、X 轴挤压与交换、Y 轴绳弹簧、视觉、输入、持久化、测试与文档。
- **不在范围**：跨行/多选、触控手势、OLE 拖出语义变更、120 Hz 硬件结论、`archive/wpf-v1/` 修改。

## 0. 结论摘要

**可行，且不需要新依赖、不扩窗口、不引入每帧模糊或回读。** 这项功能真正的难点不是弹簧，而是三件事：

1. **输入所有权**：夹子目前落在“顶部带点击 → 收起线带”的路径上，且按下 4 DIP 后现有逻辑会进入 OLE `DoDragDrop`。夹子必须在带点击、卡片点击和 OLE 拖出之前被识别，并且不能复用 `state.pressed`。
2. **睡眠语义**：`Scene::next_wake_in` 在 `dragging.is_some()` 时返回 `None`（`src/scene/mod.rs:411`），而 overlay 的渲染线程会在 `recv()` 上睡死。拖拽必须使用独立的 `grab` 状态，并让 `next_wake_in` 返回 `Some(0.0)`。
3. **交换后的几何**：槽距 174、卡宽 150，交换瞬间被拖卡与邻卡必然重叠约 98 DIP。任何“无重叠交换”的方案都会让卡片瞬移、脱离指针或把整行推走。

**推荐裁决**：被拖卡始终浮在邻卡之上；邻卡在阈值处向左做一次 0.20 s 的 hop 并落回空出的槽位；松手后被拖卡弹到新槽位，重叠自然消失。把“拖拽中允许重叠”写进规范，而不是假装它不存在。Y 轴用带限位的橡皮筋，最大下拉 26 DIP，pin 永远不会看起来离开绳子。

**需要拍板的 4 件事**见 §12。预计工作量 **4.5 人日 + 0.5 人日缓冲**。

## 1. 需求拆解与验收口径

| 用户描述 | 本报告的落地解释 | 可验收的结果 |
|:--|:--|:--|
| 鼠标移到夹子上，夹子高亮并微微放大，稍微发光 | pin 独立于卡片 hover 的四态视觉：idle / hover / pressed / drag；放大 1.12、暖白光晕、金属提亮 | 200% DPI 截图；0.16 s 弹簧过渡；不触发大图预览 |
| 按住左键拖动该挂图 | 在 pin 上按下即进入 `Grab`，移动 ≥4 DIP 后开始跟手；指针由渲染线程逐帧采样 | 120 Hz 下 1:1 跟手；33 ms UI tick 不参与拖拽位置 |
| 动画和之前一样，物理模拟，有弹性与惯性 | 水平跟随用 response 0.075 / ζ0.85 的弹簧；松手落位 0.38 / ζ0.72 并带速度；倾斜跟随水平速度 | 快拖有滞后与回弹；松手有轻微过冲；不瞬移 |
| 往右拖，右侧挂图受向右挤压力，位移+微微旋转 | 挤压按真实重叠量计算：邻卡右移最多 34 DIP，旋转最多 3°，再下一张被连带 4–10 DIP | 邻卡可见地位移与倾斜；不是整行一起平移 |
| 挤到一定位置，右侧图弹向左侧，与被拖图交换 | 被拖卡与下一张重叠 ≥28 DIP 并保持 60 ms → 交换数组顺序；邻卡以 −260 DIP/s 弹向空槽，并做 16 DIP 的 hop | 交换瞬间有“弹过去”的动感；顺序在松手后持久化 |
| 被操作的图可能还比较靠左，因此也受向左挤压力 | 交换瞬间给被拖卡一个 −160 DIP/s 的反向冲量；它的目标仍是指针，冲量表现为短暂后坐与左倾 | 交换后卡片有一下“被顶住”的回弹，但不脱离指针 |
| Y 轴像有弹簧把夹子拉回挂绳，鼠标偏离时夹子仍在绳附近 | `drop` 目标 = 橡皮筋(指针偏移)，0–18 DIP 线性、18–26 DIP 压缩到 0.22 倍，硬上限 26 DIP | 指针下拉 80 DIP 时 pin 最多离绳 26 DIP；窗口 210 DIP 内不裁切 |

## 2. 交互状态机

```
Idle
 └─ 指针进入 pin 命中框（UI 30 Hz 检测） → PinHover
PinHover
 ├─ 左键按下 → PinPressed（SetCapture、click-through=false）
 └─ 指针离开 → Idle
PinPressed
 ├─ 移动 ≥ 4 DIP → Drag（grab.moved = true）
 ├─ 左键抬起 → Idle（不复制、不打开、不编辑、不收起）
 └─ 屏幕/DPI 变化、卡片被删、窗口关闭 → Idle（取消）
Drag
 ├─ 每帧：采样指针 → 更新 x/drop → 计算挤压 → 可能交换
 ├─ 左键抬起（钩子/窗口消息/GetAsyncKeyState 任一） → Settle
 └─ 取消条件同上 → Settle
Settle
 └─ x/drop/lean/pin 弹簧收敛 → Idle
```

| 状态 | 谁拥有 | 视觉 | 唤醒 |
|:--|:--|:--|:--|
| Idle | Scene | pin 1.0，无光晕 | 正常 `next_wake_in` |
| PinHover | Scene（UI 发 `PinHover`） | pin 1.12、光晕 0.22、金属提亮 | 弹簧未收敛时按帧 |
| PinPressed | Scene（`Grab` 已到，`moved=false`） | pin 0.94、光晕 0.26；卡片不缩 | 必须 `Some(0.0)`，等指针 |
| Drag | Scene（渲染线程采样） | pin 1.08、光晕 0.20；卡片置顶、阴影 0.30 | 必须 `Some(0.0)` |
| Settle | Scene | pin 回落；卡片落位、邻卡回弹 | 弹簧/倾斜/hop 未收敛时按帧 |

**取消不是“回滚动画”**：取消时直接把 `x.target` 设为当前索引的槽位、`drop.target=0`，让现有弹簧把它送回去；不要新建一条“取消动画”。

## 3. 物理模型

### 3.1 坐标与抓取

- 全部用窗口 DIP；x 向右，y 向下；pin 的锚点是卡片顶边中心 `(75, 0)`（卡片局部坐标），pin 视觉中心 `(75, 13)`。
- 抓取偏移：`grab_dx = 按下时 pin 的 x − 指针 x`；`grab_dy = 按下时 pin 的 y − 指针 y`。按下点通常就在 pin 上，所以两者都很小；保留它们是为了“抓住 pin 的哪一点就从哪一点拖”，而不是把卡片中心吸到指针。
- `grab` 记录：`id`、`index`、`start_pointer`、`grab_dx/dy`、`moved`、`swap_anchor`、`swap_dir`、`last_swap_at`。
- 死区 4 DIP 与现有 `DRAG_DIP=4.0`（`src/app/mod.rs:119`）一致；死区内卡片不动，只显示 pressed 视觉。

### 3.2 水平跟随

```
x.target = clamp(pointer_x + grab_dx, 75, width_dip − 75)
```

- 用 `Spring::new(0.075, 0.85, …)` 跟随。response 0.075 s 在 1000 DIP/s 的快速拖动下约落后 20 DIP，松手后正好把这点滞后转成落位惯性。
- 指针每帧位移限制 120 DIP（防 RDP/远程桌面瞬移），超出则按 120 DIP 插值；交换每帧最多一次。
- **采样在渲染线程**：`GetCursorPos` + `ScreenToClient` → DIP，`scene.set_pointer(x, y, now)`。不要走 33 ms UI tick，也不要把坐标塞进命令队列（2.4.2 的输入队列教训：队列峰值 543 → 12，延迟 4.5 s → 0.8 ms）。
- 渲染线程只在 `scene.grabbing()` 为真时采样；静止时零系统调用。

### 3.3 Y 轴绳弹簧（橡皮筋）

```
rest_y(x)   = rope_y(x, width) − PIN_ABOVE          // 绳上的 pin 顶
raw         = pointer_y − grab_dy − rest_y          // 指针想让 pin 下移多少
drop_target = min(26, rubber(clamp(raw, 0, +∞)))
rubber(v)   = if v ≤ 18 { v } else { 18 + 0.22 × (v − 18) }
```

- `drop` 用 `Spring::new(0.30, 0.55, …)`：略欠阻尼，松手会回弹一次，像绳子把夹子拉回去。
- 只向下：`raw < 0` 时目标为 0，pin 不会跑到绳上方（窗口顶端也没有空间）。
- 硬上限 26 DIP 是窗口几何决定的：拖拽中动作药丸隐藏，最坏的中心卡是 pin 顶 30.5 + 卡框 14 + 照片 112 + 下拉 26 = 182.5 < `PANEL_HEIGHT=210`；边缘卡更低。若将来在拖拽中显示药丸，26 会超出约 4.5 DIP，需要降到 21 或继续隐藏药丸。26 DIP 的偏离在视觉上仍是“挂在绳附近”。
- 指针下拉 40 DIP → 目标 22.8；下拉 80 DIP → 目标 26（封顶）。指针继续往下，pin 不动，形成“拉到头”的手感。

### 3.4 挤压：按真实穿透量（碰撞前），不按槽位差

设被拖卡 A 的当前索引为 `i`，实际宽 `w_a`（照片 150、便签 144），拖拽方向 `dir = sign(pointer_x − start_pointer_x)`（位移为 0 时用上一帧方向）。

对 `dir > 0` 的右侧邻居依次计算（`j = i+1, i+2, …`，最多两跳）：

```
prev_right = A.x + w_a/2
push_j     = clamp(prev_right − (slot_j − w_j/2), 0, 34)
target_j   = clamp(slot_j + push_j, 75, width_dip − 75)
prev_right = slot_j + push_j + w_j/2
```

- `push` 是**碰撞前的穿透量**：邻卡按这个量让开，所以让开之后 A 与 B 刚好贴住、不重叠。下文说的“28 DIP”都是指让开之前的穿透量。
- 第一张邻卡最多右移 **34 DIP**，第二张最多 **10 DIP**（因为 `push_2 = clamp(push_1 − 接触间隙, 0, 34)`，照片的接触间隙 = 174 − 150 = 24；便签 144 宽时为 30）。第三张恒为 0。
- 旋转：`squeeze_angle_j = −(push_j / 34) × 3.0°`（右推为负，左推为正，屏幕坐标顺时针为正）。最多 3°，不做额外弹簧——`x` 已经把它平滑了。
- 左侧对称：`dir < 0` 时对 `i−1, i−2` 用同一公式，符号相反；目标同样夹在 `[75, width−75]`。
- **只挤压拖拽方向前方**的邻居；身后的邻卡不参与。这条规则解决交换后的错误反推（见 §3.5）。

**为什么不用“槽位差 × 系数”**：交换后 A 的索引变了，`A.x − 新槽位` 会变成负数，模型会误判为“向左拖”，把刚落到左边的邻卡继续往左推。按“A 的右边缘 vs 邻卡左边缘”的真实重叠计算，交换后 A 在新槽位左侧、与右邻无接触，挤压自然归零。

### 3.5 交换：阈值、顺序与“向左的挤压力”

**触发**（右拖）：

```text
pen = A.right − B.left                   // 碰撞前的穿透量（B 尚未让开）
pen ≥ 28 DIP 连续 60 ms
且 now ≥ cooldown_until                  // 180 ms
且 反向武装：若上次交换方向与本次相反，
   A 必须越过 swap_anchor − 43（上次向右换过）
   或 swap_anchor + 43（上次向左换过）
```

`28 DIP` 是卡片宽的 18.7%，视觉上邻卡已经被压进去一截；对 150 宽的卡，它出现在 A 从自己的槽位向右拖了 **52 DIP**（0.30 个槽距）时。此刻 B 被推开 28 DIP，B 的右边缘压到 C 的左边缘（C 被连带 4 DIP）——正好对应“快挤压到右边的右边”

> 注意：`pen` 是让开之前的穿透量；B 的 `x.target = slot + pen` 之后，A 与 B 视觉上刚好贴住（间隙 0），不是仍然重叠 28。交换之后 B 落到 A 的旧槽位、A 仍在指针处，这时才是真正的“A 浮在 B 上方 98 DIP”（见下表）。

**交换步骤**（以右拖、A 索引 i、B 索引 i+1 为例）：

1. `self.cards.swap(i, i+1)`；A 的 `index` 变为 i+1，B 变为 i。
2. B 的 `x.target = slot(i)`；`B.x.velocity −= 260`（弹向左侧空槽）。
3. `B.hop_t = 0`，`B.hop_dir = −1`（0.20 s 内抬升 16 DIP 的视觉弧线，越过 A 的上方）。
4. A 的目标不变（仍是指针），但 `A.x.velocity −= 160`：这就是用户要的“被操作的图受到向左的挤压力”。A 会短暂后坐/左倾，然后继续跟手。
5. `swap_anchor = A.x`；`swap_dir = +1`；`cooldown_until = now + 0.18`。
6. 下一帧重算挤压：A 在新槽位左侧，右侧下一张是 C，两者不接触，挤压为 0；B 在身后，不参与。

**交换瞬间的位置**（6 张、1920 DIP 的例子，槽位 525/699/873/1047/1221/1395）：

| 时刻 | A（被拖） | B（右邻） | C | 挤压量 / 实际间隙 |
|:--|--:|--:|--:|:--|
| 静止 | 699 | 873 | 1047 | 0 / 24 DIP 间隙 |
| 触发（拖 52） | 751 | 901（+28） | 1051（+4） | A→B 28，B→C 4；让开后均贴合 |
| 交换后 | 751（索引 2，槽 873） | 699（索引 1） | 1047 | A 浮在 B 上方，实际重叠 98 DIP |
| 松手落位 | 873 | 699 | 1047 | 0 / 24 DIP 间隙 |

“A 浮在 B 上方，98 DIP”是这套几何的必然结果：A 距离新槽位 122 DIP，而 B 已经落到 A 的旧槽位。**不要试图在交换瞬间把 A 吸到新槽位**——那会让卡片瞬移 122 DIP，和“1:1 跟手”直接冲突。交换后 A 继续跟随指针，松手时弹簧把它送到 873，重叠在 0.3 s 内消失。

**反向交换的滞回**：交换后 A 与 B 仍然重叠 98 DIP；如果只看重叠，指针稍微往回抖就会立刻换回去。规则是：反向交换必须让 A 越过 `swap_anchor` 43 DIP（0.25 槽距）才重新武装——上次向右换过就要求 `A.x ≤ swap_anchor − 43`，上次向左换过就要求 `A.x ≥ swap_anchor + 43`——再满足 28 DIP 穿透 + 60 ms。`43 DIP` 是“明确往回拖”的下限，不是防抖系数。

**连续换位**：交换后 A 的索引 +1，右侧下一张变成 C；继续右拖到与 C 重叠 28 DIP，再交换。每交换一次索引 +1，最多到行尾；行尾没有下一张，不交换。同一张卡在一次拖拽里可以换多格，但每格只换一次（索引单调，天然不会重复）。

**左拖**完全镜像：`dir < 0`，与左邻重叠 ≥28 DIP、60 ms、越过 `swap_anchor + 43`，`cards.swap(i, i−1)`；左邻获得 `+260`，A 获得 `+160`。

### 3.6 松手与惯性

- `EndGrab`：`x.target = slot(index)`，`drop.target = 0`，`press.target = 1.0`，pin 视觉回落；保留 `x.velocity`，但上限 600 DIP/s（防止甩出屏幕）。
- `x` 的松手参数切到 `0.38 / 0.72`（欠阻尼，允许一次约 4% 的过冲），落位后自动回到 `ROW_RESPONSE/ROW_DAMPING`。
- 倾斜：`lean.target = clamp(−x.velocity × 0.006, −5°, +5°)`，`lean` 用 `0.22 / 0.70`。800 DIP/s 对应 4.8°；松手后速度衰减，倾斜回 0，看起来像卡片被“甩”了一下。
- 如果顺序变过，松手后的第一个 UI tick 读取 `snapshot.order`，重排 `line.items` 并写 `pegged.txt`；没变则不写。

### 3.7 参数总表

| 参数 | 值 | 单位 | 理由 |
|:--|--:|:--|:--|
| `GRAB_START` | 4.0 | DIP | 与现有 `DRAG_DIP` 一致，避免单击误拖 |
| `POINTER_MAX_STEP` | 120 | DIP/帧 | 防远程桌面瞬移；正常 120 Hz 拖动远小于此 |
| `FOLLOW_RESPONSE` | 0.075 | s | 跟手但不硬绑；快速拖动约 20 DIP 滞后 |
| `FOLLOW_DAMPING` | 0.85 | — | 几乎无过冲，拖拽要可预测 |
| `RELEASE_RESPONSE` | 0.38 | s | 落位有惯性，不拖沓 |
| `RELEASE_DAMPING` | 0.72 | — | 一次轻微过冲 |
| `RELEASE_SPEED_CAP` | 600 | DIP/s | 防甩出窗口 |
| `Y_RESPONSE` | 0.30 | s | 绳子的软弹 |
| `Y_DAMPING` | 0.55 | — | 松手回弹一次 |
| `DROP_MAX` | 26 | DIP | 210 DIP 窗口内不裁切 |
| `DROP_KNEE` | 18 | DIP | 超过后开始“拉不动” |
| `RUBBER` | 0.22 | — | 18→26 的压缩比 |
| `PUSH_MAX` | 34 | DIP | 邻卡位移上限；仍保留 10 DIP 的次级挤压 |
| `PUSH_ROT` | 3.0 | ° | 34 DIP 时的倾斜，上限 3° |
| `SWAP_OVERLAP` | 28 | DIP | 邻卡被压入 18.7% 卡宽，肉眼明确 |
| `SWAP_DWELL` | 60 | ms | 防高速掠过时误交换 |
| `SWAP_COOLDOWN` | 180 | ms | 一次交换至少 0.18 s，防连击 |
| `REVERSE_ARM` | 43 | DIP | 0.25 槽距；反向交换的滞回 |
| `NEIGHBOR_POP` | 260 | DIP/s | 邻卡弹向左的初速 |
| `A_RECOIL` | 160 | DIP/s | 被拖卡的左向后坐 |
| `HOP_SECONDS` | 0.20 | s | 邻卡越过 A 的时长 |
| `HOP_LIFT` | 16 | DIP | 视觉抬升，不参与命中 |
| `LEAN_GAIN` | 0.006 | °/(DIP/s) | 800 DIP/s → 4.8° |
| `LEAN_MAX` | 5 | ° | 不破坏挂绳的垂直感 |
| `PIN_SCALE_HOVER` | 1.12 | — | “微微放大” |
| `PIN_SCALE_PRESS` | 0.94 | — | 按下的压缩反馈 |
| `PIN_SCALE_DRAG` | 1.08 | — | 抓住后略小于 hover，避免遮挡邻卡 |
| `PIN_GLOW_HOVER` | 0.22 | alpha | 暖白光晕 |
| `PIN_GLOW_PRESS` | 0.26 | alpha | 按下更亮 |
| `PIN_GLOW_DRAG` | 0.20 | alpha | 拖拽时仍可见 |
| `PIN_RESPONSE` | 0.16 | s | 四态切换不拖泥带水 |
| `PIN_DAMPING` | 0.85 | — | 不晃 |
| `PIN_HIT_W × H` | 20 × 34 | DIP | 9×26 的实体太小；向四周扩 4–5.5 DIP |
| `PIN_GLOW_RADIUS` | 12 | DIP | 光晕半径 |
| `PIN_GLOW_SIGMA` | 3.0 | DIP | 烘焙模糊的 σ |
| `TICK_MS` | 33 | ms | 不变；只用于 hover/命中，不用于拖拽位置 |

### 3.8 稳定性与可终止性

- `Spring::step` 已经按 `ceil(dt×240)` 分子步，60/120 Hz 都是帧率无关的；不要为拖拽另写一套积分。
- `dt` 上限沿用现有（不因拖拽改变）；每帧最多一次交换、每帧指针位移上限 120 DIP。
- **不能睡**：`next_wake_in` 在 `grab.is_some()` 时必须返回 `Some(0.0)`；`hop_t >= 0`、`pin` 弹簧未收敛、`lean` 未收敛也同样不能返回 `None`。
- **NaN 防护**：`pointer` 只在 `is_finite()` 时写入；`width_dip` 为 0（窗口初始化）时忽略拖拽；`x.target` 的 clamp 保证不会出现 `NaN`。
- **交换乒乓**：60 ms dwell + 180 ms cooldown + 43 DIP 反向滞回，三重；手指/鼠标抖动不可能同时满足。
- **永不收敛的循环**：`push` 目标每帧由几何计算，A 一旦离开接触区就归零；邻卡的 `x` 弹簧收敛后 `settled()` 为真，帧循环可以休眠。
- **渲染线程与 UI 线程**：拖拽状态只存在于 Scene（渲染线程）；UI 只持有 `pin_press` 的输入所有权。两边都可以独立结束抓取，`EndGrab` 幂等。

## 4. 视觉规格

### 4.1 pin 四态

| 态 | 缩放（绕 pin 中心 75,13） | 光晕 | 金属 | 过渡 |
|:--|--:|:--|:--|:--|
| idle | 1.00 | 无 | 现有 `metal` 渐变（`src/render/mod.rs:304-345`） | — |
| hover | 1.12 | r=12 DIP，σ=3.0，alpha 0.22，暖白 `#FFF4D6` | `metal_hot`（各档 +18，clamp 255） | 0.16 s / ζ0.85 |
| pressed | 0.94 | alpha 0.26 | `metal_hot` | 0.12 s / ζ1.0 |
| drag | 1.08 | alpha 0.20 | `metal_hot` | 0.22 s / ζ0.85 |

- 缩放绕 `(75, 13)`，不是卡片的 `(75, 14)` 压点；两者差 1 DIP，但 pin 自己的中心更自然。pin 的 1.12 是局部缩放，卡片 hover 的 1.035 会叠加（净 1.16）；若觉得大，把 hover 目标降到 1.10，不要去掉卡片的 hover 缩放。
- 光晕顶部会被窗口上边缘裁掉（最外侧的卡 pin 顶在 y≈0.5），这是 210 DIP 窗口的物理限制；如果观感不够，把光晕中心下移 2 DIP，不要扩窗。
- 光晕画在 pin 阴影和本体之前：`glow → pin_shadow → body → slot`。光晕顶部会被窗口上边缘裁掉（pin 顶在 y≈0.5），这是 210 DIP 窗口的物理限制；如果观感不够，把光晕中心下移 2 DIP，不要扩窗。
- 像素对齐：`left = round(left × scale) / scale`，宽度 `round(9 × scale)`；96 DPI 的 70.5 DIP 半像素在 200% 下变成整数 141 px。pin 本体、slot、光晕都按同一 snap，避免 1 px 抖动。
- `metal_hot` 是第二个线性渐变笔刷，启动和 DPI 变化时创建；过渡时用 `push_opacity(t)` 叠加在 `metal` 上，不重建笔刷。

### 4.2 光晕烘焙（禁止每帧 blur）

- 复用 `create_blurred_rect`（`src/render/mod.rs:1230`），给它加 `tint: [u8; 3]`（现有阴影传 `[0,0,0]`），把 `src/render/mod.rs:1276-1286` 的硬编码黑改成预乘的 tint。
- 光晕纹理：33 × 50 DIP（9×26 + 12×2），σ=3.0，暖白；在 `pin_shadow` 的同一个生命周期里烘焙（`src/render/mod.rs:457` 创建、`:589` DPI 变化重建）。200% 下 66 × 100 px BGRA ≈ 26 KB。
- `draw_pin` 每帧只多一次 `DrawBitmap`（alpha 由 `pin_glow` 弹簧值调制）；**零 per-frame blur、零 per-frame 分配**。
- 阴影仍是现有 9-patch 烘焙，不动；光晕不是阴影，不参与 `draw_shadow`。

### 4.3 被拖卡

- z-order：主循环跳过被拖卡，循环结束后单独画一次；有 hop 的邻卡再画在其上。逻辑顺序仍是 `cards` 的 Vec 顺序，只有绘制顺序变。
- 阴影用 hover 烘焙、opacity 0.30；不透明度 1.0（BEHAVIOR §5.2 的 0.45 只保留给 OLE 拖出，不能复用到这里）。
- 抓取点不吸附：`grab_dx/dy` 保留按下时的手感；卡片不会突然把 pin 中心对齐到指针。
- 倾斜：`lean` 直接加进 `angle = swing + tilt + lean + squeeze_angle`；被拖卡的 `squeeze_angle=0`。
- 卡片本体不缩（`press.target=1.0`）；只有 pin 放大。这样“抓住夹子把画拎起来”的语义清楚，而“卡片被点击缩小”不会和拖拽混淆。
- 动作药丸、预览、cross 都不显示/不触发；被拖卡仍可被 hit-test 到（用于 release 后的 hover），但 `hovered=false`。

### 4.4 邻卡挤压视觉

- 位移：`x.target = slot + push`（最多 34 DIP）；倾斜：`−(push/34)×3.0°`；方向由 `push` 的符号决定。
- 挤压角不加额外弹簧：`x` 弹簧（0.28 / 0.75）已经把 push 平滑了；再叠一个弹簧会让旋转比位移晚，看起来“身体和脚分开”。
- 交换时：邻卡 `x.velocity −260`，同时 `hop_t` 从 0 到 1，`hop_y = −sin(πt)×16`；hop 是纯视觉偏移，不进入 `HitCard`，不影响命中。hop 结束（0.20 s）后 `hop_t=-1`。
- hop 期间邻卡画在被拖卡之上，形成“从上面越过去”的读感；hop 结束后回到正常 z-order。
- 被拖卡的左向后坐由 `x.velocity −160` 产生，倾斜由 `lean` 跟着速度走；**不额外做一条向左的位移动画**，否则会和指针位置打架。

### 4.5 光标与声音

- hover：`IDC_HAND`（复用 `src/window/overlay.rs:243` 的 `CursorState.hand`）。
- drag：`IDC_SIZEWE`（`src/window/overlay.rs:45` 的 `AtomicBool` 改成 3 态：arrow / hand / sizewe；`:592` 分支）。拖拽被约束在水平绳轴上，`IDC_SIZEALL` 会谎报纵向自由；`IDC_SIZEWE` 与 2.5.0 的缩放光标一致。
- 声音：不新增 cue；hover/press/swap 全部静音（120 Hz 下会连响成噪音）。仅当松手后顺序确实变化，在落位帧播一次既有 `Cue::Tink`（`src/sound.rs`），受 `soundOff` 控制。若用户不喜欢，删掉这一行即可。

## 5. 代码落点（基线 `bc30655`）

| 文件:行 | 现状 | 2.6.0 改动 |
|:--|:--|:--|
| `src/layout.rs:84` | `rope_y` | 增加 `PIN_HIT_W/H`、`pin_hit_rect()`、`push_for()`、`rubber_drop()` 纯函数 |
| `src/layout.rs:93` | `slot_x` | 交换后槽位按新索引计算；不改公式 |
| `src/layout.rs:133` | `card_size` | 挤压用实际卡宽（照片 150 / 便签 144） |
| `src/spring.rs:19` | `step` | 不改；继续 1/240 子步 |
| `src/scene/mod.rs:92` | `row_targets` | 增删/落位时使用；拖拽中由 grab 每帧设目标 |
| `src/scene/mod.rs:411` | `next_wake_in` | **必须修**：`grab.is_some()` 时返回 `Some(0.0)`，不能沿用 `dragging` 的 `None` |
| `src/scene/mod.rs:639` | `relayout_cards` | 松手/增删后按新顺序重设目标；**交换时不要调用**，否则 A 会被拉向槽位而不是指针 |
| `src/scene/mod.rs:705` | `update` | 加 grab、squeeze、swap、hop、lean、pin 视觉；gust 条件加 `grab.is_none()` |
| `src/scene/mod.rs:866` | `publish` | `HitCard.pin_ready/dragged`；`FrameSnapshot.order/grabbing`；order 复用 Vec 容量 |
| `src/scene/paint.rs:14` | `draw` | 主循环跳过被拖卡；结束后先画被拖卡，再画 hop 卡 |
| `src/scene/paint.rs:43` | 卡片循环 | 传 `pin_scale/pin_glow/hot/lean/hop`；被拖卡 `hovered=false` |
| `src/render/mod.rs:304` | `Palette` | 加 `metal_hot` 线性渐变 |
| `src/render/mod.rs:849` | `draw_pin` | 新签名 `(left, top, scale, glow, hot)`；画光晕、缩放、hot 叠加 |
| `src/render/mod.rs:839` | `draw_shadow` | 不改 |
| `src/render/mod.rs:1230` | `create_blurred_rect` | 加 tint 参数；烘焙 `pin_glow` |
| `src/render/mod.rs:457/:589` | `pin_shadow` | 同生命周期烘焙光晕；DPI 变化重建 |
| `src/types.rs:320` | `card_at_impl` | 先查 `hit_pin`，再查 frame/actions；`HitCard::hit_pin` 反旋转后测 20×34 |
| `src/app/pointer.rs:52` | `on_left_down` | pin 命中在 band/swatch/action/hit 之前；设 `pin_press`、SetCapture、发 `Grab`；**不设 `state.pressed`** |
| `src/app/pointer.rs:100` | `hit_test_exact` | 不改；pin 走独立入口 |
| `src/app/pointer.rs:146` | `on_left_up` | 先处理 `pin_press`：发 `EndGrab`、释放 capture、直接 return |
| `src/app/pointer.rs:194` | `on_band_click` | 不能被 pin 按下触发；pin 检查在其前 |
| `src/app/pointer.rs:252` | `update_pointer` | busy 加 `pin_press`；抑制预览/长按/动作药丸；按钮抬起/换屏兜底 |
| `src/app/pointer.rs:434` | `update_click_through` | `pin_press.is_some()` → false |
| `src/app/cards.rs:213` | `save_pegged` | 松手后按 `snapshot.order` 重排 `line.items`，变化才写 |
| `src/app/mod.rs:80` | `TICK_MS=33` | 不改；拖拽位置不走 tick |
| `src/app/mod.rs:119` | `DRAG_DIP=4.0` | pin 复用同一阈值 |
| `src/app/mod.rs:886` | `tick` | 松手后的下一 tick 检查 order；兜底结束 grab |
| `src/app/mod.rs:886` 的 `update_preview` 路径 | 0.35 s 大图预览 | pin hover/press 时跳过并重置计时 |
| `src/app/mod.rs:3446` | `start_drag` | 不改；pin 路径永远不会设 `state.pressed`，所以不会进 OLE |
| `src/window/overlay.rs:45` | `CursorState` | 3 态（arrow/hand/sizewe） |
| `src/window/overlay.rs:243` | `set_hand_cursor` | 加 sizewe 分支 |
| `src/window/overlay.rs:341` | 渲染循环 | `grabbing()` 时采样指针 + `GetAsyncKeyState(VK_LBUTTON)`；`set_pointer` 后 `update` |
| `src/window/overlay.rs:592` | 光标选择 | drag → `IDC_SIZEWE` |
| `src/settings.rs:273` | `write_pegged` | 不改；由新的松手保存路径调用 |
| `src/sound.rs` | 三个 cue | 复用 `Tink`；不新增 |
| `src/app/dev.rs:75-136` | demo 解析 | 加 `--pin-drag-demo` 与 `PEGLINE_PIN_DRAG_*` |

**行号会随实现漂移**；这张表的用途是定位“改哪里”，不是精确补丁。

新增 API（最小面）：

| 位置 | 新增 |
|:--|:--|
| `src/scene/mod.rs` | `Command::Grab { id, pointer }`、`Command::EndGrab`、`Command::PinHover(Option<CardId>)`；`Scene::grab`、`Scene::set_pointer(x, y, now)`、`Scene::grabbing()`、`Scene::cancel_grab(now)` |
| `src/scene/mod.rs` `Card` | `push: f32`、`lean: Spring`、`hop_t: f32`、`pin_scale: Spring`、`pin_glow: Spring`、`pin_hot: f32` |
| `src/types.rs` | `HitCard::hit_pin(x, y)`、`HitCard.dragged`、`HitCard.pin_ready`；`FrameSnapshot.order: Vec<CardId>`、`FrameSnapshot.grabbing: bool`（复用 Vec 容量） |
| `src/render/mod.rs` | `draw_pin(left, top, scale, glow, hot)`；`pin_glow` 烘焙位图；`metal_hot` 笔刷 |
| `src/win32.rs` | `cursor_client_dip(hwnd, scale)`、`left_button_down()` |

## 6. 输入与优先级

### 6.1 pin 命中

- 命中框：卡片局部 `(65, −4, 20, 34)`（中心 75，pin 顶 0；向左右各扩 5.5、向上 4、向下 8）。反旋转用现有 `HitCard::in_card_frame`，和卡片一起倾斜。
- 命中顺序：`hit_pin` → `on_band_click` → `swatch_at` → `action_at` → `hit_test_exact` → cross。pin 在顶部，不会和药丸/色板重叠；和 cross 也不重叠（cross 在 frame 左上角）。
- pin 只在 `arrived && !restore_pending && !flying && !rehang` 时可抓；启动恢复期间和 demo 模式下按 §7 的规则禁用。
- 命中框比视觉大是故意的：9×26 的实体在 200% DPI 下也只有 18×52 px，鼠标很难点中；20×34 的重叠区只覆盖卡片顶部 16 DIP、横向 20 DIP，不影响正文点击。

### 6.2 与现有手势的优先级

| 手势 | 优先级 | 规则 |
|:--|:--|:--|
| 编辑器/备注/标注窗口打开 | 最高 | 现有 `on_left_down` 的提前 return 不变，pin 不可用 |
| pin 按下/拖拽 | 高 | 在 band、色板、动作、卡片体、cross 之前 |
| OLE 拖出（卡片体 >4 DIP） | 中 | 只在没有 `pin_press` 时发生；pin 路径不设 `state.pressed` |
| 双击打开 / 单击复制 | 中 | pin 上不触发；pin 抬起直接 return |
| 长按 0.45 s 编辑 | 中 | `pin_press` 存在时跳过 `LONG_PRESS` |
| 0.35 s 大图预览 | 低 | pin hover/press 时抑制；离开 pin 后重新计时 |
| 右键菜单 | 低 | 拖拽中忽略；松手后正常 |
| 顶部带点击收起 | 最低 | pin 命中时完全不进入 `on_band_click` |

### 6.3 渲染线程采样

```
// overlay 渲染循环，仅 grabbing() 时执行
if scene.grabbing() {
    if let Some((x, y)) = win32::cursor_client_dip(hwnd, scale) {
        scene.set_pointer(x, y, now);
    }
    if !win32::left_button_down() {           // GetAsyncKeyState(VK_LBUTTON) 高位
        scene.cancel_grab(now);                // 场景自己落位，UI 下一 tick 清理
    }
}
scene.update(dt, now);
```

- `cursor_client_dip` 是新的小助手：`GetCursorPos` → `ScreenToClient` → 除以 overlay scale；`ScreenToClient` 需要 `unsafe`，注释前置条件“hwnd 是本线程的有效窗口”。
- `left_button_down` 是新的小助手：`GetAsyncKeyState(VK_LBUTTON) & 0x8000 != 0`；`unsafe` 注释“只读键态，无窗口前置条件”。
- 不要在这条路径上分配；不要通过命令通道发坐标（每帧一次 `send` 会排队，33 ms 的 UI 消费会积压）。

### 6.4 释放兜底

三个来源，任意一个都能结束：

1. `WH_MOUSE_LL` 钩子排队的 `WM_LBUTTONUP` → UI `on_left_up` → `EndGrab`。
2. `SetCapture` 后窗口收到的 `WM_LBUTTONUP` → 同一个 `on_left_up`（钩子和窗口消息可能都到，`pin_press.take()` 保证只处理一次）。
3. 渲染线程的 `GetAsyncKeyState` 轮询：鼠标在窗口外、钩子被 UAC/锁屏挡住时，仍能看到按钮已抬起；`scene.cancel_grab` 立即落位。

`EndGrab` 必须幂等：UI 发一次、场景自己取消一次，不能重复写盘或重复播声音。

### 6.5 click-through / 自动收起

- `update_click_through` 的 `pass` 条件加 `state.pin_press.is_none()`；拖拽中即使 hover 丢失也不能变回穿透。
- `update_pointer` 的“busy”集合加 `pin_press`：指针离开带区、计时到 `RETRACT_DELAY` 也不收起。
- 拖拽中不显示大图预览、不显示动作药丸、不进入长按编辑；这些都在 UI 侧判断，不要依赖 Scene 的 hover。
- 全屏应用/锁屏：先 `EndGrab`，再走现有的 dismiss 逻辑；不要把卡片留在半空。

## 7. 边界情况登记表

标记：**必须修** = 不做就会验收失败或数据错误；**建议** = 首版可延后但要记录；**接受** = 明确不处理，写进文档。

### 7.1 输入

| # | 触发 | 期望 | 标记 |
|:--|:--|:--|:--|
| 1 | 点 pin 时先命中顶部带 | 线带不收起，进入 pin press | 必须修 |
| 2 | pin 命中框 9×26 且不随卡片旋转 | 20×34，反旋转；200% 下可点 | 必须修 |
| 3 | pin 上按下 <4 DIP 后抬起 | 无复制/打开/编辑/收起 | 必须修 |
| 4 | pin 上拖 >4 DIP 进入 OLE `DoDragDrop` | 不进 OLE；pin 路径不设 `state.pressed` | 必须修 |
| 5 | pin 上长按 0.45 s | 不打开编辑器；pressed 视觉保持 | 必须修 |
| 6 | pin 上双击 | 不打开文件；两次 press 都不产生动作 | 必须修 |
| 7 | pin hover 0.35 s 后弹出大图预览 | 抑制预览；离开 pin 后重新计时 | 必须修 |
| 8 | 拖拽中指针离开带区、自动收起计时到 | 不收起（busy 含 pin_press） | 必须修 |
| 9 | 拖拽中 hover 丢失导致 click-through 翻 true | 保持 false（pin_press） | 必须修 |
| 10 | 在窗口外松开、钩子丢事件 | SetCapture + 渲染线程轮询兜底 | 必须修 |
| 11 | 拖拽中右键 | 忽略，不弹菜单、不抢 capture | 必须修 |
| 12 | 拖拽中换屏/改 DPI | 取消抓取 → 落位 → `place_on_screen` | 必须修 |
| 13 | 拖拽中全屏应用出现/锁屏 | 先取消抓取，再 dismiss；不悬空 | 必须修 |
| 14 | 线带还在 reveal（slide < −1）时点 pin | 不进入抓取；让带子先落定 | 建议 |
| 15 | 触控/触控板 | 4 DIP 阈值偏敏感；首版只保证鼠标 | 接受（文档） |
| 16 | 键盘用户 | pin 拖拽是鼠标手势，无键盘等价 | 接受（文档） |

### 7.2 几何与物理

| # | 触发 | 期望 | 标记 |
|:--|:--|:--|:--|
| 17 | 空线 / 1 张卡 | 不进入抓取；pin hover 仍可显示 | 必须修（测试） |
| 18 | 2 张卡，交换后 A 成为最后一张 | 只交换一次；继续右拖不再换；松手落位 | 必须修（测试） |
| 19 | 12 张满容量 | 槽位可能超出窗口；x 夹在 `[75,width−75]`，邻居同样夹紧 | 必须修（测试） |
| 20 | 拖到最右/最左边缘 | 卡片不越界；邻卡目标也夹紧；不交换 | 必须修 |
| 21 | 向下拖 80 DIP | drop 封顶 26；卡片/药丸仍在 210 窗口内 | 必须修（测试） |
| 22 | pin 顶在 y≈0.5，向上拖 | drop 目标 0；不会跑到绳上方 | 接受（几何） |
| 23 | 一次拖拽连换两格 | 每次索引 +1；cooldown + 60 ms；每格只换一次 | 必须修（测试） |
| 24 | 交换后指针轻微回抖 | 不立刻换回；必须越过 anchor∓43 DIP | 必须修（测试） |
| 25 | 远程桌面/高速瞬移 | 指针每帧限 120 DIP；最多一次交换 | 必须修 |
| 26 | 窗口窄到只剩 3 张容量 | 挤压目标夹紧；不出现负宽度/NaN | 必须修（测试） |
| 27 | 便签（144 宽）与照片混排 | 用实际卡宽算重叠；阈值仍 28 DIP | 建议（测试） |
| 28 | 拖拽中阵风 `Gust` | 暂停阵风；`grab.is_none()` 才允许 | 必须修 |
| 29 | 拖拽中 copied 药丸还在 | 进入抓取时清掉 `copied` | 建议 |
| 30 | 120 Hz 显示器 | 弹簧按真实 dt；指针逐帧采样；不直接赋值 | 必须修（实现）/待硬件 |
| 31 | 减少动效设置 | 缩短/关闭 hop 与倾斜，保留功能 | 建议 |
| 32 | 拖拽中新建截图/便签到达 | 延迟 AddCard 到松手后，避免槽位重排/容量驱逐 | 必须修 |
| 33 | 拖拽中 `prune_missing` 要删被拖卡 | 跳过被拖 id；松手后再判/删 | 必须修 |
| 34 | 拖拽中卡片变为图片/改颜色 | 现有 UI 路径被编辑器阻塞；若发生则忽略 | 接受 |
| 35 | 拖拽中托盘退出/第二实例/关机 | 先 `EndGrab`；场景随窗口销毁；不写半截 order | 必须修（关闭路径） |

### 7.3 持久化与回归

| # | 触发 | 期望 | 标记 |
|:--|:--|:--|:--|
| 36 | 松手后顺序变了 | 下一个 UI tick 重排 `line.items` 并写 `pegged.txt` | 必须修 |
| 37 | 顺序没变 | 不写盘、不播声音 | 必须修 |
| 38 | 启动恢复中（`restore_pending>0`） | pin 不可抓；恢复完成后再启用 | 建议 |
| 39 | 崩溃在松手写盘前 | 最坏丢失本次顺序；不写坏文件（原子写沿用现有） | 接受 |
| 40 | 从卡片体拖出到资源管理器 | OLE 语义完全不变；pin 路径不干扰 | 必须修（回归） |
| 41 | 单击/双击/长按/右键/预览 | 全部保持 2.5.1 行为 | 必须修（回归） |
| 42 | 200% DPI 截图 | pin 四态、光晕、hop、挤压角都正确 | 必须修 |
| 43 | 96 DPI | 70.5 DIP 半像素 snap；无 1 px 抖动 | 建议（本机无 96 屏） |

**最可能被验收打回的 5 个坑**：

1. 点 pin 收起了线带（输入顺序没改）。
2. pin 命中框太小/不随卡片旋转，200% 下点不中。
3. 4 DIP 后仍进了 OLE，卡片被拖到资源管理器而不是换位。
4. 拖拽中 hover 丢失，click-through 翻 true，鼠标事件漏到桌面。
5. 交换后没有写 `pegged.txt`，重启顺序复原。

## 8. 验收计划

### 8.1 单元测试（纯逻辑，`cargo test`）

- `push_for(overlap)`：0 / 24 / 34 / 负值；`push_2 = clamp(push_1 − 24, 0, 34)` 的 0 / 28 / 34 输入。
- `rubber_drop`：0 / 18 / 40 / 80 / 负值；上限 26。
- 交换判定：28 DIP + 60 ms 触发；27 DIP 不触发；cooldown 内不触发；反向 43 DIP 内不触发。
- 交换序列：3 张卡右拖一次 → `[B,A,C]`；再右拖 → `[B,C,A]`；左拖镜像；2 张卡只换一次。
- 命中：pin 在 0°/±10°/±16° 下的 20×34 框；框外 1 DIP 不命中；卡片体点击不误判为 pin。
- 夹紧：x 在 `[75,width−75]`；drop 在 `[0,26]`；窄窗口不产生负槽位/NaN。
- 槽位/顺序：交换后 `row_targets` 与 `cards` 索引一致；`order` 等于 Vec 顺序。

### 8.2 可脚本复现的 demo

沿用 `src/app/dev.rs:75-136` 的开关模式：

- `--pin-drag-demo`：启动后布置 6 张卡，按脚本路径拖拽。
- `PEGLINE_PIN_DRAG_SCENARIO=hover|press|swap|release|left|edge`：直接停在对应姿态，便于截图。
- `PEGLINE_PIN_DRAG_POINTER=x,y`：合成指针位置（窗口 DIP），渲染线程用它代替 `GetCursorPos`。
- `PEGLINE_PIN_DRAG_SPEED=<DIP/s>`：匀速推进合成指针。
- `PEGLINE_PIN_ORDER=n`：只保留 n 张卡，测 1/2/12 张。
- 其他 demo（`--note-resize-demo` 等）禁用 pin 抓取，避免互相干扰。

### 8.3 截图清单

| 文件 | 内容 |
|:--|:--|
| `pin-idle-200.png` | 静止 pin，作为基线 |
| `pin-hover-200.png` | 1.12 + 光晕 0.22 + metal_hot |
| `pin-press-200.png` | 0.94 + 光晕 0.26，卡片未缩 |
| `pin-drag-200.png` | 被拖卡置顶、邻卡右移 28 + 3° |
| `pin-swap-200.png` | 交换后 hop 中，邻卡在上方越过 |
| `pin-drop-200.png` | 指针下拉 80 DIP，pin 离绳 26 DIP |
| `pin-release-200.png` | 松手落位，重叠消失 |

96 DPI 本机只有 200% 显示器：**不要伪造**；96 DPI 由单测 + 第二台机器补测，写进 `docs/VISUAL-TESTS.md`。

### 8.4 probe 指标

- 新增 `--mode=pins --seconds=10`（或复用 `--mode=line` + `PEGLINE_PIN_DRAG_DEMO=1`）：跑一段包含 hover → 按下 → 交换 → 松手的循环。
- 阈值：fps ≥59.5（60 Hz）；draw p50 ≤1.0 ms（当前基线 0.80）、p95 ≤1.5 ms；private 增量 ≤1.5 MB（烘焙光晕 + 字段）。
- 120 Hz：本机无硬件，**不填数字**；实现后在有 120 Hz 的机器上补测，记录到 `PLAN.md` §14。

### 8.5 手工验收

1. 悬停 pin：1.12、暖白光晕、无大图预览、线带不收起。
2. 按下不动：0.94、0.26；0.45 s 不打开编辑器；抬起无动作。
3. 右拖 30 DIP：右邻右移 + 轻微倾斜；C 几乎不动。
4. 右拖 52 DIP：右邻被压到 C，60 ms 后弹向左，hop 越过后落位；A 有一下左向后坐。
5. 继续右拖换第二格：顺序 B、C、A；每格只换一次。
6. 松手：A 弹到新槽位；`pegged.txt` 顺序变化；重启后保持。
7. 下拉 80 DIP：pin 最多离绳 26 DIP；松手回弹一次。
8. 拖到窗口外再松手：卡片落回最后一个槽位，不悬空。
9. 拖拽中右键/指针离开带区：菜单不弹、线带不收起。
10. 从卡片体拖到资源管理器：OLE ghost 正常，顺序不变。

## 9. 文档更新

| 文档 | 改动 |
|:--|:--|
| `docs/BEHAVIOR.md` | 新增 §5.5「夹子交互与拖拽排序」；§3.1 加 `PIN_HIT`/挤压常量；§3.2 加参数；§3.3 加光晕/hot 金属；§5.2 的 0.45 alpha 明确只用于 OLE；§8 手势优先级 |
| `docs/PARITY.md` | 7.22–7.26：pin 四态、拖动跟手、挤压、交换、Y 弹簧、持久化 |
| `docs/DECISIONS.md` | **D14**：手写指针重排，不复用 OLE；理由：`DoDragDrop` 的模态泵会吞掉绳上的实时反馈，且 OLE 语义是“拖出文件”，不是“换位” |
| `docs/VISUAL-TESTS.md` | §3.17 pin 四态/交换/hop 截图；96 DPI 待补 |
| `PLAN.md` | §14 实现后的实测表（帧率、draw、内存；120 Hz 留空） |
| `docs/RELEASE-2.6.0.md` | 发布说明、已知限制（触控、键盘、120 Hz 未测） |
| `CHANGELOG.md` | 2.6.0 条目 |
| `AGENTS.md` | 发布后更新状态段；设计阶段不动 |

## 10. 里程碑与工时

| 里程碑 | 内容 | 估时 |
|:--|:--|--:|
| M1 输入骨架 | pin 命中、`pin_press`、SetCapture、click-through、busy、预览抑制、延后 AddCard | 1.0 d |
| M2 物理与交换 | grab、渲染线程采样、X 跟随、Y 橡皮筋、挤压、交换、hop、反坐、松手 | 1.5 d |
| M3 视觉 | 光晕烘焙、metal_hot、z-order、lean、光标、声音 | 0.75 d |
| M4 持久化与测试 | 顺序重排/写盘、demo 开关、单测 | 0.75 d |
| M5 文档与验收 | BEHAVIOR/PARITY/DECISIONS/VISUAL-TESTS/RELEASE、probe、截图 | 0.5 d |
| **合计** | | **4.5 d** |
| 缓冲 | 输入/几何的返工 | 0.5 d |

## 11. 被否决的方案

| 方案 | 否决理由 |
|:--|:--|
| 复用 OLE `DoDragDrop` 做重排 | 模态泵阻塞 UI/渲染；没有实时挤压；语义是拖出文件，不是换位 |
| 复用 `Command::Dragging` 表示抓取 | 它会让 `next_wake_in` 返回 `None`（`src/scene/mod.rs:411`），overlay 直接睡死；还会关掉 hover/gust |
| 交换瞬间把 A 吸到新槽位 | A 会瞬移 122 DIP，和 1:1 跟手冲突；用户会看到“卡片跳走” |
| 交换时把邻卡放到 `A.x − 174` | 行首/行尾会把整行推出屏幕；行的固定槽位语义丢失 |
| 只在松手时交换（拖拽中不换） | 用户明确要“挤到位置就弹过去”；保留为重叠观感不可接受时的 fallback |
| 每帧 blur 画光晕 | 违反 AGENTS.md“阴影只烘焙一次”；掉帧 |
| 33 ms UI tick 采样指针 | 120 Hz 下明显滞后；2.4.2 已证明输入队列会积压 |
| 全局 Esc 钩子取消拖拽 | 为一个罕见取消新增键盘钩子；左键抬起已足够 |
| 扩高 210 DIP 窗口跟随下拉 | 透明窗口变高 = 更多 GPU/合成开销，且会遮挡下方内容 |
| 新增 swap 音效 | 120 Hz 下容易连响；复用 `Tink` 足够，且可一键删 |

## 12. 待用户拍板

1. **交换后的重叠**：推荐“被拖卡浮在邻卡上方 + 邻卡 hop 左移 + 松手落位”（本文方案）。备选：只在松手时交换（拖拽中只挤压不换位，视觉最干净，但少了“弹过去”的瞬间）。
2. **“被拖图受到向左挤压力”**：推荐交换瞬间给 A 一个 −160 DIP/s 的反向冲量（表现为后坐 + 左倾）。备选：不做反向冲量，只让 A 落回新槽位。
3. **pin 单击**：推荐什么都不做（pin 只用于拖拽）。备选：单击 pin 打开大图预览/便签编辑器。
4. **声音**：推荐顺序变化时播一次 `Tink`。备选：完全静音。

默认按推荐实现；如果第 1 条的重叠观感在真机上不可接受，再切到“松手交换”的 fallback，其余参数不变。

## 附录 A 团队结论与分歧

- **Lane A（物理）**：独立设计了状态机、跟随/松手参数、橡皮筋和交换滞回；正确指出“174 槽距 + 150 卡宽 → 交换必然重叠”，但提出的“邻卡落到 A.x−174”会把行推走，未采纳。
- **Lane B（集成）**：找到 `next_wake_in` 的睡眠 bug 和输入优先级冲突；提出渲染线程采样、`HitCard.pin_box`、`save_pegged` 重排；其“不新增字段、只改 Scene”的建议与视觉需求冲突，最终在 `Card` 上增加了 6 个轻量字段。
- **Lane C（边界）**：确认“点 pin 会收起线带”、命中框太小、OLE 抢拖拽、click-through 翻转、顺序不落盘五个高风险；其建议的 96 DIP 交换阈值偏高，按几何改成 28 DIP 重叠。
- **Lane D（视觉）**：给出四态数值、烘焙光晕、z-order、hop、光标和验收清单；其“push 上限 174 → 永不重叠”只对邻卡成立，对被拖卡与邻卡不成立，已在 §3.5 修正。
- **红队评审**：`reviewer` lane 因工具白名单（compress/acp）失败，未产出独立 critic 报告；上述分歧由主代理按代码事实裁决。四份 artifact 均完整保存在本轮会话中。

**一句话**：这不是“给卡片加个拖拽”，而是“在不破坏现有 8 种手势的前提下，给 pin 开一条独立的、渲染线程驱动的、可交换顺序的输入通道”。先修输入与睡眠，再谈弹簧。
