# Changelog

## 2.3.0 — 2026-11-06

### Changes

- **Picture and note hover previews now share one frame.** A picture's
  preview wears the same glass card as a note's: a 4 DIP inset (it was a
  6 DIP white frame), the same 16 DIP radius, hairline border and baked
  shadow, inside a 30 DIP transparent window margin so the shadow is no
  longer clipped. The visible card keeps the 12 DIP gap from the card above
  and the 12 DIP work-area margins, and the 900×650 DIP cap describes the
  card: a 4000×3000 source at 200 % measures 1728×1300 px. The note preview
  comes up by the same 30 DIP, so both hang 12 DIP below the hover controls.
- **The note editor continues Markdown lists.** Enter on a list line starts the
  next item with the same bullet (indentation kept, a task continues unchecked)
  or the following ordered number; Enter on an empty item removes the marker and
  ends the list, and one Backspace deletes the whole auto-inserted marker.
- **Hovering a note on the line shows a preview, like a picture.** After the
  same 0.35 s dwell, the note's summary is rendered as a 408×264 DIP sheet
  inside the note's glass card and hangs below the card's controls. It is
  rendered on a worker, is click-through like a photo preview, and is released
  when hidden. `--note-preview-demo` parks a synthetic pointer on one demo note
  for the visual acceptance run.

### Fixes

- **The annotation text tool draws the IME composition itself.** Pinyin used to
  be painted by the input method in its own transparent floating window, hanging
  off the white box's bottom-right corner and drifting right with every committed
  character, because the tool only read `WM_IME_CHAR` and let `WM_IME_*` fall
  through to `DefWindowProc`. It now handles the composition messages, reads
  `GCS_RESULTSTR` (the single commit path) and `GCS_COMPSTR`/`GCS_CURSORPOS`,
  clears `ISC_SHOWUICOMPOSITIONWINDOW`, and lays out `text + composition` with
  the pinyin underlined inside the box; the box grows with it and the caret sits
  after the composition cursor. The candidate list is anchored under the box and
  re-anchored once the render thread has measured the new composition, so it no
  longer lags a keystroke. Esc cancels the pinyin and keeps the text box; a
  click, save or focus loss cancels an open composition instead of baking raw
  pinyin into the PNG. Verified with Microsoft Pinyin at 200% DPI; see
  `docs/VISUAL-TESTS.md` §3.16.
- The note editor no longer deletes two characters per Backspace (and inserts
  two lines per Enter): Tab, Enter and Backspace are handled from the key-down
  message only, not again through `WM_CHAR`.
- The unordered-list bullet is centred on the text's optical middle (0.33 em
  above the baseline, measured with Microsoft YaHei UI) instead of on the line
  box, whose spare 1.55 line spacing sits below the baseline and dropped the
  dot onto the descenders. The task checkbox follows the same line.
- The note editor no longer logs `EndDraw(paper): D2DERR_WRONG_STATE` when it
  opens. The paper image cached for editor flights painted its inline-code
  backgrounds and selection into the window's device context instead of the
  WIC target it was drawing into, which corrupted the context state; both now
  use the target passed to the paint routine.
- The new-note entry no longer jumps sideways when a picture or note in front of
  it is removed. The peeking corner and its hit area ride the same horizontal
  spring as the row, so the entry slides to its new slot with the rest of the
  line instead of snapping there.

## 2.2.0 — 2026-11-06

### Changes

- **Hover buttons and default clicks follow the card kind.** A picture's left
  hover pill is now **Annotate** (the annotation-editor entry) and its right
  pill stays Open; a note keeps **Copy / Open**. A single click still copies a
  picture, while a single click on a note now opens the WYSIWYG note editor.
- **Editor flights no longer carry an enlarged thumbnail.** Before a tilted
  card flies into the annotation editor it turns upright with a short damped
  angular spring, then the flight carries a source image decoded at up to
  2048 px. Note flights start moving at once and cross-fade the card paper into
  the editor's own paper (rendered once through the WIC paint path, 3.7–8.7 ms
  at 920×720) during the first 0.12 s, so there is no fade-then-fly pause; the
  return flight carries the committed editor snapshot. The clothespin belongs to
  the line: a returning note shows it only during the last 0.18 s as it reaches
  the slot, and an existing note leaving for the editor fades its pin out in the
  first 0.12 s instead of dragging it to the editor. The annotation return
  carries a 2048 px marked preview instead of the 720 px card preview. The
  editor window keeps its last frame and composition target until the return
  flight's first frame is up, which removes the one-frame flicker on exit. See
  `docs/BEHAVIOR.md` §9.5, §10.1, §11 and `docs/DECISIONS.md` D10.
- **The annotation editor is now a floating hanging picture.** It has no title
  bar and no window buttons: a frameless, transparent, always-on-top window
  holds an enlarged white-glass card with a clothespin and a baked shadow, and
  the toolbar floats around it as five glass pills — tools, colours and line
  widths above the card, undo/redo and Cancel/Save below it, 18 DIP from
  the card and 26 DIP from the window edge. Inside the pills every control is
  concentric with it: the tools, colour dots and line-width buttons are
  circles, the wide action buttons are stadiums. The four tool icons are the
  open-source Lucide glyphs (ISC/MIT, sources and license in `assets/icons/`). The picture is fitted to the work area
  (never upscaled, capped at 1400×1000 DIP), the photo and its marks are painted
  into an opaque card bitmap so text keeps ClearType, and the pads are
  composited through a rounded mask. Opening from a card grows the picture out of
  its slot and shows the editor when the flight lands; clicking outside the card,
  losing focus, Ctrl+S or the Save button writes the PNG and shrinks the marked
  flight preview (up to 2048 px) back onto the line, while Esc cancels and flies
  the untouched picture home. The source card is hidden for the whole edit, so the
  line never shows a second copy. See `docs/BEHAVIOR.md` §11.
- The annotation toolbar has a fifth tool: an **eraser** (fourth in the row)
  removes whole marks under its 14 DIP disc, a drag counts as one undo step and
  Esc cancels the stroke. The mosaic moved to fifth and now uses Lucide's
  `eye-off` glyph. The three line widths are shown as 4/8/12 dots instead of
   lines, and the selected width gets a ring rather than a fill so the dot's
   size stays readable.
- Restored cards now remember the picture's real pixel size instead of the
  thumbnail's, so their aspect ratio, the annotation flight and the editor
  layout are right after a restart.
- A development switch `--annotate-demo` hangs one demo card and opens the real
  Annotate flow on it, so the two flights can be captured without a mouse.
- **Sticky notes now have a WYSIWYG editor.** An invisible entry appears to the
  right of the last card: after 120 ms a blank note flies in to the slot where it
  would hang and bobs there. The mouse pulls it with a distance/direction force
  against a damped return spring, so it lags, overshoots and settles instead of
  following rigidly; leaving the entry flies it out again. Clicking it flies to a
  460×360 DIP paper at the centre of the screen. The caret line shows
  its Markdown source while the rest is rendered live (headings, bold, italic,
  strike, inline code, fenced code, lists, tasks, quotes, links, rules). Clicking
  outside, losing focus, Esc or Ctrl+S commits; the note is written atomically as
  UTF-8 BOM + CRLF and flies back to its card. Empty new notes write nothing;
  empty existing notes and every way of taking a note down move the file into
  `Notes\Archive`. The editor is borderless, has no scrollbars, and takes keyboard
  focus and input methods, including inline Chinese composition. See
  `docs/NOTE-EDITOR.md` and `docs/DECISIONS.md` D9.
- All non-symbol text now uses `Microsoft YaHei UI`: the card labels and hint,
  the annotation editor's toolbar and text tool, the shortcut recorder and the
  sticky-note body. Only the undo/redo arrow glyphs keep `Segoe UI Symbol`; the
  toolbar tool icons remain vector drawings.
- ClearType is now explicit per surface: the opaque annotation editor and the
  GDI shortcut recorder use subpixel ClearType, while the transparent line
  overlay and the 3x-supersampled notes use grayscale antialiasing. Saved
  annotation PNGs are forced to grayscale so the file never contains coloured
  subpixel fringes.
- A picture's context menu now says **Annotate** and opens the annotation
  window directly. Saving writes the marked-up PNG back over the card's file
  and refreshes the thumbnail in place; cancelling leaves the file untouched.
  A note's **Edit** now opens the WYSIWYG editor, and its old **Discard**
  command is **Archive**.
- Removed the old **Ctrl+Alt+S** "screenshot then annotate" flow, along with
  its tray menu item and shortcut recorder entry. The note shortcut
  (`Ctrl+Alt+N`) and the fixed show/hide shortcut (`Ctrl+Alt+T`) remain. A
  legacy `snipKey` in `settings.ini` is preserved but ignored, so switching
  back to the WPF build keeps working.
- The annotation window now has the Pegline icon in its title bar and Alt-Tab,
  and its toolbar was regrouped into tools, colours, line widths and actions.
  Gaps inside a group are tighter than the gaps around 1 px separators, the
  undo/redo pair sits with Cancel/Save on the right, tool icons are drawn as
  crisp vector strokes, and each line-width button previews the actual stroke.
  A compact layout keeps everything inside the 720 DIP minimum window in all
  three languages.

### Fixes

- Editing an existing note no longer leaves a second copy hanging on the line
  while its paper is in the editor. The card is hidden under the opening
  flight's first frame (`SetRehang`, so the row keeps its slot) and reappears
  with the return flight; a failed save or archive puts it back immediately.
- The annotation arrow's head now points the way the drag went and sits on the
  end of its shaft. `atan2` returns radians while `Matrix3x2::rotation` wraps
  `D2D1MakeRotateMatrix`, which takes **degrees**, so every head was rotated by
  a fraction of a degree: an arrow drawn left, up or down grew a right-pointing
  triangle beside its line instead of on its tip. The shaft end and the head
  transform now come from one `arrow_head` helper, covered by a regression test
  over all four directions (see `docs/visual/editor-arrows.png`).
- Choosing an item from a card or tray context menu no longer also replays the
  click on the card control underneath it. The low-level mouse queue is now
  discarded after the modal menu closes, so a card cannot be opened or copied
  as a side effect of picking "Annotate" or another menu item.

## 2.1.1 — 2026-11-03

### Changes

- The line drops for 2.5 s on every launch (5 s on the first run, together
  with the empty hint), so a start is visible instead of just a tray icon
  appearing. It tucks itself away as usual unless the pointer is inside the
  strip.

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
