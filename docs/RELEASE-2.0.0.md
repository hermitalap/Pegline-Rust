# Pegline 2.0.0 发布说明

发布日期：**2026-11-03**
平台：Windows 10 / 11，x86_64
产物：`dist/Pegline-2.0.0-win-x64.zip`（单文件 exe + README + LICENSE + CHANGELOG）

## 这是什么

2.0.0 是 Pegline 从 C#/WPF 迁到 Rust + Win32 + DirectComposition 后的第一个
完整版本。功能与 1.x 对齐，并补上了悬停预览、便签、标注编辑器、拖拽预览、
音效、三语、快捷键录制和完整的托盘菜单；不再需要 .NET 或任何运行时。

## 安装与升级

- 解压后把 `pegline.exe` 放到任意目录运行即可，没有安装程序。
- 数据仍在 `%APPDATA%\Pegline` 与 `%LOCALAPPDATA%\Pegline`，与 WPF 1.x 共用；
  换回旧版或再换到 2.0.0 都不会丢线带上的图。
- 设置、便签颜色、启动项和已挂截图沿用原文件；2.0.0 不迁移也不删除任何
  用户数据。

## 2.0.0 的主要变化

完整列表见 [CHANGELOG.md](../CHANGELOG.md)。本版收尾修复了：

- 标注编辑器的文字工具没有 IME 上下文，拼音只出拉丁字母；
- 组合串和输入法候选窗停在屏幕左上角，不在文字框光标下方（本版重点修复）。

## 验证

同机实测（release，6016×3384 @200%，60 Hz，单屏）：

| 项 | 结果 |
|:--|:--|
| `pegline.exe` | 1,385,984 B（1.32 MB），静态 CRT |
| 冷启动到托盘 | 81–86 ms（热缓存，3 次）；冷缓存实测 105–107 ms |
| 6 卡线带 @60 Hz | 60.30 fps，p95 16.96 ms，draw p95 1.11 ms |
| 1600 px 飞行 | 61.52 fps，draw max 10.9 ms |
| 私有内存（6 卡可见 / 飞行） | 92.4 MB / 89.3 MB |
| 200 次连续截图 | 峰值 135.7 MB，45 s 后 105.8 MB（+1.5%），PASS |

验收清单见 [PARITY.md](PARITY.md)，逐项截图与逐帧记录见
[VISUAL-TESTS.md](VISUAL-TESTS.md)。

## 已知限制

- **120 Hz、副屏、休眠/唤醒**没有在开发机上实测。帧循环、弹簧和预算都不依赖
  固定刷新率，但发布说明不编造这三项的数字。
- **音效主观听感**未做。三段波形已用 WASAPI loopback 验证（Pop / Tink /
  Rustle 都到达系统混音器），但开发机默认音频端点是静音。
- 6 卡可见时私有内存 92.4 MB，高于最初的 60 MB MVP 目标；用户已接受这个
  取舍。6K 全屏飞行峰值约 155 MB，同样高于 100 MB 门槛。
- 输入法候选窗使用 IMM32 客户区坐标定位，已在 Microsoft Pinyin + 200% DPI
  下验证；只走 TSF 的第三方输入法未单独测试。

## 校验

```
SHA256  40f626f3eafe7fb0c85959b3fe7932c66e1d46fce73bbd8840b72130c698e894  Pegline-2.0.0-win-x64.zip
```

## 从源码构建

```powershell
cargo test
cargo clippy --all-targets -- -D warnings
pwsh tools\package-release.ps1
```

`tools/package-release.ps1` 会重新跑测试和 clippy、检查 exe 内嵌的 2.0.0
版本资源、生成 zip 和 `.sha256`。
