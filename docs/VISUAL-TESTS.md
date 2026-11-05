# 视觉与交互验收记录（2026-11-03）

本轮只做“看得见”的验收：在真实桌面上运行 release 版，逐项截图、逐帧录屏，
并把发现的问题修掉。硬件仍是 **6016×3384 @200% / 60 Hz / 单屏**；120 Hz、
第二块屏和休眠唤醒仍无法在本机实测。

## 1. 方法

- 截图：`tools/backdrop.ps1` 铺一层中性灰渐变（可带网格），线带、卡片、预览
  和拖拽 ghost 都对着已知背景看，避免壁纸颜色干扰。
- 动画：`ffmpeg -f gdigrab -framerate 60` 录局部区域，再用
  `-fps_mode passthrough` 抽帧；逐帧量卡片/绳子的位置，而不是只看“感觉”。
- 数值：`probe.exe --mode=line|flight|static`，同一台机器、release 构建。
- 每个交互都用 `SendInput`/`SendMessage` 在真实进程上触发；日志
  （`PEGLINE_TRACE=1` 或 `%APPDATA%\Pegline\trace.on`）记录命中、拖拽、
  飞行/掉落和快捷键窗口。

## 2. 证据

| 图 | 验证的内容 |
|:--|:--|
| ![线带、悬停按钮和大图预览](visual/line-hover-preview.png) | 悬停后的叉/复制/打开、按钮下方的预览、相框和夹子阴影；预览不再盖住按钮 |
| ![复制提示](visual/line-copied.png) | 点击卡片后的“✓ 已复制”胶囊；按钮同时让位 |
| ![便签色板](visual/line-note-swatches.png) | 便签正文、折角、四色色板和选中勾；这是在文件夹里已有、但不在线上的便签被重新挂回 |
| ![便签四色](visual/line-note-colors.png) | 点击蓝/粉/灰三色后纸面、折角、墨色和勾选立即切换；`settings.ini` 记录并重启恢复 |
| ![12 卡容量](visual/line-capacity.png) | 连续加 13 张后线上保持 12 张、间距 174 DIP 居中，最旧一张掉落 |
| ![自己的截图右键菜单](visual/context-menu-own.png) | 自己保存的截图：保存到桌面 / 丢弃 |
| ![外部文件右键菜单](visual/context-menu-external.png) | 外部图片：取下 / 移到回收站（没有保存到桌面/丢弃） |
| ![拖拽预览](visual/line-drag-ghost.png) | 0.9 倍、0.85 不透明度、跟随指针的 ghost；线带上的原卡变半透明 |
| ![掉落](visual/line-drop.png) | 掉落中的卡片带完整白框和夹子，按 `e = k³` 加速、旋转、淡出 |
| ![飞行](visual/line-flight.png) | 飞行中的截图；相框和夹子随进度淡入，落地前与线上卡片重合 |
| ![标注编辑器](visual/editor.png) | 深色标题栏（与深色工具栏一致）、工具/颜色/线宽选中态和悬停态、撤销/取消/保存 |
| ![文字输入框](visual/editor-text-box.png) | 文字工具：白底输入框、彩色边框和插入光标；输入框盖住旧箭头，回车后框消失，只留下文字 |
| ![中文文字框](visual/editor-text-chinese.png) | 中文正文“中文测试”输入、字体回退和光标定位都正确 |
| ![IME 中文字](visual/editor-ime-char.png) | `WM_IME_CHAR` 与 `WM_CHAR` 各注入 `0x4E2D`，文字工具出现“中中” |
| ![IME 中文上屏](visual/editor-ime-composed.png) | 真实拼音 `zhongwen` + 空格，文字工具上屏“中文” |
| ![修复前的 IME 候选窗](visual/editor-ime-candidate-before.png) | 修复前：组合串和候选列表停在屏幕左上角，压在浏览器标签栏上 |
| ![修复后的 IME 候选窗](visual/editor-ime-candidate.png) | 修复后：组合串和候选列表紧跟文字框光标，落在屏幕 (2890,1703) |
| ![空格剪贴板提示](visual/clipboard-empty.png) | 剪贴板没有文字时按 `Ctrl+Alt+N`：系统气泡显示“剪贴板里没有文字…”和当前快捷键 |
| ![快捷键录制](visual/shortcut-recorder.png) | 冲突提示：红字完整显示在按钮上方；录制时本程序的全局热键已全部释放 |
| ![西语快捷键录制](visual/shortcut-recorder-es.png) | 西语标题、两行说明和三个按钮完整，无截断 |
| ![西语部分修饰键](visual/shortcut-recorder-es-partial.png) | 按住 Ctrl+Alt 时只显示 `Ctrl+Alt`，不再出现 `VK00` |
| ![首次运行接管询问](visual/first-run-offer.png) | 首次运行 1.2 s 后出现“让 Pegline 接管你的截图吗？”；点“是”后 `catchClipboard=1` |
| ![真实托盘图标点击](visual/line-tray-click.png) | 从任务栏溢出面板左键点击 Pegline 图标，空线带放下并显示提示 |
| ![真实托盘菜单](visual/tray-menu-real.png) | 真实右键托盘图标：菜单、热键被占用提示、“接管截图”勾选完整 |
| ![全屏抑制](visual/fullscreen-suppressed.png) | 上：修复前线带压在蓝色全屏窗口上；下：修复后全屏窗口保持干净 |
| ![点击穿透](visual/clickthrough.png) | 绿色窗口在下方：空白处点击落到窗体（日志 50,400），卡片上的点击被线带接住 |
| ![拖到资源管理器](visual/drag-drop-explorer.png) | 把 demo 卡片拖进资源管理器，`demo-2.png`（1.23 KB）真实落地 |
| ![音频波形](visual/audio-cues.png) | 临时解除默认端点静音后，WASAPI loopback 录到 Pop（约 320 Hz）、Tink（1760/3520 Hz）和 Rustle（低频噪声） |
| ![三语编辑器工具栏](visual/editor-languages.png) | 中/西/英三套标题与工具栏标签完整，无裁切（上：中，中：西，下：英） |
| ![西班牙语空提示](visual/line-es.png) | 西班牙语提示整句放入 pill；托盘菜单的西语长标签同样完整 |

空线带提示、首次运行欢迎与三语字符串：

![空线带提示](visual/line-empty.png)

![首次运行欢迎](visual/line-welcome.png)

## 3. 本轮发现并修复的问题

按用户可见程度排序。每一条都先在真实进程里复现，再修，再截同样的图确认。

1. **飞行/掉落的相框和夹子会消失。**
   `draw_flight_card` 只在 `push_opacity` 真的压入图层时才画内容；当 `chrome`
   恰好为 1.0 时该函数走“不压层”的快速路径并返回 `false`，于是整张卡只剩
   裸照片。掉落的 `run_drop` 也有同样问题（`opacity = 1` 时第一帧完全不画）。
   现在无论是否压层都画，`push` 只决定要不要 `pop`；相框也改回整张卡片大小，
   照片缩短边距，白框不会藏到照片后面。
2. **按钮、复制提示、空提示在完全不透明时会消失。**
   同一类 `push_opacity` 误用出现在场景的五个调用点（卡片、叉、复制胶囊、
   便签色板、按钮行、空提示）。全部改为“先画、后按需 pop”。悬停 3 s 后按钮
   仍在，复制 1.2 s 内提示仍在。
3. **“打开”按钮点了却复制。** 按钮只有绘制，没有命中测试；点击任意按钮都会
   落到卡片的单击复制。现在复制/打开两枚胶囊各自有精确命中框，点击只执行
   自己的动作；打开走 `ShellExecute`，复制走 PNG + CF_DIB + 文件。
4. **按钮没有悬停反馈。** 加回 WPF 的 `#F2FFFFFF` 高亮、整行 4 DIP 的上浮
   淡入；悬停按钮不再和卡片悬停混在一起。
5. **卡片外 10 DIP 的“悬停宽容区”也会触发点击。** 这个宽容区原本只该让指针
   追倾斜卡片时不掉悬停；它导致点卡片旁边的空白也会复制。现在悬停仍宽容
   10 DIP，点击和右键菜单用精确矩形。
6. **大图预览盖住复制/打开按钮。** 预览原来从卡片下沿 +12 DIP 开始，正好压住
   按钮行。现在锚点包含按钮行/色板高度（WPF `HoverFrame` 的规则），预览从
   **按钮下方** 12 DIP 开始。
7. **悬停阴影不变大。** 按 BEHAVIOR 的 20/5 → 28/8 参数预烘焙两张九宫格阴影，
   悬停时切换纹理，不再每帧模糊；同时把闲置阴影的偏移从 4 改成规范的 5 DIP。
8. **夹子没有阴影。** 预烘焙一张 9×26 DIP 的软阴影（4 px 模糊、下移 1.5、
   0.3 不透明度），卡片在浅色和深色背景上都能“贴”在绳子上。
9. **快捷键/托盘打开空线带，0.7 s 后自己收起。** 有意打开的空线带应保持显示
   （WPF `keepOpen`）；现在 `toggle` 打开时置位、关闭且为空时清除。
10. **快捷键录制窗口的三个问题。** 录制时只注销了目标热键，按 `Ctrl+Alt+T`
    会被本程序的显示/隐藏热键吞掉，界面显示 `Ctrl+Alt+VK00`；现在录制期间
    注销显示/隐藏和两个动作热键，关闭后全部恢复。错误行原来被标题栏吃掉的
    客户区高度挤出按钮之外；现在按客户区 420×236 DIP 调窗口，错误行完整
    显示在按钮上方。错误文字原来是蓝色（COLORREF 是 BGR）；现在是 WPF 的
    `#C42B1C` 红。
11. **同文便签在文件夹里但不在线上时，只把空线带拉下来。** 现在重新读取并
    挂回已有的 `.txt`，不新建副本，符合“重复文字不新建，只展示已有便签”。
12. **`--demo` 的卡片没有真实文件。** 预览、复制、打开都依赖真实路径；demo
    现在把三张图写到 `%LOCALAPPDATA%\Pegline\demo`，所有交互走正式链路。
13. **`RegisterClassW` 把“类已存在”当错误记日志。** 正确的
    `HRESULT_FROM_WIN32(ERROR_CLASS_ALREADY_EXISTS)` 是 `0x80070582`；修掉后
    拖动 ghost / 预览反复创建不再污染 `error.log`。
14. **文字工具没有输入框和光标。** 文字直接浮在图上，用户看不到插入点；
    现在输入期间画 WPF 风格的半透明白底、彩色边框和 2 px 光标，输入框盖住
    旧标记、当前文字再盖在输入框上；回车后只留文字。见
    `visual/editor-text-box.png`。
15. **编辑器没有光标反馈。** 工具栏改为手形、画布改为十字；选中文字工具后
    画布为 I 形。用 `GetCursorInfo` 对比系统句柄确认三种形状。
16. **线带按钮没有手形光标。** 复制/打开按钮和便签色点现在显示手形，卡片
    本身保持箭头；与 WPF 的 `Cursors.Hand` 一致。用 `GetCursorInfo` 和
    `trace.log` 的 hover action 记录核对。
17. **首次运行不显示欢迎线带。** 启动时只设了 `keep_open` 和 5 s 计时，
    没有 `present/reveal`；现在首次运行会像 WPF 一样自动放下空线带显示提示，
    5 s 后无卡自动收起。实测 1.5 s 可见、5 s 后隐藏。
18. **文字工具只处理 `WM_CHAR`。** 输入法提交中文走 `WM_IME_CHAR`，原来会被
    丢掉；现在两条消息都转成文字（含代理对）。直接注入 `0x4E2D` 实测得到
    “中中”；随后补 IME 上下文后真实拼音已能上屏（问题 22）。
19. **已显示的线带不会因为全屏应用而消失。** 全屏检测原来只在“打开”路径里，
    已经在屏幕上的线带会继续压在视频/游戏/幻灯片上。现在 `present` 拒绝在全屏
    应用前打开，已显示时立即 `dismiss`；实测全屏窗口盖住后线带消失，关掉全屏
    再碰顶边线带恢复。
20. **快捷键录制按住修饰键时显示 `Ctrl+Alt+VK00`。** 部分修饰键状态用一个
    `virtual_key: 0` 的占位 Hotkey 格式化，尾部的 `+VK00` 被带了出来。现在
    `virtual_key == 0` 只输出修饰键（并去掉末尾的 `+`），实测显示 `Ctrl+Alt`。
21. **“保存到桌面”写到了错误的桌面。** 代码拼的是 `%USERPROFILE%\Desktop`，但
    本机桌面通过 OneDrive 重定向到 `D:\UserData\OneDrive\Desktop`，文件落在一个
    用户看不到的旧目录。改用 `SHGetKnownFolderPath(FOLDERID_Desktop)`；实测文件
    出现在资源管理器显示的桌面目录，旧路径没有文件。
22. **文字工具没有 IME 上下文，拼音只会出拉丁字母。** 普通 Win32 窗口在
    Windows 11 TSF 下 `ImmGetContext` 返回 0，Microsoft Pinyin 不会组合。
    现在窗口创建时 `ImmCreateContext` + `ImmAssociateContext` +
    `ImmSetOpenStatus(TRUE)`，WM_DESTROY 时解绑并 `ImmDestroyContext`；
    实测输入 `zhongwen` 后按空格上屏 **“中文”**。`WM_IME_CHAR` 的直接注入
    仍保留为兜底。
23. **输入法候选窗跑到屏幕左上角。** 补上 IME 上下文后，组合串和候选列表
    虽然能出现，但位置固定在 (0,0) 附近，和文字框完全分离（修复前截图里
    组合串压在浏览器标签栏上）。原因是只创建了 IME 上下文，从未告诉 IME
    插入点在哪里；`IMM32` 的默认位置就是客户区原点。现在渲染线程把文字光标的
    客户区物理像素坐标写进共享状态，UI 线程在 `WM_IME_STARTCOMPOSITION` /
    `WM_IME_COMPOSITION` 里调用 `ImmSetCompositionWindow(CFS_POINT)` 和
    `ImmSetCandidateWindow(CFS_CANDIDATEPOS)`。200% DPI 下实测客户端坐标
    **1002,797** 对应组合窗 `MSCTFIME Composition` 屏幕矩形
    **(2890,1703)–(2978,1725)**，候选列表紧接其下，见
    `visual/editor-ime-candidate.png`。

### 3.1 标注编辑器实测（2026-11-03，2026-11-03 晚更新工具栏）

用新开发开关 `--editor-demo --editor-hold` 打开真实 900×600 源图，逐项操作：

- **箭头**：拖动后出现红线+箭头；`Ctrl+Z` 撤掉，`Ctrl+Y` 恢复，按钮禁用态同步。
  2026-11-05 修复箭头头部的方向与锚点：`atan2` 得到的是弧度，而
  `Matrix3x2::rotation`（`D2D1MakeRotateMatrix`）接受的是角度；以前头部只转了
  零点几度，左/上/下的箭头都在线段旁边多出一个朝右的三角。现在头部沿拖动方向
  旋转，尖端正好落在拖动终点。从中心向四个方向拖出加一条斜线，窗口预览与保存的
  900×600 PNG 都逐像素检查过，见 `visual/editor-arrows.png`。
- **矩形**：选蓝色 + 12 px，边框位置、粗细、圆角正确。
- **文字**：点击落点后输入英文/中文（“中文测试”），白底框、彩色边框、光标、
  字体回退都正确；回车后框消失。
- **马赛克**：拖动 16 px 块；保存后的 PNG 里仍是块状像素。
- **缩放**：窗口从 2266×1591 缩到 1470×1111（客户区 720×520 DIP 以上），工具栏不重叠，
  画布等比重新适配；紧凑布局下撤销/重做自动变为图标。
- **图标与分组**：标题栏显示 Pegline 图标；工具栏按“工具 | 颜色 | 线宽 | 操作”
  分组，组间有 1 px 分隔线；工具图标为矢量线条，线宽按钮显示 4/8/12 px 横线预览。
- **保存**：`Ctrl+S` 输出 **900×600** PNG（与源图同像素尺寸），文件随即挂回线带。
- **取消**：`Esc` 关闭，源文件时间戳不变，不留新卡。
- **光标**：工具栏手形、画布十字、文字工具 I 形（见上）。

![箭头从中心向四个方向拉出，头部朝外并落在各自的终点](visual/editor-arrows.png)

### 3.2 线带容量、便签颜色与大图预览

- **容量**：连续放入 13 张 320×200 截图，线上保持 **12 张**（CAP 02–13），
  CAP 01 走掉落动画；间距 174 DIP、整体居中，无重叠。
- **便签颜色**：点击蓝/粉/灰三色，纸面渐变、折角、墨色和勾选一起更新；
  `settings.ini` 写入 `noteColor:<文件名>=3`，重启后仍是灰色。
- **大图预览**：4000×3000 源图悬停 0.35 s，预览白框实测 **1726×1300 物理
  像素**（上限 1800×1300，按 4:3 等比），从按钮行下方开始，不遮挡按钮。
- **右键菜单状态**：用真实 PNG 剪贴板触发一次自己的截图（own=true），菜单
  出现“保存到桌面/丢弃”；外部 demo 图只有“取下/移到回收站”。

### 3.3 声音（波形已验证，未做主观听感结论）

- `sound::tests` 通过；真实进程触发便签 Tink 时 `PlaySoundW` 返回 **TRUE**。
- 本机默认渲染端点是 **`U32T1`**（`43cea139-…` 的显示音频），查询到它处于
  **mute=True、volume=0.00**，所以之前 WASAPI loopback 和 `dshow` 都录到静音。
- 临时把它设为 mute=False、volume=0.5，用自写 WASAPI loopback（48 kHz / 2 ch /
  32-bit float）录到三段：
  - Pop：0.00–0.10 s，主频约 **320 Hz**（向下扫频的第一段）；
  - Tink：0.48–0.60 s，峰值 **1760 Hz** 和 **3520 Hz**；
  - Rustle：清空截图文件（1 个）时，0.01–0.27 s、峰值 0.041、宽带低频噪声。
- 录完立即恢复 mute=True、volume=0.00，并确认端点状态；`settings.ini` 也恢复
  `soundOff=1`。
- 三段音效的主观听感仍未做（没有扬声器监听），但输出波形已证明三段声音都
  离开进程并到达系统混音器。

### 3.4 三语界面

新增仅供验收的 `PEGLINE_LANG=en|es|zh` 环境变量；同一份 release 依次启动三次：

- 中文：标题“标注截图”，撤销/重做/取消/保存。
- 西班牙语：标题“Anotar captura”，Deshacer/Rehacer/Cancelar/Guardar；西语
  是三种语言里最长的，工具栏没有裁切，空线带提示整句放入 pill。
- 英语：标题“Mark up screenshot”，Undo/Redo/Cancel/Save。

托盘菜单的西语长项（“Atajo de nota: Ctrl+Alt+N…”等）也完整显示；菜单由系统
绘制、按内容自适应宽度。

最小窗口 **720×520 DIP**（200% 下客户区 1440×1040）：西语的 Deshacer/Rehacer
自动切换为紧凑图标，Cancelar/Guardar 仍是完整文字，工具栏没有重叠。

![西语编辑器最小窗口](visual/editor-minimum-es.png)

西语快捷键录制窗口（420×236 DIP）：标题、两行说明、`Sin atajo` /
`Cancelar` / `Aceptar` 全部完整；按住 Ctrl+Alt 时显示 `Ctrl+Alt`，不再出现
`VK00`。

![西语快捷键录制](visual/shortcut-recorder-es.png)

### 3.5 首次运行接管询问

补上 WPF 的一次性 MessageBox：启动 1.2 s 后询问是否让 Pegline 接管截图，
`catchOffered` 只写一次；点“是”写 `catchClipboard=1`，点“否”保持原设置。

自动化启动时前台锁让对话框没有自动取得键盘焦点（窗口仍然置顶可见），
鼠标点“是”后设置正确写入；正常从资源管理器双击启动时由 Windows 决定焦点。
为避免打断开发流程，`--demo` / `--editor-demo` 不弹这个框。

### 3.6 旧版 Ctrl+Alt+S 全链路（2026-11-03 晚移除）

> 该功能已按用户要求移除；本节保留为历史记录，不再对应当前版本。

默认 `Ctrl+Alt+S` 被本机其他程序占用，所以把 `snipKey` 临时设为 `Ctrl+Alt+K`，
走同一条代码路径：

- 按下快捷键 → Windows 截图工具全屏覆盖出现；
- 拖出 **1200×800** 选区 → Pegline 接管剪贴板，打开编辑器，标题/画布显示
  `editor picture: 1200x800`；
- 同时 trace 记录 `clipboard duplicate ignored`（Windows 自己保存的那份与剪贴板
  副本被识别为同一次截图）；
- `Ctrl+S` 保存 → 输出 PNG 仍是 **1200×800**，文件随即挂回线带。

这补上了之前因热键被占用而一直“无法实测”的 §5.8 真实链路。

### 3.6.3 丢弃到回收站

从自己的截图卡片右键选择“丢弃”：

- 源文件立即消失，trace 记录 `recycled 1 files`，并有完整的掉落动画
  （`drop render start` → `drop finished after 36 frames`）。
- 通过 `Shell.Application.Namespace(10)` 在回收站里找到同名条目，确认是
  `SHFileOperationW(FO_DELETE)` 的回收，不是直接删除。

### 3.6.2 拖到资源管理器

- 先把资源管理器窗口移到终端窗口之外并确认 `GetForegroundWindow` 就是它，
  否则合成拖拽会落在终端上（第一次失败就是这个原因，不是产品问题）。
- 从 demo 第 3 张卡片拖到 `target\drops`：trace 记录
  `press card 3` → `drag start card 3` → `drag ghost created`，目录里出现
  **demo-2.png（1.23 KB）**。
- 这验证了真实 OLE `IDropTarget` 路径，而不只是 ghost 动画。

### 3.6.1 点击穿透

在 (2400,0) 放一个 1400×600 的绿色 WinForms 窗口并记录 `MouseDown`：

- 点空白线带 (2450,400)：窗口日志 `click 50,400 button=Left`（局部坐标），
  说明 `WS_EX_TRANSPARENT` 在非卡片区域生效。
- 点第一张卡片的图片 (2675,250)：窗口没有新日志，线带接住了点击。
- 同一张截图里还能看到悬停预览和复制/打开按钮，它们同样属于线带的点击区域。

### 3.7 托盘图标真实点击

- 用 `Shell_NotifyIconGetRect` 定位到 Pegline 的托盘图标（被收进溢出区时返回的是
  溢出按钮），展开溢出面板后，深蓝底、三张彩色卡片的 Pegline 图标清晰可见。
- 真实左键点击该图标（不是向隐藏窗口发消息）后，空线带放下并显示提示，
  证明 `Shell_NotifyIcon` 回调与回调消息参数在真实任务栏路径上工作。
- 真实右键点击同一图标能打开菜单：“接管截图”有勾选、清空计数为 0；菜单关闭正常。
  （2026-11-03 晚起菜单不再包含“截图”和“截图快捷键”，只保留便签快捷键。）

### 3.8 输入法候选窗（2026-11-03 收尾）

用 `--editor-demo --editor-hold` 打开 900×600 源图，依次点击文字工具和画布，
再用 `SendInput` 发送 `z h o n g w e n` 八个字母（不按空格，保持组合）：

- trace 记录 `IME position 1002,797`，位置随每次 `WM_IME_COMPOSITION` 重发。
- 枚举窗口找到本进程的 `MSCTFIME Composition`，屏幕矩形
  **(2890,1703)–(2978,1725)**；编辑器客户区原点 (1888,906)，换算正好是
  客户端 **(1002,797)**，与 trace 一致。这说明 `ImmSetCompositionWindow` 的
  客户区坐标被 IME 接受，而不再退回 (0,0)。
- 候选列表从组合串下方开始，整行（1 中文 … 7 宗文）在光标右下方展开；
  截图见 `visual/editor-ime-candidate.png`。
- 组合完成后按空格上屏“中文”，`editor-ime-composed.png` 仍是同一路径。

修复前同一操作下组合串和候选列表出现在屏幕左上角，见
`visual/editor-ime-candidate-before.png`；本轮修复后不再复现。

### 3.9 图片右键“标注”与工具栏改版（2026-11-03）

需求变更：图片右键的“编辑”改为“标注”，复用原标注窗口；移除 `Ctrl+Alt+S`
“截图并标注”；标注窗口补标题栏图标并优化图标/间距。

已完成的验证：

- `cargo test`（117 项通过）、`cargo clippy --all-targets -- -D warnings`、
  `cargo fmt --check` 全部通过。
- `--editor-demo --editor-hold` 在 200% DPI 下实测默认 **1120×760 DIP** 与
  最小 **720×520 DIP** 两种客户区：
  - 标题栏和 Alt-Tab 图标显示 Pegline 图标，不再是空白方块；
  - 工具栏四组（工具 | 颜色 | 线宽 | 操作）之间的 1 px 分隔线、组内 3–6 DIP
    与组间 8–14 DIP 的间距一致；撤销/重做与取消/保存右对齐；
  - 720 DIP 下自动切换紧凑尺寸，中文、西语、英语三种标签都不重叠、不裁切；
  - 线宽按钮显示 4/8/12 px 的横线预览，黑色/白色线也有灰色轮廓。
- 截图更新：`visual/editor.png`、`visual/editor-minimum-es.png`。

真实点击验收（隔离的 `--demo`，3 张演示卡片）：

- 右键第 1 张图片卡片，菜单第三项为**“标注”**（`visual/context-menu-annotate.png`）；
  点选后编辑器打开 `demo-0.png`，标题栏图标正常。
- 画一条红色箭头后 `Ctrl+S`：源文件 SHA-256 从 `B94D…` 变为 `A142…`，
  尺寸仍为 **480×270**；线带上的第 1 张卡片立刻显示箭头，卡片总数仍为 **3**
  （没有多挂一张）。
- 再打开同一张卡片后按 `Esc`：源文件 SHA-256 完全不变，卡片保持原样。
- 便签卡片右键菜单第三项仍是**“编辑”**（`visual/context-menu-note.png`），
  没有变成“标注”。
- 菜单选择点击不会再被低层鼠标钩子当成卡片按钮：修复后 trace 只有
  `editor opened`，不再同时出现 `action Open on card 1`。
- `Ctrl+Alt+S` 按下后没有出现 `ScreenClippingHost`/截图工具进程，trace 也没有
  新的 `editor opened`；快捷键注册日志中没有 `RegisterHotKey(snip)`。
- 托盘菜单的“截图/截图快捷键”两项已从 `MenuCommand` 和构建代码中删除，由
  `tray::tests` 的全量枚举测试覆盖；本轮未单独截图托盘菜单。

![图片右键标注](visual/context-menu-annotate.png)

![便签右键编辑](visual/context-menu-note.png)

![标注保存后卡片原地刷新](visual/line-annotated.png)

### 3.10 统一 Microsoft YaHei UI 与 ClearType（2026-11-04）

需求：除符号外所有字体统一为 Microsoft YaHei / Microsoft YaHei UI，并适配或
优化 ClearType。

- 字体统一：
  - 线带卡片标签/空提示、标注编辑器工具栏与文字工具、快捷键窗口、便签正文
    全部改为 `Microsoft YaHei UI`；
  - 只有撤销/重做的 `↶/↷` 保留 `Segoe UI Symbol`；工具图标仍是矢量绘制。
  - 200% DPI 下实测中文、西语、英语的卡片标签和编辑器按钮都不裁切；西语
    720×520 DIP 最小窗口仍完整显示 `Cancelar/Guardar`（`visual/editor-minimum-es.png`）。
  - 便签中英文混排（“Pegline 便签字体测试 YaHei UI 123”）正常折行，没有
    超出纸面。
- ClearType 分表面处理：
  - 标注窗口的交换链不透明，显式设置 `D2D1_TEXT_ANTIALIAS_MODE_CLEARTYPE`；
    放大截图能看到白色文字边缘的红/青子像素条纹，确认不是灰度回退。
  - 快捷键窗口是普通不透明 GDI 窗口，字体质量改为 `CLEARTYPE_QUALITY`；
    中文窗口实测见 `visual/shortcut-recorder.png`。西语提示用 GDI
    `DT_CALCRECT` 在 380 DIP 宽、12 DIP 字号下量得 34 px 高，低于 48 DIP
    提示区，不会压住输入框。
  - 线带覆盖层是预乘 alpha 的透明表面，显式固定为
    `D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE`；文字原点继续按设备像素对齐。
  - 便签是 3 倍超采样后缩小，保留灰度抗锯齿，避免子像素彩边被重采样。
  - 标注保存的 PNG 带 alpha，显式固定为灰度抗锯齿，文件里不会出现 ClearType
    彩边。
- 体积：字体全部来自 Windows，没有新增字体文件；exe 仍在 **1.4 MB** 级别。
- 截图更新：`visual/editor.png`（中文、新字体、ClearType）、
  `visual/editor-minimum-es.png`（西语最小窗口）。

### 3.11 WYSIWYG 便签编辑器（2026-11-04）

本机 6016×3384 @200%，线带 6 张卡（last card 是便签）。用三个开发用命令行
开关在真实桌面复现，截图裁到应用像素：

| 模式 | 复现 | 证据 |
|:--|:--|:--|
| `--note-hover-demo` | 线带保持放下，合成指针停在入口热点中心右下 78/80 DIP | `visual/note-hover-physics.png`：空白便签**装在和挂图一致的玻璃外框里**（144×96 DIP，纸面内缩 4 DIP），纸面上方是同墨色的圆形加号图标，下面“新建便签”与短便签正文同款（16 DIP 半粗、#4A3B12），停在理论挂点，被指针力拉向右下、**顺时针倾斜**并上下浮动；`visual/note-entry-label.jpg` 是图标与文字的近照，`visual/note-entry-frame.png` 是入口便签与左侧挂图的并排对比（外框厚度、圆角、阴影一致） |
| `--note-hover-demo --note-flow-demo` | 悬停便签存在时自动打开编辑器，1300×2200 连续截帧 | 飞向中央的飞行**没有挂夹**：飞行层只淡入玻璃外框和纸面，纸面上也没有“新建便签”字样（那是入口提示，不是内容） |
| `--note-discard-demo` | 打开空白新便签，1.5 s 后自动提交（空文本），连续截帧 | `visual/note-discard-frames.png`：编辑窗口（占位符）→ 飞行层接管（纸面大小不变、只多一圈外框）→ 缩小到 45 % 并自转 → 带横向速度下落；trace `tumble finished after 57 frames, 0.89s, angle=202`，随后 `note editor closed` |
| `PEGLINE_NOTE_DEMO_POINTER` 四象限 | 同一模式，指针分别放在热点中心左上/右上/左下/右下 | `visual/note-hover-lean-quadrants.png`：左上 **+2.3°**、右上 **−2.3°**、左下 **−4.0°**、右下 **+4.0°**（正值 = 顺时针），即“左上/右下顺时针、右上/左下逆时针” |
| 连续截帧（90 ms 间隔） | 同一条目，记录 16 帧 | 便签中心从 (4239,195) 缓慢回摆到 (4239,186)，带阻尼；没有抖动或跑出工作区 |
| 连续截帧（135 ms 间隔，倾斜） | 默认指针，从飞入落地开始记录 30 帧 | 倾角 **0 → +1.02 → +3.45 → +4.24（峰值）→ +4.08 → +3.97°（稳定）**：0.6 阻尼下只轻微过冲约 7 %，约 0.6 s 回正，没有回摆 |
| `PEGLINE_NOTE_SLOW=4` 飞入 | 慢放 4× 后连续截帧 | `visual/note-hover-enter-frames.png`：旧版便签从工作区上方进入、下降（这张是 2026-11-04 的）；2026-11-05 起改为从屏幕顶边的露角位置落下并回正，见下方露角/展开条目 |
| `PEGLINE_NOTE_SLOW=4` 飞出 | 悬停后把指针移出热点，连续截帧 | `visual/note-hover-exit-frames.png`：便签沿 0.42 s 曲线上移消失，窗口随后隐藏（同上，外观为旧版） |
| 同上 + 鼠标点击便签 | 点击进入中央飞行 | `visual/note-center-flight.png`：便签沿曲线飞向中央（旧版带夹子；2026-11-05 起新便签的这次飞行不画挂夹，见上面 `--note-hover-demo --note-flow-demo` 一行） |
| `--note-editor-demo` | 打开示例文档，滚轮下滚 | `visual/note-editor-lists.png`（标题/列表/任务）、`visual/note-editor-code.png`（引用/行内代码/链接/代码块/中文） |
| 同一模式的空文档 | 点击空白便签 | `visual/note-editor-empty.png`：460×360 DIP 纸面、占位符“写点什么…” |
| `--note-flow-demo` | 自动输入 25 字节并提交 | `visual/note-card.png`：新卡片，有夹子、摘要为 `Demo note / • one / [x] two` |
| `PEGLINE_LANG=es` + 空编辑器 | 西语占位符 | `visual/note-editor-es.png`：“Escribe algo…” |
| 双击标题词 | 双击选中 `Pegline` | `visual/note-word-select.png`：选区在字形下方，`#` 标记保持淡化 |
| 中文 IME 组合 | Microsoft Pinyin 输入 `nihao` | `visual/note-editor-ime-composition.png`：活跃行内 `ni'hao` 带下划线，候选窗贴光标 |
| 中文 IME 提交 | 按空格选“你好” | `visual/note-editor-ime-committed.png`：`你好` 作为源码插入，组合下划线消失 |
| `--note-storage-demo` | 临时便签：建卡→同路径编辑器→改字提交→归档→清理 | trace：`has_card=true text_has_edit=true bytes=32 bom=true crlf=true`；归档文件存在，测试文件和卡片已清理 |

- 入口露角、快速展开与编辑后挂回（2026-11-05，debug 构建，6016×3384 @200%）：
  - 用 `--note-hover-demo` + `PEGLINE_NOTE_PEEK_POINTER=2000,300` 把合成指针
    停在入口列之外，入口大部分在屏幕外、屏幕顶边只露出倾斜 18° 卡片的右下角
    **12 DIP**；`visual/note-entry-peek.jpg`。
  - 把合成指针放到顶边露角列内（`PEGLINE_NOTE_PEEK_POINTER=3866,6`）并设
    `PEGLINE_NOTE_SLOW=10` 逐帧截取：完整便签从屏幕外的同一位置、同一倾角
    落下，边落边回正，最后停在理论挂点；不再有原地突变的出现方式；
    `visual/note-entry-fall.jpg` 是下落中间帧。
  - `PEGLINE_NOTE_PEEK_POINTER` 是视觉验收用的合成指针：绝对屏幕像素 `x,y`，
    只在 `--note-hover-demo` 下生效；默认仍用热点内的
    `PEGLINE_NOTE_DEMO_POINTER`。
  - 从卡片右键“编辑”打开已有便签，指针移到中央编辑窗口 2 s 后线带仍在屏幕
    上（编辑期间钉住）；点纸面外且没改字，编辑窗口关闭，飞行层带着卡片从
    编辑窗口飞回原槽位，落地后两卡位置不变；`visual/note-rehang-unchanged.jpg`
    是飞行中间帧。
  - `--note-storage-demo` 改字提交：飞行中间帧里其余卡片不补位、不重排，
    落回原槽位后第三张便签出现；`visual/note-rehang-changed.jpg`。
  - 本轮是 debug 构建的逐帧截图，只验证动画/状态，没有新的内存或帧率数字。

- 体积与内存（release，200%，6 卡，线带可见；`PrivateMemorySize64`）：
  - `pegline.exe` **1,649,664 B（1.57 MB）**，比上一版 1,385,984 B 增加
    **258 KB（+0.25 MB）**，低于 0.5 MB 预算。
  - 打开编辑器前 **111.94 MB**，编辑器打开后 **158.11 MB**（+46.02 MB：
    D3D 设备 + 纸面位图 + 阴影 + 交换链）；提交并等飞行窗口空闲释放后
    **116.27 MB**（比基线 +4.33 MB，新卡片缩略图/卡片本身）。
  - 编辑器关闭时渲染线程停止、设备释放；常驻路径不保留编辑器资源。

- 文档内容与行为：
  - 活跃行（光标在文档开头）显示原文 `# Pegline`，`#` 淡化；其余行实时渲染。
  - 标题 24 DIP 粗体、正文 15 DIP、列表圆点/复选框/数字、引用 2 DIP 竖线、
    行内代码浅底、链接下划线 + 强调蓝、代码块 Consolas + 左侧竖线。
  - 中文与英文混排正常，没有裁切；纸面右下折角沿用到编辑器。
- 数据：`--note-flow-demo` 生成 `便签 2026-11-04 140758.txt`，30 字节，
  UTF-8 BOM、CRLF；卡片摘要去掉 Markdown 标记。`pegged.txt` 追加该文件；
  测试后已删除该文件与对应行。
- 悬停动画（2026-11-04 打磨）：
  - 飞入慢放 4× 的 trace：中心 y **-190.4 → 169.8**，连续下降，没有贴住屏幕
    顶边。之前 `clamp_center` 把入场位置钳在 y=136，造成“先出现在顶部、最后
    突然落位”；已移除入场阶段的 clamp。
  - 飞出（正常速度）trace：中心 y **162.5 → -190.4**，0.42 s 连续上移，随后
    `note hover cancelled` 并隐藏；预览窗口在空闲后释放。
  - 退出判定改为**固定热点外 24 DIP**，不再看“鼠标是否在便签上”。之前便签会
    被拖拽力拉向鼠标、一直保持在鼠标下方，所以移开后不飞出；现在离开入口
    区域后 0.42 s 飞出并消失。
- 悬停便签的倾斜与阻尼（2026-11-05，本机 200%）：
  - 四象限用 `PEGLINE_NOTE_DEMO_POINTER` 固定合成指针（热点中心左上/右上/
    左下/右下），每格 2.8 s 后截帧，用纸面掩膜的图像矩测角度：左上 **+2.31°**、
    右上 **−2.33°**、左下 **−3.97°**、右下 **+3.97°**。正值为顺时针，两轴对称，
    与“左上/右下顺时针、右上/左下逆时针”一致；纸面 bbox 269×177 px
    （136×88 DIP）。
  - 阻尼与范围（2026-11-05 调参）：回位弹簧与角弹簧的阻尼比都从 0.27 提到
    **0.6**，指针拖拽的平衡偏移从 `min(距离 × 0.28, 42 DIP)` 收到
    `min(距离 × 0.22, 30 DIP)`。默认指针下便签稳定在挂点右下 **约 27 DIP**
    （旧参数约 36 DIP），整体移动范围小约 25 %。
  - 倾斜的瞬态（默认指针，30 帧 × ~115 ms）：飞入落地时 **−0.01°**（直立），
    随后 **+1.02 → +3.45 → +4.24（峰值）→ +4.08 → +3.97°** 稳定。0.6 阻尼下
    只过冲约 **7 %**，约 0.6 s 回正，没有回摆；目标角随便签被拉近而减小，所以
    过冲幅度比纯阶跃更小。
  - 窗口是 **384×288 物理像素**（200%：144×96 DIP 卡片 + 24 DIP padding），
    倾斜只重绘内容、不改变窗口尺寸；请求合并，静止后不再重绘（截帧期间没有
    闪烁或重影）。
- 存储链路（`--note-storage-demo`，2026-11-04）：
  - 临时创建 `Notes\Note …txt`，作为便签卡片挂上，走**已有便签**的编辑器路径；
  - 追加 ` edited` 后提交；trace 显示 `has_card=true text_has_edit=true`，
    文件 32 字节、BOM 和 CRLF 都正确；
  - 随后 `discard` 触发归档，文件出现在 `Notes\Archive`，卡片播放掉落动画
    （23 帧）并移除；演示结束删除归档文件和残留，`pegged.txt` 回到 7 条。
- 尚未截图/实测：200% → 100% DPI 改变后的重新居中、三语菜单/气泡、
  按键到像素延迟。`PARITY` 7.15 仍未标记通过。

## 4. 逐帧结果

- **放下**：0.3 s 悬停后开始，约 0.65 s 内从 -222 DIP 弹到 0，轻微过冲
  （1.5 DIP）后停住；抽帧未见闪烁或跳变。
- **收起**：`hideT²` 缓入，约 0.22 s 内完全离开屏幕；窗口在 0.4 s 后隐藏。
- **掉落**：37 帧 @60 Hz（约 0.6 s），下落 520 DIP，倾角从 `tilt` 增大到
  `tilt + (tilt×7+20)`，`1-e` 淡出；卡片带完整相框/夹子。
- **飞行**：1600 px 图 82 帧 / 1.31 s（含落地淡出），62 fps；相框 10%–55%
  淡入，落地与线上卡片尺寸/圆角一致，不再出现“裸图盖住已落地的相框”。

## 5. 本轮数值（同机 release）

| 项 | 实测 | 备注 |
|:--|:--|:--|
| `pegline.exe` | **1,385,984 B（1.32 MB）** | 预算 ≤ 10 MB；含 2.0.0 版本资源 |
| 6 卡线带 @60 Hz | **60.30 fps**，p95 16.96 ms，draw p50 0.79 / p95 1.11 ms | 发布前复测（≥ 58 fps） |
| 飞行（1600 px，复用 3 次） | **61.52 fps**，draw max 10.9 ms | 发布前复测；最大值为首帧上传 |
| 私有内存（6 卡、probe 可见） | **92.4 MB** | 用户已接受；与上一轮 92.0 MB 持平 |
| 私有内存（飞行） | **89.3 MB** | 低于 100 MB MVP 门槛 |
| 私有内存（6 卡 1 帧） | **61.5 MB** | 之前 58.8 MB；+2.7 MB 来自第二张阴影和夹子阴影 |
| 200 次连拍 | 峰值 135.7 MB，45 s 后 **105.8 MB**（热身 104.2 MB，+1.5%），PASS |
