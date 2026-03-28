use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Widget};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::theme;

/// A single visual row produced by `wrap_lines()`.
#[derive(Debug, Clone)]
struct VisualRow {
    /// The text content of this row (no prompt).
    text: String,
    /// Byte offset in the original text where this row's content starts.
    byte_start: usize,
    /// Byte offset in the original text where this row's content ends.
    byte_end: usize,
    /// Display width of the prompt prefix for this row.
    prompt_width: usize,
}

/// Wrap text into visual rows, respecting CJK character widths.
/// Each logical line (split by '\n') gets a prompt prefix.
/// The first logical line uses `first_prompt_w`, subsequent lines use `cont_prompt_w`.
/// `width` is the total available width including prompt.
fn wrap_lines(text: &str, width: usize, first_prompt_w: usize, cont_prompt_w: usize) -> Vec<VisualRow> {
    if width == 0 {
        return vec![VisualRow {
            text: String::new(),
            byte_start: 0,
            byte_end: 0,
            prompt_width: first_prompt_w,
        }];
    }
    let mut rows = Vec::new();
    wrap_lines_impl(text, width, first_prompt_w, cont_prompt_w, &mut rows);
    rows
}

fn wrap_lines_impl(
    text: &str,
    width: usize,
    first_prompt_w: usize,
    cont_prompt_w: usize,
    rows: &mut Vec<VisualRow>,
) {
    let mut global_byte = 0usize;

    let lines: Vec<&str> = text.split('\n').collect();
    for (line_idx, line) in lines.iter().enumerate() {
        let is_first_logical = line_idx == 0;
        let logical_prompt_w = if is_first_logical { first_prompt_w } else { cont_prompt_w };

        let mut is_first_row_of_line = true;
        let mut row_start = global_byte;
        let mut col = 0usize;
        let mut row_text = String::new();

        for ch in line.chars() {
            let prompt_w = if is_first_row_of_line { logical_prompt_w } else { 0 };
            let avail = if width > prompt_w { width - prompt_w } else { 1 };
            let cw = UnicodeWidthChar::width(ch).unwrap_or(0);

            if col + cw > avail && col > 0 {
                rows.push(VisualRow {
                    text: std::mem::take(&mut row_text),
                    byte_start: row_start,
                    byte_end: global_byte,
                    prompt_width: prompt_w,
                });
                is_first_row_of_line = false;
                row_start = global_byte;
                col = 0;
            }
            row_text.push(ch);
            col += cw;
            global_byte += ch.len_utf8();
        }

        let prompt_w = if is_first_row_of_line { logical_prompt_w } else { 0 };
        rows.push(VisualRow {
            text: row_text,
            byte_start: row_start,
            byte_end: global_byte,
            prompt_width: prompt_w,
        });

        // Skip '\n' byte (except after last line)
        if line_idx < lines.len() - 1 {
            global_byte += 1;
        }
    }

    if rows.is_empty() {
        rows.push(VisualRow {
            text: String::new(),
            byte_start: 0,
            byte_end: 0,
            prompt_width: first_prompt_w,
        });
    }
}

pub struct InputBox {
    pub text: String,
    /// Cursor position as byte index into `text`.
    pub cursor_pos: usize,
    // Completion state
    pub completions: Vec<String>,
    candidates: Vec<String>,
    selected: Option<usize>,
    popup_visible: bool,
}

impl InputBox {
    pub fn new() -> Self {
        Self {
            text: String::new(),
            cursor_pos: 0,
            completions: Vec::new(),
            candidates: Vec::new(),
            selected: None,
            popup_visible: false,
        }
    }

    pub fn insert(&mut self, c: char) {
        self.text.insert(self.cursor_pos, c);
        self.cursor_pos += c.len_utf8();
        self.auto_complete();
    }

    pub fn insert_str(&mut self, s: &str) {
        self.text.insert_str(self.cursor_pos, s);
        self.cursor_pos += s.len();
        self.auto_complete();
    }

    pub fn backspace(&mut self) {
        if self.cursor_pos > 0 {
            // Find the char boundary before cursor_pos
            let before = &self.text[..self.cursor_pos];
            if let Some(ch) = before.chars().next_back() {
                self.cursor_pos -= ch.len_utf8();
                self.text.remove(self.cursor_pos);
            }
        }
        self.auto_complete();
    }

    pub fn delete(&mut self) {
        if self.cursor_pos < self.text.len() {
            self.text.remove(self.cursor_pos);
        }
    }

    pub fn move_left(&mut self) {
        if self.cursor_pos > 0 {
            let before = &self.text[..self.cursor_pos];
            if let Some(ch) = before.chars().next_back() {
                self.cursor_pos -= ch.len_utf8();
            }
        }
    }

    pub fn move_right(&mut self) {
        if self.cursor_pos < self.text.len() {
            let ch = self.text[self.cursor_pos..].chars().next().unwrap();
            self.cursor_pos += ch.len_utf8();
        }
    }

    pub fn move_home(&mut self) {
        self.cursor_pos = 0;
    }

    pub fn move_end(&mut self) {
        self.cursor_pos = self.text.len();
    }

    /// Move to start of current line (Ctrl+A)
    pub fn move_line_start(&mut self) {
        let before = &self.text[..self.cursor_pos];
        if let Some(nl_pos) = before.rfind('\n') {
            self.cursor_pos = nl_pos + 1;
        } else {
            self.cursor_pos = 0;
        }
    }

    /// Move cursor up one line, preserving column position.
    pub fn move_up(&mut self) -> bool {
        let before = &self.text[..self.cursor_pos];
        let cur_line_start = before.rfind('\n').map(|p| p + 1).unwrap_or(0);
        if cur_line_start == 0 {
            return false;
        }
        let col = self.text[cur_line_start..self.cursor_pos].chars().count();
        let prev_line_start = self.text[..cur_line_start - 1]
            .rfind('\n')
            .map(|p| p + 1)
            .unwrap_or(0);
        let prev_line_end = cur_line_start - 1;
        let prev_line_len = self.text[prev_line_start..prev_line_end].chars().count();
        let target_col = col.min(prev_line_len);
        // Convert char offset to byte offset within prev line
        self.cursor_pos = self.text[prev_line_start..]
            .char_indices()
            .nth(target_col)
            .map(|(i, _)| prev_line_start + i)
            .unwrap_or(prev_line_end);
        true
    }

    /// Move cursor down one line, preserving column position.
    pub fn move_down(&mut self) -> bool {
        let before = &self.text[..self.cursor_pos];
        let cur_line_start = before.rfind('\n').map(|p| p + 1).unwrap_or(0);
        let col = self.text[cur_line_start..self.cursor_pos].chars().count();
        let after = &self.text[self.cursor_pos..];
        let next_nl = after.find('\n');
        if next_nl.is_none() {
            return false;
        }
        let next_line_start = self.cursor_pos + next_nl.unwrap() + 1;
        let next_line_end = self.text[next_line_start..]
            .find('\n')
            .map(|p| next_line_start + p)
            .unwrap_or(self.text.len());
        let next_line_len = self.text[next_line_start..next_line_end].chars().count();
        let target_col = col.min(next_line_len);
        self.cursor_pos = self.text[next_line_start..]
            .char_indices()
            .nth(target_col)
            .map(|(i, _)| next_line_start + i)
            .unwrap_or(next_line_end);
        true
    }

    /// Returns true if the text contains multiple lines.
    pub fn is_multiline(&self) -> bool {
        self.text.contains('\n')
    }

    /// Move to end of current line (Ctrl+E)
    pub fn move_line_end(&mut self) {
        let after = &self.text[self.cursor_pos..];
        if let Some(nl_pos) = after.find('\n') {
            self.cursor_pos += nl_pos;
        } else {
            self.cursor_pos = self.text.len();
        }
    }

    /// Kill from cursor to end of current line (Ctrl+K)
    pub fn kill_to_line_end(&mut self) {
        let after = &self.text[self.cursor_pos..];
        if let Some(nl_pos) = after.find('\n') {
            self.text.drain(self.cursor_pos..self.cursor_pos + nl_pos);
        } else {
            self.text.truncate(self.cursor_pos);
        }
    }

    /// Kill from start of current line to cursor (Ctrl+U)
    pub fn kill_to_line_start(&mut self) {
        let before = &self.text[..self.cursor_pos];
        let line_start = if let Some(nl_pos) = before.rfind('\n') {
            nl_pos + 1
        } else {
            0
        };
        self.text.drain(line_start..self.cursor_pos);
        self.cursor_pos = line_start;
    }

    /// Delete the word before cursor (Ctrl+W)
    pub fn delete_word(&mut self) {
        if self.cursor_pos == 0 {
            return;
        }
        let before = &self.text[..self.cursor_pos];
        // Skip trailing whitespace (but not newlines)
        let trimmed = before.trim_end_matches(|c: char| c.is_whitespace() && c != '\n');
        // Find word boundary in the remaining text
        let word_start = trimmed
            .rfind(|c: char| c.is_whitespace())
            .map(|p| p + trimmed[p..].chars().next().map(|c| c.len_utf8()).unwrap_or(1))
            .unwrap_or(0);
        self.text.drain(word_start..self.cursor_pos);
        self.cursor_pos = word_start;
        self.auto_complete();
    }

    pub fn take(&mut self) -> String {
        self.cursor_pos = 0;
        self.dismiss_popup();
        std::mem::take(&mut self.text)
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Confirm the current (or first) candidate: apply, append space, close popup.
    pub fn tab(&mut self) {
        if self.popup_visible && !self.candidates.is_empty() {
            let idx = self.selected.unwrap_or(0);
            self.apply_candidate(idx);
            if self.cursor_pos >= self.text.len()
                || !self.text[self.cursor_pos..].starts_with(' ')
            {
                self.text.insert(self.cursor_pos, ' ');
                self.cursor_pos += 1;
            }
            self.dismiss_popup();
        } else {
            self.build_candidates();
            if !self.candidates.is_empty() {
                self.apply_candidate(0);
                if self.cursor_pos >= self.text.len()
                    || !self.text[self.cursor_pos..].starts_with(' ')
                {
                    self.text.insert(self.cursor_pos, ' ');
                    self.cursor_pos += 1;
                }
            }
            self.dismiss_popup();
        }
    }

    /// Move selection down in the popup (no confirm).
    pub fn select_next(&mut self) {
        if self.popup_visible && !self.candidates.is_empty() {
            let next = match self.selected {
                Some(i) => (i + 1) % self.candidates.len(),
                None => 0,
            };
            self.selected = Some(next);
        }
    }

    /// Move selection up in the popup (no confirm).
    pub fn select_prev(&mut self) {
        if self.popup_visible && !self.candidates.is_empty() {
            let prev = match self.selected {
                Some(0) | None => self.candidates.len() - 1,
                Some(i) => i - 1,
            };
            self.selected = Some(prev);
        }
    }

    /// Shift+Tab: move selection up (alias for select_prev).
    pub fn shift_tab(&mut self) {
        self.select_prev();
    }

    pub fn is_popup_visible(&self) -> bool {
        self.popup_visible
    }

    pub fn dismiss_popup(&mut self) {
        self.popup_visible = false;
        self.candidates.clear();
        self.selected = None;
    }

    /// Auto-show completion popup when typing `/` or `@` prefixed words.
    fn auto_complete(&mut self) {
        let word = self.current_word();
        if word.starts_with('/') || word.starts_with('@') {
            self.build_candidates();
            if self.candidates.is_empty() {
                self.dismiss_popup();
            } else {
                self.popup_visible = true;
                self.selected = None;
            }
        } else {
            self.dismiss_popup();
        }
    }

    fn build_candidates(&mut self) {
        let word = self.current_word().to_lowercase();
        self.candidates = self
            .completions
            .iter()
            .filter(|c| c.to_lowercase().starts_with(&word))
            .cloned()
            .collect();
    }

    fn current_word(&self) -> String {
        let before = &self.text[..self.cursor_pos];
        before
            .rsplit(|c: char| c.is_whitespace())
            .next()
            .unwrap_or("")
            .to_string()
    }

    fn apply_candidate(&mut self, idx: usize) {
        if let Some(candidate) = self.candidates.get(idx) {
            let word = self.current_word();
            let word_byte_start = self.cursor_pos - word.len();
            self.text
                .replace_range(word_byte_start..self.cursor_pos, candidate);
            self.cursor_pos = word_byte_start + candidate.len();
        }
    }

    /// Number of visual lines the input takes at given width.
    pub fn visual_line_count(&self, width: u16) -> u16 {
        if width <= 2 {
            return 1;
        }
        let rows = wrap_lines(&self.text, width as usize, 2, 2);
        rows.len().max(1) as u16
    }

    fn cursor_visual_position(&self, width: u16) -> (u16, u16) {
        let inner_w = width as usize;
        if inner_w == 0 {
            return (0, 0);
        }
        let rows = wrap_lines(&self.text, inner_w, 2, 2);

        // Find which row contains the cursor
        for (row_idx, row) in rows.iter().enumerate() {
            if self.cursor_pos >= row.byte_start && self.cursor_pos <= row.byte_end {
                // Cursor is in this row
                let text_before_cursor = &self.text[row.byte_start..self.cursor_pos];
                let col = row.prompt_width + UnicodeWidthStr::width(text_before_cursor);
                return (col as u16, row_idx as u16);
            }
        }

        // Fallback: cursor at end
        if let Some(last) = rows.last() {
            let col = last.prompt_width + UnicodeWidthStr::width(last.text.as_str());
            return (col as u16, (rows.len() - 1) as u16);
        }
        (0, 0)
    }

    fn scroll_offset_for_height(&self, width: u16, visible_height: u16) -> u16 {
        if visible_height == 0 {
            return 0;
        }
        let (_, cursor_row) = self.cursor_visual_position(width);
        cursor_row.saturating_sub(visible_height.saturating_sub(1))
    }

    pub fn render(&self, area: Rect, buf: &mut Buffer) {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme::BORDER))
            .title(" Input ")
            .title_style(Style::default().fg(theme::BORDER));

        let inner = block.inner(area);
        block.render(area, buf);

        let rows = wrap_lines(&self.text, inner.width as usize, 2, 2);
        let scroll_y = self.scroll_offset_for_height(inner.width, inner.height);

        if rows.len() > 3 {
            tracing::info!(
                "INPUT_RENDER: rows={} inner_h={} inner_w={} scroll_y={} text_len={}",
                rows.len(), inner.height, inner.width, scroll_y, self.text.len()
            );
        }

        let prompt_style = Style::default().fg(Color::DarkGray);
        let text_style = Style::default();

        for (i, row) in rows.iter().enumerate() {
            let visual_y = i as u16;
            if visual_y < scroll_y {
                continue;
            }
            let screen_y = inner.y + visual_y - scroll_y;
            if screen_y >= inner.y + inner.height {
                break;
            }

            // Render prompt
            if row.prompt_width > 0 {
                let prompt = if i == 0 { "> " } else { "  " };
                buf.set_string(inner.x, screen_y, prompt, prompt_style);
            }

            // Render text
            let text_x = inner.x + row.prompt_width as u16;
            buf.set_string(text_x, screen_y, &row.text, text_style);
        }
    }

    pub fn render_popup(&self, input_area: Rect, buf: &mut Buffer) {
        if !self.popup_visible || self.candidates.is_empty() {
            return;
        }
        let max_show = 6.min(self.candidates.len());
        let width = self
            .candidates
            .iter()
            .map(|c| UnicodeWidthStr::width(c.as_str()))
            .max()
            .unwrap_or(10) as u16
            + 4;

        let height = max_show as u16;
        let x = input_area.x + 2;
        let y = input_area.y.saturating_sub(height);
        let popup = Rect::new(x, y, width.min(input_area.width), height);

        Clear.render(popup, buf);
        for (i, candidate) in self.candidates.iter().take(max_show).enumerate() {
            let style = if self.selected == Some(i) {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };
            let row = popup.y + i as u16;
            if row < popup.y + popup.height {
                buf.set_string(popup.x + 1, row, candidate, style);
            }
        }
    }

    /// Get cursor screen position.
    pub fn cursor_position(&self, area: Rect) -> (u16, u16) {
        let inner_x = area.x + 1;
        let inner_y = area.y + 1;
        let inner_width = area.width.saturating_sub(2);
        let visible_height = area.height.saturating_sub(2);
        let (col, visual_row) = self.cursor_visual_position(inner_width);
        let scroll_y = self.scroll_offset_for_height(inner_width, visible_height);
        (inner_x + col, inner_y + visual_row.saturating_sub(scroll_y))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── wrap_lines unit tests ──

    #[test]
    fn wrap_lines_empty() {
        let rows = wrap_lines("", 80, 2, 2);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].text, "");
        assert_eq!(rows[0].prompt_width, 2);
    }

    #[test]
    fn wrap_lines_single_line_no_wrap() {
        let rows = wrap_lines("hello", 80, 2, 2);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].text, "hello");
        assert_eq!(rows[0].byte_start, 0);
        assert_eq!(rows[0].byte_end, 5);
    }

    #[test]
    fn wrap_lines_wraps_at_width() {
        // width=12, prompt=2, so 10 chars fit per first row
        let rows = wrap_lines("abcdefghijklmno", 12, 2, 2);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].text, "abcdefghij"); // 10 chars in first row (prompt=2)
        assert_eq!(rows[1].text, "klmno");       // remaining, no prompt (continuation)
        assert_eq!(rows[1].prompt_width, 0);
    }

    #[test]
    fn wrap_lines_cjk() {
        // "你好世界" = 4 CJK chars, each 2 cols = 8 cols
        // width=8, prompt=2, avail=6, so 3 CJK chars fit (6 cols)
        let rows = wrap_lines("你好世界", 8, 2, 2);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].text, "你好世");
        assert_eq!(rows[1].text, "界");
    }

    #[test]
    fn wrap_lines_multiline() {
        let rows = wrap_lines("abc\ndef", 80, 2, 2);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].text, "abc");
        assert_eq!(rows[0].prompt_width, 2);
        assert_eq!(rows[1].text, "def");
        assert_eq!(rows[1].prompt_width, 2);
    }

    #[test]
    fn wrap_lines_mixed_cjk_ascii() {
        // "hi你好" = 2 + 4 = 6 display cols
        // width=8, prompt=2, avail=6 — just fits
        let rows = wrap_lines("hi你好", 8, 2, 2);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].text, "hi你好");
    }

    #[test]
    fn wrap_lines_byte_offsets_cjk() {
        // "你好" = 6 bytes (3 per char)
        let rows = wrap_lines("你好", 80, 2, 2);
        assert_eq!(rows[0].byte_start, 0);
        assert_eq!(rows[0].byte_end, 6);
    }

    #[test]
    fn wrap_lines_multiline_byte_offsets() {
        // "ab\ncd" — 'a'=0, 'b'=1, '\n'=2, 'c'=3, 'd'=4
        let rows = wrap_lines("ab\ncd", 80, 2, 2);
        assert_eq!(rows[0].byte_start, 0);
        assert_eq!(rows[0].byte_end, 2);
        assert_eq!(rows[1].byte_start, 3);
        assert_eq!(rows[1].byte_end, 5);
    }

    // ── InputBox basic tests ──

    #[test]
    fn new_is_empty() {
        let input = InputBox::new();
        assert!(input.is_empty());
        assert_eq!(input.cursor_pos, 0);
    }

    #[test]
    fn insert_and_take() {
        let mut input = InputBox::new();
        input.insert('h');
        input.insert('i');
        assert_eq!(input.text, "hi");
        assert_eq!(input.cursor_pos, 2);

        let text = input.take();
        assert_eq!(text, "hi");
        assert!(input.is_empty());
        assert_eq!(input.cursor_pos, 0);
    }

    #[test]
    fn insert_str() {
        let mut input = InputBox::new();
        input.insert_str("hello world");
        assert_eq!(input.text, "hello world");
        assert_eq!(input.cursor_pos, 11);
    }

    #[test]
    fn insert_cjk() {
        let mut input = InputBox::new();
        input.insert_str("你好");
        assert_eq!(input.text, "你好");
        assert_eq!(input.cursor_pos, 6); // byte index: 3 bytes per CJK char
    }

    #[test]
    fn insert_mixed_cjk_ascii() {
        let mut input = InputBox::new();
        input.insert_str("hi你好world");
        assert_eq!(input.cursor_pos, "hi你好world".len());
    }

    #[test]
    fn backspace() {
        let mut input = InputBox::new();
        input.insert_str("abc");
        input.backspace();
        assert_eq!(input.text, "ab");
        assert_eq!(input.cursor_pos, 2);
    }

    #[test]
    fn backspace_cjk() {
        let mut input = InputBox::new();
        input.insert_str("你好");
        input.backspace();
        assert_eq!(input.text, "你");
        assert_eq!(input.cursor_pos, 3); // one CJK char = 3 bytes
    }

    #[test]
    fn backspace_at_start_is_noop() {
        let mut input = InputBox::new();
        input.backspace();
        assert!(input.is_empty());
    }

    #[test]
    fn delete_at_cursor() {
        let mut input = InputBox::new();
        input.insert_str("abc");
        input.move_home();
        input.delete();
        assert_eq!(input.text, "bc");
        assert_eq!(input.cursor_pos, 0);
    }

    #[test]
    fn delete_at_end_is_noop() {
        let mut input = InputBox::new();
        input.insert_str("abc");
        input.delete();
        assert_eq!(input.text, "abc");
    }

    #[test]
    fn cursor_movement() {
        let mut input = InputBox::new();
        input.insert_str("hello");

        input.move_home();
        assert_eq!(input.cursor_pos, 0);

        input.move_end();
        assert_eq!(input.cursor_pos, 5);

        input.move_left();
        assert_eq!(input.cursor_pos, 4);

        input.move_right();
        assert_eq!(input.cursor_pos, 5);

        // Right at end is noop
        input.move_right();
        assert_eq!(input.cursor_pos, 5);

        // Left at start is noop
        input.move_home();
        input.move_left();
        assert_eq!(input.cursor_pos, 0);
    }

    #[test]
    fn cursor_movement_cjk() {
        let mut input = InputBox::new();
        input.insert_str("你好");
        // cursor at end: byte 6
        assert_eq!(input.cursor_pos, 6);

        input.move_left();
        assert_eq!(input.cursor_pos, 3); // before '好'

        input.move_left();
        assert_eq!(input.cursor_pos, 0); // before '你'

        input.move_right();
        assert_eq!(input.cursor_pos, 3); // after '你'
    }

    #[test]
    fn insert_in_middle() {
        let mut input = InputBox::new();
        input.insert_str("ac");
        input.move_left();
        input.insert('b');
        assert_eq!(input.text, "abc");
        assert_eq!(input.cursor_pos, 2);
    }

    #[test]
    fn insert_in_middle_cjk() {
        let mut input = InputBox::new();
        input.insert_str("你界");
        input.move_left(); // before '界', cursor_pos = 3
        input.insert_str("好世");
        assert_eq!(input.text, "你好世界");
        assert_eq!(input.cursor_pos, 9); // "你好世" = 9 bytes
    }

    // ── Ctrl+W delete word tests ──

    #[test]
    fn delete_word_single_word() {
        let mut input = InputBox::new();
        input.insert_str("hello");
        input.delete_word();
        assert_eq!(input.text, "");
        assert_eq!(input.cursor_pos, 0);
    }

    #[test]
    fn delete_word_multiple_words() {
        let mut input = InputBox::new();
        input.insert_str("hello world");
        input.delete_word();
        assert_eq!(input.text, "hello ");
        assert_eq!(input.cursor_pos, 6);
    }

    #[test]
    fn delete_word_with_trailing_spaces() {
        let mut input = InputBox::new();
        input.insert_str("hello   ");
        input.delete_word();
        assert_eq!(input.text, "");
        assert_eq!(input.cursor_pos, 0);
    }

    #[test]
    fn delete_word_cjk() {
        let mut input = InputBox::new();
        input.insert_str("你好 世界");
        input.delete_word();
        assert_eq!(input.text, "你好 ");
    }

    #[test]
    fn delete_word_at_start_is_noop() {
        let mut input = InputBox::new();
        input.insert_str("hello");
        input.move_home();
        input.delete_word();
        assert_eq!(input.text, "hello");
    }

    #[test]
    fn delete_word_in_middle() {
        let mut input = InputBox::new();
        input.insert_str("one two three");
        // Move cursor to end of "two" (byte 7)
        input.cursor_pos = 7; // "one two"
        input.delete_word();
        assert_eq!(input.text, "one  three");
    }

    // ── Visual cursor position tests ──

    #[test]
    fn cursor_visual_position_empty() {
        let input = InputBox::new();
        let (col, row) = input.cursor_visual_position(80);
        assert_eq!(col, 2); // prompt width
        assert_eq!(row, 0);
    }

    #[test]
    fn cursor_visual_position_ascii() {
        let mut input = InputBox::new();
        input.insert_str("hello");
        let (col, row) = input.cursor_visual_position(80);
        assert_eq!(col, 7); // prompt(2) + "hello"(5)
        assert_eq!(row, 0);
    }

    #[test]
    fn cursor_visual_position_cjk() {
        let mut input = InputBox::new();
        input.insert_str("你好");
        let (col, row) = input.cursor_visual_position(80);
        assert_eq!(col, 6); // prompt(2) + 2 CJK × 2 cols = 6
        assert_eq!(row, 0);
    }

    #[test]
    fn cursor_visual_position_wrapping() {
        let mut input = InputBox::new();
        // width=12, prompt=2, avail=10
        input.insert_str("abcdefghijklm");
        let (col, row) = input.cursor_visual_position(12);
        // First row: "abcdefghij" (10 chars), second row: "klm" (3 chars, no prompt)
        assert_eq!(row, 1);
        assert_eq!(col, 3); // no prompt on continuation
    }

    #[test]
    fn cursor_visual_position_multiline() {
        let mut input = InputBox::new();
        input.insert_str("abc\ndef");
        let (col, row) = input.cursor_visual_position(80);
        assert_eq!(row, 1);
        assert_eq!(col, 5); // prompt(2) + "def"(3)
    }

    #[test]
    fn cursor_visual_position_cjk_wrapping() {
        let mut input = InputBox::new();
        // width=8, prompt=2, avail=6. Each CJK = 2 cols, so 3 fit per row
        input.insert_str("你好世界"); // 4 CJK chars
        let (col, row) = input.cursor_visual_position(8);
        // Row 0: "你好世" (6 cols), Row 1: "界" (2 cols)
        assert_eq!(row, 1);
        assert_eq!(col, 2); // no prompt on continuation
    }

    // ── cursor_position (with border) tests ──

    #[test]
    fn cursor_position_with_border() {
        let input = InputBox::new();
        let area = Rect::new(0, 0, 80, 5);
        let (cx, cy) = input.cursor_position(area);
        assert_eq!(cx, 3); // border(1) + prompt(2)
        assert_eq!(cy, 1); // border(1)
    }

    #[test]
    fn cursor_position_with_text() {
        let mut input = InputBox::new();
        input.insert_str("hello");
        let area = Rect::new(0, 0, 80, 5);
        let (cx, cy) = input.cursor_position(area);
        assert_eq!(cx, 8); // border(1) + prompt(2) + "hello"(5)
        assert_eq!(cy, 1);
    }

    #[test]
    fn cursor_position_with_cjk() {
        let mut input = InputBox::new();
        input.insert_str("你好");
        let area = Rect::new(0, 0, 80, 5);
        let (cx, cy) = input.cursor_position(area);
        assert_eq!(cx, 7); // border(1) + prompt(2) + 2×2
        assert_eq!(cy, 1);
    }

    #[test]
    fn cursor_position_multiline() {
        let mut input = InputBox::new();
        input.insert_str("line1\nab");
        let area = Rect::new(0, 0, 80, 7);
        let (cx, cy) = input.cursor_position(area);
        assert_eq!(cx, 5); // border(1) + prompt(2) + "ab"(2)
        assert_eq!(cy, 2); // border(1) + row 1
    }

    #[test]
    fn cursor_position_scrolls_with_long_content() {
        let mut input = InputBox::new();
        input.insert_str("line1\nline2\nline3\nline4");
        let area = Rect::new(0, 0, 20, 5);
        let (cx, cy) = input.cursor_position(area);
        assert_eq!(cx, 8); // border(1) + prompt(2) + "line4"(5)
        assert_eq!(cy, 3);
    }

    // ── Other tests ──

    #[test]
    fn insert_newline() {
        let mut input = InputBox::new();
        input.insert_str("line1");
        input.insert('\n');
        input.insert_str("line2");
        assert_eq!(input.text, "line1\nline2");
        assert_eq!(input.cursor_pos, 11); // "line1\nline2".len()
    }

    #[test]
    fn tab_completion_single_match() {
        let mut input = InputBox::new();
        input.completions = vec!["/create".into(), "/help".into(), "/list".into()];
        input.insert_str("/cr");
        input.tab();
        assert_eq!(input.text, "/create ");
    }

    #[test]
    fn tab_completion_multiple_matches_confirms_first() {
        let mut input = InputBox::new();
        input.completions = vec!["/create".into(), "/cancel".into(), "/close".into()];
        input.insert_str("/c");
        assert!(input.is_popup_visible());
        input.tab();
        assert!(input.text.starts_with("/c"));
        assert!(!input.is_popup_visible());
    }

    #[test]
    fn tab_no_match_is_noop() {
        let mut input = InputBox::new();
        input.completions = vec!["/create".into()];
        input.insert_str("xyz");
        input.tab();
        assert_eq!(input.text, "xyz");
    }

    #[test]
    fn shift_tab_navigates_selection() {
        let mut input = InputBox::new();
        input.completions = vec!["@alice".into(), "@bob".into(), "@charlie".into()];
        input.insert_str("@");
        assert!(input.is_popup_visible());
        input.shift_tab();
        assert_eq!(input.text, "@");
        assert!(input.is_popup_visible());
    }

    #[test]
    fn ctrl_n_p_navigates_then_tab_confirms() {
        let mut input = InputBox::new();
        input.completions = vec!["@alice".into(), "@bob".into(), "@charlie".into()];
        input.insert_str("@");
        input.select_next();
        input.select_next();
        input.tab();
        assert_eq!(input.text, "@bob ");
        assert!(!input.is_popup_visible());
    }

    #[test]
    fn visual_line_count() {
        let mut input = InputBox::new();
        assert_eq!(input.visual_line_count(80), 1);

        input.insert_str(&"a".repeat(100));
        assert!(input.visual_line_count(80) >= 2);

        assert_eq!(input.visual_line_count(2), 1);
        assert_eq!(input.visual_line_count(1), 1);
    }

    #[test]
    fn visual_line_count_multiline() {
        let mut input = InputBox::new();
        input.insert_str("line1\nline2\nline3");
        assert!(input.visual_line_count(80) >= 3);
    }

    #[test]
    fn visual_line_count_exact_fit() {
        let mut input = InputBox::new();
        input.insert_str(&"a".repeat(78));
        assert_eq!(input.visual_line_count(80), 1);
    }

    #[test]
    fn visual_line_count_cjk_exact_fit() {
        let mut input = InputBox::new();
        let cjk: String = std::iter::repeat('你').take(39).collect();
        input.insert_str(&cjk);
        assert_eq!(input.visual_line_count(80), 1);
    }

    #[test]
    fn scroll_offset_keeps_cursor_visible() {
        let mut input = InputBox::new();
        input.insert_str("line1\nline2\nline3\nline4");
        assert_eq!(input.scroll_offset_for_height(20, 2), 2);
    }

    #[test]
    fn command_detection() {
        let mut input = InputBox::new();
        input.insert_str("/add claude alice");
        let text = input.take();
        assert!(text.starts_with('/'));

        input.insert_str("@alice hello");
        let text = input.take();
        assert!(text.starts_with('@'));

        input.insert_str("plain message");
        let text = input.take();
        assert!(!text.starts_with('/'));
        assert!(!text.starts_with('@'));
    }

    #[test]
    fn multiline_ctrl_a_moves_to_current_line_start() {
        let mut input = InputBox::new();
        input.insert_str("hello\nworld");
        input.move_line_start();
        assert_eq!(input.cursor_pos, 6); // byte index of 'w' in "world"
    }

    #[test]
    fn multiline_ctrl_e_moves_to_current_line_end() {
        let mut input = InputBox::new();
        input.insert_str("hello\nworld");
        input.cursor_pos = 6; // start of "world"
        input.move_line_end();
        assert_eq!(input.cursor_pos, 11); // end

        input.cursor_pos = 2; // in "hello"
        input.move_line_end();
        assert_eq!(input.cursor_pos, 5); // before \n
    }

    #[test]
    fn multiline_ctrl_k_kills_to_current_line_end() {
        let mut input = InputBox::new();
        input.insert_str("hello\nworld");
        input.cursor_pos = 8; // at 'r' in "world"
        input.kill_to_line_end();
        assert_eq!(input.text, "hello\nwo");
        assert_eq!(input.cursor_pos, 8);
    }

    #[test]
    fn multiline_ctrl_k_stops_at_newline() {
        let mut input = InputBox::new();
        input.insert_str("hello\nworld");
        input.cursor_pos = 2; // at 'l' in "hello"
        input.kill_to_line_end();
        assert_eq!(input.text, "he\nworld");
        assert_eq!(input.cursor_pos, 2);
    }

    #[test]
    fn multiline_ctrl_u_kills_to_current_line_start() {
        let mut input = InputBox::new();
        input.insert_str("hello\nworld");
        input.cursor_pos = 8; // at 'r' in "world"
        input.kill_to_line_start();
        assert_eq!(input.text, "hello\nrld");
        assert_eq!(input.cursor_pos, 6);
    }

    #[test]
    fn multiline_ctrl_u_on_first_line() {
        let mut input = InputBox::new();
        input.insert_str("hello\nworld");
        input.cursor_pos = 3; // at second 'l' in "hello"
        input.kill_to_line_start();
        assert_eq!(input.text, "lo\nworld");
        assert_eq!(input.cursor_pos, 0);
    }
}
