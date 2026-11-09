# Pegline 2.4.0 发布说明

发布日期：**2026-11-09**
平台：Windows 10 / 11，x86_64
产物：`dist/Pegline-2.4.0-win-x64.zip`（单文件 exe + README + LICENSE + CHANGELOG）

## 这是什么

2.4.0 把线带卡片、操作/取色药丸、飞行框、悬停预览、拖拽幽灵和标注编辑器
的玻璃表面，从一块固定的半透明白色换成**壁纸派生的 Mica 材质**：读取当前
桌面壁纸，按虚拟屏幕等比裁切、模糊、取 72% 应用白 + 28% 壁纸，再加一层
很轻的噪声，按屏幕坐标采样。它不会实时采样窗口后面的内容——这是与系统
Acrylic 的明确边界；换来的是零新运行时、Win10 也能用，并且跟随壁纸和显示
布局变化。

## 安装与升级

- 解压后运行 `pegline.exe`，没有安装程序，没有 .NET/WinUI 运行时依赖。
- 数据仍在 `%APPDATA%\Pegline` 与 `%LOCALAPPDATA%\Pegline`，与 2.3.x 共用。
- 透明效果关闭、壁纸读取失败或上传失败时，自动回退到 2.3.x 的
  `#B8F7F7F7` 实色玻璃；不会出现空白窗口。

## 2.4.0 的主要变化

完整列表见 [CHANGELOG.md](../CHANGELOG.md)。要点：

- 所有有填充玻璃的表面使用同一张壁纸 Mica 纹理；线带、飞行、预览、幽灵
  共享进程级 CPU 图像，每个 renderer 只上传一份约 2.3 MB 的纹理。
- 壁纸变化（`WM_SETTINGCHANGE` / `SPI_SETDESKWALLPAPER`）和显示布局变化
  （`WM_DISPLAYCHANGE`）会重建纹理；卡片旋转/缩放时纹理仍锚在屏幕上，不
  会跟着卡片转。
- 卡片常态玻璃不透明度 82%，悬停 95%；选中的工具按钮仍用实色强调色。
- 便签编辑器没有填充玻璃面，只保留原来的 1.5 DIP 渐变边框。

## 验证

同机实测（release，6016×3384 @200%，60 Hz，单屏，Intel Arc B390）：

| 项 | 2.3.2 | 2.4.0 |
|:--|:--|:--|
| `probe --mode=line` 私有内存 | 92.2–92.5 MB | **97.0 MB** |
| line 帧率 | 59.95 fps | **59.95 fps** |
| line draw p50 | 0.80 ms | **1.635 ms** |
| `probe --mode=static` 私有内存 | 58.8 MB | **66.0 MB** |
| `probe --mode=flight` 私有内存 | 76.7 MB | **81.4 MB** |
| 首帧 Mica 构建 | — | **54.276 ms**（仅首次） |
| `--prim=card6` draw p50 | — | **0.323 ms** |
| app `--demo` 私有内存（6 s） | 170.3 MB | **178.8 MB** |

`pegline.exe` 为 **1,819,136 B（1.82 MB）**，仍远低于 10 MB 预算。首帧
54 ms 的构建发生在创建 renderer 时，后续帧只做一次纹理采样；line 的
draw p50 仍远低于 8.3 ms 预算。

视觉证据：

- `docs/visual/mica-editor-testwall.png`：标注编辑器的五个药丸随红/绿/蓝
  测试壁纸改变底色，卡片的白色边框保持不变。
- `docs/visual/mica-line-testwall.png`：线带卡片在红/绿测试壁纸前的样子。
- `docs/visual/mica-line.png`：深色真实壁纸前的线带。

## 已知限制

- **不采样实时窗口**：壁纸派生的 Mica 在壁纸前成立，在窗口前不是 Acrylic。
  需要实时背景模糊只能迁到 Windows.UI.Composition 的 host backdrop，代价
  见 `docs/DECISIONS.md` D12 和 `docs/research/ACRYLIC-MICA.md`。
- **4 DIP 卡片相框几乎看不出材质**：卡片只有 4 DIP 玻璃环，200% 下 8 px；
  真正能看出材质的是编辑器药丸、预览和线带药丸。没有为此加宽相框。
- **壁纸样式**：当前按虚拟屏幕等比 cover 裁切，适配 Windows 默认的“填充”；
  “居中/平铺/跨区”会出现位置或重复差异，未单独适配。
- **首帧 54 ms**：壁纸解码、模糊、上传仍在 renderer 创建线程上，没有后台化。
- **多显示器**：按虚拟屏幕定位，副屏原点/负坐标已有映射，但未在双屏硬件上
  实测；120 Hz、休眠/唤醒同样未实测。
- 标注编辑器的文字/IME、线带交互等行为未改动，沿用 2.3.2 的验收记录。

## 校验

```
SHA256  46bbda42710bf6a4b2a353fbd4f472f576097bbe6c2d01ecb3e6e8aa2bf55ccb  Pegline-2.4.0-win-x64.zip
```

## 从源码构建

```powershell
cargo test
cargo clippy --all-targets -- -D warnings
pwsh tools\package-release.ps1
```

`tools/package-release.ps1` 会重新跑测试和 clippy、检查 exe 内嵌的 2.4.0
版本资源、生成 zip 和 `.sha256`。
