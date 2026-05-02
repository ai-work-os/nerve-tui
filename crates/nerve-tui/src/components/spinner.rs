use ratatui::style::Color;

pub const BRAILLE_FRAMES: [&str; 10] = [
    "⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏",
];
pub const BRAILLE_INTERVAL_MS: u64 = 80;

pub struct BrailleSpinner {
    index: usize,
}

impl BrailleSpinner {
    pub fn new() -> Self {
        Self { index: 0 }
    }

    pub fn frame(&self) -> &'static str {
        BRAILLE_FRAMES[self.index]
    }

    pub fn advance(&mut self) {
        self.index = (self.index + 1) % BRAILLE_FRAMES.len();
    }
}

pub const SCANNER_INTERVAL_MS: u64 = 40;
const SCANNER_TRAIL_LEN: usize = 4;

pub struct KnightRiderScanner {
    pub width: usize,
    pos: usize,
    going_right: bool,
}

impl KnightRiderScanner {
    pub fn new(width: usize) -> Self {
        Self { width, pos: 0, going_right: true }
    }

    pub fn head_pos(&self) -> usize {
        self.pos
    }

    pub fn advance(&mut self) {
        if self.width <= 1 {
            return;
        }
        if self.going_right {
            if self.pos >= self.width - 1 {
                self.going_right = false;
                self.pos = self.pos.saturating_sub(1);
            } else {
                self.pos += 1;
            }
        } else {
            if self.pos == 0 {
                self.going_right = true;
                self.pos = 1;
            } else {
                self.pos -= 1;
            }
        }
    }

    /// Returns alpha values (0.0-1.0) for each cell position.
    pub fn render(&self) -> Vec<f32> {
        let mut cells = vec![0.0f32; self.width];
        for i in 0..=SCANNER_TRAIL_LEN {
            let trail_pos = if self.going_right {
                self.pos as isize - i as isize
            } else {
                self.pos as isize + i as isize
            };
            if trail_pos >= 0 && (trail_pos as usize) < self.width {
                let alpha = 1.0 - (i as f32 / (SCANNER_TRAIL_LEN + 1) as f32);
                cells[trail_pos as usize] = alpha.max(0.3);
            }
        }
        cells
    }

    /// Render scanner to colored block characters for a given base color.
    pub fn render_spans(&self, base_color: Color) -> Vec<(char, Color)> {
        let alphas = self.render();
        let (br, bg, bb) = match base_color {
            Color::Rgb(r, g, b) => (r, g, b),
            _ => (0xc4, 0x65, 0x2a),
        };
        alphas
            .iter()
            .map(|&a| {
                if a < 0.1 {
                    (' ', Color::Reset)
                } else {
                    let r = (br as f32 * a) as u8;
                    let g = (bg as f32 * a) as u8;
                    let b = (bb as f32 * a) as u8;
                    ('█', Color::Rgb(r, g, b))
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn braille_frame_cycles() {
        let mut s = BrailleSpinner::new();
        let f0 = s.frame();
        for _ in 0..BRAILLE_FRAMES.len() {
            s.advance();
        }
        assert_eq!(s.frame(), f0);
    }

    #[test]
    fn braille_frame_returns_valid_char() {
        let s = BrailleSpinner::new();
        assert!(BRAILLE_FRAMES.contains(&s.frame()));
    }

    #[test]
    fn scanner_positions_bounce() {
        let mut sc = KnightRiderScanner::new(20);
        let mut positions = Vec::new();
        for _ in 0..40 {
            positions.push(sc.head_pos());
            sc.advance();
        }
        assert!(positions.contains(&0));
        assert!(positions.contains(&19));
    }

    #[test]
    fn scanner_renders_correct_width() {
        let sc = KnightRiderScanner::new(10);
        let cells = sc.render();
        assert_eq!(cells.len(), 10);
    }

    #[test]
    fn scanner_width_1_does_not_panic() {
        let mut sc = KnightRiderScanner::new(1);
        sc.advance();
        assert_eq!(sc.head_pos(), 0);
    }

    #[test]
    fn render_spans_length_matches_width() {
        let sc = KnightRiderScanner::new(15);
        let spans = sc.render_spans(Color::Rgb(0xc4, 0x65, 0x2a));
        assert_eq!(spans.len(), 15);
    }
}
