//! DiffView widget — renders unified diffs with colored line numbers and add/remove highlighting.

use crate::theme::Theme;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum DiffLine {
    Context { old_num: usize, new_num: usize, text: String },
    Added { new_num: usize, text: String },
    Removed { old_num: usize, text: String },
}

#[derive(Debug, Clone)]
pub struct DiffHunk {
    pub lines: Vec<DiffLine>,
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Parse a unified diff string into hunks.
/// Skips `---` / `+++` header lines.
/// `@@` lines start a new hunk and set the running line numbers.
pub fn parse_unified_diff(input: &str) -> Vec<DiffHunk> {
    let mut hunks: Vec<DiffHunk> = Vec::new();
    let mut current_hunk: Option<DiffHunk> = None;
    let mut old_num: usize = 0;
    let mut new_num: usize = 0;

    for line in input.lines() {
        if line.starts_with("--- ") || line.starts_with("+++ ") {
            continue;
        }

        if line.starts_with("@@ ") {
            // Save any in-progress hunk
            if let Some(hunk) = current_hunk.take() {
                hunks.push(hunk);
            }
            // Parse `@@ -old_start[,count] +new_start[,count] @@`
            let (os, ns) = parse_hunk_header(line);
            old_num = os;
            new_num = ns;
            current_hunk = Some(DiffHunk { lines: Vec::new() });
            continue;
        }

        let hunk = match current_hunk.as_mut() {
            Some(h) => h,
            None => continue, // lines before any @@ — skip
        };

        if let Some(rest) = line.strip_prefix('+') {
            hunk.lines.push(DiffLine::Added {
                new_num,
                text: rest.to_string(),
            });
            new_num += 1;
        } else if let Some(rest) = line.strip_prefix('-') {
            hunk.lines.push(DiffLine::Removed {
                old_num,
                text: rest.to_string(),
            });
            old_num += 1;
        } else {
            // Context line: leading space or empty (bare newline in diff)
            let text = if line.starts_with(' ') {
                line[1..].to_string()
            } else {
                line.to_string()
            };
            hunk.lines.push(DiffLine::Context { old_num, new_num, text });
            old_num += 1;
            new_num += 1;
        }
    }

    if let Some(hunk) = current_hunk {
        hunks.push(hunk);
    }

    hunks
}

/// Parse the hunk header `@@ -old_start[,count] +new_start[,count] @@` and return
/// (old_start, new_start).  Falls back to (1, 1) on parse failure.
fn parse_hunk_header(line: &str) -> (usize, usize) {
    // Expected format: "@@ -A[,B] +C[,D] @@"
    let inner = line.trim_start_matches('@').trim();
    let mut parts = inner.split_whitespace();
    let old_part = parts.next().unwrap_or("-1");
    let new_part = parts.next().unwrap_or("+1");

    let old_start = parse_range_start(old_part.trim_start_matches('-'));
    let new_start = parse_range_start(new_part.trim_start_matches('+'));
    (old_start, new_start)
}

fn parse_range_start(s: &str) -> usize {
    // s is either "N" or "N,M"
    let base = s.split(',').next().unwrap_or("1");
    base.parse::<usize>().unwrap_or(1)
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Render diff hunks to ratatui `Line`s using the unified layout:
///
/// ```text
///    1 │ + added_text
///    2 │ - removed_text
///    3 │   context_text
/// ```
pub fn render_unified<'a>(hunks: &[DiffHunk], theme: &Theme) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();

    let num_style = Style::default().fg(theme.diff_line_number);
    let text_color = theme.text;

    for hunk in hunks {
        for diff_line in &hunk.lines {
            let (line_num_str, prefix, bg, text_str) = match diff_line {
                DiffLine::Added { new_num, text } => (
                    format!("{:>4}", new_num),
                    "+",
                    theme.diff_added_bg,
                    text.clone(),
                ),
                DiffLine::Removed { old_num, text } => (
                    format!("{:>4}", old_num),
                    "-",
                    theme.diff_removed_bg,
                    text.clone(),
                ),
                DiffLine::Context { new_num, text, .. } => (
                    format!("{:>4}", new_num),
                    " ",
                    theme.diff_context_bg,
                    text.clone(),
                ),
            };

            let bg_style = Style::default().bg(bg).fg(text_color);
            let line = Line::from(vec![
                Span::styled(line_num_str, num_style.bg(bg)),
                Span::styled(" │ ", Style::default().fg(Color::DarkGray).bg(bg)),
                Span::styled(prefix.to_string(), bg_style),
                Span::styled(" ".to_string(), bg_style),
                Span::styled(text_str, bg_style),
            ]);
            out.push(line);
        }
    }

    out
}

/// Truncate or pad a string to exactly `width` display columns.
/// Uses unicode_width for CJK support.
/// Truncates with "…" if too long, pads with spaces if too short.
fn truncate_or_pad(s: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let display_w = UnicodeWidthStr::width(s);
    if display_w == width {
        return s.to_string();
    }
    if display_w > width {
        // Truncate: find the cut point where adding "…" keeps us at `width`
        let target = width.saturating_sub(1); // 1 for "…"
        let mut acc = 0usize;
        let mut cut = 0usize;
        for ch in s.chars() {
            let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            if acc + cw > target {
                break;
            }
            acc += cw;
            cut += ch.len_utf8();
        }
        format!("{}…{}", &s[..cut], " ".repeat(width.saturating_sub(acc + 1)))
    } else {
        // Pad with spaces
        let pad = width - display_w;
        format!("{}{}", s, " ".repeat(pad))
    }
}

/// Render diff hunks in split (side-by-side) layout.
///
/// Each line shows:
/// ```text
///   1 │ old content        │   1 │ new content
/// ```
/// Left = old (removed/context), right = new (added/context).
pub fn render_split<'a>(hunks: &[DiffHunk], width: u16, theme: &Theme) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();

    let num_style = Style::default().fg(theme.diff_line_number);
    let sep_style = Style::default().fg(Color::DarkGray);
    let text_color = theme.text;

    // Layout: each side = 4 (num) + 3 (" │ ") + content_w = half_w
    // Middle sep = " │ " (3 chars)
    // total = 2 * (4 + 3 + content_w) + 3 = width
    // content_w = (width - 3 - 2 * 7) / 2 = (width - 17) / 2
    let total = width as usize;
    let content_w = if total > 17 { (total - 17) / 2 } else { 1 };

    for hunk in hunks {
        for diff_line in &hunk.lines {
            match diff_line {
                DiffLine::Added { new_num, text } => {
                    let left_bg = theme.diff_added_bg;
                    let right_bg = theme.diff_added_bg;
                    let left_num = "    ".to_string();
                    let left_content = truncate_or_pad("", content_w);
                    let right_num = format!("{:>4}", new_num);
                    let right_content = truncate_or_pad(text, content_w);

                    out.push(Line::from(vec![
                        Span::styled(left_num, num_style.bg(left_bg)),
                        Span::styled(" │ ", sep_style.bg(left_bg)),
                        Span::styled(left_content, Style::default().bg(left_bg).fg(text_color)),
                        Span::styled(" │ ", sep_style),
                        Span::styled(right_num, num_style.bg(right_bg)),
                        Span::styled(" │ ", sep_style.bg(right_bg)),
                        Span::styled(right_content, Style::default().bg(right_bg).fg(text_color)),
                    ]));
                }
                DiffLine::Removed { old_num, text } => {
                    let left_bg = theme.diff_removed_bg;
                    let right_bg = theme.diff_removed_bg;
                    let left_num = format!("{:>4}", old_num);
                    let left_content = truncate_or_pad(text, content_w);
                    let right_num = "    ".to_string();
                    let right_content = truncate_or_pad("", content_w);

                    out.push(Line::from(vec![
                        Span::styled(left_num, num_style.bg(left_bg)),
                        Span::styled(" │ ", sep_style.bg(left_bg)),
                        Span::styled(left_content, Style::default().bg(left_bg).fg(text_color)),
                        Span::styled(" │ ", sep_style),
                        Span::styled(right_num, num_style.bg(right_bg)),
                        Span::styled(" │ ", sep_style.bg(right_bg)),
                        Span::styled(right_content, Style::default().bg(right_bg).fg(text_color)),
                    ]));
                }
                DiffLine::Context { old_num, new_num, text } => {
                    let bg = theme.diff_context_bg;
                    let left_num = format!("{:>4}", old_num);
                    let right_num = format!("{:>4}", new_num);
                    let left_content = truncate_or_pad(text, content_w);
                    let right_content = truncate_or_pad(text, content_w);

                    out.push(Line::from(vec![
                        Span::styled(left_num, num_style.bg(bg)),
                        Span::styled(" │ ", sep_style.bg(bg)),
                        Span::styled(left_content, Style::default().bg(bg).fg(text_color)),
                        Span::styled(" │ ", sep_style),
                        Span::styled(right_num, num_style.bg(bg)),
                        Span::styled(" │ ", sep_style.bg(bg)),
                        Span::styled(right_content, Style::default().bg(bg).fg(text_color)),
                    ]));
                }
            }
        }
    }

    out
}

/// Public entry point. Parses the diff text and renders it.
/// Uses split mode when width > 120, unified mode otherwise.
pub fn render_diff(diff_text: &str, width: u16, theme: &Theme) -> Vec<Line<'static>> {
    let hunks = parse_unified_diff(diff_text);
    if width > 120 {
        render_split(&hunks, width, theme)
    } else {
        render_unified(&hunks, theme)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Theme;

    #[test]
    fn parse_unified_diff_basic() {
        let diff = "--- a/file.rs\n+++ b/file.rs\n@@ -1,3 +1,3 @@\n fn main() {\n-    println!(\"old\");\n+    println!(\"new\");\n }";
        let hunks = parse_unified_diff(diff);
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].lines.len(), 4);
        assert!(matches!(hunks[0].lines[0], DiffLine::Context { .. }));
        assert!(matches!(hunks[0].lines[1], DiffLine::Removed { .. }));
        assert!(matches!(hunks[0].lines[2], DiffLine::Added { .. }));
        assert!(matches!(hunks[0].lines[3], DiffLine::Context { .. }));
    }

    #[test]
    fn render_unified_has_line_numbers() {
        let diff = "--- a/f.rs\n+++ b/f.rs\n@@ -1,2 +1,2 @@\n-old\n+new";
        let hunks = parse_unified_diff(diff);
        let theme = Theme::warm_light();
        let lines = render_unified(&hunks, &theme);
        let text: String = lines[0].spans.iter().map(|s| s.content.to_string()).collect();
        assert!(text.contains("1"), "should have line number, got: {}", text);
    }

    #[test]
    fn empty_diff_returns_empty() {
        let hunks = parse_unified_diff("");
        assert!(hunks.is_empty());
    }

    #[test]
    fn multiple_hunks() {
        let diff = "--- a/f.rs\n+++ b/f.rs\n@@ -1,2 +1,2 @@\n-old1\n+new1\n@@ -10,2 +10,2 @@\n-old2\n+new2";
        let hunks = parse_unified_diff(diff);
        assert_eq!(hunks.len(), 2);
    }

    #[test]
    fn render_diff_returns_lines() {
        let diff = "--- a/f.rs\n+++ b/f.rs\n@@ -1,1 +1,1 @@\n-old\n+new";
        let theme = Theme::warm_light();
        let lines = render_diff(diff, 80, &theme);
        assert!(!lines.is_empty());
    }

    #[test]
    fn added_line_has_correct_new_num() {
        let diff = "--- a/f.rs\n+++ b/f.rs\n@@ -5,1 +5,1 @@\n-old\n+new";
        let hunks = parse_unified_diff(diff);
        match &hunks[0].lines[1] {
            DiffLine::Added { new_num, .. } => assert_eq!(*new_num, 5),
            other => panic!("expected Added, got {:?}", other),
        }
    }

    #[test]
    fn removed_line_has_correct_old_num() {
        let diff = "--- a/f.rs\n+++ b/f.rs\n@@ -3,1 +3,1 @@\n-old\n+new";
        let hunks = parse_unified_diff(diff);
        match &hunks[0].lines[0] {
            DiffLine::Removed { old_num, .. } => assert_eq!(*old_num, 3),
            other => panic!("expected Removed, got {:?}", other),
        }
    }

    #[test]
    fn context_line_increments_both_nums() {
        let diff = "--- a/f.rs\n+++ b/f.rs\n@@ -1,3 +1,3 @@\n ctx1\n ctx2\n ctx3";
        let hunks = parse_unified_diff(diff);
        assert_eq!(hunks[0].lines.len(), 3);
        match &hunks[0].lines[2] {
            DiffLine::Context { old_num, new_num, .. } => {
                assert_eq!(*old_num, 3);
                assert_eq!(*new_num, 3);
            }
            other => panic!("expected Context, got {:?}", other),
        }
    }

    #[test]
    fn render_unified_added_uses_added_bg() {
        let diff = "--- a/f.rs\n+++ b/f.rs\n@@ -1,1 +1,1 @@\n+added_line";
        let hunks = parse_unified_diff(diff);
        let theme = Theme::warm_light();
        let lines = render_unified(&hunks, &theme);
        // Background color on spans should be diff_added_bg
        let has_added_bg = lines[0].spans.iter().any(|s| s.style.bg == Some(theme.diff_added_bg));
        assert!(has_added_bg, "added line should use diff_added_bg");
    }

    #[test]
    fn render_unified_removed_uses_removed_bg() {
        let diff = "--- a/f.rs\n+++ b/f.rs\n@@ -1,1 +1,1 @@\n-removed_line";
        let hunks = parse_unified_diff(diff);
        let theme = Theme::warm_light();
        let lines = render_unified(&hunks, &theme);
        let has_removed_bg = lines[0].spans.iter().any(|s| s.style.bg == Some(theme.diff_removed_bg));
        assert!(has_removed_bg, "removed line should use diff_removed_bg");
    }

    #[test]
    fn render_diff_wide_uses_split() {
        // At width > 120, render_diff uses split layout (different from unified)
        let diff = "--- a/f.rs\n+++ b/f.rs\n@@ -1,1 +1,1 @@\n-old\n+new";
        let theme = Theme::warm_light();
        let lines_wide = render_diff(diff, 200, &theme);
        let lines_narrow = render_diff(diff, 80, &theme);
        // Wide (split): 2 lines (one for removed, one for added shown separately side-by-side)
        // Narrow (unified): same number of lines but different span structure
        assert!(!lines_wide.is_empty());
        assert!(!lines_narrow.is_empty());
        // Split layout has more spans per line (7 spans) than unified (5 spans)
        assert!(lines_wide[0].spans.len() > lines_narrow[0].spans.len(),
            "split should have more spans per line than unified");
    }

    #[test]
    fn render_split_has_two_columns() {
        let diff = "--- a/f.rs\n+++ b/f.rs\n@@ -1,2 +1,2 @@\n-old line\n+new line";
        let hunks = parse_unified_diff(diff);
        let theme = Theme::warm_light();
        let lines = render_split(&hunks, 140, &theme);
        assert!(!lines.is_empty());
    }

    #[test]
    fn render_diff_uses_split_for_wide() {
        let diff = "--- a/f.rs\n+++ b/f.rs\n@@ -1,1 +1,1 @@\n-old\n+new";
        let theme = Theme::warm_light();
        let lines_wide = render_diff(diff, 140, &theme);
        let lines_narrow = render_diff(diff, 80, &theme);
        // Wide should use split (different format)
        assert!(!lines_wide.is_empty());
        assert!(!lines_narrow.is_empty());
    }

    #[test]
    fn truncate_or_pad_short_pads() {
        let result = truncate_or_pad("hi", 6);
        assert_eq!(result, "hi    ");
        assert_eq!(UnicodeWidthStr::width(result.as_str()), 6);
    }

    #[test]
    fn truncate_or_pad_exact_unchanged() {
        let result = truncate_or_pad("hello", 5);
        assert_eq!(result, "hello");
    }

    #[test]
    fn truncate_or_pad_long_truncates() {
        let result = truncate_or_pad("hello world", 7);
        assert_eq!(UnicodeWidthStr::width(result.as_str()), 7);
        assert!(result.contains('…'));
    }

    #[test]
    fn truncate_or_pad_cjk() {
        // Each CJK char is 2 columns wide
        // "你好" = 4 columns, pad to 6
        let result = truncate_or_pad("你好", 6);
        assert_eq!(UnicodeWidthStr::width(result.as_str()), 6);
    }

    #[test]
    fn render_split_context_appears_both_sides() {
        let diff = "--- a/f.rs\n+++ b/f.rs\n@@ -1,1 +1,1 @@\n ctx_line";
        let hunks = parse_unified_diff(diff);
        let theme = Theme::warm_light();
        let lines = render_split(&hunks, 140, &theme);
        assert_eq!(lines.len(), 1);
        let text: String = lines[0].spans.iter().map(|s| s.content.to_string()).collect();
        // ctx_line should appear twice (both sides)
        assert_eq!(text.matches("ctx_line").count(), 2,
            "context line should appear on both sides, got: {}", text);
    }

    #[test]
    fn render_split_added_left_blank() {
        let diff = "--- a/f.rs\n+++ b/f.rs\n@@ -1,1 +1,1 @@\n+added_only";
        let hunks = parse_unified_diff(diff);
        let theme = Theme::warm_light();
        let lines = render_split(&hunks, 140, &theme);
        assert_eq!(lines.len(), 1);
        // Left side (spans[0..=2]) should have blank line number "    "
        let left_num = lines[0].spans[0].content.as_ref();
        assert_eq!(left_num.trim(), "", "added line left side should have blank line number");
    }

    #[test]
    fn render_split_removed_right_blank() {
        let diff = "--- a/f.rs\n+++ b/f.rs\n@@ -1,1 +1,1 @@\n-removed_only";
        let hunks = parse_unified_diff(diff);
        let theme = Theme::warm_light();
        let lines = render_split(&hunks, 140, &theme);
        assert_eq!(lines.len(), 1);
        // Right side (spans[4]) should have blank line number "    "
        let right_num = lines[0].spans[4].content.as_ref();
        assert_eq!(right_num.trim(), "", "removed line right side should have blank line number");
    }
}
