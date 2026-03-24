use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Widget, Wrap};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::theme;

pub struct InputBox {
    pub text: String,
    /// Cursor position as character index (not byte index)
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

    /// Convert character index to byte index for String operations.
    fn byte_index(&self) -> usize {
        self.text
            .char_indices()
            .map(|(i, _)| i)
            .nth(self.cursor_pos)
            .unwrap_or(self.text.len())
    }

    pub fn insert(&mut self, c: char) {
        let idx = self.byte_index();
        self.text.insert(idx, c);
        self.cursor_pos += 1;
        self.auto_complete();
    }

    pub fn insert_str(&mut self, s: &str) {
        let idx = self.byte_index();
        self.text.insert_str(idx, s);
        self.cursor_pos += s.chars().count();
        self.auto_complete();
    }

    pub fn backspace(&mut self) {
        if self.cursor_pos > 0 {
            self.cursor_pos -= 1;
            let idx = self.byte_index();
            let ch_len = self.text[idx..]
                .chars()
                .next()
                .map(|c| c.len_utf8())
                .unwrap_or(0);
            self.text.drain(idx..idx + ch_len);
        }
        self.auto_complete();
    }

    pub fn delete(&mut self) {
        let idx = self.byte_index();
        if idx < self.text.len() {
            let ch_len = self.text[idx..]
                .chars()
                .next()
                .map(|c| c.len_utf8())
                .unwrap_or(0);
            self.text.drain(idx..idx + ch_len);
        }
    }

    pub fn move_left(&mut self) {
        self.cursor_pos = self.cursor_pos.saturating_sub(1);
    }

    pub fn move_right(&mut self) {
        if self.cursor_pos < self.text.chars().count() {
            self.cursor_pos += 1;
        }
    }

    pub fn move_home(&mut self) {
        self.cursor_pos = 0;
    }

    pub fn move_end(&mut self) {
        self.cursor_pos = self.text.chars().count();
    }

    /// Move to start of current line (Ctrl+A)
    pub fn move_line_start(&mut self) {
        let byte_idx = self.byte_index();
        let before = &self.text[..byte_idx];
        if let Some(nl_pos) = before.rfind('\n') {
            self.cursor_pos = self.text[..nl_pos + 1].chars().count();
        } else {
            self.cursor_pos = 0;
        }
    }

    /// Move cursor up one line, preserving column position.
    pub fn move_up(&mut self) -> bool {
        let byte_idx = self.byte_index();
        let before = &self.text[..byte_idx];
        // Find start of current line
        let cur_line_start = before.rfind('\n').map(|p| p + 1).unwrap_or(0);
        if cur_line_start == 0 {
            return false; // already on first line
        }
        // Column offset (in chars) within current line
        let col = self.text[cur_line_start..byte_idx].chars().count();
        // Find start of previous line
        let prev_line_start = self.text[..cur_line_start - 1]
            .rfind('\n')
            .map(|p| p + 1)
            .unwrap_or(0);
        let prev_line_end = cur_line_start - 1; // position of '\n'
        let prev_line_len = self.text[prev_line_start..prev_line_end].chars().count();
        let target_col = col.min(prev_line_len);
        self.cursor_pos = self.text[..prev_line_start].chars().count() + target_col;
        true
    }

    /// Move cursor down one line, preserving column position.
    pub fn move_down(&mut self) -> bool {
        let byte_idx = self.byte_index();
        let before = &self.text[..byte_idx];
        // Find start of current line
        let cur_line_start = before.rfind('\n').map(|p| p + 1).unwrap_or(0);
        // Column offset within current line
        let col = self.text[cur_line_start..byte_idx].chars().count();
        // Find end of current line (position of next '\n')
        let after = &self.text[byte_idx..];
        let next_nl = after.find('\n');
        if next_nl.is_none() {
            return false; // already on last line
        }
        let next_line_start_byte = byte_idx + next_nl.unwrap() + 1;
        let next_line_end_byte = self.text[next_line_start_byte..]
            .find('\n')
            .map(|p| next_line_start_byte + p)
            .unwrap_or(self.text.len());
        let next_line_len = self.text[next_line_start_byte..next_line_end_byte]
            .chars()
            .count();
        let target_col = col.min(next_line_len);
        self.cursor_pos = self.text[..next_line_start_byte].chars().count() + target_col;
        true
    }

    /// Returns true if the text contains multiple lines.
    pub fn is_multiline(&self) -> bool {
        self.text.contains('\n')
    }

    /// Move to end of current line (Ctrl+E)
    pub fn move_line_end(&mut self) {
        let byte_idx = self.byte_index();
        let after = &self.text[byte_idx..];
        if let Some(nl_pos) = after.find('\n') {
            self.cursor_pos = self.text[..byte_idx + nl_pos].chars().count();
        } else {
            self.cursor_pos = self.text.chars().count();
        }
    }

    /// Kill from cursor to end of current line (Ctrl+K)
    pub fn kill_to_line_end(&mut self) {
        let byte_idx = self.byte_index();
        let after = &self.text[byte_idx..];
        if let Some(nl_pos) = after.find('\n') {
            self.text.drain(byte_idx..byte_idx + nl_pos);
        } else {
            self.text.truncate(byte_idx);
        }
    }

    /// Kill from start of current line to cursor (Ctrl+U)
    pub fn kill_to_line_start(&mut self) {
        let byte_idx = self.byte_index();
        let before = &self.text[..byte_idx];
        let line_start_byte = if let Some(nl_pos) = before.rfind('\n') {
            nl_pos + 1
        } else {
            0
        };
        let chars_removed = self.text[line_start_byte..byte_idx].chars().count();
        self.text.drain(line_start_byte..byte_idx);
        self.cursor_pos -= chars_removed;
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
            // Append trailing space for ergonomics
            let byte_idx = self.byte_index();
            if byte_idx >= self.text.len()
                || !self.text[byte_idx..].starts_with(' ')
            {
                self.text.insert(byte_idx, ' ');
                self.cursor_pos += 1;
            }
            self.dismiss_popup();
        } else {
            // No popup: build candidates and confirm immediately
            self.build_candidates();
            if !self.candidates.is_empty() {
                self.apply_candidate(0);
                let byte_idx = self.byte_index();
                if byte_idx >= self.text.len()
                    || !self.text[byte_idx..].starts_with(' ')
                {
                    self.text.insert(byte_idx, ' ');
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
        let before = &self.text[..self.byte_index()];
        before
            .rsplit(|c: char| c.is_whitespace())
            .next()
            .unwrap_or("")
            .to_string()
    }

    fn apply_candidate(&mut self, idx: usize) {
        if let Some(candidate) = self.candidates.get(idx) {
            let word = self.current_word();
            let byte_idx = self.byte_index();
            let word_byte_start = byte_idx - word.len();
            let word_char_count = word.chars().count();
            self.text
                .replace_range(word_byte_start..byte_idx, candidate);
            self.cursor_pos = self.cursor_pos - word_char_count + candidate.chars().count();
        }
    }

    /// Number of visual lines the input takes at given width.
    pub fn visual_line_count(&self, width: u16) -> u16 {
        if width <= 2 {
            return 1;
        }
        let w = width as usize;
        let prompt_w: usize = 2;
        let text_lines: Vec<&str> = if self.text.contains('\n') {
            self.text.split('\n').collect()
        } else {
            vec![&self.text]
        };
        let mut total: usize = 0;
        for line in text_lines.iter() {
            let mut col = prompt_w;
            let mut rows = 1usize;
            for ch in line.chars() {
                let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
                if col + cw > w {
                    rows += 1;
                    col = cw;
                } else {
                    col += cw;
                }
            }
            total += rows;
        }
        total.max(1) as u16
    }

    fn cursor_visual_position(&self, width: u16) -> (u16, u16) {
        let prompt_w: usize = 2; // "> " or "  "
        let inner_w = width as usize;
        if inner_w == 0 {
            return (0, 0);
        }
        let text_before = &self.text[..self.byte_index()];

        let mut visual_row: usize = 0;
        let mut col: usize = 0;

        for (i, line) in text_before.split('\n').enumerate() {
            if i > 0 {
                // Newline: move to next row
                visual_row += 1;
            }
            // Each line starts with prompt (2 cols)
            col = prompt_w;
            // Walk characters, wrapping when a char doesn't fit
            for ch in line.chars() {
                let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
                if col + cw > inner_w {
                    // Char doesn't fit on current line, wrap
                    visual_row += 1;
                    col = cw;
                } else {
                    col += cw;
                }
            }
        }

        (col as u16, visual_row as u16)
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

        // Build lines: first line has prompt "> ", continuation lines don't
        let mut lines: Vec<Line> = Vec::new();
        let text_lines: Vec<&str> = if self.text.contains('\n') {
            self.text.split('\n').collect()
        } else {
            vec![&self.text]
        };

        for (i, tl) in text_lines.iter().enumerate() {
            if i == 0 {
                let prompt = Span::styled("> ", Style::default().fg(Color::DarkGray));
                let text = Span::raw(tl.to_string());
                lines.push(Line::from(vec![prompt, text]));
            } else {
                let prompt = Span::styled("  ", Style::default().fg(Color::DarkGray));
                let text = Span::raw(tl.to_string());
                lines.push(Line::from(vec![prompt, text]));
            }
        }

        let scroll_y = self.scroll_offset_for_height(inner.width, inner.height);
        if lines.len() > 3 {
            tracing::info!(
                "INPUT_RENDER: lines={} inner_h={} inner_w={} scroll_y={} text_len={}",
                lines.len(), inner.height, inner.width, scroll_y, self.text.len()
            );
        }
        Paragraph::new(lines)
            .scroll((scroll_y, 0))
            .wrap(Wrap { trim: false })
            .render(inner, buf);
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
        // Position popup above input
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
        let inner_x = area.x + 1; // left border
        let inner_y = area.y + 1; // top border
        let inner_width = area.width.saturating_sub(2); // left + right borders
        let visible_height = area.height.saturating_sub(2); // top + bottom borders
        let (col, visual_row) = self.cursor_visual_position(inner_width);
        let scroll_y = self.scroll_offset_for_height(inner_width, visible_height);
        (inner_x + col, inner_y + visual_row.saturating_sub(scroll_y))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn backspace() {
        let mut input = InputBox::new();
        input.insert_str("abc");
        input.backspace();
        assert_eq!(input.text, "ab");
        assert_eq!(input.cursor_pos, 2);
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
    fn insert_in_middle() {
        let mut input = InputBox::new();
        input.insert_str("ac");
        input.move_left(); // cursor at 1
        input.insert('b');
        assert_eq!(input.text, "abc");
        assert_eq!(input.cursor_pos, 2); // after 'b'
    }

    #[test]
    fn backspace_multibyte() {
        let mut input = InputBox::new();
        input.insert_str("你好");
        input.backspace();
        assert_eq!(input.text, "你");
    }

    #[test]
    fn tab_completion_single_match() {
        let mut input = InputBox::new();
        input.completions = vec!["/create".into(), "/help".into(), "/list".into()];
        input.insert_str("/cr");
        input.tab();
        assert_eq!(input.text, "/create "); // Tab confirms with trailing space
    }

    #[test]
    fn tab_completion_multiple_matches_confirms_first() {
        let mut input = InputBox::new();
        input.completions = vec!["/create".into(), "/cancel".into(), "/close".into()];
        input.insert_str("/c");
        // auto_complete already showed popup
        assert!(input.is_popup_visible());
        input.tab(); // Confirms first candidate and closes popup
        assert!(input.text.starts_with("/c")); // Applied a candidate
        assert!(!input.is_popup_visible()); // Popup closed
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
        // auto_complete opens popup with no selection
        assert!(input.is_popup_visible());
        input.shift_tab(); // wraps to last candidate
        // shift_tab only navigates, doesn't apply
        assert_eq!(input.text, "@"); // text unchanged
        assert!(input.is_popup_visible()); // popup still open
    }

    #[test]
    fn ctrl_n_p_navigates_then_tab_confirms() {
        let mut input = InputBox::new();
        input.completions = vec!["@alice".into(), "@bob".into(), "@charlie".into()];
        input.insert_str("@");
        // auto_complete opens popup
        input.select_next(); // select @alice (idx 0)
        input.select_next(); // select @bob (idx 1)
        input.tab(); // confirm @bob
        assert_eq!(input.text, "@bob ");
        assert!(!input.is_popup_visible());
    }

    #[test]
    fn visual_line_count() {
        let mut input = InputBox::new();
        assert_eq!(input.visual_line_count(80), 1);

        input.insert_str("a".repeat(100).as_str());
        assert!(input.visual_line_count(80) >= 2);

        // Narrow width
        assert_eq!(input.visual_line_count(2), 1); // edge case
        assert_eq!(input.visual_line_count(1), 1); // edge case
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
    fn cursor_position_with_border() {
        let input = InputBox::new();
        let area = Rect::new(0, 0, 80, 5);
        let (cx, cy) = input.cursor_position(area);
        // Empty text: cursor at left border (1) + prompt (2) = 3, y=1 (top border)
        assert_eq!(cx, 3);
        assert_eq!(cy, 1);
    }

    #[test]
    fn cursor_position_with_text() {
        let mut input = InputBox::new();
        input.insert_str("hello");
        let area = Rect::new(0, 0, 80, 5);
        let (cx, cy) = input.cursor_position(area);
        // left border (1) + prompt (2) + "hello" (5) = 8
        assert_eq!(cx, 8);
        assert_eq!(cy, 1);
    }

    #[test]
    fn cursor_position_with_cjk() {
        let mut input = InputBox::new();
        input.insert_str("你好"); // each CJK char is 2 columns wide
        let area = Rect::new(0, 0, 80, 5);
        let (cx, cy) = input.cursor_position(area);
        // left border (1) + prompt (2) + 2 CJK × 2 = 7
        assert_eq!(cx, 7);
        assert_eq!(cy, 1);
    }

    #[test]
    fn insert_newline() {
        let mut input = InputBox::new();
        input.insert_str("line1");
        input.insert('\n');
        input.insert_str("line2");
        assert_eq!(input.text, "line1\nline2");
        assert_eq!(input.cursor_pos, 11);
    }

    #[test]
    fn visual_line_count_multiline() {
        let mut input = InputBox::new();
        input.insert_str("line1\nline2\nline3");
        // 3 logical lines → at least 3 visual lines
        assert!(input.visual_line_count(80) >= 3);
    }

    #[test]
    fn cursor_position_multiline() {
        let mut input = InputBox::new();
        input.insert_str("line1\nab");
        let area = Rect::new(0, 0, 80, 7);
        let (cx, cy) = input.cursor_position(area);
        // Second line: left border (1) + prompt "  " (2) + "ab" (2) = col 5
        assert_eq!(cx, 5);
        // First line takes row 1, second line cursor at row 2
        assert_eq!(cy, 2);
    }

    #[test]
    fn cursor_position_scrolls_with_long_content() {
        let mut input = InputBox::new();
        input.insert_str("line1\nline2\nline3\nline4");
        let area = Rect::new(0, 0, 20, 5);
        let (cx, cy) = input.cursor_position(area);
        // left border (1) + prompt (2) + "line4" (5) = 8, scrolled to fit in 3 content rows
        assert_eq!(cx, 8);
        assert_eq!(cy, 3);
    }

    #[test]
    fn scroll_offset_keeps_cursor_visible() {
        let mut input = InputBox::new();
        input.insert_str("line1\nline2\nline3\nline4");
        assert_eq!(input.scroll_offset_for_height(20, 2), 2);
    }

    #[test]
    fn visual_line_count_exact_fit() {
        // Text that exactly fills one line (prompt 2 + text 78 = 80 = width)
        let mut input = InputBox::new();
        input.insert_str(&"a".repeat(78));
        // Should be 1 line, not 2
        assert_eq!(input.visual_line_count(80), 1);
    }

    #[test]
    fn visual_line_count_cjk_exact_fit() {
        // CJK: each char is 2 columns. prompt 2 + 39 CJK chars × 2 = 80 = width
        let mut input = InputBox::new();
        let cjk: String = std::iter::repeat('你').take(39).collect();
        input.insert_str(&cjk);
        // Should be 1 line, not 2
        assert_eq!(input.visual_line_count(80), 1);
    }

    #[test]
    fn multiline_ctrl_a_moves_to_current_line_start() {
        let mut input = InputBox::new();
        input.insert_str("hello\nworld");
        // cursor at end (pos 11)
        input.move_line_start();
        assert_eq!(input.cursor_pos, 6); // start of "world", not 0
    }

    #[test]
    fn multiline_ctrl_e_moves_to_current_line_end() {
        let mut input = InputBox::new();
        input.insert_str("hello\nworld");
        input.cursor_pos = 6; // start of "world"
        input.move_line_end();
        assert_eq!(input.cursor_pos, 11); // end of "world"

        // Now test on first line
        input.cursor_pos = 2; // in "hello"
        input.move_line_end();
        assert_eq!(input.cursor_pos, 5); // end of "hello", before \n
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
