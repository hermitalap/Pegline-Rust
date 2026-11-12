# Pegline 2.4.2 发布说明

发布日期：2026-11-12
产物：`dist/Pegline-2.4.2-win-x64.zip`（单文件 exe + README + LICENSE + CHANGELOG）

`pegline.exe` 为 **1,840,640 B（1.84 MB）**，ProductVersion `2.4.2.0`。

## 2.4.2 修复

便签编辑器有两处影响手感的交互问题：纸面上的鼠标光标不是编辑光标，拖动选择
文字时选区明显落后于指针。

1. **光标**：编辑器窗口注册的类光标是 `NULL`，并且没有处理 `WM_SETCURSOR`，
   所以纸面上一直是箭头（或上一个窗口留下的光标）。现在纸面显示 I 形文本光标，
   纸面外的阴影边距显示箭头；按住拖动选区时即使指针移出纸面也保持 I 形，松开后
   按所指位置恢复（纸面 I 形、边距箭头）。实现上只在光标类别变化时写 trace。
2. **拖动延迟**：渲染线程原来每帧只处理一条队列命令，然后等待 vsync，125–1000 Hz
   的鼠标移动会把输入队列堆到无界。600 次移动 / 0.9 s 的实测：

   - 修复前：队列深度 p50 290 / max 543；应用延迟 p50 4.48 s / p95 8.58 s；
     指针停止后约 9 s 选区仍在追赶。
   - 修复后：队列最大 12；应用延迟 p50 0.76 ms / p95 1.58 ms；最后一次移动后
     1.1 ms 完全追上。

   现在每帧成批排空队列（最多 512 条），连续的指针移动只应用最后一条，其余命令
   保持原顺序；输入队列、批大小、延迟和绘制时间可通过 `PEGLINE_TRACE=1` 的
   `note editor drag:` / `note editor loop:` 统计查看。
3. **Ctrl+C**：按键映射缺少 `VK_C`，复制快捷键不可达（Ctrl+X 正常）；已补上。
4. **性能**：光标在同一行内移动时不再重新解析 Markdown 或重建全部文本布局，
   单次 rebuild 从 p50 29 µs 降到 0–1 µs。帧时间仍由 vsync 的 `Present` 主导
   （p50 15.7–16.6 ms），这是附带的小幅节省，不是卡顿修复本身。
5. **测试钩子**：`PEGLINE_ALLOW_SECOND_INSTANCE=1` 允许脚本化验收运行第二份实例；
   正常启动仍然是单实例。

## 验证

- `cargo test`：**223 passed，0 failed，1 ignored**（另 `src/main.rs` 1 passed）
- `cargo clippy --all-targets -- -D warnings`：通过
- `cargo fmt --check`：通过
- 独立验证（从 HEAD 另行构建修复前基线，与最终构建对比）：光标句柄为
  `IDC_IBEAM` 65541（纸面）/ `IDC_ARROW` 65539（边距）；最终构建 500 次合成移动
  后 0 ms 安静（修复前 1,533–6,385 ms、77,590 px 尾巴）；回归检查（双击选词、
  点击纸面外提交、Esc、Ctrl+S、输入、Ctrl+C/X、便签存储与丢弃流程）全部通过；
  0 panic；用户当时运行中的 2.4.1 实例未被触碰。
- 证据与复现：`docs/VISUAL-TESTS.md` §3.20、`PLAN.md` §12、
  `docs/visual/note-drag-backlog.png` / `note-drag-settled.png`。

## 校验和

```
SHA256  729892d358f8386c2db15c0872f5bdc1f57c42772932478caf533a07805a7d0a  Pegline-2.4.2-win-x64.zip
```

## 升级

解压覆盖旧 `pegline.exe` 即可；设置、便签和截图目录不变。若要回退，2.4.1 的
产物仍在 `dist/Pegline-2.4.1-win-x64.zip`。
