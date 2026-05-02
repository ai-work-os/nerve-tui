use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Clear, Widget};
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
    // Input history (shell-style up/down navigation)
    history_entries: Vec<String>,
    history_cursor: Option<usize>,
    history_draft: String,
    history_max: usize,
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
            history_entries: Vec::new(),
            history_cursor: None,
            history_draft: String::new(),
            history_max: 100,
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

    // --- Input history (shell-style up/down) ---

    /// Push a message into history. Skips empty and consecutive duplicates.
    pub fn history_push(&mut self, text: &str) {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return;
        }
        if self.history_entries.last().map(|s| s.as_str()) == Some(trimmed) {
            return;
        }
        self.history_entries.push(trimmed.to_string());
        if self.history_entries.len() > self.history_max {
            self.history_entries.remove(0);
        }
        self.history_cursor = None;
    }

    /// Number of history entries.
    pub fn history_len(&self) -> usize {
        self.history_entries.len()
    }

    /// Navigate to the previous (older) history entry. Returns true if text was changed.
    pub fn history_up(&mut self) -> bool {
        if self.history_entries.is_empty() {
            return false;
        }
        let new_cursor = match self.history_cursor {
            None => {
                // Save current input as draft before entering history
                self.history_draft = self.text.clone();
                self.history_entries.len() - 1
            }
            Some(0) => return false, // already at oldest
            Some(c) => c - 1,
        };
        self.history_cursor = Some(new_cursor);
        self.text = self.history_entries[new_cursor].clone();
        self.cursor_pos = self.text.len();
        true
    }

    /// Navigate to the next (newer) history entry or restore draft. Returns true if text was changed.
    pub fn history_down(&mut self) -> bool {
        let cursor = match self.history_cursor {
            None => return false, // not browsing history
            Some(c) => c,
        };
        if cursor + 1 < self.history_entries.len() {
            let new_cursor = cursor + 1;
            self.history_cursor = Some(new_cursor);
            self.text = self.history_entries[new_cursor].clone();
            self.cursor_pos = self.text.len();
        } else {
            // Past newest entry — restore draft
            self.history_cursor = None;
            self.text = std::mem::take(&mut self.history_draft);
            self.cursor_pos = self.text.len();
        }
        true
    }

    /// Reset history browsing state (called after sending a message).
    pub fn history_reset(&mut self) {
        self.history_cursor = None;
        self.history_draft.clear();
    }

    pub fn take(&mut self) -> String {
        self.cursor_pos = 0;
        self.dismiss_popup();
        self.history_reset();
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
                self.popup_visible = true;
                self.selected = None;
            } else {
                self.dismiss_popup();
            }
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

    /// Auto-show completion popup when typing `/` or `@` prefixed words,
    /// or after command keywords like `/dm `.
    fn auto_complete(&mut self) {
        let word = self.current_word();
        let before = &self.text[..self.cursor_pos];
        let has_cmd_context = word.is_empty() && {
            let t = before.trim_end();
            t == "/dm" || t == "/stop" || t == "/cancel" || t == "/add" || t == "/split"
        };
        if word.starts_with('/') || word.starts_with('@') || has_cmd_context {
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
        let before = &self.text[..self.cursor_pos];


        // Context-aware: if input starts with a command that takes a specific argument type,
        // filter candidates to only show relevant items.
        let context_prefix = if word.is_empty() {
            let trimmed = before.trim_end();
            if trimmed == "/dm" {
                // After /dm: show agent names (bare and @ prefix)
                Some("agent")
            } else if trimmed == "/stop" || trimmed == "/cancel" {
                // After /stop, /cancel: show bare agent names only
                Some("bare_agent")
            } else if trimmed == "/add" {
                // After /add: show adapter names
                Some("adapter")
            } else if trimmed == "/split" {
                // After /split: show @agent names
                Some("@")
            } else if trimmed == "/ch" {
                // After /ch: show #channel names
                Some("#")
            } else {
                None
            }
        } else {
            // word is non-empty: check if text before the word ends with a command
            let prefix_text = before[..before.len() - word.len()].trim_end();
            if prefix_text == "/dm" {
                Some("agent")
            } else if prefix_text == "/stop" || prefix_text == "/cancel" {
                Some("bare_agent")
            } else if prefix_text == "/add" {
                Some("adapter")
            } else if prefix_text == "/split" {
                Some("@")
            } else if prefix_text == "/ch" {
                Some("#")
            } else {
                None
            }
        };


        self.candidates = self
            .completions
            .iter()
            .filter(|c| {
                let cl = c.to_lowercase();
                match context_prefix {
                    Some("agent") => {
                        // Show agent names (not commands, not adapters)
                        !c.starts_with('/') && cl.starts_with(&word)
                    }
                    Some("bare_agent") => {
                        // Show bare agent names only (no @ prefix, no commands)
                        !c.starts_with('/') && !c.starts_with('@') && cl.starts_with(&word)
                    }
                    Some("adapter") => {
                        // Show adapter names only
                        !c.starts_with('/') && !c.starts_with('@') && cl.starts_with(&word)
                    }
                    Some("@") => {
                        // Show @agent names
                        c.starts_with('@') && cl.starts_with(&word)
                    }
                    Some("#") => {
                        // Show #channel names — match word against name with or without #
                        if word.starts_with('#') {
                            c.starts_with('#') && cl.starts_with(&word)
                        } else {
                            c.starts_with('#') && cl[1..].starts_with(&word)
                        }
                    }
                    _ => cl.starts_with(&word),
                }
            })
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
        let rows = wrap_lines(&self.text, width as usize, 0, 0);
        let raw = rows.len() as u16 + 1; // +1 for metadata row
        raw.max(2).min(10 + 1) // min 2, max 10 content rows + 1 metadata
    }

    fn cursor_visual_position_inner(&self, width: u16, first_prompt: usize, cont_prompt: usize) -> (u16, u16) {
        let inner_w = width as usize;
        if inner_w == 0 {
            return (0, 0);
        }
        let rows = wrap_lines(&self.text, inner_w, first_prompt, cont_prompt);

        // Find which row contains the cursor
        for (row_idx, row) in rows.iter().enumerate() {
            if self.cursor_pos >= row.byte_start && self.cursor_pos <= row.byte_end {
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

    #[allow(dead_code)] // used in tests
    fn scroll_offset_for_height(&self, width: u16, visible_height: u16) -> u16 {
        if visible_height == 0 {
            return 0;
        }
        let (_, cursor_row) = self.cursor_visual_position_inner(width, 0, 0);
        cursor_row.saturating_sub(visible_height.saturating_sub(1))
    }

    pub fn render(&self, area: Rect, buf: &mut Buffer) {
        let t = theme::current();
        // Fill with L2 background
        for y in area.y..area.y + area.height {
            for x in area.x..area.x + area.width {
                if let Some(cell) = buf.cell_mut((x, y)) {
                    cell.set_bg(t.background_element);
                }
            }
        }

        // Inner area: 2-char horizontal padding, 1 row top padding
        let inner = Rect {
            x: area.x + 2,
            y: area.y + 1,
            width: area.width.saturating_sub(4),
            height: area.height.saturating_sub(1),
        };

        let rows = wrap_lines(&self.text, inner.width as usize, 0, 0);
        let visible_height = inner.height.saturating_sub(1); // reserve 1 row for metadata
        let scroll_y = if visible_height == 0 {
            0
        } else {
            let (_, cursor_row) = self.cursor_visual_position_inner(inner.width, 0, 0);
            cursor_row.saturating_sub(visible_height.saturating_sub(1))
        };

        let text_style = Style::default().fg(t.text).bg(t.background_element);

        for (i, row) in rows.iter().enumerate() {
            let visual_y = i as u16;
            if visual_y < scroll_y {
                continue;
            }
            let screen_y = inner.y + visual_y - scroll_y;
            if screen_y >= inner.y + visible_height {
                break;
            }
            buf.set_string(inner.x, screen_y, &row.text, text_style);
        }

        // Metadata line at bottom
        let meta_y = area.y + area.height - 1;
        let meta_style = Style::default().fg(t.text_muted).bg(t.background_element);
        let right_hint = "↩ 发送 · ⇧↩ 换行";
        let hint_w = unicode_width::UnicodeWidthStr::width(right_hint) as u16;
        let hint_x = area.x + area.width.saturating_sub(hint_w + 2);
        buf.set_string(hint_x, meta_y, right_hint, meta_style);
    }

    pub fn render_with_meta(&self, area: Rect, buf: &mut Buffer, meta_left: &str, agent_color: Option<Color>) {
        let t = theme::current();
        // Fill with background_element
        for y in area.y..area.y + area.height {
            for x in area.x..area.x + area.width {
                if let Some(cell) = buf.cell_mut((x, y)) {
                    cell.set_bg(t.background_element);
                }
            }
        }

        // Draw left border when agent_color is set
        if let Some(color) = agent_color {
            for y in area.y..area.y + area.height {
                if let Some(cell) = buf.cell_mut((area.x, y)) {
                    cell.set_char('│');
                    cell.set_fg(color);
                    cell.set_bg(t.background_element);
                }
            }
        }

        // Content area: shift right by 2 extra chars when border present (border + space)
        let border_offset: u16 = if agent_color.is_some() { 2 } else { 0 };
        let inner = Rect {
            x: area.x + 2 + border_offset,
            y: area.y + 1,
            width: area.width.saturating_sub(4 + border_offset),
            height: area.height.saturating_sub(1),
        };

        let rows = wrap_lines(&self.text, inner.width as usize, 0, 0);
        let visible_height = inner.height.saturating_sub(1);
        let scroll_y = if visible_height == 0 {
            0
        } else {
            let (_, cursor_row) = self.cursor_visual_position_inner(inner.width, 0, 0);
            cursor_row.saturating_sub(visible_height.saturating_sub(1))
        };

        let text_style = Style::default().fg(t.text).bg(t.background_element);

        for (i, row) in rows.iter().enumerate() {
            let visual_y = i as u16;
            if visual_y < scroll_y {
                continue;
            }
            let screen_y = inner.y + visual_y - scroll_y;
            if screen_y >= inner.y + visible_height {
                break;
            }
            buf.set_string(inner.x, screen_y, &row.text, text_style);
        }

        // Metadata line at bottom
        let meta_y = area.y + area.height - 1;
        let meta_style = Style::default().fg(t.text_muted).bg(t.background_element);
        let meta_x = area.x + 2 + border_offset;
        if !meta_left.is_empty() {
            buf.set_string(meta_x, meta_y, meta_left, meta_style);
        }
        let right_hint = "↩ 发送 · ⇧↩ 换行";
        let hint_w = unicode_width::UnicodeWidthStr::width(right_hint) as u16;
        let hint_x = area.x + area.width.saturating_sub(hint_w + 2);
        buf.set_string(hint_x, meta_y, right_hint, meta_style);
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
        self.cursor_position_inner(area, 0)
    }

    /// Get cursor screen position with optional left border offset (2 chars: border + space).
    pub fn cursor_position_with_border(&self, area: Rect, has_border: bool) -> (u16, u16) {
        let border_offset: u16 = if has_border { 2 } else { 0 };
        self.cursor_position_inner(area, border_offset)
    }

    fn cursor_position_inner(&self, area: Rect, border_offset: u16) -> (u16, u16) {
        let inner_x = area.x + 2 + border_offset;
        let inner_y = area.y + 1;
        let inner_width = area.width.saturating_sub(4 + border_offset);
        let visible_height = area.height.saturating_sub(2); // 1 top pad + 1 metadata
        let (col, visual_row) = self.cursor_visual_position_inner(inner_width, 0, 0);
        let scroll_y = if visible_height == 0 {
            0
        } else {
            visual_row.saturating_sub(visible_height.saturating_sub(1))
        };
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
        let (col, row) = input.cursor_visual_position_inner(80, 0, 0);
        assert_eq!(col, 0); // no prompt
        assert_eq!(row, 0);
    }

    #[test]
    fn cursor_visual_position_ascii() {
        let mut input = InputBox::new();
        input.insert_str("hello");
        let (col, row) = input.cursor_visual_position_inner(80, 0, 0);
        assert_eq!(col, 5); // "hello"(5), no prompt
        assert_eq!(row, 0);
    }

    #[test]
    fn cursor_visual_position_cjk() {
        let mut input = InputBox::new();
        input.insert_str("你好");
        let (col, row) = input.cursor_visual_position_inner(80, 0, 0);
        assert_eq!(col, 4); // 2 CJK × 2 cols = 4, no prompt
        assert_eq!(row, 0);
    }

    #[test]
    fn cursor_visual_position_wrapping() {
        let mut input = InputBox::new();
        // width=12, prompt=0, avail=12
        input.insert_str("abcdefghijklm");
        let (col, row) = input.cursor_visual_position_inner(12, 0, 0);
        // First row: "abcdefghijkl" (12 chars), second row: "m" (1 char)
        assert_eq!(row, 1);
        assert_eq!(col, 1);
    }

    #[test]
    fn cursor_visual_position_multiline() {
        let mut input = InputBox::new();
        input.insert_str("abc\ndef");
        let (col, row) = input.cursor_visual_position_inner(80, 0, 0);
        assert_eq!(row, 1);
        assert_eq!(col, 3); // "def"(3), no prompt
    }

    #[test]
    fn cursor_visual_position_cjk_wrapping() {
        let mut input = InputBox::new();
        // width=8, prompt=0, avail=8. Each CJK = 2 cols, so 4 fit per row
        input.insert_str("你好世界"); // 4 CJK chars = 8 cols, fits in one row
        let (col, row) = input.cursor_visual_position_inner(8, 0, 0);
        assert_eq!(row, 0);
        assert_eq!(col, 8); // all 4 CJK fit in width 8
    }

    // ── cursor_position (with border) tests ──

    #[test]
    fn cursor_position_layout() {
        let input = InputBox::new();
        let area = Rect::new(0, 0, 80, 5);
        let (cx, cy) = input.cursor_position(area);
        assert_eq!(cx, 2); // padding(2)
        assert_eq!(cy, 1); // top_pad(1)
    }

    #[test]
    fn cursor_position_with_text() {
        let mut input = InputBox::new();
        input.insert_str("hello");
        let area = Rect::new(0, 0, 80, 5);
        let (cx, cy) = input.cursor_position(area);
        assert_eq!(cx, 7); // padding(2) + "hello"(5)
        assert_eq!(cy, 1);
    }

    #[test]
    fn cursor_position_with_cjk() {
        let mut input = InputBox::new();
        input.insert_str("你好");
        let area = Rect::new(0, 0, 80, 5);
        let (cx, cy) = input.cursor_position(area);
        assert_eq!(cx, 6); // padding(2) + 2×2
        assert_eq!(cy, 1);
    }

    #[test]
    fn cursor_position_multiline() {
        let mut input = InputBox::new();
        input.insert_str("line1\nab");
        let area = Rect::new(0, 0, 80, 7);
        let (cx, cy) = input.cursor_position(area);
        assert_eq!(cx, 4); // padding(2) + "ab"(2)
        assert_eq!(cy, 2); // top_pad(1) + row 1
    }

    #[test]
    fn cursor_position_scrolls_with_long_content() {
        let mut input = InputBox::new();
        input.insert_str("line1\nline2\nline3\nline4");
        let area = Rect::new(0, 0, 20, 5);
        let (cx, cy) = input.cursor_position(area);
        // visible_height = 5 - 2 = 3 (1 top pad + 1 metadata)
        // 4 rows of text, cursor on row 3 (line4)
        // scroll_y = 3 - (3-1) = 1
        // screen_y = 1 + 3 - 1 = 3
        assert_eq!(cx, 7); // padding(2) + "line4"(5)
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
        assert_eq!(input.visual_line_count(80), 2); // 1 text row + 1 metadata, min 2

        input.insert_str(&"a".repeat(100));
        assert!(input.visual_line_count(80) >= 3); // 2+ text rows + 1 metadata

        assert_eq!(input.visual_line_count(2), 1);
        assert_eq!(input.visual_line_count(1), 1);
    }

    #[test]
    fn visual_line_count_multiline() {
        let mut input = InputBox::new();
        input.insert_str("line1\nline2\nline3");
        assert!(input.visual_line_count(80) >= 4); // 3 text rows + 1 metadata
    }

    #[test]
    fn visual_line_count_exact_fit() {
        let mut input = InputBox::new();
        // prompt=0, so 80 chars fit in width 80
        input.insert_str(&"a".repeat(80));
        assert_eq!(input.visual_line_count(80), 2); // 1 text row + 1 metadata
    }

    #[test]
    fn visual_line_count_cjk_exact_fit() {
        let mut input = InputBox::new();
        // prompt=0, so 40 CJK chars (80 cols) fit in width 80
        let cjk: String = std::iter::repeat('你').take(40).collect();
        input.insert_str(&cjk);
        assert_eq!(input.visual_line_count(80), 2); // 1 text row + 1 metadata
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

    // --- Task 4c: /ch channel name completion tests ---

    #[test]
    fn ch_command_triggers_context_completion() {
        let mut input = InputBox::new();
        input.completions = vec![
            "/ch".into(),
            "#main".into(),
            "#ops".into(),
            "@alice".into(),
        ];
        input.insert_str("/ch ");
        input.tab();

        // After "/ch ", candidates should show channel names (# prefixed)
        assert!(
            input.candidates.iter().any(|c| c == "#main"),
            "candidates after '/ch ' should include #main, got: {:?}",
            input.candidates
        );
        assert!(
            input.candidates.iter().any(|c| c == "#ops"),
            "candidates after '/ch ' should include #ops, got: {:?}",
            input.candidates
        );
        // Should NOT include agent names
        assert!(
            !input.candidates.iter().any(|c| c.contains("alice")),
            "candidates after '/ch ' should not include agents"
        );
    }

    #[test]
    fn ch_command_prefix_match_without_hash() {
        // Bug: typing `/ch guar` should match `#guardians` via prefix,
        // just like `/dm ali` matches `alice`. Currently fails because
        // context_prefix is None when word is non-empty, so filter
        // uses default `starts_with("guar")` which won't match `#guardians`.
        let mut input = InputBox::new();
        input.completions = vec![
            "/ch".into(),
            "#guardians".into(),
            "#general".into(),
            "#ops".into(),
            "@alice".into(),
        ];
        input.insert_str("/ch guar");
        input.tab();

        assert!(
            input.candidates.iter().any(|c| c == "#guardians"),
            "typing '/ch guar' should match #guardians by prefix, got: {:?}",
            input.candidates
        );
        assert_eq!(
            input.candidates.len(),
            1,
            "only #guardians should match, got: {:?}",
            input.candidates
        );
    }

    #[test]
    fn ch_command_filters_by_partial_input() {
        let mut input = InputBox::new();
        input.completions = vec![
            "#main".into(),
            "#ops".into(),
            "#ops-dev".into(),
        ];
        input.insert_str("/ch #op");
        input.tab();

        // Should filter to only channels starting with "#op"
        assert!(input.candidates.iter().all(|c| c.starts_with("#op")),
            "candidates should be filtered by partial input, got: {:?}",
            input.candidates
        );
        assert!(input.candidates.len() >= 1);
    }

    // --- Task 4e: Input history tests ---

    #[test]
    fn history_push_stores_message() {
        let mut input = InputBox::new();
        input.history_push("hello");
        assert_eq!(input.history_len(), 1);
    }

    #[test]
    fn history_push_ignores_empty() {
        let mut input = InputBox::new();
        input.history_push("");
        input.history_push("   ");
        assert_eq!(input.history_len(), 0);
    }

    #[test]
    fn history_up_shows_previous() {
        let mut input = InputBox::new();
        input.history_push("first");
        input.history_push("second");

        assert!(input.history_up());
        assert_eq!(input.text, "second");

        assert!(input.history_up());
        assert_eq!(input.text, "first");
    }

    #[test]
    fn history_up_at_oldest_returns_false() {
        let mut input = InputBox::new();
        input.history_push("only");

        assert!(input.history_up());
        assert_eq!(input.text, "only");

        // Already at oldest — should return false and stay
        assert!(!input.history_up());
        assert_eq!(input.text, "only");
    }

    #[test]
    fn history_down_navigates_forward() {
        let mut input = InputBox::new();
        input.history_push("first");
        input.history_push("second");

        input.history_up(); // "second"
        input.history_up(); // "first"

        assert!(input.history_down());
        assert_eq!(input.text, "second");
    }

    #[test]
    fn history_down_past_newest_restores_draft() {
        let mut input = InputBox::new();
        input.history_push("old");
        input.insert_str("draft");

        input.history_up(); // "old", draft saved
        assert_eq!(input.text, "old");

        assert!(input.history_down()); // back to draft
        assert_eq!(input.text, "draft");

        // Past newest — no-op
        assert!(!input.history_down());
        assert_eq!(input.text, "draft");
    }

    #[test]
    fn history_preserves_cursor_at_end() {
        let mut input = InputBox::new();
        input.history_push("hello");

        input.history_up();
        assert_eq!(input.text, "hello");
        assert_eq!(input.cursor_pos, 5); // cursor at end
    }

    #[test]
    fn history_up_with_no_history_returns_false() {
        let mut input = InputBox::new();
        assert!(!input.history_up());
    }

    #[test]
    fn history_down_with_no_history_returns_false() {
        let mut input = InputBox::new();
        assert!(!input.history_down());
    }

    #[test]
    fn history_cap_at_100() {
        let mut input = InputBox::new();
        for i in 0..150 {
            input.history_push(&format!("msg{}", i));
        }
        assert_eq!(input.history_len(), 100);

        // Oldest messages (0..50) should be dropped, newest kept
        input.history_up();
        assert_eq!(input.text, "msg149");
    }

    #[test]
    fn history_not_triggered_in_multiline() {
        // This tests the data structure constraint:
        // move_up() returns true in multiline → app should NOT call history_up()
        let mut input = InputBox::new();
        input.history_push("old");
        input.insert_str("line1\nline2");

        // In multiline, move_up works for line navigation
        assert!(input.is_multiline());
        assert!(input.move_up()); // moves cursor, not history

        // Text should still be the multiline content, not history
        assert_eq!(input.text, "line1\nline2");
    }

    #[test]
    fn history_reset_on_send() {
        let mut input = InputBox::new();
        input.history_push("first");
        input.history_push("second");

        input.history_up(); // browsing history
        assert_eq!(input.text, "second");

        // Simulate send: take() should reset history index
        let _ = input.take();

        // Next history_up should start from newest again
        input.history_up();
        assert_eq!(input.text, "second");
    }

    #[test]
    fn history_does_not_duplicate_consecutive() {
        let mut input = InputBox::new();
        input.history_push("same");
        input.history_push("same");
        assert_eq!(input.history_len(), 1);
    }

    #[test]
    fn render_uses_l2_background() {
        let t = theme::current();
        let input = InputBox::new();
        let area = Rect::new(0, 0, 40, 5);
        let mut buf = Buffer::empty(area);
        input.render(area, &mut buf);
        let cell = buf.cell((1, 1)).unwrap();
        assert_eq!(cell.bg, t.background_element);
    }

    #[test]
    fn render_no_border_chars() {
        let input = InputBox::new();
        let area = Rect::new(0, 0, 40, 5);
        let mut buf = Buffer::empty(area);
        input.render(area, &mut buf);
        let tl = buf.cell((0, 0)).unwrap();
        assert_ne!(tl.symbol(), "╭");
        assert_ne!(tl.symbol(), "┌");
    }
}
