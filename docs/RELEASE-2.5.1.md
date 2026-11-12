# Pegline 2.5.1 发布说明

发布日期：2026-11-12
产物：`dist/Pegline-2.5.1-win-x64.zip`（单文件 exe + README + LICENSE + CHANGELOG），
818,671 B，sha256 `be2f3c5d1b39ef967116a9515f80281218a5924b2168925354723b639e54710f`。
重新打包：`pwsh tools\package-release.ps1`。

`pegline.exe` 为 **1,879,552 B（1.88 MB）**，ProductVersion `2.5.1.0`（2.5.0 的 1,878,528 B
只多了 1,024 B：`available_paper` 与中心摆放取代了生长包络）。

## 2.5.1 改变

**便签编辑器改为围绕纸面中心缩放。** 2.5.0 拖动右/下边缘时左上角固定、窗口向右下生长；
现在被拖的边 1:1 跟手，对边镜像移动同样的距离：

- 向右拖 100 px：右边缘跟 100 px，左边缘同时向左 100 px，纸面宽度增加 200 px。
- 向下拖 60 px：高度增加 120 px，纸面中心点始终不动。
- 窗口打开时在工作区居中；拖到工作区边界只**夹住尺寸**，不会移动中心。
- 右/下/右下角的范围（460×360 ～ 820×640 DIP）、悬停高亮、尺寸记忆和渲染都不变。

## 实现

- `note_editor/window.rs`：`Drag.start_window` → `Drag.centre`；`resized_paper` 按
  `start + 2Δ` 计算，新增 `available_paper(centre, work, scale)` 以中心到工作区两侧
  较短的一半为上限；`drag_resize`/`finish_resize` 用
  `SetWindowPos(centre − size/2, …, SWP_NOACTIVATE)` 摆放；`frame_for` 去掉生长包络，
  只负责居中与夹紧。
- 单元测试：`a_resize_drag_mirrors_the_dragged_edge_around_the_centre`、
  `a_work_area_too_small_for_the_paper_clamps_without_moving_the_centre`、
  `the_editor_is_centred_in_the_work_area`。

## 验证

真实鼠标（200% DPI，`--note-resize-demo`，`GetWindowRect`）：

| 步骤 | 窗口 | 中心 |
|:--|:--|:--|
| 打开 | 1000×800 | (3008,1644) |
| 右边缘 +100 px | 1200×800 | (3008,1644) |
| 下边缘 +60 px | 1200×920 | (3008,1644) |
| 右下角往里拖 140×90 px | 1000×800（夹到最小） | (3008,1644) |

尺寸记忆复验：右边缘 +80 px → `settings.ini` 写入
`noteSize:便签 ….txt=540x360`，归档后删除。
`probe --mode=note`：460×360 打开 56.8 MB private，820×640 打开 72.7 MB，
draw p50 0.22 ms；`--note-flow-demo` 与 `--note-discard-demo` 正常。
237 个测试通过，`clippy --all-targets -- -D warnings` 与 `cargo fmt --check` 全绿。

## 已知限制

- 100% DPI、120 Hz 刷新率、第二显示器和睡眠唤醒仍未在真实硬件上实测（几何由单元测试覆盖）。
- 编辑器仍不能移动，只有右/下/右下角可拖。

## 未改变

功能、快捷键、文件格式、`settings.ini` 键、内存与帧率都没有变化；2.5.0 的缩放范围、
悬停高亮和尺寸记忆原样保留。
