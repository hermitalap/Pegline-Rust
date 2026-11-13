## 2.6.2 — 2026-11-13

### Changes

- **The clothespin drag is now direct manipulation.** The dragged card sits
  exactly under the pointer (1:1, no follow spring and no lag), the neighbour
  slides one slot with a single critically damped 0.24 s spring, and the order
  changes when the card centre crosses the neighbouring slot's midpoint by
  12 DIP — 24 DIP of hysteresis instead of a dwell timer, a cooldown and a
  re-arm distance. The pendulum, air drag, relative wind, flex shear, squeeze,
  swap hop/recoil and all their tuning plumbing are deleted:
  `src/scene/pendulum.rs`, `src/scene/trace.rs`, `docs/PAPER-TUNING.md`,
  `tools/paper-trace.ps1`, `tools/paper-plot.py`, `tools/paper-sim.html` and the
  `PEGLINE_PAPER_*` variables are gone. Three springs (neighbour 0.24/1.0,
  release 0.30/1.0, speed lean 0.14/1.0) and two thresholds (4 DIP slop,
  12 DIP order deadband) are the whole gesture. A 4° lean scaled by pointer
  speed keeps the weight cue; release inherits the pointer velocity (capped at
  600 DIP/s) and settles without overshoot; `pegged.txt` and `Tink` still fire
  once, only when the order actually changed.
- **The note editor's bottom-right resize anchor is 16×16 DIP.** 24² = 576 DIP²
  becomes 16² = 256 DIP², a 56 % area cut. The edge bands stay 2 DIP and the
  diagonal resize cursor still appears as soon as the pointer enters the anchor.
- **A pin drag no longer drops the "new note" entry.** While the clothespin is
  held, the entry ignores the pointer even when the drag crosses its hotspot,
  and the peeking corner retracts on the existing 0.22 s reveal spring. The next
  tick after release brings the corner back the same way (or drops the full note
  if the pointer is still over the hotspot).

### Verification

- 248 lib tests + 1 main test pass; `cargo clippy --all-targets -- -D warnings`
  and `cargo fmt --check` are clean.
- Real mouse at 200 % DPI (`--demo`, 3 cards): 1:1 tracking, one reorder at the
  midpoint, the neighbour glides a full slot with no overshoot, the row settles
  on the three slots after release (`docs/visual/pin-2.6.2-*.png`).
- Real mouse: dragging a card across the note-entry hotspot leaves the entry
  alone and retracts the peeking corner (`note peek false`, no hover lines); it
  reappears with the same spring after the release (`pin-2.6.2-entry-*.png`).
- `probe --mode=line`: 60.06 fps, draw p50 0.677 / p95 1.027 ms, private
  97.0 MB; `pegline.exe` 1,903,104 B (2.6.1: 1,920,512 B).

## 2.6.1 — 2026-11-13

### Fixes

- **The bottom hover preview no longer appears while a clothespin is held.**
  When the pin takes the pointer, the UI stops reporting a card hover, clears
  any pending preview and ignores a preview decode that finishes mid-drag; the
  0.35 s preview starts again only after the button is released. This also
  removes the stale preview that could stay visible underneath the dragged
  card. The pin gesture itself is unchanged.
- **The note editor's bottom-right resize anchor is easier to hit.** The corner
  was the 2 DIP intersection of the right and bottom bands; it is now a
  24×24 DIP rectangle reaching into the paper and over the shadow margin. The
  edge bands stay 2 DIP, and the diagonal resize cursor appears anywhere in
  the anchor, so it is visible well before the old 2 DIP strip.

## 2.6.0 — 2026-11-13

### Features

- **Drag a card by its clothespin.** Hovering the pin scales it to 1.12, adds a
  warm white glow and brightens the metal; holding the left button drags the
  card with a 0.075 s spring, an inertial release and a downward-only rubber
  band on Y (linear to 18 DIP, then 0.22x, capped at 26). The pin shrinks to
  0.94 while pressed and tracks at 1.08
  while dragging; a click shorter than 4 DIP is a no-op.
- **The right neighbour is squeezed and swapped.** The card ahead of the drag
  is pushed by the pre-collision penetration (34 DIP cap, 10 for a second card)
  and rotates up to 3 degrees. At 28 DIP for 60 ms the two cards swap slots:
  the neighbour springs left at 260 DIP/s with a 6 DIP hop, the dragged card
  recoils at 160 DIP/s and keeps following the pointer, so it floats over its
  neighbour until release. A 180 ms cooldown and a 43 DIP re-arm prevent
  chattering; dragging left mirrors the whole behaviour.
- **The cards swing like paper, not like an iron block.** Each card gets its
  own effective pendulum length from its size (`L_eff=(w²/12+h²/3)/(h/2)`,
  60–220 DIP), so a landscape photo, a portrait photo, a square and a note have
  different periods. A hand-written integrator uses linear damping 1.116 plus
  quadratic air drag 0.279 (aspect-ratio scaled: a tall card gets 1.550/0.589);
  horizontal acceleration drives the angle (atan(a_x / g_eff), ±6 degrees), and
  the Y drop also excites it by changing the effective gravity. A ±1.2 degree
  shear, driven by the angular velocity, bends the card slightly around the
  pin. A normal stop or swap now gives one visible overshoot and settles within
  about half a second instead of ringing two or three times; the CSV trace counts the
  small-angle ring separately. The popped neighbour gets the same sway from its
  260 DIP/s impulse; the 6 DIP hop is only a hint, not a jump.
- **Paper picks up air drag.** Each card's linear damping, quadratic drag and
  wind gain now scale with its aspect ratio (a tall card gets 1.550/0.589 and a
  stronger wind gain); a relative-wind torque at the centre of pressure
  (`Q=wind_gain·|v_p|·(v_p·(cosθ,sinθ))·r`) tilts a card back while it moves.
  `PEGLINE_PAPER_TRACE` writes a per-frame CSV; `tools/paper-trace.ps1` drives a
  demo drag and `tools/paper-plot.py` plots angle/target, angular and linear
  velocity with peak/zero-crossing/settle metrics. The earlier 0.45/0.05 baseline
  was 5.50°, 7 crossings (including small-angle ring) and 817 ms to settle; the
  stronger-air preset is tuned with `tools/paper-sim.html` and by hand.
- **The order persists.** On release the UI thread reorders `line.items` from
  the scene snapshot and writes `pegged.txt` only when the order changed,
  playing `Tink` once. Pin hover owns the card's hover state, so hovering the
  pin does not show the action pills or the large preview.
- The pin glow is baked once at startup and on DPI changes; each frame adds one
  bitmap blit. Measured at 200% DPI: 59.88–60.16 fps, draw p50 0.585–0.701 ms,
  private 96.9–98.9 MB (HEAD baseline 60.10/60.06 fps, 0.658/0.660 ms, 97.1–97.4 MB);
  255 lib + 1 main tests pass (1 ignored).

## 2.5.1 — 2026-11-12

### Changes

- **The note editor now resizes around its centre.** Dragging the paper's right
  edge, bottom edge or bottom-right corner used to keep the top-left corner
  fixed and grow the window to the right and down. The dragged edge now tracks
  the pointer 1:1 and the opposite edge mirrors it, so the paper grows (or
  shrinks) by twice the pointer's travel — half on each side — and the paper's
  centre never moves. The window is centred in the work area when the editor
  opens, and the work-area edge only clamps the size; it never shifts the
  centre. The size range (460×360 to 820×640 DIP), the clamps, the hover strip,
  the per-note size memory and the rendering are unchanged.

## 2.5.0 — 2026-11-12

### Features

- **The note editor resizes.** Drag the paper's right edge, bottom edge or
  bottom-right corner to grow it from 460×360 to 820×640 DIP (window 500×400 to
  860×680). The top-left stays put and the window grows right and down; the paper,
  fold, shadow, text layout, selection and scroll position follow live. Only a
  width change reflows the text; a height-only drag just re-clamps the scroll.
  Dragging stops at the work-area edge; the left/top edges and the window position
  are not adjustable. Resizing is not undoable and does not dirty the note.
- **Hover feedback on the resize bands.** The hit edge shows a 2 DIP
  `COLOR_HIGHLIGHT` strip and the cursor becomes `IDC_SIZEWE` / `IDC_SIZENS` /
  `IDC_SIZENWSE`. The strip is drawn in the window layer, so the paper bitmap keeps
  ClearType and the return flight never carries it.
- **Each note remembers its size.** Dragging writes
  `noteSize:<file name>=WxH` (whole DIP) to `settings.ini`; reopening uses it.
  Dragging back to 460×360 removes the key, archiving removes it, a new note is
  only recorded once its file exists, and a discarded new note records nothing.
  Missing, corrupt or out-of-range values fall back to (or clamp to) the default.
- **DPI and work-area changes keep the chosen size.** `WM_DPICHANGED`,
  `WM_DISPLAYCHANGE` and work-area changes re-flow around the growth envelope
  instead of returning to 500×400; a work area too small for the minimum shrinks
  the paper below its minimum and fills the work area.

### Implementation

- Hand-written `SetCapture` + `SetWindowPos` resize instead of `WS_THICKFRAME`
  (no extra non-client pixels, no modal sizing loop re-entering the mouse hook,
  IME or commit logic); hit testing is pure geometry in
  `note_editor::display::hit_test`.
- The shadow is a 9-patch atlas baked once per DPI (48 DIP corners, 8 DIP
  middles) instead of a full-window blur per resize step.
- Measured: `probe --mode=note` opens at 56.5–57.1 MB private for 460×360 and
  72–73 MB for 820×640 (200% DPI), draw p50 0.20–0.31 ms / p95 0.23–0.33 ms,
  60 fps budget untouched. MAX↔MIN round trips settle at a one-time 9–15 MB
  high-water mark, not a per-cycle leak.

# Changelog

## 2.4.2 — 2026-11-12

### Fixes

- **The note editor shows the text-editing I-beam cursor.** The editor window
  registered a NULL class cursor and never handled `WM_SETCURSOR`, so the arrow
  (or whatever cursor the window underneath had left) stayed over the editable
  paper. The paper now shows `IDC_IBEAM`, the shadow margin `IDC_ARROW`, and a
  drag selection keeps the I-beam even when the pointer leaves the paper.
- **Drag selection keeps up with the pointer.** The render thread handled one
  queued command per frame and then waited for vsync, so pointer moves from
  125–1000 Hz mice piled up without bound. Measured on the reference machine,
  600 moves over 0.9 s: queue depth p50 290 / max 543, applied-move lag p50
  4.48 s / p95 8.58 s, and the selection was still catching up about 9 s after
  the pointer stopped. A frame now drains the queued input (up to 512 commands)
  and applies only the last of a run of pointer moves, keeping every other
  command in order: queue max 12, applied-move lag p50 0.76 ms / p95 1.58 ms,
  fully caught up 1.1 ms after the last move.
- **Ctrl+C works in the note editor.** `VK_C` was missing from the key map, so
  the copy shortcut was unreachable even though Ctrl+X worked.

### Performance

- A caret move within the current line no longer re-parses the Markdown or
  rebuilds the text layouts: one rebuild drops from p50 29 µs to 0–1 µs. The
  frame is dominated by the vsync-blocked `Present` (p50 ~15.7–16.6 ms), so
  this is a small additional saving rather than the drag fix.
- `PEGLINE_TRACE=1` now also writes rate-limited `note editor drag:` and
  `note editor loop:` statistics (queue depth, batch size, apply lag, handle and
  draw time); the instrumentation is silent and costs nothing when traces are
  off.
- `PEGLINE_ALLOW_SECOND_INSTANCE=1` lets the scripted acceptance runs start a
  second copy beside the tray app; a normal start is still single-instance.

## 2.4.1 — 2026-11-10

### Fixes

- **Disabled Undo/Redo labels are readable on the Mica pills.** The annotation
  editor drew disabled action labels in `#9A9A9A`, which nearly disappeared on
  the light wallpaper glass; they now use `#4A4A4A`. Enabled actions stay at
  `#1E1E1E`. No layout, animation or memory change.

## 2.4.0 — 2026-11-09

### Changes

- **The glass now carries the desktop wallpaper.** Every filled glass surface —
  line cards, the peek note, action and colour pills, the flight frame, the
  hover preview, the drag ghost and the annotation editor's card and floating
  pills — samples a wallpaper-derived Mica texture instead of one flat
  `#B8F7F7F7` fill. The texture is decoded once, cover-fitted to the virtual
  screen, blurred, tinted 72 % toward the app's white glass and given a faint
  grain; it is cached across windows and rebuilt after a wallpaper or display
  change. When transparency is off or the wallpaper cannot be read, the old
  solid glass is the fallback. The note editor has no filled glass panel, so
  its 1.5 DIP border is unchanged.

### Performance

- `probe --mode=line`: 97.0 MB private, 59.95 fps, draw p50 1.635 ms
  (2.3.2: 92.2–92.5 MB, 0.80 ms). The first frame after startup spends about
  54 ms building the texture; later frames are unaffected. Details and the
  per-surface limits are in `docs/DECISIONS.md` D12 and `PLAN.md` §11.

## 2.3.2 — 2026-11-07

### Changes

- **The line can stay down on its own.** A new **Keep line on screen** tray
  toggle (Chinese “挂绳常驻”, off by default, `lineAlways` in `settings.ini`)
  keeps the rope down and the strip present when the pointer moves away and when
  there are no cards. A full-screen app still takes precedence and the line
  comes back when it leaves; “Hide line” and a click on the top band put it away
  until the next reveal without clearing the switch.
- **The note editor's placeholder mentions Markdown.** It now reads “Write
  something… (basic Markdown supported)” / “Escribe algo… (compatible con
  Markdown básico)” / “写点什么…（支持 Markdown 基本语法）”, so the Markdown
  editing the note has always supported is discoverable from the empty paper.

## 2.3.1 — 2026-11-07

### Fixes

- **The card and tray menus came up as blank white boxes.** The popup drew its
  frame, separators and hover pill but not one character of its item text, and
  clicks still ran the right command. While a menu — or a message box, an OLE
  drag, the shortcut recorder — pumped this thread, the app's window procedure
  answered **every** message with 0 so it could not touch the `App` borrowed by
  the frame that entered the loop. A menu's owner still has to answer
  `WM_ENTERIDLE`, `WM_ACTIVATEAPP` and `WM_SETCURSOR`; without them USER32 laid
  the popup out at 0.835× and painted no glyphs. Undelivered messages now reach
  the default procedure, which never touches `App`. The same tray menu went from
  380×394 with no text to 455×472 with its Chinese labels, check marks and
  separators. `WM_APP_QUIT` also joins the queue of messages replayed after the
  loop, so a quit requested while a menu is open is no longer dropped. See
  `docs/VISUAL-TESTS.md` §3.17 and `docs/DECISIONS.md` D11.

### Changes

- The app, editor, flight and scene modules were split into submodules with no
  public path changes, and the review behind the split fixed the window
  lifecycles, the drag ghost's following and the DPI placement of a revealed
  line. Measured against 2.3.0: unchanged frame rate, draw time and memory
  (`PLAN.md` §10).

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
