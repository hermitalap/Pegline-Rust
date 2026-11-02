# WPF 版（v1）归档

这是 Pegline 的 **C# / WPF 实现**，2026-11-02 冻结，作为 Rust 重写的行为参考。
它仍然是可构建、可运行的完整程序；只是不再继续开发。

## 为什么在这里

仓库根目录正在重写为 Rust + Win32 + DirectComposition。旧实现整体移到这里，
好处是：

- 新工程从空仓库开始，`src/` 不会被两套代码争用；
- 旧代码随时可以构建，用来对照行为、复现 bug、对比帧率与内存；
- 历史与设计记录（`PLAN.md`）跟着代码走。

## 构建与运行

需要 Windows 10/11 自带的 .NET Framework 4.8（不需要安装任何 SDK）：

```powershell
.\build.cmd          # -> .\build\Pegline.exe
.\test.ps1           # 必须打印 "All preview tests passed."
.\build\Pegline.exe
```

产物在 **本目录** 的 `build\` 下（已被仓库 `.gitignore` 忽略）。

## 目录

| 路径 | 内容 |
|:--|:--|
| `src/` | C# 源码，一个类一个文件 |
| `tests/` | 预览布局与窗口测试 |
| `tools/MakeIcon.cs` | 构建时生成图标 |
| `build.ps1` / `build.cmd` / `test.ps1` | 构建与测试脚本 |
| `README.md` | 原产品说明（操作、差异、数据位置） |
| `AGENTS.md` | 原开发约定（构建规则、性能教训） |
| `PLAN.md` | 原优化计划与全部实测数据 |

## 没有归档的东西

- `build/`：生成的 exe、图标、探针。探针源码当时在根目录的 `build/probe/`，
  未纳入版本控制；其中 `MemProbe` / `LadderProbe` / `DecodePeak` 的测量结论已经
  整理进 `docs/MEMORY-ANALYSIS.md`。
- 运行中的数据（`%APPDATA%\Pegline`、`%LOCALAPPDATA%\Pegline`）：与 Rust 版共用，
  见下。

## 与 Rust 版共存

- 两个版本读写**同一套**数据：`settings.ini`、`pegged.txt`、`Screenshots`、`Notes`。
- 两个版本使用同一个单实例互斥体名 `Local\Pegline.SingleInstance`，所以不能同时运行。
- 托盘图标、开机启动注册表值（`Pegline`）也同名：后启动的版本会覆盖前一个的启动项，
  这是有意的，方便来回切换。

## 许可

代码 MIT（仓库根目录 `LICENSE`）。上游 Tendedero 要求修改版换名换图标，
本实现使用 `Pegline` 名称与自有图标，符合要求。

## 备注

归档时根目录的 `PLAN.md` 被新计划覆盖，本目录的 `PLAN.md` 依据会话记录逐字重建，
内容与原文件一致（包括 2026-11-02 追加的内存分析一节）。
