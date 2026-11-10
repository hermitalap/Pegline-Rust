# Pegline 2.4.1 发布说明

发布日期：2026-11-10
产物：`dist/Pegline-2.4.1-win-x64.zip`（单文件 exe + README + LICENSE + CHANGELOG）

`pegline.exe` 为 **1,819,648 B（1.82 MB）**，ProductVersion `2.4.1.0`。

## 2.4.1 修复

标注编辑器底部左侧的“撤销/重做”在不可用时使用 `#9A9A9A`；Mica 变成浅色壁纸
玻璃后，这两个标签几乎看不见。现在禁用标签改为 `#4A4A4A`，可用标签仍为
`#1E1E1E`，“取消/保存”不变。

- 布局、按钮尺寸、点击区域、撤销/重做逻辑和动画均未改动。
- 不增加内存，不改变探针帧率/绘制时间。
- 视觉证据：`docs/visual/editor-disabled-actions.png`；验证记录见
  `docs/VISUAL-TESTS.md` §3.19。

## 验证

- `cargo test`：**212 passed，0 failed，1 ignored**
- `cargo clippy --all-targets -- -D warnings`：通过
- `cargo fmt --check`：通过
- 以 `--editor-demo --editor-hold` 在 200% DPI 下检查：禁用“撤销/重做”清晰可读，
  “取消”近黑，“保存”白字蓝底。

## 校验和

```
SHA256  45483f9d4e00c8ff4dabd2c6a7ef8204e992d5ce436a201ff5b0c1de04397d38  Pegline-2.4.1-win-x64.zip
```

## 升级

解压覆盖旧 `pegline.exe` 即可；设置、便签和截图目录不变。若要回退，2.4.0 的
产物仍在 `dist/Pegline-2.4.0-win-x64.zip`。
