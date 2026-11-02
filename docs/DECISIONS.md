# 实现决策记录

`docs/BEHAVIOR.md` 是行为契约；这里只记录 **实现方式** 与原 WPF 版不同、
但用户可见行为相同的地方，以及被实测否决、没有进代码的优化。

## D1 便签位图用 GDI 绘制（`src/notes.rs`）

WPF 用 `RenderTargetBitmap` 把 `Grid` 画成 408×264 的便签。Rust 版不再有视觉树，
改为在一个 32 位顶向下 DIB 上先用像素渐变画纸面与折角、再用 GDI `DrawTextW`
画文字（`KaiTi` 优先，字体缺失时由 Windows 字体映射回退）。

- 只在新建便签、换色、外部编辑后重绘一次，不在帧循环里。
- 3 倍超采样、字体大小/边距/折角尺寸与 `Notes.cs` 一致。
- 画完后把 alpha 强制为 255：纸是不透明的，上传到 D2D 的预乘格式也不会出错。
- 没有引入 D2D 离屏目标，避免为了四张便签再建一套设备与交换链。

## D2 快捷键设置窗口用原生窗口 + GDI（`src/window/shortcuts.rs`）

设置窗口是低频、静态的界面，不属于线带覆盖层。用一个普通带标题栏的窗口、
`WM_KEYDOWN` 直接录制、GDI 绘制文字与按钮，而不是再造一条 DirectComposition
管线。行为与 `ShortcutDialog` 一致：F1–F24 / PrtScn 可单用、其余必须带修饰键、
Esc 取消、Enter 确定、冲突/被占用就地提示。

## D3 拖拽预览用一个小型 DirectComposition 窗口（`src/window/ghost.rs`）

没有使用 `UpdateLayeredWindow`。ghost 复用 `Renderer`（D3D11 + D2D + DComp），
内容只在开始拖拽时绘制一次；15 ms 定时器只做 `SetWindowPos`，不重绘。
拖拽结束隐藏并把交换链缩到 2×2；空闲 10 s 后销毁整个窗口与设备。

## D4 飞行窗口允许被新截图打断（`src/window/flight.rs`）

WPF 版一次飞行一条时间线；Rust 版加了 generation 计数：新的飞行/掉落会把
正在跑的那条时间线作废（渲染线程每帧检查），因此连拍 5 张时每张都立刻起飞，
不会排在旧动画后面。旧卡片在被打断时立即回到槽位（`SetFlying(false)`），
不会永久不可见。

## D5 池化窗口空闲后销毁（M6）

实测一枚空闲的 D3D11 设备 + 交换链在 6K 机器上约 30–35 MB 私有内存：

| 窗口 | 空闲释放时间 | 重建代价 |
|:--|--:|:--|
| 飞行窗口 | 30 s | 一次设备创建，约 20 ms，发生在截图解码之后 |
| 悬停预览 | 10 s | 首次悬停解码 0.35 s 之后才建，用户看不到 |
| 拖拽 ghost | 10 s | 下一次拖拽开始时建好，拖拽本身是模态的 |

实测（10 张连续截图，release）：飞行窗口空闲前 99.7 MB，30 s 后 87.9 MB。

## D6 被否决的优化

| 优化 | 实测 | 结论 |
|:--|:--|:--|
| `lto = "fat"` | exe 1,192,960 B vs thin 1,198,080 B（−0.4%），release 构建 36 s | 收益小于构建代价，回到 thin |
| 线带隐藏时把交换链缩到 2×2 | `probe --mode=static` 58.8 MB vs shrink 59.6 MB | 私有内存没有下降，只增加每次放下/收起的 `ResizeBuffers`，不做 |
| 缩略图图集 | 未实现；PLAN §3.2 估计再省 5–10 MB，但要把 6 次 `DrawBitmap` 改成子矩形 | 用户已接受当前内存，暂不做 |
