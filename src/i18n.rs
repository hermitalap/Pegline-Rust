//! Every user-visible string, in the three languages the WPF app shipped.
//!
//! The translations are copied verbatim from `archive/wpf-v1/src` (`Loc.L(...)`
//! calls); do not retranslate. Strings with `{}` are Rust format strings; the
//! argument order matches the original `{0}`, `{1}`.
//!
//! Language follows the Windows UI language: Spanish for `es`, Chinese for
//! `zh`, English for everything else.

use std::sync::OnceLock;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Lang {
    En,
    Es,
    Zh,
}

/// All strings the app can draw or show.
pub struct Strings {
    /// Empty line hint, 12 px pill in the middle of the rope.
    pub hint: &'static str,
    /// Card button row.
    pub copy: &'static str,
    pub open: &'static str,
    pub copied: &'static str,
    /// Card context menu.
    pub edit: &'static str,
    pub show_in_explorer: &'static str,
    pub save_to_desktop: &'static str,
    pub discard: &'static str,
    pub take_down: &'static str,
    pub move_to_recycle: &'static str,
    /// Tray menu.
    pub hide_line: &'static str,
    pub show_line: &'static str,
    pub hang_note: &'static str,
    pub note_shortcut: &'static str,
    pub none: &'static str,
    pub in_use_by_another_app: &'static str,
    pub key_separator: &'static str,
    pub take_everything_down: &'static str,
    pub delete_caught: &'static str,
    pub handle_screenshots: &'static str,
    pub handle_screenshots_tip: &'static str,
    pub open_screenshots_folder: &'static str,
    pub sounds: &'static str,
    pub open_at_login: &'static str,
    pub quit: &'static str,
    /// First-run offer.
    pub catch_title: &'static str,
    pub catch_body: &'static str,
    /// Tray balloon when the clipboard has no text.
    pub clipboard_empty: &'static str,
    pub the_shortcut: &'static str,
    /// Confirmation before emptying the inbox.
    pub delete_caught_title: &'static str,
    pub delete_caught_body: &'static str,
    pub recycle_failed: &'static str,
    /// Localized "Screenshot" prefix for inbox file names.
    pub screenshot_file_prefix: &'static str,
    /// Localized "Note" prefix for note file names.
    pub note_file_prefix: &'static str,
    /// Annotation editor (BEHAVIOR §11).
    pub editor_title: &'static str,
    pub arrow: &'static str,
    pub rectangle: &'static str,
    pub text: &'static str,
    pub mosaic: &'static str,
    pub undo: &'static str,
    pub redo: &'static str,
    pub save: &'static str,
    pub cancel: &'static str,
    pub line_width: &'static str,
    pub save_failed: &'static str,
    /// Shortcut recorder (BEHAVIOR §12.2).
    pub shortcut_hint: &'static str,
    pub no_shortcut: &'static str,
    pub ok: &'static str,
    pub add_modifier: &'static str,
    pub conflict_toggle: &'static str,
    pub conflict_external: &'static str,
}

const EN: Strings = Strings {
    hint: "Take a screenshot and it will hang here",
    copy: "Copy",
    open: "Open",
    copied: "Copied",
    edit: "Edit",
    show_in_explorer: "Show in Explorer",
    save_to_desktop: "Save to Desktop",
    discard: "Discard",
    take_down: "Take down",
    move_to_recycle: "Move to Recycle Bin",
    hide_line: "Hide line",
    show_line: "Show line",
    hang_note: "Hang copied text as a note",
    note_shortcut: "Note shortcut",
    none: "none",
    in_use_by_another_app: " (in use by another app)",
    key_separator: ": ",
    take_everything_down: "Take everything down",
    delete_caught: "Delete caught screenshots ({})…",
    handle_screenshots: "Handle screenshots",
    handle_screenshots_tip: "Screenshots from Win+Shift+S and PrtScn hang instantly and skip the Desktop",
    open_screenshots_folder: "Open screenshots folder",
    sounds: "Sounds",
    open_at_login: "Open at login",
    quit: "Quit Pegline",
    catch_title: "Let Pegline handle your screenshots?",
    catch_body: "Screenshots will hang on the line the instant you take them, with Win+Shift+S or PrtScn, without saving them by hand and without piling up on your Desktop. Drag one to a folder to keep it, or discard it with the cross. You can turn this off from the tray icon.",
    clipboard_empty: "There is no text on the clipboard. Copy some text, then press {}.",
    the_shortcut: "the shortcut",
    delete_caught_title: "Delete caught screenshots",
    delete_caught_body: "Move the {} screenshots Pegline caught to the Recycle Bin? {} of them hang on the line and come down too.\n\nScreenshots saved to Pictures\\Screenshots (Win+PrtScn) are not touched.",
    recycle_failed: "{} files could not be moved to the Recycle Bin. They may be open in another app.",
    screenshot_file_prefix: "Screenshot",
    note_file_prefix: "Note",
    editor_title: "Mark up screenshot",
    arrow: "Arrow",
    rectangle: "Rectangle",
    text: "Text",
    mosaic: "Mosaic",
    undo: "Undo",
    redo: "Redo",
    save: "Save",
    cancel: "Cancel",
    line_width: "Line width",
    save_failed: "Pegline could not save this screenshot.",
    shortcut_hint: "Press the keys you want to use. Include Ctrl, Alt, Shift or Win, or use F1–F24 or PrtScn on their own.",
    no_shortcut: "No shortcut",
    ok: "OK",
    add_modifier: "Add Ctrl, Alt, Shift or Win.",
    conflict_toggle: "Ctrl+Alt+T already shows and hides the line.",
    conflict_external: "Another app is using this combination. Try another one.",
};

const ES: Strings = Strings {
    hint: "Haz una captura y se quedará colgada aquí",
    copy: "Copiar",
    open: "Abrir",
    copied: "Copiado",
    edit: "Editar",
    show_in_explorer: "Mostrar en el Explorador",
    save_to_desktop: "Guardar en el Escritorio",
    discard: "Descartar",
    take_down: "Descolgar",
    move_to_recycle: "Mover a la Papelera",
    hide_line: "Ocultar tendedero",
    show_line: "Mostrar tendedero",
    hang_note: "Colgar el texto copiado como nota",
    note_shortcut: "Atajo de nota",
    none: "ninguno",
    in_use_by_another_app: " (en uso por otra aplicación)",
    key_separator: ": ",
    take_everything_down: "Descolgar todo",
    delete_caught: "Borrar capturas guardadas ({})…",
    handle_screenshots: "Encargarse de las capturas",
    handle_screenshots_tip: "Las capturas de Win+Mayús+S e ImprPant se cuelgan al instante",
    open_screenshots_folder: "Abrir carpeta de capturas",
    sounds: "Sonidos",
    open_at_login: "Abrir al iniciar sesión",
    quit: "Salir de Pegline",
    catch_title: "¿Quieres que Pegline se encargue de tus capturas?",
    catch_body: "Las capturas se colgarán al instante, con Win+Mayús+S o ImprPant, sin guardarlas a mano y sin acumularse en el Escritorio. Arrastra una a una carpeta para guardarla, o descártala con la cruz. Puedes desactivarlo desde el icono de la bandeja.",
    clipboard_empty: "No hay texto en el portapapeles. Copia un texto y pulsa {}.",
    the_shortcut: "el atajo",
    delete_caught_title: "Borrar capturas guardadas",
    delete_caught_body: "¿Mover a la Papelera las {} capturas que guardó Pegline? {} están colgadas y también se descolgarán.\n\nLas capturas guardadas en Imágenes\\Capturas de pantalla (Win+ImprPant) no se tocan.",
    recycle_failed: "No se pudieron mover {} archivos a la Papelera. Puede que estén abiertos en otra aplicación.",
    screenshot_file_prefix: "Captura",
    note_file_prefix: "Nota",
    editor_title: "Anotar captura",
    arrow: "Flecha",
    rectangle: "Rectángulo",
    text: "Texto",
    mosaic: "Mosaico",
    undo: "Deshacer",
    redo: "Rehacer",
    save: "Guardar",
    cancel: "Cancelar",
    line_width: "Grosor",
    save_failed: "Pegline no pudo guardar esta captura.",
    shortcut_hint: "Pulsa las teclas que quieras usar. Incluye Ctrl, Alt, Mayús o Win, o usa F1–F24 o ImprPant solas.",
    no_shortcut: "Sin atajo",
    ok: "Aceptar",
    add_modifier: "Añade Ctrl, Alt, Mayús o Win.",
    conflict_toggle: "Ctrl+Alt+T ya muestra y oculta el tendedero.",
    conflict_external: "Otra aplicación está usando esta combinación. Prueba con otra.",
};

const ZH: Strings = Strings {
    hint: "截一张图，它就会挂在这里",
    copy: "复制",
    open: "打开",
    copied: "已复制",
    edit: "编辑",
    show_in_explorer: "在资源管理器中显示",
    save_to_desktop: "保存到桌面",
    discard: "丢弃",
    take_down: "取下",
    move_to_recycle: "移到回收站",
    hide_line: "收起绳子",
    show_line: "放下绳子",
    hang_note: "把复制的文字挂成便签",
    note_shortcut: "便签快捷键",
    none: "未设置",
    in_use_by_another_app: "（被其他程序占用）",
    key_separator: "：",
    take_everything_down: "全部取下",
    delete_caught: "清空截图文件（{} 个）…",
    handle_screenshots: "接管截图",
    handle_screenshots_tip: "Win+Shift+S 和 PrtScn 的截图立刻挂上，不落到桌面",
    open_screenshots_folder: "打开截图文件夹",
    sounds: "音效",
    open_at_login: "开机启动",
    quit: "退出 Pegline",
    catch_title: "让 Pegline 接管你的截图吗？",
    catch_body: "用 Win+Shift+S 或 PrtScn 截的图会立刻挂到绳子上，不用手动保存，也不会堆在桌面。想留下就拖进文件夹，不要就点叉。随时可以在托盘图标里关掉。",
    clipboard_empty: "剪贴板里没有文字。先复制一段文字，再按 {}。",
    the_shortcut: "快捷键",
    delete_caught_title: "清空截图文件",
    delete_caught_body: "把 Pegline 接管保存的 {} 个截图文件移到回收站？其中 {} 张挂在绳子上，会一起取下。\n\n「图片\\Screenshots」里的截图（Win+PrtScn 存的）不受影响。",
    recycle_failed: "有 {} 个文件没能移到回收站，可能正被别的程序打开。",
    screenshot_file_prefix: "截图",
    note_file_prefix: "便签",
    editor_title: "标注截图",
    arrow: "箭头",
    rectangle: "矩形",
    text: "文字",
    mosaic: "马赛克",
    undo: "撤销",
    redo: "重做",
    save: "保存",
    cancel: "取消",
    line_width: "线宽",
    save_failed: "Pegline 无法保存这张截图。",
    shortcut_hint: "直接按下想用的组合键。需要包含 Ctrl、Alt、Shift 或 Win；F1–F24、PrtScn 可以单独使用。",
    no_shortcut: "不使用快捷键",
    ok: "确定",
    add_modifier: "请加上 Ctrl、Alt、Shift 或 Win。",
    conflict_toggle: "Ctrl+Alt+T 已经是显示/隐藏绳子了。",
    conflict_external: "其他程序占用了这个组合键，换一个试试。",
};

static ACTIVE: OnceLock<&'static Strings> = OnceLock::new();

/// The strings for the current UI language.
pub fn s() -> &'static Strings {
    ACTIVE.get_or_init(|| match detect() {
        Lang::Es => &ES,
        Lang::Zh => &ZH,
        Lang::En => &EN,
    })
}

/// Installs a fixed language. Used by tests; the app calls `s()` directly.
pub fn set_lang(lang: Lang) -> &'static Strings {
    match lang {
        Lang::En => &EN,
        Lang::Es => &ES,
        Lang::Zh => &ZH,
    }
}

/// Windows UI language, mapped to the three supported languages.
pub fn detect() -> Lang {
    // Test hook: lets the visual acceptance runs exercise all three layouts
    // without changing the Windows display language.
    if let Ok(value) = std::env::var("PEGLINE_LANG") {
        match value.to_ascii_lowercase().as_str() {
            "es" => return Lang::Es,
            "zh" => return Lang::Zh,
            "en" => return Lang::En,
            _ => {}
        }
    }
    // SAFETY: GetUserDefaultUILanguage has no parameters and cannot fail.
    let langid = unsafe { windows::Win32::Globalization::GetUserDefaultUILanguage() };
    // The low 10 bits are the primary language id.
    match langid & 0x03FF {
        0x04 => Lang::Zh, // LANG_CHINESE
        0x0A => Lang::Es, // LANG_SPANISH
        _ => Lang::En,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_language_is_complete() {
        // The struct is exhaustive, so this test exists to catch empty strings
        // and accidental format-placeholder drift in one language.
        for strings in [&EN, &ES, &ZH] {
            assert!(!strings.hint.is_empty());
            assert!(!strings.quit.is_empty());
            assert_eq!(strings.delete_caught.matches("{}").count(), 1);
            assert_eq!(strings.delete_caught_body.matches("{}").count(), 2);
            assert_eq!(strings.recycle_failed.matches("{}").count(), 1);
            assert_eq!(strings.clipboard_empty.matches("{}").count(), 1);
        }
    }

    #[test]
    fn placeholders_format() {
        let text = s().delete_caught.replace("{}", "3");
        assert!(text.contains('3'));
    }
}
