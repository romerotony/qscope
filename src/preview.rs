//! A bounded, presentation-only terminal decoder for captured progress bars.
//! It never sends terminal commands, wraps to a screen width, or reads outside
//! the current file page. Unknown escape sequences are discarded.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

const LIMIT: usize = 64 * 1024;

#[derive(Default)]
enum Cell {
  #[default]
  Blank,
  Text(String),
  Continuation,
}

#[derive(Default)]
struct Screen {
  lines: Vec<Vec<Cell>>,
  row: usize,
  column: usize,
  cells: usize,
  work: usize,
}

pub fn render(bytes: &[u8]) -> String {
  let text = String::from_utf8_lossy(&bytes[..bytes.len().min(LIMIT)]);
  if !text
    .chars()
    .any(|c| matches!(c, '\r' | '\x08' | '\x1b' | '\u{0090}'..='\u{009f}'))
  {
    // Ordinary logs keep their original whitespace without allocating a grid.
    return text
      .chars()
      .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
      .collect();
  }
  let mut screen = Screen::default();
  let mut rest = text.as_ref();
  while let Some(c) = rest.chars().next() {
    let current = rest;
    rest = &rest[c.len_utf8()..];
    match c {
      '\r' => screen.column = 0,
      '\n' => {
        screen.row = (screen.row + 1).min(LIMIT - 1);
        screen.column = 0;
        screen.ensure_row();
      }
      '\t' => screen.column = ((screen.column / 8 + 1) * 8).min(LIMIT - 1),
      '\x08' => screen.column = screen.column.saturating_sub(1),
      '\x1b' => {
        if let Some(kind) = rest.chars().next() {
          rest = &rest[kind.len_utf8()..];
          match kind {
            '[' => rest = screen.csi(rest),
            ']' => rest = string_end(rest, true),
            'P' | 'X' | '^' | '_' => rest = string_end(rest, false),
            // Discard character-set/designation sequences as a unit too.
            ' '..='/' => {
              rest = rest.trim_start_matches(|c| (' '..='/').contains(&c));
              if rest
                .as_bytes()
                .first()
                .is_some_and(|b| (0x30..=0x7e).contains(b))
              {
                rest = &rest[1..];
              }
            }
            _ => {}
          }
        }
      }
      '\u{009b}' => rest = screen.csi(rest),
      '\u{009d}' => rest = string_end(rest, true),
      '\u{0090}' | '\u{0098}' | '\u{009e}' | '\u{009f}' => rest = string_end(rest, false),
      c if c.is_control() => {}
      _ => {
        // Process whole graphemes so wide characters and combining marks occupy
        // the right number of terminal columns when a progress line is updated.
        rest = current;
        let end = rest.find(char::is_control).unwrap_or(rest.len());
        for grapheme in rest[..end].graphemes(true) {
          screen.put(grapheme);
        }
        rest = &rest[end..];
      }
    }
  }
  screen.finish()
}

/// Consume OSC/DCS/etc. payloads, including hyperlinks and clipboard commands.
/// An incomplete sequence is hidden until the next poll supplies its terminator.
fn string_end(text: &str, bell: bool) -> &str {
  for (i, c) in text.char_indices() {
    if c == '\u{009c}' || (bell && c == '\x07') {
      return &text[i + c.len_utf8()..];
    }
    if c == '\x1b' && text[i..].starts_with("\x1b\\") {
      return &text[i + 2..];
    }
  }
  ""
}

impl Screen {
  fn ensure_row(&mut self) {
    self
      .lines
      .resize_with(self.lines.len().max(self.row + 1), Vec::new);
  }

  fn put(&mut self, text: &str) {
    self.ensure_row();
    let width = text.width();
    let end = self.column.saturating_add(width).min(LIMIT);
    let line = &mut self.lines[self.row];
    if width == 0 {
      let mut at = self
        .column
        .saturating_sub(1)
        .min(line.len().saturating_sub(1));
      while at > 0 && matches!(line[at], Cell::Continuation) {
        at -= 1;
      }
      if let Some(Cell::Text(previous)) = line.get_mut(at) {
        previous.push_str(text);
      }
      return;
    }
    let growth = end.saturating_sub(line.len());
    let work = growth + width;
    if self.cells + growth > LIMIT || end - self.column != width || self.work + work > LIMIT * 8 {
      return;
    }
    self.work += work;
    self.cells += growth;
    line.resize_with(line.len().max(end), Cell::default);
    for at in self.column..end {
      clear_cell(line, at);
    }
    line[self.column] = Cell::Text(text.into());
    for cell in &mut line[self.column + 1..end] {
      *cell = Cell::Continuation;
    }
    self.column = end;
  }

  fn csi<'a>(&mut self, text: &'a str) -> &'a str {
    for (i, byte) in text.bytes().enumerate() {
      if (0x40..=0x7e).contains(&byte) {
        let params = &text[..i];
        if params.len() <= 64 && params.bytes().all(|b| b.is_ascii_digit() || b == b';') {
          self.command(byte, params);
        }
        return &text[i + 1..];
      }
      if !(0x20..=0x3f).contains(&byte) {
        // Malformed sequences must not eat a following newline or UTF-8 glyph.
        return &text[i..];
      }
    }
    ""
  }

  fn command(&mut self, command: u8, params: &str) {
    let mut values = params.split(';');
    let parse = |s: &str| {
      if s.is_empty() {
        Some(0)
      } else {
        s.parse::<usize>().ok()
      }
    };
    let Some(first) = values.next().and_then(parse) else {
      return;
    };
    let amount = first.clamp(1, LIMIT);
    match command {
      b'A' => self.row = self.row.saturating_sub(amount),
      b'B' => self.row = self.row.saturating_add(amount).min(LIMIT - 1),
      b'C' => self.column = self.column.saturating_add(amount).min(LIMIT - 1),
      b'D' => self.column = self.column.saturating_sub(amount),
      b'E' | b'F' => {
        self.row = if command == b'E' {
          self.row.saturating_add(amount).min(LIMIT - 1)
        } else {
          self.row.saturating_sub(amount)
        };
        self.column = 0;
      }
      b'G' => self.column = amount - 1,
      b'H' | b'f' => {
        let Some(column) = values.next().map(parse).unwrap_or(Some(1)) else {
          return;
        };
        self.row = amount - 1;
        self.column = column.clamp(1, LIMIT) - 1;
      }
      b'K' => self.erase_line(first),
      // Colors, modes, queries, and all other commands have no side effects.
      _ => {}
    }
  }

  fn erase_line(&mut self, mode: usize) {
    let Some(line) = self.lines.get_mut(self.row) else {
      return;
    };
    // Repeated huge cursor/erase sequences must not turn a small page into
    // unbounded allocation/clearing work.
    if self.work + line.len() > LIMIT * 8 {
      return;
    }
    self.work += line.len();
    match mode {
      0 => {
        if self.column < line.len() {
          clear_cell(line, self.column);
          self.cells -= line.len() - self.column;
          line.truncate(self.column);
        }
      }
      1 => {
        for at in 0..line.len().min(self.column + 1) {
          clear_cell(line, at);
        }
      }
      2 => {
        self.cells -= line.len();
        line.clear();
      }
      _ => {}
    }
  }

  fn finish(self) -> String {
    let mut text = String::new();
    for (i, line) in self.lines.into_iter().enumerate() {
      if i > 0 {
        text.push('\n');
      }
      for cell in line {
        match cell {
          Cell::Text(value) => text.push_str(&value),
          Cell::Blank => text.push(' '),
          Cell::Continuation => {}
        }
      }
    }
    text
  }
}

fn clear_cell(line: &mut [Cell], at: usize) {
  let mut start = at;
  while start > 0 && matches!(line[start], Cell::Continuation) {
    start -= 1;
  }
  line[start] = Cell::Blank;
  let mut next = start + 1;
  while next < line.len() && matches!(line[next], Cell::Continuation) {
    line[next] = Cell::Blank;
    next += 1;
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn ordinary_logs_and_crlf_keep_their_lines() {
    for text in [
      "hello\nworld\n",
      "tab\tvalue\n",
      "日本語 👩‍💻 e\u{301}\n",
      "\n\n",
    ] {
      assert_eq!(render(text.as_bytes()), text);
    }
    assert_eq!(render(b"hello\r\nworld\r\n"), "hello\nworld\n");
    assert_eq!(render(b"bad:\xff\x00\n"), "bad:\u{fffd}\n");
  }

  #[test]
  fn carriage_returns_replace_progress_without_discarding_log_lines() {
    assert_eq!(
      render(b"Starting\n\rDownloading 10%\rDownloading 20%\rDownloading 30%\nDone\n"),
      "Starting\nDownloading 30%\nDone\n"
    );
    assert_eq!(render(b"abcde\rXY"), "XYcde"); // CR moves; it does not erase.
    assert_eq!(render(b"abc\x08X"), "abX");
    assert_eq!(render(b"\rA\tB"), "A       B");
  }

  #[test]
  fn erase_line_and_cursor_movement_support_nested_bars() {
    assert_eq!(render(b"abcdef\rxy\x1b[K"), "xy");
    assert_eq!(render(b"abcdef\r\x1b[2Kxy"), "xy");
    assert_eq!(render(b"abcdef\x1b[3G\x1b[1K"), "   def");
    assert_eq!(
      render(b"\router 10%\n\rinner 20%\x1b[A\router 30%\n\rinner 40%\x1b[A"),
      "outer 30%\ninner 40%"
    );
    assert_eq!(render(b"abc\x1b[2DXY\x1b[2CZ"), "aXY  Z");
    assert_eq!(render(b"a\nb\x1b[1;1HX"), "X\nb");
    assert_eq!(render(b"a\nb\x1b[FZ\x1b[EY"), "Z\nY");
  }

  #[test]
  fn overwrites_use_unicode_display_columns() {
    assert_eq!(render("\r界X\rA".as_bytes()), "A X");
    assert_eq!(render("\re\u{301}X\rZ".as_bytes()), "ZX");
    assert_eq!(render("\r👩‍💻X\rOK".as_bytes()), "OKX");
  }

  #[test]
  fn escapes_are_decoded_or_discarded_never_forwarded_to_the_terminal() {
    let input = b"\x1b[31mred\x1b[0m\n\x1b]0;secret title\x07safe \
\x1b]52;c;clipboard payload\x1b\\end\x1bPprivate payload\x1b\\\
\x1b[?25l\x1b[6n\x1b(B!";
    assert_eq!(render(input), "red\nsafe end!");
    assert_eq!(render(b"visible\x1b[12"), "visible");
    assert_eq!(render(b"visible\x1b]unclosed payload"), "visible");
    assert_eq!(render(b"visible\x1bPunterminated"), "visible");
    assert_eq!(render("a\u{009d}title\u{009c}b".as_bytes()), "ab");
    assert_eq!(render(b"ok\x1b[bad\nnext"), "okad\nnext");
  }

  #[test]
  fn cursor_parameters_and_output_memory_are_bounded() {
    let input = b"\x1b[999999999999999999999999999999Hsmall";
    assert_eq!(render(input), "small");
    let output = render(b"\x1b[65535;65535HX\x1b[1;65535HY\x1b[65535;1HZ");
    assert!(output.len() <= LIMIT * 3);
    assert!(!output.chars().any(|c| c.is_control() && c != '\n'));
    let abusive = "\x1b[65535GX\r\x1b[2K".repeat(3000);
    let output = render(abusive.as_bytes());
    assert!(output.len() <= LIMIT);
    // Arbitrary/truncated byte streams cannot panic or leak control characters.
    for value in 0..=255 {
      let mut bytes = vec![value; 256];
      bytes.extend_from_slice(b"\x1b[2K\rend");
      let result = render(&bytes);
      assert!(
        !result
          .chars()
          .any(|c| c.is_control() && !matches!(c, '\n' | '\t'))
      );
    }
  }
}
