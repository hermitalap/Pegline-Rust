# Pegline 2.6.0 发布说明

发布日期：2026-11-13
产物：尚未打包；打包命令 `pwsh tools\package-release.ps1`，产物为
`dist/Pegline-2.6.0-win-x64.zip`（单文件 exe + README + LICENSE + CHANGELOG）。

`target\release\pegline.exe` 为 **1,920,000 B（1.92 MB）**，ProductVersion
`2.6.0.0`（2.5.1 的 1,879,552 B 增加 40,448 B：新的夹子命中/弹簧/挤压/纸片摆/柔性/
空气阻力/追踪代码与烘焙光晕位图）。

## 2.6.0 改变

**夹子可拖，线内顺序可重排。** 指针移到一张挂图的夹子上时，夹子放大到 1.12 倍、
加一层暖白光晕并提亮金属；按住左键即可拖动这张图：

- 水平方向 1:1 跟手，带弹簧的轻微滞后与松手后的惯性回槽；卡片不会离开屏幕，
  也不能拖出 210 DIP 的线带窗口。
- 向右拖时，右侧邻卡被“挤”着向右偏并微微旋转；挤到 28 DIP 的穿透阈值后，
  邻卡弹向左侧、与被拖图交换槽位。向左拖对称。
- 交换后邻卡落回被拖图原来的槽位，被拖图仍跟着指针，因此会短暂叠在邻卡上方，
  松手才归位；这个 98 DIP 的重叠是有意保留的手感。
- 纸片摆动：被拖卡和交换跳位卡像纸片一样摆动，而不是统一参数的铁块。每张卡按
  尺寸算有效摆长 `L_eff=(w²/12+h²/3)/(h/2)`（60–220 DIP），横卡、竖卡、方卡和
  便签周期不同；水平加速度驱动目标 `atan(a_x/g_eff)`（±6°），Y 轴下落通过
  `g_eff=clamp(35500-a_y, 0.25g, 1.75g)` 参与；线性阻尼 1.116 + 二次空气阻力 0.279
  （当前“更强空气”预设），正常拖拽/交换只过冲一次，不再像铁块那样来回 2–3 次。
  另叠加 ±1.2° 的柔性剪切，夹子和顶边
  不动、底边轻微滞后；交换小跳只有 6 DIP，不再向上弹起。
- 纸片兜风：每张卡的线性阻尼、二次阻力和风增益随高宽比增大（竖卡 1.116→1.550、
  0.279→0.589、风增益 ×0.7→×1.3）；压心 `r=h/2` 处的相对风扭矩
  `Q=wind_gain·|v_p|·(v_p·(cosθ,sinθ))·r` 直接加进摆角加速度，所以匀速拖动时
  卡片会稳定地略微向后倒，而不是永远竖直。
- Y 轴有回绳力：向下拉 0–18 DIP 是线性的，再往后只按 0.22 倍移动，最多 26 DIP；
  即使指针离开挂绳，夹子也始终在绳子附近。
- 夹子悬停/按住/拖拽分别是 1.12 / 0.94 / 1.08 倍，光晕 0.22 / 0.26 / 0.20；
  悬停时卡片本身不抬起、不出按钮行，也不会启动 0.35 s 预览。
- 单击夹子（移动 <4 DIP）是无操作：不复制、不打开、不收起线带；长按、双击和
  OLE 拖出都不会被触发。
- 交换后松手，顺序写入 `pegged.txt`；顺序真的变化才写、才播一次 `Tink`
  （受“静音”设置控制）。
- 光标：夹子悬停为手形，拖拽中为 `IDC_SIZEWE`。

## 实现

- `src/scene/mod.rs`：`Scene.grab` 与 `Command::Grab/EndGrab/PinHover`；X 跟手
  0.075/0.85，Y 橡胶 0.30/0.55，挤压 0.22/0.80，松手 0.46/0.80；交换用向量
  `swap`，不在交换时 `relayout_cards`；`Card.pendulum` 是自定义积分器
  （`src/scene/pendulum.rs`），线性 1.116 + 二次阻力 0.279，加速度限 ±6000、死区 300，
  `SWAP_HOP_DIP = 6.0`；`Card.flex` 用 0.12/0.55 驱动 ±1.2° 剪切。
  `PaperCoefficients` 按 `aspect=h/w` 推导每卡阻尼/阻力/风增益，`relative_wind` 把
  压心速度变成摆角加速度；`src/scene/trace.rs` 在 `PEGLINE_PAPER_TRACE` 下逐帧写 CSV。
- `tools/paper-trace.ps1`：200% DPI 的 `--demo` 夹子拖拽驱动，写 `target/paper-trace.csv`；
  `tools/paper-plot.py`：只用标准库，生成 `angle/target`、`omega`、`vx/vy` 图和
  峰值/过零/停稳指标（`target/paper-trace.html`）。
- `tools/paper-sim.html`：单文件 SVG 调参器，拖动夹子、滑杆、预设和角度/角速度曲线；
  “更强空气”预设的 0.2 高宽阻力增量 ×1.55 = 0.31 有效值。
- `src/window/overlay.rs` + `src/win32.rs`：渲染线程每帧 `GetCursorPos` +
  `ScreenToClient` 采样，`GetAsyncKeyState(VK_LBUTTON)` 兜底结束；`next_wake_in`
  在 grab 时返回 `Some(0.0)`。
- `src/app/pointer.rs`：夹子命中优先于顶边点击/色板/按钮/OLE；`begin_pin_press`
  发送 `Grab` 且不设 `state.pressed`；`finish_pin_press` 发送 `EndGrab` 并标记
  待同步顺序。
- `src/app/cards.rs`：`sync_pin_order` 在 `snapshot.grabbing == false` 后按
  `snapshot.order` 重排 `line.items`，变化才 `save_pegged()` 与 `Tink`。
- `src/render/mod.rs`：`create_blurred_rect` 增加 tint，启动/DPI 变化时烘一次
  `pin_glow`；`draw_pin` 支持缩放、光晕和 `metal_hot` 交叉淡化。

## 验证

200% DPI、6016×420 probe 窗口：

| 项目 | 基线 HEAD bc30655 | 2.6.0 |
|:--|--:|--:|
| fps | 60.10 / 60.06 | 59.88–60.16 |
| p50 帧时间 | 16.66 ms | 16.64–16.67 ms |
| draw p50 | 0.658 / 0.660 ms | 0.585–0.701 ms |
| draw p95 | — | 0.859–1.698 ms |
| private | 97.1–97.4 MB | 96.9–98.9 MB（空气阻力版 98.4） |

- 空气阻力版最终复测：60.16 fps、draw p50 0.585 / p95 0.915 ms、private 98.4 MB；
  夹子悬停时 draw p50 约 1.2 ms，仍远低于 8.3 ms 预算。
- 旧 0.45/0.05 模型的 `--demo` CSV 基线：峰值 5.50°、7 次过零（含小角度抖动）、817 ms 停稳，
  max |ω| 97.5°/s、max |vx|/|vy| 3444/311 DIP/s（仅历史参考；当前“更强空气”预设待手测）；
  `tools/paper-plot.py` 生成 `target/paper-trace.html`。
- `--demo` 三卡：夹子 (1614,87) 按下、拖 +200 px 后顺序 `[7,8,9] → [8,7,9]`，
  卡片移动 +351 px（174 DIP 一格）；hover/press/drag/swap 截图在
  `docs/visual/pin-*-200.png`（drag/swap 是带惯性摆动的帧）。
- 真实 6 卡：第一张便签夹子 (2138,87) 拖 +200 px 后，`pegged.txt` 第一行从便签
  变为 `屏幕截图 2026-11-10 201140.png`，便签第二行，其余顺序不变（测试后已恢复）。
- 255 个 lib 测试 + 1 个 main 测试通过（1 ignored），`clippy --all-targets -- -D warnings` 与
  `cargo fmt --check` 全绿。

## 已知限制

- 空气阻力仍是经验模型：只做每卡的摆角阻尼/二次阻力和压心相对风扭矩，不做平移阻力对
  x/drop 的反馈、升力/涡脱落/附加质量或逐顶点弯曲；`wind_gain` 不是 `0.5ρCdA`。
  调参用 `PEGLINE_PAPER_DAMPING/DRAG/WIND/ASPECT` 和 `docs/PAPER-TUNING.md` 的
  CSV/绘图流程，真实手感仍需人工验收。
- 120 Hz、第二显示器、睡眠/唤醒仍未在真实硬件上实测；100% DPI 的夹子命中/光晕
  也还没有截图（几何由 200% DPI 与单元测试覆盖）。
- 交换后被拖图与邻卡重叠约 98 DIP 是设计选择；松手后归位。若希望“交换即分开”，
  需要改为松手时才交换，会失去拖拽中的实时换位感。
- 夹子拖拽只重排线内顺序，不创建文件、不调用 `DoDragDrop`；把图拖到别的应用仍是
  原来的 OLE 拖出。

## 未改变

单击/双击/长按/右键/OLE 拖出的语义、复制格式、文件格式、`settings.ini` 键、
托盘/快捷键、便签与标注编辑器都没有变化。
