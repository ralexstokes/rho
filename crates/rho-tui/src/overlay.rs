use ratatui::{
    Frame,
    layout::Rect,
    text::Line,
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayAnchor {
    Center,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
    TopCenter,
    BottomCenter,
    LeftCenter,
    RightCenter,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SizeValue {
    Absolute(u16),
    Percent(u16),
}

impl SizeValue {
    fn resolve(self, total: u16) -> u16 {
        match self {
            SizeValue::Absolute(value) => value,
            SizeValue::Percent(percent) => {
                total.saturating_mul(percent.min(100)).saturating_div(100)
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct OverlayMargin {
    pub top: u16,
    pub right: u16,
    pub bottom: u16,
    pub left: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverlayOptions {
    pub width: Option<SizeValue>,
    pub min_width: Option<u16>,
    pub max_height: Option<SizeValue>,
    pub anchor: OverlayAnchor,
    pub offset_x: i16,
    pub offset_y: i16,
    pub row: Option<SizeValue>,
    pub col: Option<SizeValue>,
    pub margin: OverlayMargin,
    pub min_terminal_width: Option<u16>,
    pub title: Option<String>,
}

impl Default for OverlayOptions {
    fn default() -> Self {
        Self {
            width: None,
            min_width: None,
            max_height: None,
            anchor: OverlayAnchor::Center,
            offset_x: 0,
            offset_y: 0,
            row: None,
            col: None,
            margin: OverlayMargin::default(),
            min_terminal_width: None,
            title: None,
        }
    }
}

#[derive(Debug, Clone)]
struct OverlayEntry {
    id: u64,
    paragraph: Paragraph<'static>,
    content_width: u16,
    content_height: u16,
    options: OverlayOptions,
    hidden: bool,
}

impl OverlayEntry {
    fn new(id: u64, lines: Vec<Line<'static>>, options: OverlayOptions) -> Self {
        let (paragraph, content_width, content_height) = build_overlay_paragraph(lines, &options);
        Self {
            id,
            paragraph,
            content_width,
            content_height,
            options,
            hidden: false,
        }
    }

    fn set_lines(&mut self, lines: Vec<Line<'static>>) {
        let (paragraph, content_width, content_height) =
            build_overlay_paragraph(lines, &self.options);
        self.paragraph = paragraph;
        self.content_width = content_width;
        self.content_height = content_height;
    }
}

#[derive(Debug, Clone, Default)]
pub struct OverlayStack {
    entries: Vec<OverlayEntry>,
    next_id: u64,
}

impl OverlayStack {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            next_id: 1,
        }
    }

    pub fn show(&mut self, lines: Vec<Line<'static>>, options: OverlayOptions) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        self.entries.push(OverlayEntry::new(id, lines, options));
        id
    }

    pub fn update(&mut self, id: u64, lines: Vec<Line<'static>>) -> bool {
        let Some(entry) = self.entries.iter_mut().find(|entry| entry.id == id) else {
            return false;
        };
        entry.set_lines(lines);
        true
    }

    pub fn hide(&mut self, id: u64) -> bool {
        let Some(index) = self.entries.iter().position(|entry| entry.id == id) else {
            return false;
        };
        self.entries.remove(index);
        true
    }

    pub fn hide_topmost(&mut self) -> bool {
        self.entries.pop().is_some()
    }

    pub fn has(&self, id: u64) -> bool {
        self.entries.iter().any(|entry| entry.id == id)
    }

    pub fn set_hidden(&mut self, id: u64, hidden: bool) -> bool {
        let Some(entry) = self.entries.iter_mut().find(|entry| entry.id == id) else {
            return false;
        };
        entry.hidden = hidden;
        true
    }

    #[allow(dead_code)]
    pub fn has_visible(&self, area: Rect) -> bool {
        self.entries
            .iter()
            .any(|entry| self.is_visible(entry, area.width))
    }

    pub fn render(&self, frame: &mut Frame<'_>, area: Rect) {
        for entry in &self.entries {
            if !self.is_visible(entry, area.width) {
                continue;
            }

            let rect = resolve_overlay_rect_from_content(
                area,
                entry.content_width,
                entry.content_height,
                &entry.options,
            );
            if rect.width == 0 || rect.height == 0 {
                continue;
            }

            frame.render_widget(Clear, rect);
            frame.render_widget(&entry.paragraph, rect);
        }
    }

    fn is_visible(&self, entry: &OverlayEntry, terminal_width: u16) -> bool {
        if entry.hidden {
            return false;
        }
        if let Some(min_width) = entry.options.min_terminal_width
            && terminal_width < min_width
        {
            return false;
        }
        true
    }
}

fn build_overlay_paragraph(
    lines: Vec<Line<'static>>,
    options: &OverlayOptions,
) -> (Paragraph<'static>, u16, u16) {
    let content_width = max_content_width(&lines);
    let content_height = u16::try_from(lines.len()).unwrap_or(u16::MAX);
    let title = options
        .title
        .clone()
        .unwrap_or_else(|| "overlay".to_string());
    let paragraph = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(title))
        .wrap(Wrap { trim: false });
    (paragraph, content_width, content_height)
}

fn resolve_overlay_rect_from_content(
    area: Rect,
    content_width: u16,
    content_height: u16,
    options: &OverlayOptions,
) -> Rect {
    let margin = options.margin;
    let available_width = area
        .width
        .saturating_sub(margin.left)
        .saturating_sub(margin.right)
        .max(1);
    let available_height = area
        .height
        .saturating_sub(margin.top)
        .saturating_sub(margin.bottom)
        .max(1);

    let mut width = options
        .width
        .map(|value| value.resolve(available_width))
        .unwrap_or(content_width.saturating_add(2).min(80));
    if let Some(min_width) = options.min_width {
        width = width.max(min_width);
    }
    width = width.min(available_width).max(1);

    let mut height = content_height
        .saturating_add(2)
        .min(available_height)
        .max(1);
    if let Some(max_height) = options.max_height {
        height = height.min(max_height.resolve(available_height)).max(1);
    }

    let mut row = if let Some(value) = options.row {
        match value {
            SizeValue::Absolute(v) => v,
            SizeValue::Percent(percent) => {
                let max_top = available_height.saturating_sub(height);
                max_top.saturating_mul(percent.min(100)).saturating_div(100)
            }
        }
    } else {
        anchor_row(options.anchor, available_height, height)
    };

    let mut col = if let Some(value) = options.col {
        match value {
            SizeValue::Absolute(v) => v,
            SizeValue::Percent(percent) => {
                let max_left = available_width.saturating_sub(width);
                max_left
                    .saturating_mul(percent.min(100))
                    .saturating_div(100)
            }
        }
    } else {
        anchor_col(options.anchor, available_width, width)
    };

    row = apply_offset(row, options.offset_y);
    col = apply_offset(col, options.offset_x);

    let max_row = available_height.saturating_sub(height);
    let max_col = available_width.saturating_sub(width);
    row = row.min(max_row);
    col = col.min(max_col);

    Rect {
        x: area.x.saturating_add(margin.left).saturating_add(col),
        y: area.y.saturating_add(margin.top).saturating_add(row),
        width,
        height,
    }
}

fn max_content_width(lines: &[Line<'_>]) -> u16 {
    lines
        .iter()
        .map(|line| {
            u16::try_from(
                line.spans
                    .iter()
                    .map(|span| span.content.chars().count())
                    .sum::<usize>(),
            )
            .unwrap_or(u16::MAX)
        })
        .max()
        .unwrap_or(1)
}

fn anchor_row(anchor: OverlayAnchor, available_height: u16, overlay_height: u16) -> u16 {
    match anchor {
        OverlayAnchor::TopLeft | OverlayAnchor::TopCenter | OverlayAnchor::TopRight => 0,
        OverlayAnchor::BottomLeft | OverlayAnchor::BottomCenter | OverlayAnchor::BottomRight => {
            available_height.saturating_sub(overlay_height)
        }
        OverlayAnchor::LeftCenter | OverlayAnchor::Center | OverlayAnchor::RightCenter => {
            available_height.saturating_sub(overlay_height) / 2
        }
    }
}

fn anchor_col(anchor: OverlayAnchor, available_width: u16, overlay_width: u16) -> u16 {
    match anchor {
        OverlayAnchor::TopLeft | OverlayAnchor::LeftCenter | OverlayAnchor::BottomLeft => 0,
        OverlayAnchor::TopRight | OverlayAnchor::RightCenter | OverlayAnchor::BottomRight => {
            available_width.saturating_sub(overlay_width)
        }
        OverlayAnchor::TopCenter | OverlayAnchor::Center | OverlayAnchor::BottomCenter => {
            available_width.saturating_sub(overlay_width) / 2
        }
    }
}

fn apply_offset(value: u16, offset: i16) -> u16 {
    if offset.is_negative() {
        value.saturating_sub(offset.unsigned_abs())
    } else {
        value.saturating_add(offset.unsigned_abs())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlay_stack_tracks_visibility() {
        let mut stack = OverlayStack::new();
        let id = stack.show(vec![Line::from("hello")], OverlayOptions::default());
        assert!(stack.has_visible(Rect::new(0, 0, 80, 24)));
        assert!(stack.set_hidden(id, true));
        assert!(!stack.has_visible(Rect::new(0, 0, 80, 24)));
    }

    #[test]
    fn percent_row_col_positioning_stays_in_bounds() {
        let options = OverlayOptions {
            row: Some(SizeValue::Percent(100)),
            col: Some(SizeValue::Percent(100)),
            width: Some(SizeValue::Absolute(30)),
            ..OverlayOptions::default()
        };
        let lines = [Line::from("x")];
        let rect = resolve_overlay_rect_from_content(
            Rect::new(0, 0, 80, 24),
            max_content_width(&lines),
            u16::try_from(lines.len()).unwrap_or(u16::MAX),
            &options,
        );
        assert!(rect.x + rect.width <= 80);
        assert!(rect.y + rect.height <= 24);
    }

    #[test]
    fn hide_topmost_removes_last_overlay_first() {
        let mut stack = OverlayStack::new();
        let first = stack.show(vec![Line::from("first")], OverlayOptions::default());
        let second = stack.show(vec![Line::from("second")], OverlayOptions::default());
        assert!(stack.hide_topmost());
        assert!(!stack.hide(second));
        assert!(stack.hide(first));
    }
}
