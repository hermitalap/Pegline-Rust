# REFERENCE-NOTES — exact facts from `archive/wpf-v1`

Source of truth for the Rust port. Every fact carries `file:line`; paths are relative to `archive/wpf-v1/src/`. The archive is frozen: where this document and the Rust code disagree, this document wins; where this document and the WPF code disagree, the WPF code wins. `\n` in a quoted string is a literal C# escape (a newline at run time); `\\` is one backslash. DIP = WPF device-independent unit (96/inch); "px" = device pixels.

## 1. Localized strings

Language: `CultureInfo.CurrentUICulture.TwoLetterISOLanguageName`; `zh` → Chinese, `es` → Spanish, anything else → English (`Support.cs:15–22`). No user-visible string contains `&` — the tray menu uses no `&`-accelerators. Ellipsis is always U+2026 `…`, never three dots.

| key / use | English | Spanish | Chinese | source |
|:--|:--|:--|:--|:--|
| offer.body (catch prompt, first run) | Screenshots will hang on the line the instant you take them, with Win+Shift+S or PrtScn, without saving them by hand and without piling up on your Desktop. Drag one to a folder to keep it, or discard it with the cross. You can turn this off from the tray icon. | Las capturas se colgarán al instante, con Win+Mayús+S o ImprPant, sin guardarlas a mano y sin acumularse en el Escritorio. Arrastra una a una carpeta para guardarla, o descártala con la cruz. Puedes desactivarlo desde el icono de la bandeja. | 用 Win+Shift+S 或 PrtScn 截的图会立刻挂到绳子上，不用手动保存，也不会堆在桌面。想留下就拖进文件夹，不要就点叉。随时可以在托盘图标里关掉。 | App.cs:193–200 |
| offer.title | Let Pegline handle your screenshots? | ¿Quieres que Pegline se encargue de tus capturas? | 让 Pegline 接管你的截图吗？ | App.cs:201–202 |
| shortcut.err.toggleInUse | Ctrl+Alt+T already shows and hides the line. | Ctrl+Alt+T ya muestra y oculta el tendedero. | Ctrl+Alt+T 已经用来放下 / 收起绳子了。 | App.cs:294–295 |
| shortcut.err.duplicate | Pegline already uses this combination for something else. | Pegline ya usa esta combinación para otra cosa. | 这个组合 Pegline 已经用在另一个功能上了。 | App.cs:297–299 |
| shortcut.err.taken | Another app is using this combination. Try another one. | Otra aplicación usa esta combinación. Prueba otra. | 这个组合已被其他程序占用，换一个试试。 | App.cs:301–303 |
| note.empty.body (`{0}` = shortcut) | There is no text on the clipboard. Copy some text, then press {0}. | No hay texto en el portapapeles. Copia un texto y pulsa {0}. | 剪贴板里没有文字。先复制一段文字，再按 {0}。 | App.cs:326–329 |
| note.empty.keyFallback | the shortcut | el atajo | 快捷键 | App.cs:329 |
| inbox.confirm.body (`{0}`=count, `{1}`=hanging) | Move the {0} screenshots Pegline caught to the Recycle Bin? {1} of them hang on the line and come down too.\n\nScreenshots saved to Pictures\\Screenshots (Win+PrtScn) are not touched. | ¿Mover a la Papelera las {0} capturas que guardó Pegline? {1} están colgadas y también se descolgarán.\n\nLas capturas guardadas en Imágenes\\Capturas de pantalla (Win+ImprPant) no se tocan. | 把 Pegline 接管保存的 {0} 个截图文件移到回收站？其中 {1} 张挂在绳子上，会一起取下。\n\n「图片\\Screenshots」里的截图（Win+PrtScn 存的）不受影响。 | App.cs:698–704 |
| inbox.confirm.title | Delete caught screenshots | Borrar capturas guardadas | 清空截图文件 | App.cs:705 |
| inbox.failed.body (`{0}` = leftover count) | {0} files could not be moved to the Recycle Bin. They may be open in another app. | No se pudieron mover {0} archivos a la Papelera. Puede que estén abiertos en otra aplicación. | 有 {0} 个文件没能移到回收站，可能正被别的程序打开。 | App.cs:712–714 |
| tray.hideLine (when revealed) | Hide line | Ocultar tendedero | 收起绳子 | App.cs:744 |
| tray.showLine (when tucked) | Show line | Mostrar tendedero | 放下绳子 | App.cs:745 |
| tray.snip | Take a screenshot | Hacer una captura | 截图 | App.cs:749 |
| tray.note | Hang copied text as a note | Colgar el texto copiado como nota | 把复制的文字挂成便签 | App.cs:751 |
| tray.keyNone | none | ninguno | 未设置 | App.cs:753 |
| tray.keyInUseSuffix (leading space) | ` (in use by another app)` | ` (en uso por otra aplicación)` | `（被其他程序占用）` | App.cs:755 |
| tray.snipTitle | Screenshot shortcut | Atajo de captura | 截图快捷键 | App.cs:756 |
| tray.noteTitle | Note shortcut | Atajo de nota | 便签快捷键 | App.cs:757 |
| tray.keySep | `": "` | `": "` | `"："` (U+FF1A, no space) | App.cs:758–759 |
| tray.takeAllDown | Take everything down | Descolgar todo | 全部取下 | App.cs:761 |
| tray.deleteInbox (`{0}` = count) | Delete caught screenshots ({0})… | Borrar capturas guardadas ({0})… | 清空截图文件（{0} 个）… | App.cs:764 |
| tray.catching | Handle screenshots | Encargarse de las capturas | 接管截图 | App.cs:767 |
| tray.catching.tip | Screenshots from Win+Shift+S and PrtScn hang instantly and skip the Desktop | Las capturas de Win+Mayús+S e ImprPant se cuelgan al instante | Win+Shift+S 和 PrtScn 的截图立刻挂上，不落到桌面 | App.cs:770–772 |
| tray.openFolder | Open screenshots folder | Abrir carpeta de capturas | 打开截图文件夹 | App.cs:774 |
| tray.sounds | Sounds | Sonidos | 音效 | App.cs:782 |
| tray.login | Open at login | Abrir al iniciar sesión | 开机启动 | App.cs:783 |
| tray.quit | Quit Pegline | Salir de Pegline | 退出 Pegline | App.cs:790 |
| card.copied (rendered as `"✓ " + s`) | Copied | Copiado | 已复制 | CardView.cs:192 |
| card.copyButton (glyph U+E8C8) | Copy | Copiar | 复制 | CardView.cs:199 |
| card.openButton (glyph U+E8A7) | Open | Abrir | 打开 | CardView.cs:200 |
| cardmenu.copy | Copy | Copiar | 复制 | Gestures.cs:183 |
| cardmenu.open | Open | Abrir | 打开 | Gestures.cs:184 |
| cardmenu.edit | Edit | Editar | 编辑 | Gestures.cs:185 |
| cardmenu.reveal | Show in Explorer | Mostrar en el Explorador | 在资源管理器中显示 | Gestures.cs:186 |
| cardmenu.saveDesktop | Save to Desktop | Guardar en el Escritorio | 保存到桌面 | Gestures.cs:188 |
| cardmenu.discard | Discard | Descartar | 丢弃 | Gestures.cs:192 |
| cardmenu.takeDown | Take down | Descolgar | 取下 | Gestures.cs:196 |
| cardmenu.trash | Move to Recycle Bin | Mover a la Papelera | 移到回收站 | Gestures.cs:197 |
| hint.emptyLine | Take a screenshot and it will hang here | Haz una captura y se quedará colgada aquí | 截一张图，它就会挂在这里 | LineWindow.cs:184–185 |
| inbox.filePrefix (caught screenshot) | Screenshot | Captura | 截图 | Watchers.cs:389 |
| notes.filePrefix | Note | Nota | 便签 | Notes.cs:56 |
| trash.failed.body (then `NewLine + path + NewLine + ex.Message`) | Taken off the line. Windows could not move the saved copy to the Recycle Bin; the copy is still here: | Retirada de la cuerda. Windows no pudo enviar la copia a la papelera; se conserva aquí: | 已从晾绳取下。Windows 无法把保存的副本移入回收站，副本仍保留在： | Line.cs:359–362 |
| shortcut.dlg.hint | Press the keys you want to use. Include Ctrl, Alt, Shift or Win, or use F1–F24 or PrtScn on their own. | Pulsa las teclas que quieras usar. Incluye Ctrl, Alt, Mayús o Win, o usa F1–F24 o ImprPant solas. | 直接按下想用的组合键。需要包含 Ctrl、Alt、Shift 或 Win；F1–F24、PrtScn 可以单独使用。 | Shortcut.cs:122–124 |
| shortcut.dlg.none | None | Ninguno | 未设置 | Shortcut.cs:129 |
| shortcut.dlg.noShortcut | No shortcut | Sin atajo | 不使用快捷键 | Shortcut.cs:150 |
| shortcut.dlg.cancel | Cancel | Cancelar | 取消 | Shortcut.cs:151 |
| shortcut.dlg.ok | OK | Aceptar | 确定 | Shortcut.cs:152 |
| shortcut.dlg.modsOnly | (no Loc call: `new Shortcut(mods, 0).ToString().Replace("None", "…")`) | same | same | Shortcut.cs:186 |
| shortcut.dlg.err.mods | Add Ctrl, Alt, Shift or Win. | Añade Ctrl, Alt, Mayús o Win. | 请加上 Ctrl、Alt、Shift 或 Win。 | Shortcut.cs:195 |
| editor.title | Mark up screenshot | Anotar captura | 标注截图 | ImageEditor.cs:48 |
| editor.arrow | Arrow | Flecha | 箭头 | ImageEditor.cs:80 |
| editor.rect | Rectangle | Rectángulo | 矩形 | ImageEditor.cs:81 |
| editor.text | Text | Texto | 文字 | ImageEditor.cs:82 |
| editor.mosaic | Mosaic | Mosaico | 马赛克 | ImageEditor.cs:83 |
| editor.undo | Undo | Deshacer | 撤销 | ImageEditor.cs:90 |
| editor.undoTip | Undo (Ctrl+Z) | Deshacer (Ctrl+Z) | 撤销 (Ctrl+Z) | ImageEditor.cs:91 |
| editor.redo | Redo | Rehacer | 重做 | ImageEditor.cs:94 |
| editor.redoTip | Redo (Ctrl+Y) | Rehacer (Ctrl+Y) | 重做 (Ctrl+Y) | ImageEditor.cs:95 |
| editor.cancel | Cancel | Cancelar | 取消 | ImageEditor.cs:100 |
| editor.save | Save | Guardar | 保存 | ImageEditor.cs:103 |
| editor.width (`+ " " + value`) | Line width | Grosor | 线宽 | ImageEditor.cs:154 |
| editor.saveFailed | Pegline could not save this screenshot. | Pegline no pudo guardar esta captura. | Pegline 无法保存这张截图。 | ImageEditor.cs:396 |

Not translated, hard-coded: tray tooltip `"Pegline"` (App.cs:724); balloon title `"Pegline"`, 3000 ms (App.cs:326); message-box title `"Pegline"` (App.cs:715, Line.cs:364, ImageEditor.cs:396); window `Title = "Pegline"` (LineWindow.cs:172); preview title `"Pegline preview"` (HoverPreview.cs:254). `"✓ "` and the MDL2 PUA glyphs (U+E8C8, U+E8A7, U+E73E swatch tick) are literals. Deliberate duplicates: `Cancel` (Shortcut.cs:151, ImageEditor.cs:100), `Copy`/`Open` (CardView.cs:199–200, Gestures.cs:183–184); "Delete caught screenshots" without a count is the dialog title (App.cs:705), the menu item adds `({0})…` (App.cs:764).

## 2. Settings

Dir `%APPDATA%\Pegline`, file `%APPDATA%\Pegline\settings.ini` (Support.cs:73–75). Screenshots and notes are **not** here — they are under `%LOCALAPPDATA%\Pegline\` (Line.cs:72–73, Notes.cs:19–20).

| key | type | meaning | written as | default (key absent) |
|:--|:--|:--|:--|:--|
| `catchClipboard` | bool | catch screenshots off the clipboard | `1`/`0` | false → catching off until the user answers the offer (App.cs:79–83, 182–186) |
| `catchOffered` | bool | the one-time "Let Pegline handle your screenshots?" prompt was shown | `1`/`0` | false → prompt 1.2 s after start (App.cs:119, 191) |
| `welcomed` | bool | first-run welcome was shown | `1`/`0` | false → 5 s welcome (§5) (App.cs:126–128) |
| `snipKey` | string | screenshot shortcut in `Shortcut.ToString()` form, or `none` | verbatim | absent → Ctrl+Alt+S; `none` → no shortcut (App.cs:94, 280–284, 304; Shortcut.cs:18) |
| `noteKey` | string | note shortcut, same form | verbatim | absent → Ctrl+Alt+N (App.cs:95; Shortcut.cs:20) |
| `soundOff` | bool | **muted**, inverted: `SoundOn == !soundOff` | `1`/`0` | false → sounds ON (Line.cs:93–97) |
| `noteColor:<file name>` | string int 0–3 | note paper colour keyed by the note's bare file name | verbatim | absent → 0 = yellow (Notes.cs:73–84) |

Format (Support.cs:79–128): plain text, one `key=value` per line, UTF-8, CRLF, trailing newline (`File.WriteAllLines`). Parsing splits on the **first** `=`; only lines with index `> 0` count; the value is the verbatim remainder (no trimming, quoting or escaping). `Get` is true **only** for the exact value `"1"` (Support.cs:99–103); every bool is written `1`/`0` (Support.cs:105–108). `GetString` → null when never set (Support.cs:111–115). Every write rewrites the whole file with all in-memory keys in dictionary insertion order (Support.cs:117–128); `IOException` is swallowed. A typical first-run file:

```
catchOffered=1
welcomed=1
catchClipboard=1
snipKey=Ctrl+Alt+S
noteKey=Ctrl+Alt+N
soundOff=0
noteColor:Note 2026-11-02 191500.txt=2
```

Shortcut text (Shortcut.cs:48–88): prefixes in this order — `Ctrl+`, `Alt+`, `Shift+`, `Win+` — then the key: one char for A–Z/0–9, `PrtScn` for VK 0x2C, otherwise the WPF `Key` enum name. `Parse` accepts only those four spellings and returns null for `none`/empty/garbage. Usable = has a modifier, or VK F1–F24 (0x70–0x87) / 0x2C (Shortcut.cs:31–38).

**`pegged.txt`** = `%APPDATA%\Pegline\pegged.txt` (Support.cs:76, 130–146): one absolute path per line, `File.WriteAllLines` (CRLF + trailing newline). Written by `Line.Save()` (Line.cs:489–492) from `Hang` (Line.cs:134), `Drop` (Line.cs:156) and `Repoint` (Line.cs:399), only for items with `!Falling`. Read exactly once in `Line.Restore()` (Line.cs:494–503): `MaxItems` is temporarily `int.MaxValue`, every non-empty path that still exists is hung quietly (no sound), then `MaxItems` is restored and the next capture trims the overflow. Blank lines and missing files are ignored; the file never deletes anything.

Autostart is **not** a settings key: `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`, value name `Pegline`, data `"<quoted exe path>"` from `Assembly.GetEntryAssembly().Location`; off = delete the value (Support.cs:148–165). Logs in the same dir: `error.log` (`"s"` timestamp, App.cs:826–834) and `trace.log` (`HH:mm:ss.fff`), enabled by `PEGLINE_TRACE=1` or a `trace.on` file (Support.cs:25–42).

## 3. Tray menu

`Forms.NotifyIcon`, `Text = "Pegline"`, icon redrawn on `DisplaySettingsChanged` (App.cs:113, 723). The strip is flat — **no submenus anywhere** — and is rebuilt from scratch on every `Opening` (App.cs:727–728, 733–735), so labels and enabled states are computed at open time. Shortcut text is right-aligned via `ShortcutKeyDisplayString`.

| # | label | checkable | enabled | handler |
|:--|:--|:--|:--|:--|
| 1 | `Hide line` if `isRevealed`, else `Show line` | no | always | `Toggle()` (App.cs:555); shortcut text `Ctrl+Alt+T` only if `HotKeyRegistered` (App.cs:746) |
| 2 | `Take a screenshot` | no | always | `Snip()` = `ArmForEditor()` + `ms-screenclip:` (App.cs:210–228) |
| 3 | `Hang copied text as a note` | no | always | `HangNote()` (App.cs:314) |
| 4 | `Screenshot shortcut: <key>…` | no | always | `ChangeKey(snipId,"snipKey",…)` → `ShortcutDialog` (App.cs:286) |
| 5 | `Note shortcut: <key>…` | no | always | `ChangeKey(noteId,"noteKey",…)` |
| 6 | `Take everything down` | no | `line.LiveCount > 0` | `line.Clear()` (Line.cs:165) |
| 7 | `Delete caught screenshots ({n})…` | no | `caught > 0 && !line.Emptying` | `EmptyInbox()` (App.cs:692) |
| 8 | `Handle screenshots` | yes, `Checked = CatchClipboard` | always | `SetCatching(!CatchClipboard)` (App.cs:182) |
| 9 | `Open screenshots folder` | no | always | `explorer.exe "<CatchClipboard ? %LOCALAPPDATA%\Pegline\Screenshots : FOLDERID_Screenshots folder>"` |
| — | separator | | | App.cs:781 |
| 10 | `Sounds` | yes, `Checked = line.SoundOn` | always | flips `line.SoundOn` → writes `soundOff` |
| 11 | `Open at login` | yes, `Checked = Settings.OpensAtLogin` | always | flips the Run key; `Beep()` on exception (App.cs:783–787) |
| — | separator | | | App.cs:789 |
| 12 | `Quit Pegline` | no | always | `Application.Current.Shutdown()` → `Exit` → `Controller.Shutdown()` (App.cs:150–160, 820) |

Items 4/5 show `none`, the held keys, or `<keys>` + ` (in use by another app)` (App.cs:753–755). Item 8's tooltip is `tray.catching.tip`.

Exact commands — open folder (App.cs:774–779): `Directory.CreateDirectory(folder)` then `Process.Start("explorer.exe", "\"" + folder + "\"")`. Clear screenshots (App.cs:692–717 → Line.cs:191–222): Yes/No box (`inbox.confirm.title`, default **No**, warning icon); on Yes the inbox cards drop staggered 0.06 s apart, `Sounds.Rustle()`, then on a thread-pool thread each file goes to the Recycle Bin via `FileSystem.DeleteFile(f, UIOption.OnlyErrorDialogs, RecycleOption.SendToRecycleBin, UICancelOption.DoNothing)` (the shell shows its own busy-file dialog). Leftovers = `files.Count(File.Exists)`; if > 0 a box with `inbox.failed.body` appears. Only `%LOCALAPPDATA%\Pegline\Screenshots` is touched. Exit: `Application.Current.Shutdown()` (App.cs:790).

Mouse: left click → `Toggle()` (App.cs:730). There is **no** double-click handler: WinForms raises `MouseClick` per physical click, so a double click runs `Toggle()` twice. Right click just opens the strip.

## 4. Capture pipeline

**4.1 Clipboard path** (`ClipboardCatcher`, Watchers.cs:149–418).
1. Message-only window `"PeglineMessages"` on `HWND_MESSAGE` + `AddClipboardFormatListener` (Watchers.cs:184–187); hotkey ids toggle `0x5045`, snip `0x5046`, note `0x5047`, all with `MOD_NOREPEAT` (Watchers.cs:160–163, 190, 222). `WM_CLIPBOARDUPDATE` is handled only when `(Enabled || now < armedUntil) && now > ignoreUntil && !OwnedByUs()` (Watchers.cs:255–256), where `OwnedByUs()` = `GetClipboardOwner` → pid == our pid (Watchers.cs:324–331).
2. Settle timer **0.15 s**, restarted per notification, then `Read()` (Watchers.cs:192–193, 260–261). `foregroundAtCapture = GetForegroundWindow()` is sampled at the notification, before the settle (Watchers.cs:259).
3. Disqualifying formats — any of them drops the capture with no image read: `DataFormats.Html`, `DataFormats.UnicodeText`, `DataFormats.FileDrop`, `DataFormats.Rtf` (Watchers.cs:344–346). Only a bare image counts. Preference: custom `"PNG"` first (`GetData("PNG") as MemoryStream` → `BitmapDecoder.Create(stream, PreservePixelFormat, OnLoad)` → `Frames[0]`), then `DataFormats.Bitmap` via `Clipboard.GetImage()` (Watchers.cs:347–357); up to 4 attempts, `COMException` → `Thread.Sleep(40)`, any other exception → give up (Watchers.cs:336, 359–363).
4. 32bpp alpha (`Opaque`, Watchers.cs:274–291): only for `Bgra32`/`Pbgra32`; scans **64-row bands**, tests the alpha byte (offset 3, stride width×4); the first non-zero alpha returns the image unchanged; if all sampled alpha are 0 the image is converted to `Bgr32` and frozen.
5. Content hash (`Hash`, Watchers.cs:297–322): FNV-1a 64, basis `14695981039346656037`, prime `1099511628211`, seeded `^ (width*31 + height)`; 32-row bands; only rows with `(y & 3) == 0`; within a row `for (i = 0; i < stride; i += 3)` on a `width*4` stride (not pixel-aligned — copy as written). Dedupe window: `hash == lastHash` and `(now - lastHashAt) < 10 s` → drop (Watchers.cs:371). Per process; single instance via mutex `Local\Pegline.SingleInstance` (App.cs:810).
6. `Freeze()` when possible (Watchers.cs:378); `edit = editArmed && now < armedUntil` (armed **60 s** by `ArmForEditor`, Watchers.cs:237–241); `CaptureArea.Guess(w, h, foregroundAtCapture)` with `fromPointer` default true (Watchers.cs:382); trace line `clipboard image {W}x{H} {PixelFormat}`. Save on a worker if frozen, else on the calling (UI) thread (Watchers.cs:399–416).

**4.2 Folder path** (`ScreenshotWatcher`, Watchers.cs:16–143).
- Folder = `SHGetKnownFolderPath(FOLDERID_Screenshots)` GUID `b7bede81-df94-4682-a7d8-57a52620b86f`; fallback `%USERPROFILE%\Pictures\Screenshots` (Native.cs:106–120). Extensions, case-insensitive: `.png .jpg .jpeg .bmp .gif .tif .tiff` (Watchers.cs:28–29). Ignored names, lowercased compare: `thumbs.db`, `desktop.ini`, `.ds_store`; files with the `Hidden` attribute are skipped too (Watchers.cs:30, 122–123). Listing sorted by `File.GetCreationTimeUtc`, errors → `DateTime.MinValue` (Watchers.cs:124, 129–133).
- Startup known-file rule: on `Start()` every listed file whose creation time is **before** the constructor's `launch` timestamp is added to `known` and never reported; anything at/after launch counts as new even if it landed before the watcher was ready (Watchers.cs:26, 62–63). Missing folder → a **3 s** timer polls until it exists (Watchers.cs:49–61).
- `FileSystemWatcher`: `IncludeSubdirectories = false`, notify `FileName | LastWrite | Size`, on `Created`/`Changed`/`Deleted`/`Renamed` (Watchers.cs:72–78). Debounce **0.25 s**, restarted by each event, then `Scan()` (Watchers.cs:39–40, 90–97).
- `Scan()`: each unknown file → `Readable()` (open `FileAccess.Read`, `FileShare.Read`, length > 0). Failure leaves it unknown and restarts the 0.25 s timer (retry-while-being-written); success marks it known and reports it. `known.IntersectWith(files)` prunes vanished names; `onChange()` (= `line.Prune`) runs on every scan (Watchers.cs:99–114).
- Freshness window: `fresh = (UtcNow - GetLastWriteTimeUtc(path)).TotalSeconds < 3`, passed as `fromPointer` to `CaptureArea.Guess` (App.cs:176–177). `Thumbnails.PixelSizeOf` reads only the header (`DelayCreation`, `CacheOption.None`).

**4.3 Inbox naming and encoding.** Folder `%LOCALAPPDATA%\Pegline\Screenshots` (Line.cs:72–73). Name = `Loc.L("Screenshot","Captura","截图") + " " + DateTime.Now.ToString("yyyy-MM-dd HHmmss") + ".png"` — local time, 24 h, no colon (Watchers.cs:389). Uniquifier `Line.UniquePath` appends ` 2`, ` 3`, … before the extension (`Screenshot 2026-11-02 191500 2.png`, Line.cs:433–441). Encoding: `PngBitmapEncoder`, one `BitmapFrame.Create(bitmap)`, `File.Create(path)` + `enc.Save` (Watchers.cs:391–393).

**4.4 Capture area** (`CaptureArea.Guess`, Flight.cs:309–338), `near(a,b) = |a-b| <= 2`:
1. `width < 3 || height < 3` → null.
2. Any screen matching the image within 2 px **and** whose bounds contain the pointer screen's top-left point → that screen's bounds. Quirk to preserve: the guard tests the pointer screen's *origin*, so a screen-sized capture normally resolves only for the matching-origin/primary monitor (Flight.cs:316–319).
3. Whole virtual screen within 2 px → the pointer screen's bounds (Flight.cs:320–324); foreground window rect within 20 px in both axes → that rect, image centred horizontally, `dx = (rectW - width) / 2` (Flight.cs:325–331).
4. `fromPointer == false` → null (fresh saves stop here); otherwise the Snipping Tool case: rect whose bottom-right corner is the pointer, `(p.X - width, p.Y - height, width, height)`, accepted only if the virtual screen contains it (Flight.cs:332–337).

**4.5 Twin / duplicate merge.** `TwinOf` (App.cs:354–372): `recent` is a `(UtcNow, path)` list pruned to **4 s**; candidates are other paths where `InInbox(candidate) != InInbox(path)` and the file exists. `path` is recorded **before** comparing so both halves see each other across workers. A match needs `Thumbnails.SamePicture`. `FoldTwins` (App.cs:376–388): the inbox copy loses — `line.Repoint(copy, saved)`, `File.Delete(copy)`, and `recent` drops the copy and re-adds the kept path with `now`.

Pixel compare (`SamePicture`/`SamePixels`, Line.cs:603–671): both decoded at longest side **2048** (`ComparePixels`), then `Bgr32`; sizes must match exactly; a per-channel difference **> 8** counts; the allowed differing count is `width*height/200` (**0.5 %**) — more → different picture. 64-row bands, two band buffers. Edited-capture guards (App.cs:236–249): while an editor is open, a saved file matching `editing.Original` (2048-px compare) becomes `editing.Twin` and is never hung; `recentlyEdited` entries live **10 s** and a late Windows-saved copy matching one is skipped entirely.

**4.6 Edited-file refresh and pruning.** `editTimer` **1 s** → `PruneSlowly()` + `ReloadEdited()` (Line.cs:88–90). `ReloadEdited` (Line.cs:471–485) reloads the thumb when `GetLastWriteTimeUtc` differs from `item.LastWrite`; `DateTime.MinValue` (stat failed) is ignored; a null thumb is retried next second. `PruneSlowly` (Line.cs:235–242): missing → `MissingTicks++`, dropped at `>= 2` (≈ 2 s, so a temp-file-then-rename save survives). `Prune` (Line.cs:225–229): immediate drop of anything not `File.Exists`, called from every watcher scan and after drags. `Drop` (Line.cs:149–163): `OnFall` (flight), `Falling = true`, `Save()`, `Pop` unless quiet, `Notify()`, and removal from `Items` after **0.6 s**. `Clear()` and `EmptyInbox()` stagger drops by **0.06 s** per item.

**4.7 `Ctrl+Alt+S`** (default `snipKey`). `catcher.ArmForEditor()` first (60 s window; forces the markup sheet even with catching off), then exactly `Process.Start("ms-screenclip:");` (App.cs:210–214). On any exception, 0.3 s later it synthesises Win+Shift+S with `keybd_event`: `VK_LWIN` 0x5B down, `VK_SHIFT` 0x10 down, `'S'` 0x53 down, `'S'` up, `VK_SHIFT` up, `VK_LWIN` up (App.cs:218–226). A capture caught while armed goes to `EditCapture` instead of `HangCapture` (App.cs:230–277): output path `editing.Twin ?? path`, the original inbox copy is deleted on save and the result hung; on cancel `Line.InInbox(path)` files are deleted.

## 5. Reveal / hide state machine

State (App.cs:29–44): `isPresent` (window shown), `isRevealed` (slid down), `pinned`, `peekUntil`, `hotZoneSince`, `awaySince`, `bandSuppressed`, `wanted`, `keepOpen`, `lastLiveCount`, `pendingScreen`.

| what | value | source |
|:--|:--|:--|
| pointer poll `mouseTimer` | **1/30 s** ≈ 33.3 ms, `DispatcherPriority.Render` | App.cs:23, 105 |
| top-edge dwell `RevealDelay` | **0.3 s** | App.cs:75, 623 |
| pointer-away `RetractDelay` | **0.5 s** | App.cs:77, 655 |
| window hide after dismiss | **0.4 s** (still visible during the 0.22 s slide-up) | App.cs:527; LineWindow.cs:366 |
| slide-up curve | `hideT += dt/0.22`, `value = from + (hidden - from)·t²`, velocity zeroed | LineWindow.cs:365–368 |
| tucked offset | `-(PanelHeight + 12)` = **−222** dip; reveal spring `Spring(0.42, 0.82, -222)` → target 0 | LineWindow.cs:147–148, 356 |
| peek after a capture / note | **2.5 s** (`Reveal(false, 2.5)`) | App.cs:339, 443, 482 |
| empty-line auto-hide | **0.7 s** after the last card leaves, if `!keepOpen` | App.cs:486–491 |
| welcome | `welcomed=1`, `keepOpen=true`, `wanted=true`, `Reveal(true,0)`, hide after **5 s** if still empty | App.cs:126–140 |
| catch offer | **1.2 s** after start, once | App.cs:119 |
| hint fade | `dt/0.3*2` toward 1 when the line is empty | LineWindow.cs:372 |

`Refresh()` (App.cs:498–511) is the only place that shows/hides: `blocked = FullScreen.IsActive(panel.Screen ?? UnderPointer)`; `wanted && !blocked → Present()` else `Dismiss()`; then `mouseTimer.Start()` when `wanted`, else `mouseTimer.Stop()` + `SetClickThrough(true)`. `Present()` = `panel.Show()` + `KeepOnTop()` (App.cs:513–519). `Dismiss()` = `preview.Hide()`, `SetRevealed(false)`, `After(0.4, Hide)` (App.cs:521–528). `Reveal(pin,peek)` (App.cs:530–538): no-op unless `isPresent`; sets `pinned` when `pin`, `peekUntil = now + peek` when `peek > 0`, clears `awaySince`, `KeepOnTop()`, `SetRevealed(true)`. `SetRevealed(false)` (App.cs:540–553) clears `pinned`, `peekUntil`, `HoverId` and forces click-through.

`wanted` becomes true on: a new item (`ItemsChanged`, App.cs:475–483, which also does `PlaceOnScreen(pendingScreen)` + `UpdateCapacity()`), a capture (App.cs:441), a note (App.cs:337), `Toggle()` showing, the welcome, and restored items at startup (App.cs:121–125). It becomes false on: live count 0 with `!keepOpen` (0.7 s later), `Toggle()` hiding an empty line, and the welcome timer.

`Tick()` (App.cs:605–661), 30 Hz, device pixels: `inBand = Screens.TopBand(screenUnderPointer).Contains(p)`; leaving the band clears `bandSuppressed`; `buttonDown = GetAsyncKeyState(VK_LBUTTON) & 0x8000`. Tucked (`!isRevealed`): reveal only if `inBand && !bandSuppressed && !buttonDown && !FullScreen.IsActive(screen)` held for `RevealDelay`, then `PlaceOnScreen(screenUnderPointer)` + `UpdateCapacity()` if the screen changed, `Refresh()`, `Reveal(false, 0)`; any other condition resets `hotZoneSince`. Revealed: `previewButtonDown` = L/R/M; `UpdateMousePassThrough`; the zone is the window's x-span from the screen **top** to the window bottom (`b.Left, screen.Bounds.Top, b.Width, b.Bottom - screen.Bounds.Top`) so moving up never hides it; `inside && pinned` → `pinned = false`; `busy = pinned || Gestures.IsDragging || Gestures.MenuOpen || line.PressedId != Empty || now < peekUntil`; `inside || busy` clears `awaySince`, otherwise `awaySince` starts and after `RetractDelay` `SetRevealed(false)`.

Pin semantics: set only by `Reveal(true, …)` (`Toggle()`, welcome); cleared when the pointer **enters** the zone once, by `SetRevealed(false)`, and by a band click. Band suppression: a `WH_MOUSE_LL` hook (id 14, App.cs:580–603) sees `WM_LBUTTONDOWN`/`WM_RBUTTONDOWN` in any screen's top band, sets `bandSuppressed = true`, clears `hotZoneSince`, and if revealed with `panel.PhotoAt(p) == Empty` clears `pinned` + `SetRevealed(false)`. `TopBand` = `(screen.X, screen.Y, screen.Width, max(2, WorkingArea.Top - Bounds.Top))` — the top-docked taskbar height, else 2 px (LineWindow.cs:74–79).

Pass-through (App.cs:663–681): skipped while dragging or a menu is open (preview hidden). Otherwise `over = panel.PhotoAt(mouse)`, `line.HoverId = over`, `panel.SetClickThrough(over == Empty && line.PressedId == Empty)`. The preview is hidden when `!isPresent || !isRevealed || any button down || PressedId != Empty || FullScreen.IsActive(panel.Screen)`; else `preview.Update(item (null for notes), panel.HoverFrame(over), panel.Screen, now)`. Hover preview dwell: the pointer must rest **350 ms** on the same card, and a changed thumb or write time re-arms it (HoverPreview.cs:24–46); caps 900×650 dip, 12 dip margin, 12 dip gap below the card, 6 dip inset (HoverPreview.cs:62–103); decode size `ceil(max(bounds.w, bounds.h) - 2*6*scale)` (HoverPreview.cs:172).

Window styles: `WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT`, and `WM_MOUSEACTIVATE → MA_NOACTIVATE`, so the strip never takes focus (LineWindow.cs:196, 203–212). Window height `ceil(210 * scale)` on the working area (LineWindow.cs:216–223).

Fullscreen (`FullScreen.IsActive`, Flight.cs:342–355): the foreground window must exist, not be `GetShellWindow()`/`GetDesktopWindow()`, and its class must not be `Progman`, `WorkerW`, `Shell_TrayWnd`, `Shell_SecondaryTrayWnd`; then its rect must cover the screen: `r.Left <= b.Left && r.Top <= b.Top && r.Right >= b.Right && r.Bottom >= b.Bottom`.

## 6. Gestures

| gesture | threshold / condition | action | source |
|:--|:--|:--|:--|
| left down on the corner cross | card-local `0 <= x < 26 && 0 <= y < 26` | `line.Discard(id)` (inbox/notes → Recycle Bin, else just drops) | CardView.cs:449–452; Gestures.cs:67–71; Line.cs:402–414 |
| left down, `ClickCount == 2` | — | `line.Open(id)` | Gestures.cs:73–79 |
| single click (up, no drag, no long press) | Manhattan movement ≤ 4 px | `line.Copy(id)` | Gestures.cs:92, 105 |
| long press | hold **0.45 s** without moving/dragging | `line.Markup(id)` | Gestures.cs:35, 55–62 |
| drag | Manhattan movement **> 4 px** with the button down | `Drag()` file drop | Gestures.cs:92–98 |
| right up | — | context menu | Gestures.cs:112–116 |

Drag: a `DataObject` with one `FileDropList` entry (the real path); missing file → `line.Prune()` and abort (Gestures.cs:119–124). A `DragGhost` window (card size × **0.9**, 85 % opacity, glass frame, 4 dip padding) follows the pointer at `(p.X - w/2, p.Y + round(12*s))`, refreshed by a **15 ms** timer *and* `GiveFeedback` (Gestures.cs:131–138, 213–250). Effects `Copy | Move`; `IsDragging`/`line.DraggingId` are set for the drag and the card frame drops to 0.45 opacity with cross and buttons hidden (CardView.cs:426–429). Cleanup: ghost closed and `line.Prune()` in `finally`, then `Prune()` again after **0.6 s** and **1.5 s** (Explorer finishes a move late) (Gestures.cs:152–171).

Exact commands: open file `Process.Start(new ProcessStartInfo(path) { UseShellExecute = true })` (Line.cs:340, failure → `Beep`); edit picture `Process.Start("mspaint.exe", "\"" + path + "\"")`, fallback shell verb `edit`, then `Open` (Line.cs:456–461); edit note `Process.Start("notepad.exe", "\"" + path + "\"")`, fallback `Open` (Line.cs:449–455); reveal `Process.Start("explorer.exe", "/select,\"" + path + "\"")` (Line.cs:468); recycle `FileSystem.DeleteFile(path, UIOption.OnlyErrorDialogs, RecycleOption.SendToRecycleBin)` with the card still dropped on failure plus `trash.failed.body + NewLine + path + NewLine + ex.Message` (Line.cs:345–369) — never a permanent delete; save to desktop `File.Move(path, UniquePath(DesktopDirectory, fileName))` + quiet drop, failure → `Beep` (Line.cs:417–431).

Card context menu (Gestures.cs:174–209), flat, `PlacementMode.MousePoint`:

| order | item | visibility |
|:--|:--|:--|
| 1–4 | `Copy`, `Open`, `Edit`, `Show in Explorer` | always |
| 5 | `Save to Desktop` | only when `line.IsOwn(id)` (inbox file or note) |
| 6 | separator | always |
| 7a | `Discard` | when `IsOwn` |
| 7b | `Take down`, then `Move to Recycle Bin` | when not `IsOwn` |

`MenuOpen` is set before `IsOpen` and cleared on `Closed`; the menu needs foreground, so `SetForegroundWindow(sourceHwnd)` runs first (Gestures.cs:200–208). Previews are dismissed on `PreviewMouseDown` and from buttons/swatches via `Gestures.Interacting` → `preview.Hide` (App.cs:89; Gestures.cs:25–31, 44–47).

## 7. Copy to clipboard

`Line.Copy` (Line.cs:262–298): a note copies as `UnicodeText` only (`SetText(text, TextDataFormat.UnicodeText)`); a picture copies, in this order, `SetImage(full-size BitmapSource)` → `SetData("PNG", MemoryStream(pngBytes))` → `SetFileDropList([path])`. `full = Thumbnails.Load(path, 0)`; PNG bytes are the raw file bytes when the extension is `.png`, else re-encoded with `PngBitmapEncoder` (Line.cs:511–525). `ClipboardCatcher.IgnoreNext()` is called **before** the write: `ignoreUntil = UtcNow + 1 s` — our own clipboard write is not re-captured (Watchers.cs:173–176). Success sets `CopiedId = id` for **1.2 s** (drives the `✓ Copied` pill) and `Notify()`.

OLE: `Clipboard.SetDataObject(data, false)` (WPF wraps `OleSetClipboard`; copy false, so the process stays the clipboard owner) in a retry loop — `COMException` → `Thread.Sleep(50)`, rethrow after attempt `>= 9` (10 tries) (Line.cs:306–322). Flush (`FlushLater`, Line.cs:324–334): if `Clipboard.IsCurrent(data)` call `Clipboard.Flush()`; on `COMException` retry at `0.1 s * (attempt + 1)` while `attempt < 10` — the data is **not** re-set, so clipboard history that opened the clipboard mid-write does not restart the race. If it is no longer current the flush is silently skipped. Total failure → `SystemSounds.Beep`, no `CopiedId` (Line.cs:287–291).

## 8. Flight

| constant | value | source |
|:--|:--|:--|
| `Arc` | 30 dip; offset `-sin(π·k)·30` | Flight.cs:19, 281 |
| `MaxStartPixels` | **1600** px longest side of the take-off rect (the file keeps full size) | Flight.cs:36, 211–218 |
| `ShadowBelowDip` | **300** dip: shadow only when the card width `w <= 300`; `0` disables | Flight.cs:29, 250–256 |
| `ChromeFrom`, `ChromeTo` | 0.10 → 0.55 of **raw** time; smoothstep for frame, clip, glass | Flight.cs:42, 276 |
| fly / fall duration | **0.65 s** / **0.55 s** | Flight.cs:142 |
| landing cross-fade | flight window fades out over **0.16 s** under the card fading in (`dt/0.16*2`) | Flight.cs:189–192; CardView.cs:402 |
| fall distance | **520** points down | Flight.cs:153, 264 |
| flight shadow | `BlurRadius 20, ShadowDepth 5, Direction 270, Opacity 0.24` | Flight.cs:55–58 |
| idle parking | window re-placed at **2×2** on finish; windows pooled in `idle` | Flight.cs:229–230, 64 |

Bounds = union of the start and end rects + margin `m = 60 + 0.05 · max(longest(a), longest(b))`, floored/ceiled to whole pixels (Flight.cs:168–178); a fall's path rect is the card rect grown by `520 · scale` in height (Flight.cs:153). Take-off rect = `Limit(from)`: shrink about the centre until the longest side is ≤ 1600 px (Flight.cs:211–218); the card's device-px target comes from `panel.CardFrame(id)` and `Local()` converts to window dip (Flight.cs:203–206).

Position: `w,h` lerp with `k = Ease.InOutCubic(raw)`; `topX` lerps between the rects' centre x; `topY = lerp(from.Y, to.Y) - sin(π·k)·30`; rotation `tilt · k` (Flight.cs:268–286). Chrome: `inset = 4·k`, `radius = 16·k`, photo clip radius `max(0, radius - inset)`; the clip is one reused `RectangleGeometry`, mutated (never replaced) (Flight.cs:288–301). Placement: container at `(topX - w/2, topY)`, rotation centre `(w/2, 0)`; clothespin at `(topX - 4.5, topY - 14)` with the same angle — 26 points tall, 12 of them over the card (Flight.cs:233–257). Fall: `e = raw³`, `angle = tilt + (tilt·7 + 20)·e`, `y = to.Y + 520·e`, `Opacity = 1 - e`, chrome fully on (Flight.cs:259–266).

Handoff: while `Pegged.Flying` the flight window draws the moving card and the real `CardView` is invisible (`arrived = true`, `drop.Value = 0`, opacity target 0 — CardView.cs:242–248, 399–405). At `k >= 1` the flight calls `completion()` → `line.Land(id)` (Flying = false), sets `fadeOut = t` and fades out over 0.16 s over the now fading-in card. `Fly` starts only when `isPresent && isRevealed && item != null && cardFrame.HasValue && image != null`, else `line.Land(id)` runs immediately (App.cs:448–458); it is scheduled `After(0.03, …)` so the line lays out first (App.cs:445). A fall requires `isPresent && isRevealed && !item.Flying` (App.cs:461–467).

## 9. Card / line geometry

Layout constants (CardView.cs:13–47), all dip:

| constant | value | note |
|:--|:--|:--|
| `PanelHeight` | 210 | window height `ceil(210 * scale)`; the strip |
| `RopeTop` | 10 | rope start y |
| `Spacing` | 174 | slot pitch |
| `CardWidth` | 150 | every card is a 150-wide canvas |
| `PinAbove` | 9.5 | card top = `RopeY - 9.5 + drop` |
| `ActionsGap` / `ActionsHeight` | 6 / 26 | card→button gap; pill height |
| `SwatchesHeight` / `RowGap` | 22 / 4 | notes only |
| `Below(note)` | 32 (photo) / 58 (note) | `6 + (note ? 22+4 : 0) + 26` |
| `Frame.Radius` / `Inset` | 16 / 4 | concentric photo radius 12 |
| `CardOffsetBelowTop` | 26 − 12 = **14** | card top below the clip's top |
| `Sag(width)` | `min(30, width·0.018)` | parabola depth |
| `RopeY(x,w)` | `10 + 4·Sag·f·(1-f)`, `f = x/w` | |
| `X(i,count,w)` | `w/2 - (count-1)·174/2 + i·174` | `count` includes falling items |

Capacity `MaxItems = clamp(3, 12, (int)((DipWidth - 200) / 174))` (App.cs:683–687), recomputed on `PlaceOnScreen`; over capacity the oldest live item is dropped (Line.cs:129–133). `Line.MaxItems` defaults to 8 (Line.cs:58) but is only a placeholder until the first `UpdateCapacity()` — the computed value wins.

Card size (`Frame.PhotoSize`/`CardSize`, CardView.cs:59–71): photo box `maxW = 150 - 14 = 136`, `maxH = 104`, `scale = min(maxW/w, maxH/h)`; card = photo + 8 per axis, at most 144 × 112. Placement (CardView.cs:420–424): `SetLeft(X - 75)`, `SetTop(RopeY(X, lineWidth) - 9.5 + drop)`, `rotate.Angle = swing + Tilt`, `scale = press`; `Tilt` is random per item in **[-2.5, +2.5)** (Line.cs:26). Child offsets inside the card: pin `(70.5, 0)` (CardView.cs:181–182); frame `((150 - cardW)/2, 14)`; copied pill `y = 14 + cardH + 16 - pillH/2`; button row `y = 14 + cardH + 6` (notes: swatches at that y, then `+22+4`); card height `14 + cardH + max(40, Below + 4)` (CardView.cs:295–313).

Rope (LineWindow.cs:305–340), redrawn only when the width changes by ≥ 0.5: geometry is a quadratic Bézier from `(-20, 10)` with control `(w/2, 10 + 2·Sag)` to `(w+20, 10)` — the control doubles the sag so the curve reaches `Sag`. Three layers: shadow `#38000000`, thickness 1.4, offset `y +1.2`, `BlurEffect Radius 2.4`; core `#8C8C8C` (140,140,140), thickness 1.2; highlight `#73FFFFFF`, thickness 0.4, offset `y −0.35`. Opacity mask `Transparent → Black @0.08 → Black @0.92 → Transparent`; `rope.Width = w`, `rope.Height = 210`, not hit-testable.

Card chrome: glass `#B8F7F7F7`, edge `#E0FFFFFF → #40FFFFFF` at 90°, border 0.75 (CardView.cs:73–81, 151–155). Card shadow `Direction 270, Black`; hover writes `Opacity 0.26 / Blur 28 / Depth 8`, idle `0.18 / 20 / 5` (CardView.cs:126, 412–418) — until the first hover change the effect keeps the WPF defaults, so treat 0.18/20/5 as the intended idle state. Clothespin 9 × 26, corner radius 3.5, 4-stop metal gradient (179,179,179 → 237 → 209 → 158), border 0.6, `DropShadow Blur 4, Depth 1.5, Direction 270, Opacity 0.3`; slot 5 × 1.4, radius 0.7, y 8.5, fill `#52000000` (CardView.cs:85–106). Swatch and action pills share `DropShadow Depth 2, Blur 8, Opacity 0.18` (CardView.cs:234, 333).

Corner cross: 20 × 20, margin 3, glass disc + `#282828` X (`M0,0 L6,6 M6,0 L0,6`, thickness 1.6, round caps), scaled `0.6 + 0.4·crossOpacity` about (10,10) (CardView.cs:163–175, 428). Action buttons: one row `Copy` then `Open`, height 26, radius 13, padding L11/R13, icon 11 px `Segoe MDL2 Assets` + 6 px gap, label 11 px semi-bold, idle glass, lit `#F2FFFFFF`, second button margin-left 6, row centred in 150; opacity ramps at `dt*14` and the row slides `-4·(1 - opacity)` (CardView.cs:197–204, 316–342, 410–431); visible only when `hover && !copied && !pressed && arrived && !Flying`. Note swatches: four 15 × 15 dots, 6 px apart, first at x 0, the current one ticked with U+E73E at 9 px, pill padding 6 horizontally (CardView.cs:206–238).

Hit testing: `LineWindow.PhotoAt(p)` converts to window dip and tests each card's `HitRect` **inflated by 10 dip**, skipping `Falling`, first hit wins in dictionary (creation) order, not z-order (LineWindow.cs:242–257). `HitRect` = `(X - cardW/2, RopeY(X) - 9.5 + drop + 14, cardW, cardH)`, unioned with the hover region `(X ± width/2, top + cardH, width, Below)` only while hovered and not dragging (CardView.cs:261–275). `CardFrame(id)` — the flight target — uses the same X/RopeY formula **without** `drop` (it is the settled position) and converts to device px (LineWindow.cs:261–271); `HoverFrame` adds `Below × Scale` to the height (LineWindow.cs:275–283). Both use `line.Items.Count`, so a falling card keeps its slot until removed 0.6 s later.

Springs (Support.cs:234–276; sub-steps ≤ 1/240 s, semi-implicit Euler): `X = Spring(0.55, 0.78, x)`; drop `Spring(0.42, 0.72, -46)` with target 0; swing `Spring(1, 1, 0)` — a new card starts at 16 and switches to `SetStiffness(46, 2.6)`; press `Spring(0.3, 0.6, 1)` with hover 1.035, pressed 0.95 and `SetResponse(0.45, 1)` while pressed; slide `Spring(0.42, 0.82, -222)`. Wind: gust timer every `7 + rand·9` s, skipped when `Items.Count == 0` or dragging (Line.cs:78–84); `Breeze` nudges each card after `rand·0.35` s by `1.6 + rand·1.8` degrees, eased over **0.3 s** (`Ease.OutQuad`), then the swing relaxes with `SetStiffness(38, 2.4)` (CardView.cs:344–358, 379–391). A landing nudges 2.2°, a copy 3° (CardView.cs:370–373). Frame loop: `CompositionTarget.Rendering`; the first frame assumes 1/60 s, then real `RenderingTime` deltas clamped to `≤ 1/20 s` (LineWindow.cs:342–349).

## 10. Icon

`AppIcon.Draw(size)` — `System.Drawing`, 32bpp ARGB, anti-aliased, high-quality pixel offset (AppIcon.cs:14–22). `tiny = size <= 20` selects the tile-less, upright variant.

| element | full size (`> 20`) | tiny (`<= 20`) | line |
|:--|:--|:--|:--|
| tile | rounded rect, margin `0.06·s`, radius `0.22·s`, vertical gradient (36,122,98) → (16,72,62) | omitted | 26–33 |
| rope | y0 `0.27·s`, sag `0.07·s`, colour (220,236,230), width `max(1, 0.03·s)`, round caps, cubic Bézier x `0.13·s … 0.87·s` through `(0.38·s, y0+1.33·sag)` and `(0.62·s, …)` | y0 `0.16·s`, sag `0.05·s`, colour (110,110,110), x `0.5 … s-0.5` | 36–42 |
| photo | `pw = 0.58·s`, `ph = 0.74·pw`, rotated **−7°**, top `0.045·s` | `pw = s-2`, `ph = round(0.72·pw)`, no rotation, top 1 | 46–53 |
| shadow | offset `(0.012·s, 0.03·s)`, radius `0.035·s`, fill (70,0,20,10) | omitted | 55–58 |
| card | radius `0.035·s`, fill (250,252,251) | radius 1.5, same fill, 1 px edge pen (200,40,48,52) | 60–66 |
| picture | inset `0.075·pw`, radius `0.012·s`; sky (120,190,240) → (196,230,250); sun `0.30·ph` at right 30 %, top 14 %, fill (255,205,80); far hill (70,128,176); near hill (48,150,112) | inset 2, radius 0.5, sun `0.42·ph` | 68–92 |
| clip | `0.12·s × 0.11·s`, `cy = -0.40·ch`, radius `0.18·cw`; steel blend (160,164,168)@0, (240,242,243)@0.35, (206,210,212)@0.65, (140,146,150)@1; edge pen alpha 110 | `4 × 4`, `cy = -1`, radius 0.8, edge alpha 200 | 94–108 |

Sizes: `Tray(scale)` renders `max(16, round(16 * scale))` and returns `Icon.FromHandle(bmp.GetHicon())` — one size per monitor DPI (AppIcon.cs:126–130). `Write(path)` emits a multi-size **PNG-entry** ICO at `{16, 20, 24, 32, 40, 48, 64, 128, 256}`, directory field 0 for 256, entries 32 bpp (AppIcon.cs:132–159). `Rounded()` uses `d = min(2r, min(w, h))` (AppIcon.cs:114–124).

## Cross-file decisions

1. **`MaxItems`** — `Line.MaxItems = 8` (Line.cs:58) loses to the computed `clamp(3, 12, (DipWidth-200)/174)` (App.cs:685–686), recomputed on screen or capacity change. `Restore()` bypasses both (`int.MaxValue`) so a saved line is not truncated at startup; the next capture trims.
2. **"Delete caught screenshots"** — dialog title without a count (App.cs:705) vs menu label with `({0})…` (App.cs:764); Rust needs two keys.
3. **Landing position** — `CardFrame` ignores `drop` (LineWindow.cs:266–267) while `HitRect` includes it (CardView.cs:266). Both are right: `CardFrame` is the settled flight target, `HitRect` must follow the card while it moves.
4. **Idle card shadow** — constructed with only `Direction = 270` (CardView.cs:126), so the first frames use WPF defaults (`Blur 5, Depth 5, Opacity 1`) until the first hover writes 0.18/20/5. Take 0.18/20/5 as the intended idle look.
5. **Two screenshot sources** — only the inbox copy is Pegline's to delete (`Line.IsOwn`/`InInbox`, Line.cs:380–414); twin folding always keeps the Windows-saved file (App.cs:376–388).
