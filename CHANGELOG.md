# Changelog

## 2.0.0 — 2026-11-03

The first complete Rust release. Pegline is now one 1.32 MB executable with no
installer and no runtime, replacing the archived C#/WPF 1.x line.

### Clothesline

- Click-through, always-on-top overlay that never takes focus; push the pointer
  against the top edge to drop the line, move away to tuck it back up.
- Spring-driven reveal/hide, idle wind, per-card tilt, flight/drop windows with
  baked shadows, frames and pegs.
- Cards: single click copies, double click opens, long press opens in Paint,
  drag out to Explorer/chat, discard to the recycle bin, context menu, hover
  actions, large preview below the buttons.
- Sticky notes with four colours, external-editor reload, duplicate handling.
- Capture from the screenshots folder, the clipboard and `Win+Shift+S`; twins
  from the same snip are merged; screenshots pause while a full-screen app is
  in front.

### Annotation editor

- Arrow, rectangle, text and mosaic tools, six colours, four line widths.
- Undo/redo, save at the source pixel size, cancel without touching the source,
  live resize with minimum-layout compaction.
- Text tool with a WPF-style input box, caret, IME context, `WM_CHAR` /
  `WM_IME_CHAR` commits, and a composition/candidate window anchored to the
  caret (fixed in this release).

### Shell

- Tray menu, show/hide, first-run screenshot takeover offer, screenshot
  shortcut plus two configurable hotkeys, shortcut recorder.
- English, Spanish and Chinese; Pop/Tink/Rustle cues; single instance; crash log.

### Numbers (6016×3384 @200%, 60 Hz, single monitor)

| Metric | Result |
|:--|:--|
| Executable | 1,385,984 B (1.32 MB), static CRT |
| Cold start to tray | 81–86 ms (warm cache); 105–107 ms measured cold |
| Six-card line at 60 Hz | 60.30 fps, p95 16.96 ms, draw p95 1.11 ms |
| 1600 px flight | 61.52 fps, draw max 10.9 ms |
| Private memory, six cards visible | 92.4 MB (accepted by the user) |
| Private memory, flight | 89.3 MB |
| 200 consecutive captures | +1.5% settled memory |

### Known limitations

- 120 Hz, a second monitor and sleep/wake could not be tested on the
  development machine. The architecture is refresh-rate independent, but no
  numbers are claimed for them.
- The three audio cues were verified as waveforms through WASAPI loopback; no
  subjective listening test was possible because the default endpoint is muted.
- Six visible cards use more private memory than the original 60 MB MVP target;
  the user accepted this trade-off. A 6K full-screen flight still peaks around
  155 MB.
- The Windows 11 input-method candidate window is anchored with IMM32 client
  coordinates. Verified with Microsoft Pinyin at 200% DPI; TSF-only IMEs were
  not separately tested.
