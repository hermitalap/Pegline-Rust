# Changelog

## 2.1.0 — 2026-11-03

### Fixes

- The landing hand-off is now seamless: the real card is already opaque
  underneath, while the flight window mirrors the card's row spring and 2.2°
  landing sway and fades over it. The two copies stay aligned, so the card no
  longer ghosts sideways or flickers through a double-transparency dip.

- `Win+Shift+S` and PrtScn captures take off from where they were taken again:
  the clipboard path decodes and sends the same 1600 px flight texture as a
  file capture, so the card no longer appears on the line out of nowhere.
- A card no longer shows on the line while it is still flying. It lands one
  full slot clear of the current right end, then the whole row — including the
  new card — slides left half a spacing into the centred layout.
- One screenshot no longer hangs twice when the clipboard copy and the
  Windows-saved file arrive at the same moment. The second capture now checks
  the line for the other half (same size and thumbnail, other owner) and folds
  into it; the twin window grew from 4 s to 8 s because Windows can save the
  file a few seconds after the snip.
- Taking a card down falls straight down instead of swinging to the left: the
  drop rotates about the card's own pin and no longer spins, travels 200 DIP
  (was 520) in 0.32 s (was 0.55).
- Card button labels are 13 px instead of 11 px. Chinese uses Microsoft YaHei
  UI Medium — the system's Segoe UI fallback at 11 px filled in dense glyphs
  such as "复" — and text origins snap to the device pixel grid.

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
