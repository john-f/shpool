// Copyright 2024 Google LLC
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use shpool_protocol::TtySize;
use tracing::info;

use crate::config::{self, SessionRestoreEngine, SessionRestoreMode};

// To prevent data getting dropped, we set this to be large, but we don't want
// to use u16::MAX, since the vt100 crate eagerly fills in its rows, and doing
// so is very memory intensive. The right fix is to get the vt100 crate to
// lazily initialize its rows, but that is likely a bunch of work.
const VTERM_WIDTH: u16 = 1024;

/// Some session shpool specific config getters
trait ConfigExt {
    /// Effective vterm width.
    ///
    /// See also `VTERM_WIDTH`.
    fn vterm_width(&self) -> u16;
}

impl ConfigExt for config::Manager {
    fn vterm_width(&self) -> u16 {
        let config = self.get();
        config.vt100_output_spool_width.unwrap_or(VTERM_WIDTH)
    }
}

pub trait SessionSpool {
    /// Resizes the internal representation to new tty size.
    fn resize(&mut self, size: TtySize);

    /// Gets a byte sequence to restore the on-screen session content.
    ///
    /// The returned sequence is expected to be able to restore the screen
    /// content regardless of any prior screen state. It thus mostly likely
    /// includes some terminal control codes to reset the screen from any
    /// state back to a known good state.
    ///
    /// Note that what exactly is restored is determined by the implementation,
    /// and thus can vary from do nothing to a few lines to a full screen,
    /// etc.
    fn restore_buffer(&self) -> Vec<u8>;

    /// Process bytes from pty master.
    fn process(&mut self, bytes: &[u8]);

    /// Get the current screen contents as plain text.
    fn contents_plain(&self) -> Vec<u8>;

    /// Get the current screen contents with ANSI formatting.
    fn contents_formatted_hardcopy(&self) -> Vec<u8>;
}

/// A spool that doesn't do anything.
pub struct NullSpool;
impl SessionSpool for NullSpool {
    fn resize(&mut self, _: TtySize) {}

    fn restore_buffer(&self) -> Vec<u8> {
        info!("generating null restore buf");
        vec![]
    }

    fn process(&mut self, _: &[u8]) {}

    fn contents_plain(&self) -> Vec<u8> {
        vec![]
    }

    fn contents_formatted_hardcopy(&self) -> Vec<u8> {
        vec![]
    }
}

/// A spool that restores the last screenful of content using shpool_vt100.
pub struct Vt100Screen {
    parser: shpool_vt100::Parser,
    /// Other options will be read dynamically from config.
    config: config::Manager,
}

impl SessionSpool for Vt100Screen {
    fn resize(&mut self, size: TtySize) {
        self.parser.screen_mut().set_size(size.rows, self.config.vterm_width());
    }

    fn restore_buffer(&self) -> Vec<u8> {
        let (rows, cols) = self.parser.screen().size();
        info!("computing screen restore buf with (rows={}, cols={})", rows, cols);
        self.parser.screen().contents_formatted()
    }

    fn process(&mut self, bytes: &[u8]) {
        self.parser.process(bytes)
    }

    fn contents_plain(&self) -> Vec<u8> {
        trim_hardcopy_plain(&self.parser.screen().contents())
    }

    fn contents_formatted_hardcopy(&self) -> Vec<u8> {
        trim_hardcopy_formatted(self.parser.screen())
    }
}

/// A spool that restores the last n lines of content using shpool_vt100.
pub struct Vt100Lines {
    parser: shpool_vt100::Parser,
    /// How many lines to restore
    nlines: u16,
    /// Other options will be read dynamically from config.
    config: config::Manager,
}

impl SessionSpool for Vt100Lines {
    fn resize(&mut self, size: TtySize) {
        self.parser.screen_mut().set_size(size.rows, self.config.vterm_width());
    }

    fn restore_buffer(&self) -> Vec<u8> {
        let (rows, cols) = self.parser.screen().size();
        info!("computing lines({}) restore buf with (rows={}, cols={})", self.nlines, rows, cols);
        self.parser.screen().last_n_rows_contents_formatted(self.nlines)
    }

    fn process(&mut self, bytes: &[u8]) {
        self.parser.process(bytes)
    }

    fn contents_plain(&self) -> Vec<u8> {
        trim_hardcopy_plain(&self.parser.screen().contents())
    }

    fn contents_formatted_hardcopy(&self) -> Vec<u8> {
        trim_hardcopy_formatted(self.parser.screen())
    }
}

/// A spool that restores the last screenful of content using shpool-vterm.
pub struct Vterm {
    term: shpool_vterm::Term,
    mode: SessionRestoreMode,
}

impl SessionSpool for Vterm {
    fn resize(&mut self, size: TtySize) {
        self.term
            .resize(shpool_vterm::Size { height: size.rows as usize, width: size.cols as usize });
    }

    fn restore_buffer(&self) -> Vec<u8> {
        match self.mode {
            SessionRestoreMode::Simple => vec![],
            SessionRestoreMode::Screen => self.term.contents(shpool_vterm::ContentRegion::Screen),
            SessionRestoreMode::Lines(nlines) => {
                self.term.contents(shpool_vterm::ContentRegion::BottomLines(nlines as usize))
            }
        }
    }

    fn process(&mut self, bytes: &[u8]) {
        self.term.process(bytes);
    }

    fn contents_plain(&self) -> Vec<u8> {
        let formatted = self.term.contents(shpool_vterm::ContentRegion::Screen);
        let plain = strip_ansi_escapes::strip(&formatted);
        trim_hardcopy_plain(&String::from_utf8_lossy(&plain))
    }

    fn contents_formatted_hardcopy(&self) -> Vec<u8> {
        // shpool-vterm doesn't have per-row formatted access like vt100,
        // so strip and trim the plain text, then re-wrap. For now, use
        // the formatted output with trailing cleanup.
        let formatted = self.term.contents(shpool_vterm::ContentRegion::Screen);
        let plain = strip_ansi_escapes::strip(&formatted);
        let plain_str = String::from_utf8_lossy(&plain);
        let lines: Vec<&str> = plain_str.lines().collect();
        let last_non_empty = lines.iter().rposition(|l| !l.trim_end().is_empty());
        match last_non_empty {
            Some(_) => {
                let mut out = formatted;
                out.extend_from_slice(b"\x1b[0m\n");
                out
            }
            None => vec![],
        }
    }
}

/// Trim trailing whitespace from each line and remove trailing blank lines.
fn trim_hardcopy_plain(text: &str) -> Vec<u8> {
    let lines: Vec<&str> = text.lines().collect();
    let last_non_empty = lines.iter().rposition(|l| !l.trim_end().is_empty());
    match last_non_empty {
        Some(idx) => {
            let trimmed: Vec<&str> = lines[..=idx].iter().map(|l| l.trim_end()).collect();
            let mut out = trimmed.join("\n");
            out.push('\n');
            out.into_bytes()
        }
        None => vec![],
    }
}

/// Emit SGR escape codes for a cell's attributes, compared to previous state.
/// Returns the new attribute state.
fn write_cell_sgr(
    out: &mut Vec<u8>,
    cell: &shpool_vt100::Cell,
    prev_fg: &mut shpool_vt100::Color,
    prev_bg: &mut shpool_vt100::Color,
    prev_bold: &mut bool,
    prev_italic: &mut bool,
    prev_underline: &mut bool,
    prev_inverse: &mut bool,
) {
    let fg = cell.fgcolor();
    let bg = cell.bgcolor();
    let bold = cell.bold();
    let italic = cell.italic();
    let underline = cell.underline();
    let inverse = cell.inverse();

    if fg == *prev_fg && bg == *prev_bg && bold == *prev_bold
        && italic == *prev_italic && underline == *prev_underline && inverse == *prev_inverse
    {
        return;
    }

    // Check if we need a full reset (when turning off attributes)
    let need_reset = (!bold && *prev_bold) || (!italic && *prev_italic)
        || (!underline && *prev_underline) || (!inverse && *prev_inverse);

    if need_reset {
        out.extend_from_slice(b"\x1b[0m");
        *prev_fg = shpool_vt100::Color::Default;
        *prev_bg = shpool_vt100::Color::Default;
        *prev_bold = false;
        *prev_italic = false;
        *prev_underline = false;
        *prev_inverse = false;
    }

    let mut codes: Vec<String> = Vec::new();
    if bold && !*prev_bold { codes.push("1".into()); }
    if italic && !*prev_italic { codes.push("3".into()); }
    if underline && !*prev_underline { codes.push("4".into()); }
    if inverse && !*prev_inverse { codes.push("7".into()); }

    if fg != *prev_fg {
        match fg {
            shpool_vt100::Color::Default => codes.push("39".into()),
            shpool_vt100::Color::Idx(i) => {
                if i < 8 { codes.push(format!("{}", 30 + i)); }
                else if i < 16 { codes.push(format!("{}", 90 + i - 8)); }
                else { codes.push(format!("38;5;{}", i)); }
            }
            shpool_vt100::Color::Rgb(r, g, b) => codes.push(format!("38;2;{};{};{}", r, g, b)),
        }
    }
    if bg != *prev_bg {
        match bg {
            shpool_vt100::Color::Default => codes.push("49".into()),
            shpool_vt100::Color::Idx(i) => {
                if i < 8 { codes.push(format!("{}", 40 + i)); }
                else if i < 16 { codes.push(format!("{}", 100 + i - 8)); }
                else { codes.push(format!("48;5;{}", i)); }
            }
            shpool_vt100::Color::Rgb(r, g, b) => codes.push(format!("48;2;{};{};{}", r, g, b)),
        }
    }

    if !codes.is_empty() {
        out.extend_from_slice(format!("\x1b[{}m", codes.join(";")).as_bytes());
    }

    *prev_fg = fg;
    *prev_bg = bg;
    *prev_bold = bold;
    *prev_italic = italic;
    *prev_underline = underline;
    *prev_inverse = inverse;
}

/// Build ANSI-formatted hardcopy from vt100 screen, cell by cell.
/// Outputs text with SGR color codes and real spaces (no cursor positioning).
/// Strips trailing blank rows and trailing whitespace per row.
fn trim_hardcopy_formatted(screen: &shpool_vt100::Screen) -> Vec<u8> {
    let (rows, cols) = screen.size();

    let mut row_bufs: Vec<Vec<u8>> = Vec::with_capacity(rows as usize);
    for r in 0..rows {
        let mut row_buf = Vec::new();
        let mut prev_fg = shpool_vt100::Color::Default;
        let mut prev_bg = shpool_vt100::Color::Default;
        let mut prev_bold = false;
        let mut prev_italic = false;
        let mut prev_underline = false;
        let mut prev_inverse = false;

        // Find last column with content to trim trailing spaces
        let mut last_content_col: i32 = -1;
        for c in (0..cols).rev() {
            if let Some(cell) = screen.cell(r, c) {
                if cell.has_contents() {
                    last_content_col = c as i32;
                    break;
                }
            }
        }

        for c in 0..=std::cmp::max(last_content_col, -1) as u16 {
            if let Some(cell) = screen.cell(r, c) {
                if cell.is_wide_continuation() {
                    continue;
                }
                write_cell_sgr(
                    &mut row_buf, cell,
                    &mut prev_fg, &mut prev_bg,
                    &mut prev_bold, &mut prev_italic,
                    &mut prev_underline, &mut prev_inverse,
                );
                if cell.has_contents() {
                    row_buf.extend_from_slice(cell.contents().as_bytes());
                } else {
                    row_buf.push(b' ');
                }
            }
        }
        row_bufs.push(row_buf);
    }

    // Find last non-empty row
    let last_non_empty = row_bufs.iter().rposition(|row| {
        let plain = strip_ansi_escapes::strip(row);
        plain.iter().any(|&b| b != b' ' && b != b'\0')
    });

    let mut out = Vec::new();
    if let Some(last) = last_non_empty {
        for (i, row) in row_bufs[..=last].iter().enumerate() {
            out.extend_from_slice(row);
            if i < last {
                out.push(b'\n');
            }
        }
    }
    out.extend_from_slice(b"\x1b[0m\n");
    out
}

/// Creates a spool given a `mode`.
pub fn new(
    config: config::Manager,
    size: &TtySize,
    scrollback_lines: usize,
) -> Box<dyn SessionSpool + 'static> {
    let restore_engine = config.get().session_restore_engine.clone().unwrap_or_default();
    let mode = config.get().session_restore_mode.clone().unwrap_or_default();
    let vterm_width = config.vterm_width();
    match (mode, restore_engine) {
        (SessionRestoreMode::Simple, _) => Box::new(NullSpool),
        (SessionRestoreMode::Screen, SessionRestoreEngine::Vt100) => Box::new(Vt100Screen {
            parser: shpool_vt100::Parser::new(size.rows, vterm_width, scrollback_lines),
            config,
        }),
        (SessionRestoreMode::Lines(nlines), SessionRestoreEngine::Vt100) => Box::new(Vt100Lines {
            parser: shpool_vt100::Parser::new(size.rows, vterm_width, scrollback_lines),
            nlines,
            config,
        }),
        (mode, SessionRestoreEngine::Vterm) => Box::new(Vterm {
            term: shpool_vterm::Term::new(
                scrollback_lines,
                shpool_vterm::Size { width: size.cols as usize, height: size.rows as usize },
            ),
            mode,
        }),
    }
}
