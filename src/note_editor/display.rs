//! Markdown projection for notes: source lines to blocks, display runs and
//! back (NOTE-EDITOR.md §6).
//!
//! Notes are **line oriented**: one `\n` is one display line, blank lines split
//! paragraphs, and CommonMark's soft-break joining is deliberately not applied.
//! The projection is therefore a small line parser rather than a CommonMark
//! event stream; `pulldown-cmark` would spend most of its work undoing that
//! difference, and its offsets would still have to be cut back to source lines.
//!
//! Every display run carries both its text and the source byte range it came
//! from. Markdown markers are runs too, flagged hidden: the editor shows them
//! only on the active line, and hit-testing skips them otherwise.

use std::ops::Range;

/// The paper the editor draws, DIP (NOTE-EDITOR.md §5.1).
pub const PAPER_WIDTH: f32 = 460.0;
pub const PAPER_HEIGHT: f32 = 360.0;
/// Text padding inside the paper, DIP.
pub const PAD_LEFT: f32 = 30.0;
pub const PAD_RIGHT: f32 = 30.0;
pub const PAD_TOP: f32 = 26.0;
pub const PAD_BOTTOM: f32 = 30.0;
/// Shadow margin between the paper and the editor window edge, DIP.
pub const MARGIN_DIP: f32 = 20.0;
/// Corner radius of the paper, DIP.
pub const RADIUS_DIP: f32 = 22.0;
/// How far inside the paper edge a resize grab still counts, DIP (BEHAVIOR
/// §10.1). The band reaches outwards over the whole shadow margin.
pub const RESIZE_GRIP_DIP: f32 = 2.0;
/// The bottom-right resize anchor is a square this many DIP across, measured
/// from the paper's bottom-right corner into the paper and out over the shadow
/// margin (BEHAVIOR §10.1). The right and bottom edge bands stay thin; only
/// the corner is enlarged, so it is easy to hit without stealing the edges.
///
/// 2.6.2 shrank it from 24 to 16 DIP: 24² = 576 DIP² of corner area becomes
/// 16² = 256 DIP², a 56 % reduction. A whole DIP keeps the 100 % and 200 %
/// device-pixel boxes aligned with the pointer grid.
pub const RESIZE_CORNER_DIP: f32 = 16.0;

/// A paper size, DIP. The window is always `MARGIN_DIP` larger on every side
/// (BEHAVIOR §10.1).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PaperSize {
    pub width: f32,
    pub height: f32,
}

impl PaperSize {
    pub const fn new(width: f32, height: f32) -> Self {
        Self { width, height }
    }

    /// Width available to text, DIP.
    pub fn content_width(self) -> f32 {
        (self.width - PAD_LEFT - PAD_RIGHT).max(1.0)
    }

    /// Height available to text, DIP.
    pub fn view_height(self) -> f32 {
        (self.height - PAD_TOP - PAD_BOTTOM).max(1.0)
    }

    /// The window that holds this paper: paper plus the shadow margin, DIP.
    pub const fn window_size(self) -> (f32, f32) {
        (
            self.width + MARGIN_DIP * 2.0,
            self.height + MARGIN_DIP * 2.0,
        )
    }

    /// Clamp to the resize range. Tolerates a reversed range and NaN: a value
    /// that cannot be clamped sensibly falls back to the minimum.
    pub fn clamp(self) -> Self {
        Self {
            width: clamp_axis(self.width, MIN_PAPER.width, MAX_PAPER.width),
            height: clamp_axis(self.height, MIN_PAPER.height, MAX_PAPER.height),
        }
    }

    /// Whole DIP, for the settings key and for the final window size.
    pub fn rounded(self) -> Self {
        Self {
            width: self.width.round(),
            height: self.height.round(),
        }
    }

    /// Whether a point in window DIP is inside the paper's rounded rectangle.
    /// The window DIP origin is the window's top-left corner, so the paper
    /// starts at `MARGIN_DIP` on both axes.
    pub fn contains(self, x: f32, y: f32) -> bool {
        let left = MARGIN_DIP;
        let top = MARGIN_DIP;
        let right = MARGIN_DIP + self.width;
        let bottom = MARGIN_DIP + self.height;
        if x < left || x >= right || y < top || y >= bottom {
            return false;
        }
        let radius = RADIUS_DIP;
        let cx = if x < left + radius {
            left + radius
        } else if x > right - radius {
            right - radius
        } else {
            x
        };
        let cy = if y < top + radius {
            top + radius
        } else if y > bottom - radius {
            bottom - radius
        } else {
            y
        };
        let dx = x - cx;
        let dy = y - cy;
        dx * dx + dy * dy <= radius * radius
    }
}

impl Default for PaperSize {
    fn default() -> Self {
        DEFAULT_PAPER
    }
}

fn clamp_axis(value: f32, min: f32, max: f32) -> f32 {
    let (low, high) = if min <= max { (min, max) } else { (max, min) };
    if value.is_nan() {
        return low;
    }
    value.max(low).min(high)
}

/// Smallest paper the editor can be dragged to, DIP (BEHAVIOR §10.1).
pub const MIN_PAPER: PaperSize = PaperSize::new(PAPER_WIDTH, PAPER_HEIGHT);
/// The size the editor opens with; resizing the window for the first time is
/// always a growth (BEHAVIOR §10.1).
pub const DEFAULT_PAPER: PaperSize = MIN_PAPER;
/// Largest paper the editor can be dragged to, DIP.
pub const MAX_PAPER: PaperSize = PaperSize::new(820.0, 640.0);
/// Default window size, DIP (paper plus the shadow margin).
pub const WINDOW_WIDTH_DIP: f32 = PAPER_WIDTH + MARGIN_DIP * 2.0;
pub const WINDOW_HEIGHT_DIP: f32 = PAPER_HEIGHT + MARGIN_DIP * 2.0;
/// Width available to text at the default paper size, DIP.
pub const CONTENT_WIDTH: f32 = PAPER_WIDTH - PAD_LEFT - PAD_RIGHT;

/// Parse the `noteSize:<file>` value (`460x360`, whole DIP). Rejects empty,
/// negative, zero and malformed values; sizes outside the resize range clamp.
pub fn parse_note_size(text: &str) -> Option<PaperSize> {
    let text = text.trim();
    let (width, height) = text.split_once(['x', 'X'])?;
    if height.contains(['x', 'X']) {
        return None;
    }
    let width: u32 = width.trim().parse().ok()?;
    let height: u32 = height.trim().parse().ok()?;
    if width == 0 || height == 0 {
        return None;
    }
    Some(PaperSize::new(width as f32, height as f32).clamp())
}

/// Format a paper size for the `noteSize:<file>` settings key.
pub fn format_note_size(size: PaperSize) -> String {
    let size = size.rounded();
    format!("{}x{}", size.width as i64, size.height as i64)
}

/// Which paper edge a pointer is over for resizing (BEHAVIOR §10.1). Only the
/// right, bottom and bottom-right corner resize: the window grows rightwards
/// and downwards and never moves.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResizeZone {
    Right,
    Bottom,
    Corner,
}

/// What a client point means to the editor. Resizing beats the paper, the
/// paper beats the margin; a point outside the window is nobody's.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Hit {
    Resize(ResizeZone),
    Paper,
    Margin,
    Outside,
}

/// Hit-tests a window point in device pixels for a window that holds `paper`
/// at `dpi`.
///
/// The resize bands cover the right and bottom shadow margins and reach
/// `RESIZE_GRIP_DIP` into the paper. The bottom-right corner is a
/// `RESIZE_CORNER_DIP` square (16 DIP since 2.6.2), larger than the band
/// intersection, so the resize anchor is easy to hit. The left and top
/// margins, including the top-right and bottom-left shadow corners, stay
/// commit areas.
pub fn hit_test(x_px: i32, y_px: i32, paper: PaperSize, dpi: u32) -> Hit {
    let scale = dpi.max(96) as f32 / 96.0;
    let x = x_px as f32 / scale;
    let y = y_px as f32 / scale;
    let (window_width, window_height) = paper.window_size();
    if x < 0.0 || y < 0.0 || x >= window_width || y >= window_height {
        return Hit::Outside;
    }
    let left = MARGIN_DIP;
    let top = MARGIN_DIP;
    let right = MARGIN_DIP + paper.width;
    let bottom = MARGIN_DIP + paper.height;
    let corner = RESIZE_CORNER_DIP;
    if x >= right - corner && y >= bottom - corner {
        return Hit::Resize(ResizeZone::Corner);
    }
    let grip = RESIZE_GRIP_DIP;
    let right_band = x >= right - grip && y >= top - grip && y <= bottom + grip;
    let bottom_band = y >= bottom - grip && x >= left - grip && x <= right + grip;
    match (right_band, bottom_band) {
        (true, true) => Hit::Resize(ResizeZone::Corner),
        (true, false) => Hit::Resize(ResizeZone::Right),
        (false, true) => Hit::Resize(ResizeZone::Bottom),
        (false, false) => {
            if paper.contains(x, y) {
                Hit::Paper
            } else {
                Hit::Margin
            }
        }
    }
}
/// Body metrics, DIP.
pub const BODY_SIZE: f32 = 15.0;
pub const BODY_SPACING: f32 = 1.55;
/// Code metrics, DIP.
pub const CODE_SIZE: f32 = 13.5;
pub const CODE_SPACING: f32 = 1.4;

/// Which block a source line belongs to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Block {
    Paragraph,
    Heading(u8),
    Quote,
    /// A list item. `number` is the source number for ordered lists.
    List {
        ordered: bool,
        number: u32,
        task: Option<bool>,
    },
    /// A code-block line. `fence` marks the ``` lines themselves.
    Code {
        fence: bool,
    },
    Rule,
    Blank,
}

impl Block {
    /// True for the ``` delimiter lines, which collapse when not active.
    pub fn is_fence(self) -> bool {
        matches!(self, Block::Code { fence: true })
    }
}

/// How one run is drawn.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Style {
    Body,
    Bold,
    Italic,
    Strike,
    Code,
    Link,
    /// A Markdown marker: hidden while the line is rendered, dim on the active
    /// line.
    Marker,
}

/// A stretch of display text and the source bytes it projects.
#[derive(Clone, Debug)]
pub struct Run {
    pub text: String,
    pub source: Range<usize>,
    pub style: Style,
    /// Markdown syntax rather than content: hidden when the line is rendered.
    pub marker: bool,
}

/// Fixed metrics for one display line. Wrapping still belongs to the renderer.
#[derive(Clone, Copy, Debug)]
pub struct LineMetrics {
    pub font_size: f32,
    pub line_spacing: f32,
    pub bold: bool,
    pub mono: bool,
    /// Left inset of the content, DIP.
    pub indent: f32,
    /// Gap above and below the line, DIP. Applied only at block boundaries.
    pub space_before: f32,
    pub space_after: f32,
    /// Ink opacity: quotes are drawn at 80 %.
    pub ink: f32,
}

/// One source line, projected.
#[derive(Clone, Debug)]
pub struct DisplayLine {
    /// Source range without the trailing newline.
    pub source: Range<usize>,
    pub block: Block,
    pub runs: Vec<Run>,
    pub metrics: LineMetrics,
}

impl DisplayLine {
    /// True when this line renders to nothing (a ``` fence shown collapsed).
    pub fn collapsed(&self, active: bool) -> bool {
        self.block.is_fence() && !active
    }

    pub fn is_fence(&self) -> bool {
        self.block.is_fence()
    }

    /// Left inset in DIP. Active list and quote lines show their raw marker at
    /// the margin instead of in a gutter.
    pub fn indent(&self, active: bool) -> f32 {
        if active && matches!(self.block, Block::List { .. } | Block::Quote) {
            0.0
        } else {
            self.metrics.indent
        }
    }

    /// The text the renderer lays out on this line.
    pub fn display_text(&self, active: bool) -> String {
        let mut text = String::new();
        for run in self.visible_runs(active) {
            text.push_str(&run.text);
        }
        text
    }

    fn visible_runs<'a>(&'a self, active: bool) -> impl Iterator<Item = &'a Run> + 'a {
        self.runs.iter().filter(move |run| active || !run.marker)
    }

    /// Maps a byte offset in the display text to a source byte offset.
    ///
    /// Hidden markers between two visible runs are skipped: clicking at the
    /// visual end of `**bold**` lands after the closing marker.
    pub fn display_to_source(&self, offset: usize, active: bool) -> usize {
        let mut display = 0;
        let mut runs = self.visible_runs(active).peekable();
        while let Some(run) = runs.next() {
            let len = run.text.len();
            if offset < display + len {
                return run.source.start + (offset - display);
            }
            if offset == display + len {
                return match runs.peek() {
                    Some(next) => next.source.start,
                    None => self.source.end,
                };
            }
            display += len;
        }
        self.source.end
    }

    /// Maps a source byte offset to a byte offset in the display text. Offsets
    /// inside hidden markers snap to the nearest visible boundary.
    pub fn source_to_display(&self, offset: usize, active: bool) -> usize {
        let mut display = 0;
        for run in self.visible_runs(active) {
            if offset < run.source.start {
                return display;
            }
            if offset <= run.source.end {
                return display + (offset - run.source.start).min(run.text.len());
            }
            display += run.text.len();
        }
        display
    }

    /// The display ranges that a source selection covers on this line.
    ///
    /// A selection that spans hidden markers comes back as one contiguous
    /// range because those markers contribute no display text.
    pub fn segments(&self, selection: Range<usize>, active: bool) -> Vec<Range<usize>> {
        if selection.start >= self.source.end || selection.end <= self.source.start {
            return Vec::new();
        }
        let mut segments: Vec<Range<usize>> = Vec::new();
        let mut display = 0;
        for run in self.visible_runs(active) {
            let start = selection.start.max(run.source.start);
            let end = selection.end.min(run.source.end);
            if start < end {
                let from = display + (start - run.source.start);
                let to = display + (end - run.source.start);
                match segments.last_mut() {
                    Some(last) if last.end == from => last.end = to,
                    _ => segments.push(from..to),
                }
            }
            display += run.text.len();
        }
        segments
    }
}

/// The whole note, line by line.
#[derive(Clone, Debug, Default)]
pub struct DisplayDoc {
    pub lines: Vec<DisplayLine>,
    /// Byte length of the source that was projected.
    pub text_len: usize,
}

impl DisplayDoc {
    /// Projects `text`, which must already use `\n` line endings.
    pub fn parse(text: &str) -> Self {
        let mut lines = Vec::new();
        let mut in_fence = false;
        let mut start = 0;
        // `split_inclusive` keeps the final empty line for text ending in '\n'.
        let mut pieces: Vec<(usize, &str)> = Vec::new();
        for piece in text.split_inclusive('\n') {
            let raw = piece.strip_suffix('\n').unwrap_or(piece);
            pieces.push((start, raw));
            start += piece.len();
        }
        if text.is_empty() || text.ends_with('\n') {
            pieces.push((text.len(), ""));
        }
        for (line_start, raw) in pieces {
            let source = line_start..line_start + raw.len();
            let (block, runs) = if in_fence {
                if is_fence_line(raw) {
                    in_fence = false;
                    (Block::Code { fence: true }, whole_marker(raw, &source))
                } else {
                    (
                        Block::Code { fence: false },
                        vec![Run {
                            text: raw.to_string(),
                            source: source.clone(),
                            style: Style::Code,
                            marker: false,
                        }],
                    )
                }
            } else if let Some((level, prefix)) = heading(raw) {
                let mut runs = Vec::new();
                push_marker(&mut runs, raw, &source, 0..prefix);
                inline(raw, &source, prefix, &mut runs);
                (Block::Heading(level), runs)
            } else if is_fence_line(raw) {
                in_fence = true;
                (Block::Code { fence: true }, whole_marker(raw, &source))
            } else if raw.trim().is_empty() {
                (Block::Blank, Vec::new())
            } else if is_rule(raw) {
                (Block::Rule, whole_marker(raw, &source))
            } else if let Some(prefix) = quote_prefix(raw) {
                let mut runs = Vec::new();
                push_marker(&mut runs, raw, &source, 0..prefix);
                inline(raw, &source, prefix, &mut runs);
                (Block::Quote, runs)
            } else if let Some(list) = list_item(raw) {
                let mut runs = Vec::new();
                push_marker(&mut runs, raw, &source, 0..list.content);
                inline(raw, &source, list.content, &mut runs);
                (
                    Block::List {
                        ordered: list.ordered,
                        number: list.number,
                        task: list.task,
                    },
                    runs,
                )
            } else {
                let mut runs = Vec::new();
                inline(raw, &source, 0, &mut runs);
                (Block::Paragraph, runs)
            };
            lines.push(DisplayLine {
                source,
                block,
                runs,
                metrics: LineMetrics {
                    font_size: BODY_SIZE,
                    line_spacing: BODY_SPACING,
                    bold: false,
                    mono: false,
                    indent: 0.0,
                    space_before: 0.0,
                    space_after: 0.0,
                    ink: 1.0,
                },
            });
        }
        for index in 0..lines.len() {
            let block = lines[index].block;
            let previous = index.checked_sub(1).map(|i| lines[i].block);
            let next = lines.get(index + 1).map(|line| line.block);
            let mut metrics = metrics_for(block);
            metrics.space_before = match previous {
                None => 0.0,
                Some(previous) if previous == block => 0.0,
                Some(_) => gap_before(block),
            };
            metrics.space_after = match next {
                None => 0.0,
                Some(next) if next == block => 0.0,
                Some(_) => gap_after(block),
            };
            lines[index].metrics = metrics;
        }
        Self {
            lines,
            text_len: text.len(),
        }
    }

    /// The line containing `offset`. The offset may sit on a newline: that
    /// belongs to the line before it, as a caret at its end.
    pub fn line_index(&self, offset: usize) -> usize {
        if self.lines.is_empty() {
            return 0;
        }
        let mut index = 0;
        for (position, line) in self.lines.iter().enumerate() {
            if line.source.start <= offset {
                index = position;
            } else {
                break;
            }
        }
        index
    }

    pub fn line_at(&self, offset: usize) -> Option<&DisplayLine> {
        self.lines.get(self.line_index(offset))
    }
}

fn metrics_for(block: Block) -> LineMetrics {
    let mut metrics = LineMetrics {
        font_size: BODY_SIZE,
        line_spacing: BODY_SPACING,
        bold: false,
        mono: false,
        indent: 0.0,
        space_before: 0.0,
        space_after: 0.0,
        ink: 1.0,
    };
    match block {
        Block::Heading(level) => {
            metrics.font_size = match level {
                1 => 24.0,
                2 => 21.0,
                3 => 18.0,
                4 => 16.0,
                _ => 15.0,
            };
            metrics.bold = true;
            metrics.line_spacing = 1.3;
        }
        Block::Quote => {
            metrics.indent = 12.0;
            metrics.ink = 0.8;
        }
        Block::List { .. } => metrics.indent = 18.0,
        Block::Code { .. } => {
            metrics.font_size = CODE_SIZE;
            metrics.line_spacing = CODE_SPACING;
            metrics.mono = true;
            metrics.indent = 8.0;
        }
        Block::Rule | Block::Blank | Block::Paragraph => {}
    }
    metrics
}

fn gap_before(block: Block) -> f32 {
    match block {
        Block::Heading(_) => 10.0,
        Block::Rule => 8.0,
        Block::Code { fence: false } => 6.0,
        Block::Quote => 4.0,
        Block::List { .. } => 2.0,
        _ => 0.0,
    }
}

fn gap_after(block: Block) -> f32 {
    match block {
        Block::Heading(_) => 6.0,
        Block::Paragraph => 6.0,
        Block::Rule => 8.0,
        Block::Code { fence: false } => 6.0,
        Block::Quote => 4.0,
        Block::List { .. } => 2.0,
        _ => 0.0,
    }
}

/// A marker run spanning `range` of the line, exactly as written.
fn push_marker(runs: &mut Vec<Run>, raw: &str, source: &Range<usize>, range: Range<usize>) {
    if range.start >= range.end || range.end > raw.len() {
        return;
    }
    runs.push(Run {
        text: raw[range.clone()].to_string(),
        source: source.start + range.start..source.start + range.end,
        style: Style::Marker,
        marker: true,
    });
}

fn whole_marker(raw: &str, source: &Range<usize>) -> Vec<Run> {
    let mut runs = Vec::new();
    push_marker(&mut runs, raw, source, 0..raw.len());
    runs
}

fn heading(raw: &str) -> Option<(u8, usize)> {
    let hashes = raw.bytes().take_while(|byte| *byte == b'#').count();
    if !(1..=6).contains(&hashes) {
        return None;
    }
    if hashes == raw.len() {
        return Some((hashes as u8, hashes));
    }
    if raw.as_bytes()[hashes] == b' ' {
        return Some((hashes as u8, hashes + 1));
    }
    None
}

fn is_fence_line(raw: &str) -> bool {
    raw.trim_start().starts_with("```")
}

fn is_rule(raw: &str) -> bool {
    let trimmed = raw.trim();
    let mut characters = trimmed.chars();
    let Some(first) = characters.next() else {
        return false;
    };
    if !matches!(first, '-' | '*' | '_') {
        return false;
    }
    let mut count = 0;
    for character in trimmed.chars() {
        if character == first {
            count += 1;
        } else if character != ' ' && character != '\t' {
            return false;
        }
    }
    count >= 3
}

fn quote_prefix(raw: &str) -> Option<usize> {
    if !raw.starts_with('>') {
        return None;
    }
    if raw.as_bytes().get(1) == Some(&b' ') {
        Some(2)
    } else {
        Some(1)
    }
}

struct ListItem {
    ordered: bool,
    number: u32,
    task: Option<bool>,
    /// Byte offset where the content starts (after `- `, `1. `, `- [x] `).
    content: usize,
}

/// What Enter does on a Markdown list line (NOTE-EDITOR.md §5.4).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ListEnter {
    /// The item has content: insert a newline plus this prefix, so the next
    /// line continues the list.
    Continue(String),
    /// The item is empty: remove `marker` instead of adding a line, ending the
    /// list. Offsets are relative to the line start.
    End { marker: Range<usize> },
}

/// The action Enter takes on `raw`, the caret's source line in `block`:
/// continue a list item, or end the list on an empty item. `None` for a line
/// that is not a list item (or is inside a code block, where a leading `- ` is
/// literal), which means a plain newline.
pub fn list_enter(block: Block, raw: &str) -> Option<ListEnter> {
    if matches!(block, Block::Code { .. }) {
        return None;
    }
    let item = list_item(raw)?;
    if raw[item.content..].trim().is_empty() {
        // The marker and its whitespace go away, leaving a blank line: Enter
        // once to finish the item, twice to leave the list.
        return Some(ListEnter::End {
            marker: 0..raw.len(),
        });
    }
    Some(ListEnter::Continue(next_list_prefix(raw, &item)))
}

/// The prefix the line after `raw` starts with: the same bullet (a task item
/// continues unchecked) or the next ordered number.
fn next_list_prefix(raw: &str, item: &ListItem) -> String {
    let leading = raw.len() - raw.trim_start_matches(' ').len();
    let indent = " ".repeat(leading);
    if item.ordered {
        return format!("{indent}{}. ", item.number.saturating_add(1));
    }
    // Keep the bullet character exactly as written (`-`, `*` or `+`); the
    // task box is part of the content, not of the marker, and comes back
    // unchecked so the next item starts undone.
    let marker = match item.task {
        Some(_) => &raw[leading..item.content - 4],
        None => &raw[leading..item.content],
    };
    match item.task {
        Some(_) => format!("{indent}{marker}[ ] "),
        None => format!("{indent}{marker}"),
    }
}

/// The marker Backspace removes when the caret sits at `caret_in_line` on an
/// item whose content is empty: one Backspace ends the list instead of
/// nibbling at the auto-inserted `- `. Offsets are relative to the line start.
pub fn list_backspace_marker(
    block: Block,
    raw: &str,
    caret_in_line: usize,
) -> Option<Range<usize>> {
    if matches!(block, Block::Code { .. }) {
        return None;
    }
    let item = list_item(raw)?;
    if caret_in_line != item.content || raw.len() != item.content {
        return None;
    }
    Some(0..item.content)
}

fn list_item(raw: &str) -> Option<ListItem> {
    let leading = raw.len() - raw.trim_start_matches(' ').len();
    if leading > 3 {
        return None;
    }
    let rest = &raw[leading..];
    let (ordered, number, marker_len) = if let Some(rest) = rest
        .strip_prefix("- ")
        .or_else(|| rest.strip_prefix("* "))
        .or_else(|| rest.strip_prefix("+ "))
    {
        let _ = rest;
        (false, 0, 2)
    } else {
        let digits = rest
            .bytes()
            .take_while(|byte| byte.is_ascii_digit())
            .count();
        if digits == 0 || digits > 9 || rest.as_bytes().get(digits) != Some(&b'.') {
            return None;
        }
        if rest.as_bytes().get(digits + 1) != Some(&b' ') {
            return None;
        }
        let number = rest[..digits].parse().unwrap_or(1);
        (true, number, digits + 2)
    };
    let mut content = leading + marker_len;
    let mut task = None;
    let rest = &raw[content..];
    if !ordered {
        if rest.starts_with("[ ] ") {
            task = Some(false);
            content += 4;
        } else if rest.starts_with("[x] ") || rest.starts_with("[X] ") {
            task = Some(true);
            content += 4;
        }
    }
    Some(ListItem {
        ordered,
        number,
        task,
        content,
    })
}

/// Parses inline Markdown in `raw[from..]` into runs, appending to `runs`.
///
/// Unmatched delimiters stay literal; nothing is dropped. The visible text of a
/// run always maps byte-for-byte onto its source range.
fn inline(raw: &str, source: &Range<usize>, from: usize, runs: &mut Vec<Run>) {
    let text = &raw[from..];
    let base = source.start + from;
    let mut plain_start = 0;
    let mut index = 0;
    while index < text.len() {
        if let Some(link) = link_at(text, index) {
            flush_plain(text, base, plain_start, index, runs);
            runs.push(Run {
                text: text[index..index + link.open_len].to_string(),
                source: base + index..base + index + link.open_len,
                style: Style::Marker,
                marker: true,
            });
            let visible_start = index + link.open_len;
            if link.visible_len > 0 {
                runs.push(Run {
                    text: text[visible_start..visible_start + link.visible_len].to_string(),
                    source: base + visible_start..base + visible_start + link.visible_len,
                    style: link.style,
                    marker: false,
                });
            }
            runs.push(Run {
                text: text[link.close_start..link.end].to_string(),
                source: base + link.close_start..base + link.end,
                style: Style::Marker,
                marker: true,
            });
            index = link.end;
            plain_start = index;
            continue;
        }
        if let Some(unclosed) = unclosed_marker(text, index) {
            // A `**` without a closing `**` is literal text, not an italic `*`
            // followed by whatever comes next.
            index += unclosed;
            continue;
        }
        let Some((end, style, marker_open, marker_close)) = inline_marker(text, index) else {
            index = next_boundary(text, index);
            continue;
        };
        flush_plain(text, base, plain_start, index, runs);
        let inner_start = index + marker_open.len();
        let inner = &text[inner_start..end - marker_close.len()];
        runs.push(Run {
            text: marker_open.to_string(),
            source: base + index..base + inner_start,
            style: Style::Marker,
            marker: true,
        });
        if !inner.is_empty() {
            runs.push(Run {
                text: inner.to_string(),
                source: base + inner_start..base + inner_start + inner.len(),
                style,
                marker: false,
            });
        }
        runs.push(Run {
            text: marker_close.to_string(),
            source: base + end - marker_close.len()..base + end,
            style: Style::Marker,
            marker: true,
        });
        index = end;
        plain_start = index;
    }
    flush_plain(text, base, plain_start, text.len(), runs);
}

/// One recognised `[text](url)` or `![alt](url)`.
struct LinkAt {
    open_len: usize,
    visible_len: usize,
    close_start: usize,
    end: usize,
    style: Style,
}

/// Finds a link or image at `index`. The visible text and the source bytes it
/// maps to are the part between the opening bracket and `](`.
fn link_at(text: &str, index: usize) -> Option<LinkAt> {
    let rest = &text[index..];
    let (open_len, style) = if rest.starts_with("![") {
        (2, Style::Body)
    } else if rest.starts_with('[') {
        (1, Style::Link)
    } else {
        return None;
    };
    let close = text[index + open_len..].find("](")? + index + open_len;
    let paren = text[close + 2..].find(')')? + close + 2;
    Some(LinkAt {
        open_len,
        visible_len: close - (index + open_len),
        close_start: close,
        end: paren + 1,
        style,
    })
}

/// Returns the length of a doubled delimiter at `index` that has no closing
/// occurrence in the line, so the caller can keep it literal.
fn unclosed_marker(text: &str, index: usize) -> Option<usize> {
    let rest = &text[index..];
    for marker in ["**", "__", "~~"] {
        if rest.starts_with(marker) {
            if !text[index + marker.len()..].contains(marker) {
                return Some(marker.len());
            }
            return None;
        }
    }
    None
}

/// Recognises one inline construct at `index`, returning its end offset and the
/// opening/closing marker text.
fn inline_marker(text: &str, index: usize) -> Option<(usize, Style, &'static str, &'static str)> {
    let rest = &text[index..];
    if rest.starts_with("**") {
        return closing(text, index, "**", "**", Style::Bold);
    }
    if rest.starts_with("__") {
        return closing(text, index, "__", "__", Style::Bold);
    }
    if rest.starts_with("~~") {
        return closing(text, index, "~~", "~~", Style::Strike);
    }
    if rest.starts_with('`') {
        return closing(text, index, "`", "`", Style::Code);
    }
    if rest.starts_with('*') {
        return closing(text, index, "*", "*", Style::Italic);
    }
    if rest.starts_with('_') {
        return closing(text, index, "_", "_", Style::Italic);
    }
    None
}

fn closing(
    text: &str,
    index: usize,
    open: &'static str,
    close: &'static str,
    style: Style,
) -> Option<(usize, Style, &'static str, &'static str)> {
    let from = index + open.len();
    let found = text[from..].find(close)? + from;
    if found == from {
        return None;
    }
    Some((found + close.len(), style, open, close))
}

fn flush_plain(text: &str, base: usize, start: usize, end: usize, runs: &mut Vec<Run>) {
    if start >= end {
        return;
    }
    runs.push(Run {
        text: text[start..end].to_string(),
        source: base + start..base + end,
        style: Style::Body,
        marker: false,
    });
}

fn next_boundary(text: &str, index: usize) -> usize {
    let mut next = index + 1;
    while next < text.len() && !text.is_char_boundary(next) {
        next += 1;
    }
    next
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make(text: &str) -> DisplayDoc {
        DisplayDoc::parse(text)
    }

    fn text_of(line: &DisplayLine, active: bool) -> String {
        line.display_text(active)
    }

    #[test]
    fn plain_paragraph_projects_one_line() {
        let doc = make("hello world");
        assert_eq!(doc.lines.len(), 1);
        assert_eq!(doc.lines[0].block, Block::Paragraph);
        assert_eq!(text_of(&doc.lines[0], false), "hello world");
        assert_eq!(text_of(&doc.lines[0], true), "hello world");
    }

    #[test]
    fn inline_styles_hide_their_markers_when_rendered() {
        let line = &make("a **bold** and *it* and `code` and [link](https://x)").lines[0];
        assert_eq!(text_of(line, false), "a bold and it and code and link");
        assert_eq!(
            text_of(line, true),
            "a **bold** and *it* and `code` and [link](https://x)"
        );
        let bold = line
            .runs
            .iter()
            .find(|run| run.style == Style::Bold)
            .expect("a bold run");
        assert_eq!(bold.text, "bold");
        assert_eq!(
            &"a **bold** and *it* and `code` and [link](https://x)"[bold.source.clone()],
            "bold"
        );
        let markers: Vec<_> = line.runs.iter().filter(|run| run.marker).collect();
        assert_eq!(markers.len(), 8);
        assert!(markers.iter().all(|run| run.style == Style::Marker));
    }

    #[test]
    fn heading_prefix_is_a_marker_and_sets_metrics() {
        let line = &make("## Title").lines[0];
        assert_eq!(line.block, Block::Heading(2));
        assert_eq!(text_of(line, false), "Title");
        assert_eq!(text_of(line, true), "## Title");
        assert!(line.metrics.bold);
        assert!(line.metrics.font_size > BODY_SIZE);
        assert!(line.runs[0].marker);
    }

    #[test]
    fn lists_tasks_and_quotes_keep_their_markers_in_source_order() {
        let doc = make("- item\n1. one\n- [x] done\n> quote");
        assert!(matches!(
            doc.lines[0].block,
            Block::List {
                ordered: false,
                task: None,
                ..
            }
        ));
        assert!(matches!(
            doc.lines[1].block,
            Block::List {
                ordered: true,
                number: 1,
                ..
            }
        ));
        assert!(matches!(
            doc.lines[2].block,
            Block::List {
                task: Some(true),
                ..
            }
        ));
        assert_eq!(text_of(&doc.lines[2], false), "done");
        assert_eq!(text_of(&doc.lines[2], true), "- [x] done");
        assert_eq!(doc.lines[3].block, Block::Quote);
        assert_eq!(text_of(&doc.lines[3], false), "quote");
        assert_eq!(text_of(&doc.lines[3], true), "> quote");
    }

    #[test]
    fn fenced_code_collapses_its_delimiters_and_keeps_content_raw() {
        let doc = make("before\n```rust\n**not bold**\n```\nafter");
        assert_eq!(doc.lines[1].block, Block::Code { fence: true });
        assert!(doc.lines[1].collapsed(false));
        assert_eq!(text_of(&doc.lines[1], false), "");
        assert_eq!(text_of(&doc.lines[1], true), "```rust");
        assert_eq!(doc.lines[2].block, Block::Code { fence: false });
        assert_eq!(text_of(&doc.lines[2], false), "**not bold**");
        assert_eq!(doc.lines[2].runs[0].style, Style::Code);
        assert!(doc.lines[3].collapsed(false));
        assert!(!doc.lines[4].collapsed(false));
    }

    #[test]
    fn rules_and_blank_lines_render_empty() {
        let doc = make("a\n\n---\nb");
        assert_eq!(doc.lines[1].block, Block::Blank);
        assert_eq!(text_of(&doc.lines[1], false), "");
        assert_eq!(doc.lines[2].block, Block::Rule);
        assert_eq!(text_of(&doc.lines[2], false), "");
        assert_eq!(text_of(&doc.lines[2], true), "---");
    }

    #[test]
    fn click_mapping_skips_hidden_markers() {
        let line = &make("a **bold** z").lines[0];
        // Display "a bold z": offset 2 is the 'b' of bold -> source 4.
        assert_eq!(line.display_to_source(2, false), 4);
        // The visual end of the bold word (display 6) maps after the closing
        // marker, to the space at source 10.
        assert_eq!(line.display_to_source(6, false), 10);
        // The end of the line maps after everything.
        assert_eq!(line.display_to_source(8, false), 12);
        assert_eq!(line.source_to_display(4, false), 2);
        assert_eq!(line.source_to_display(8, false), 6);
        // An offset inside a hidden marker snaps to its display boundary.
        assert_eq!(line.source_to_display(3, false), 2);
    }

    #[test]
    fn selection_segments_span_hidden_markers() {
        let line = &make("a **bold** z").lines[0];
        // Select the whole line: one contiguous display range.
        assert_eq!(line.segments(0..12, false), vec![0..8]);
        // Select just the bold word including markers: display 2..6.
        assert_eq!(line.segments(2..10, false), vec![2..6]);
        // A selection entirely inside a hidden marker is invisible.
        assert_eq!(line.segments(2..4, false), Vec::<Range<usize>>::new());
    }

    #[test]
    fn active_line_mapping_is_one_to_one() {
        let line = &make("a **bold** z").lines[0];
        assert_eq!(text_of(line, true).len(), 12);
        for offset in 0..=12 {
            assert_eq!(line.display_to_source(offset, true), offset);
            assert_eq!(line.source_to_display(offset, true), offset);
        }
    }

    #[test]
    fn unmatched_and_nested_markup_stays_literal() {
        let line = &make("**unclosed and *also").lines[0];
        assert_eq!(text_of(line, false), "**unclosed and *also");
        let line = &make("**bold with `code`**").lines[0];
        assert_eq!(text_of(line, false), "bold with `code`");
    }

    #[test]
    fn two_spaces_and_non_markdown_characters_survive() {
        let text = "a  b\tc";
        let line = &make(text).lines[0];
        assert_eq!(text_of(line, false), text);
        assert_eq!(line.source, 0..text.len());
    }

    #[test]
    fn line_index_handles_newlines_and_the_trailing_empty_line() {
        let doc = make("a\n\nb");
        assert_eq!(doc.line_index(0), 0);
        assert_eq!(doc.line_index(1), 0);
        assert_eq!(doc.line_index(2), 1);
        assert_eq!(doc.line_index(3), 2);
        let doc = make("a\n");
        assert_eq!(doc.lines.len(), 2);
        assert_eq!(doc.line_index(2), 1);
    }

    #[test]
    fn unicode_markers_map_without_losing_bytes() {
        let line = &make("**中文**").lines[0];
        assert_eq!(text_of(line, false), "中文");
        assert_eq!(line.display_to_source(0, false), 2);
        assert_eq!(line.display_to_source(6, false), 10);
    }

    #[test]
    fn enter_continues_unordered_and_ordered_lists() {
        assert_eq!(
            list_enter(Block::Paragraph, "- one"),
            Some(ListEnter::Continue("- ".to_string()))
        );
        assert_eq!(
            list_enter(Block::Paragraph, "* two"),
            Some(ListEnter::Continue("* ".to_string()))
        );
        assert_eq!(
            list_enter(Block::Paragraph, "+ three"),
            Some(ListEnter::Continue("+ ".to_string()))
        );
        assert_eq!(
            list_enter(Block::Paragraph, "   - indented"),
            Some(ListEnter::Continue("   - ".to_string()))
        );
        assert_eq!(
            list_enter(Block::Paragraph, "1. one"),
            Some(ListEnter::Continue("2. ".to_string()))
        );
        assert_eq!(
            list_enter(Block::Paragraph, "9. nine"),
            Some(ListEnter::Continue("10. ".to_string()))
        );
        // Tasks continue as an unticked box, keeping the bullet and indent.
        assert_eq!(
            list_enter(Block::Paragraph, "- [x] done"),
            Some(ListEnter::Continue("- [ ] ".to_string()))
        );
        assert_eq!(
            list_enter(Block::Paragraph, "  - [ ] todo"),
            Some(ListEnter::Continue("  - [ ] ".to_string()))
        );
        // Plain lines and prose with a dash are not lists.
        assert_eq!(list_enter(Block::Paragraph, "word - more"), None);
        assert_eq!(list_enter(Block::Paragraph, ""), None);
    }

    #[test]
    fn enter_on_an_empty_item_removes_the_marker_instead() {
        assert_eq!(
            list_enter(Block::Paragraph, "- "),
            Some(ListEnter::End { marker: 0..2 })
        );
        assert_eq!(
            list_enter(Block::Paragraph, "1. "),
            Some(ListEnter::End { marker: 0..3 })
        );
        assert_eq!(
            list_enter(Block::Paragraph, "  - [ ] "),
            Some(ListEnter::End { marker: 0..8 })
        );
        // Trailing whitespace also ends the item.
        assert_eq!(
            list_enter(Block::Paragraph, "-   "),
            Some(ListEnter::End { marker: 0..4 })
        );
        assert_eq!(
            list_enter(Block::Paragraph, " 10. "),
            Some(ListEnter::End { marker: 0..5 })
        );
    }

    #[test]
    fn backspace_at_the_marker_end_removes_the_auto_marker() {
        assert_eq!(list_backspace_marker(Block::Paragraph, "- ", 2), Some(0..2));
        assert_eq!(
            list_backspace_marker(Block::Paragraph, "  1. ", 5),
            Some(0..5)
        );
        assert_eq!(
            list_backspace_marker(Block::Paragraph, "- [ ] ", 6),
            Some(0..6)
        );
        assert_eq!(
            list_backspace_marker(Block::Paragraph, "1. ", 3),
            Some(0..3)
        );
    }

    #[test]
    fn backspace_inside_or_after_content_deletes_one_character() {
        // Caret inside the marker, or after typed content: a normal delete.
        assert_eq!(list_backspace_marker(Block::Paragraph, "- text", 1), None);
        assert_eq!(list_backspace_marker(Block::Paragraph, "- text", 2), None);
        assert_eq!(list_backspace_marker(Block::Paragraph, "- text", 6), None);
        // A space after the marker means the line is not the auto-inserted one.
        assert_eq!(list_backspace_marker(Block::Paragraph, "-  ", 2), None);
        // Not a list at all.
        assert_eq!(list_backspace_marker(Block::Paragraph, "text", 4), None);
    }

    #[test]
    fn code_block_lines_keep_a_leading_dash_literal() {
        let code = Block::Code { fence: false };
        assert_eq!(list_enter(code, "- one"), None);
        assert_eq!(list_enter(code, "1. one"), None);
        assert_eq!(list_enter(code, "- "), None);
        assert_eq!(list_backspace_marker(code, "- ", 2), None);
        // Plus a rule and a quote only look like lists from outside.
        assert_eq!(list_enter(Block::Paragraph, "---"), None);
        assert_eq!(list_enter(Block::Paragraph, "> - one"), None);
    }

    #[test]
    fn the_default_paper_matches_the_window_constants() {
        assert_eq!(DEFAULT_PAPER, PaperSize::new(460.0, 360.0));
        assert_eq!(DEFAULT_PAPER.window_size(), (500.0, 400.0));
        assert_eq!(WINDOW_WIDTH_DIP, 500.0);
        assert_eq!(WINDOW_HEIGHT_DIP, 400.0);
        assert_eq!(DEFAULT_PAPER.content_width(), 400.0);
        assert_eq!(DEFAULT_PAPER.view_height(), 304.0);
    }

    #[test]
    fn the_largest_paper_stays_inside_its_range() {
        assert_eq!(MAX_PAPER.content_width(), 760.0);
        assert_eq!(MAX_PAPER.view_height(), 584.0);
        assert_eq!(MAX_PAPER.window_size(), (860.0, 680.0));
    }

    #[test]
    fn paper_sizes_clamp_to_the_resize_range() {
        assert_eq!(PaperSize::new(10.0, 10.0).clamp(), MIN_PAPER);
        assert_eq!(PaperSize::new(4000.0, 4000.0).clamp(), MAX_PAPER);
        assert_eq!(
            PaperSize::new(600.0, 4000.0).clamp(),
            PaperSize::new(600.0, 640.0)
        );
        // NaN degrades to the minimum rather than poisoning the layout.
        assert_eq!(PaperSize::new(f32::NAN, 500.0).clamp().width, 460.0);
        assert_eq!(PaperSize::new(f32::NAN, 500.0).clamp().height, 500.0);
        assert_eq!(PaperSize::new(f32::INFINITY, 500.0).clamp().width, 820.0);
        assert_eq!(
            PaperSize::new(f32::NEG_INFINITY, 500.0).clamp().width,
            460.0
        );
        // Whole-DIP rounding for the persisted value.
        assert_eq!(
            PaperSize::new(600.4, 500.6).rounded(),
            PaperSize::new(600.0, 501.0)
        );
    }

    #[test]
    fn note_sizes_parse_and_format() {
        assert_eq!(parse_note_size("460x360"), Some(MIN_PAPER));
        assert_eq!(parse_note_size("820X640"), Some(MAX_PAPER));
        assert_eq!(parse_note_size(" 460x360 "), Some(MIN_PAPER));
        assert_eq!(
            parse_note_size("600x500"),
            Some(PaperSize::new(600.0, 500.0))
        );
        // Legal integers outside the range clamp instead of failing.
        assert_eq!(parse_note_size("99999x99999"), Some(MAX_PAPER));
        assert_eq!(parse_note_size("1x2"), Some(MIN_PAPER));
        // Malformed values fall back to the default through the caller.
        for bad in [
            "", "0x0", "-1x2", "abc", "1x2x3", "x", "1x", "x2", "1.5x2", "  ",
        ] {
            assert_eq!(parse_note_size(bad), None, "{bad:?} should not parse");
        }
        assert_eq!(format_note_size(PaperSize::new(600.0, 500.0)), "600x500");
        assert_eq!(format_note_size(DEFAULT_PAPER), "460x360");
    }
}
