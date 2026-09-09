//! Program output turned into lines a plain reader can be shown, in one place.
//!
//! Cursor motion is spent here rather than passed on: it would reach the popup's own screen as a
//! command rather than as content. Colour is kept, as the style of the characters it applied to, so a
//! reader that can paint gets what the program meant and one that cannot asks for the text.
//!
//! A carriage return is the exception that has to be acted on rather than dropped: a progress bar is
//! one line rewritten in place, and a filter that dropped the return would hand every frame of it back
//! concatenated into one endless line. Returning the cursor to the start of the line and letting what
//! follows overwrite leaves the bar's final state, which is the frame a reader wanted.
//!
//! The source hands over whatever the last read happened to hold, so nothing may assume a chunk ends
//! anywhere in particular: an escape sequence, a UTF-8 character and a line can each be cut in half by
//! a chunk boundary and carried into the next one.

/// How long a line may get before it is handed over unfinished. A full-screen program draws with
/// absolute cursor motion and may never write a newline at all, and a filter that waited for one would
/// hold the whole run in memory.
const LONGEST_LINE: usize = 8192;

/// A colour as the sequence named it. Which paint an `Ansi` or `Indexed` ends up being is the
/// reader's business: it names a palette entry, and the palette belongs to the terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Colour {
    /// 0..=7 as written, 8..=15 for the bright half.
    Ansi(u8),
    Indexed(u8),
    Rgb(u8, u8, u8),
}

/// How a run of characters was painted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Sgr {
    pub fg: Option<Colour>,
    pub bg: Option<Colour>,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub reverse: bool,
}

impl Sgr {
    /// One parameter list of a `CSI ... m`, applied over what was already in force. Anything not
    /// understood is skipped rather than reset: a sequence a reader cannot paint is not a reason to
    /// drop the paint it can.
    fn apply(&mut self, params: &str) {
        let mut codes = params
            .split(';')
            .map(|code| code.trim().parse::<u16>().unwrap_or(0));
        while let Some(code) = codes.next() {
            match code {
                0 => *self = Sgr::default(),
                1 => self.bold = true,
                2 => self.dim = true,
                3 => self.italic = true,
                4 => self.underline = true,
                7 => self.reverse = true,
                22 => {
                    self.bold = false;
                    self.dim = false;
                }
                23 => self.italic = false,
                24 => self.underline = false,
                27 => self.reverse = false,
                30..=37 => self.fg = Some(Colour::Ansi((code - 30) as u8)),
                38 => self.fg = extended(&mut codes),
                39 => self.fg = None,
                40..=47 => self.bg = Some(Colour::Ansi((code - 40) as u8)),
                48 => self.bg = extended(&mut codes),
                49 => self.bg = None,
                90..=97 => self.fg = Some(Colour::Ansi((code - 90 + 8) as u8)),
                100..=107 => self.bg = Some(Colour::Ansi((code - 100 + 8) as u8)),
                _ => {}
            }
        }
    }
}

/// The `5;n` and `2;r;g;b` tails of a 38 or 48. A tail that runs out mid-colour leaves the slot
/// untouched, which is the same as the sequence never having arrived.
fn extended(codes: &mut impl Iterator<Item = u16>) -> Option<Colour> {
    match codes.next()? {
        5 => Some(Colour::Indexed(codes.next()? as u8)),
        2 => Some(Colour::Rgb(
            codes.next()? as u8,
            codes.next()? as u8,
            codes.next()? as u8,
        )),
        _ => None,
    }
}

/// A run of characters that share one style.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub text: String,
    pub sgr: Sgr,
}

/// One finished line, in the runs the program painted it in.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Line {
    segments: Vec<Segment>,
}

impl Line {
    pub fn plain(text: impl Into<String>) -> Line {
        let text = text.into();
        match text.is_empty() {
            true => Line::default(),
            false => Line {
                segments: vec![Segment {
                    text,
                    sgr: Sgr::default(),
                }],
            },
        }
    }

    pub fn segments(&self) -> &[Segment] {
        &self.segments
    }

    pub fn text(&self) -> String {
        self.segments
            .iter()
            .map(|segment| segment.text.as_str())
            .collect()
    }

    pub fn width(&self) -> usize {
        self.segments
            .iter()
            .map(|segment| segment.text.chars().count())
            .sum()
    }

    pub fn is_empty(&self) -> bool {
        self.width() == 0
    }

    /// The first `width` characters, keeping each one's style. Cut, never reflowed.
    pub fn clip(&self, width: usize) -> Line {
        let mut left = width;
        let mut segments = Vec::new();
        for segment in &self.segments {
            if left == 0 {
                break;
            }
            let text: String = segment.text.chars().take(left).collect();
            left -= text.chars().count();
            segments.push(Segment {
                text,
                sgr: segment.sgr,
            });
        }
        Line { segments }
    }
}

impl std::fmt::Display for Line {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for segment in &self.segments {
            f.write_str(&segment.text)?;
        }
        Ok(())
    }
}

impl PartialEq<&str> for Line {
    fn eq(&self, other: &&str) -> bool {
        self.text() == *other
    }
}

impl PartialEq<String> for Line {
    fn eq(&self, other: &String) -> bool {
        &self.text() == other
    }
}

/// One character as the cursor left it, style included, so a rewritten line keeps the paint of
/// whichever frame won each column.
#[derive(Debug, Clone, Copy)]
struct Cell {
    character: char,
    sgr: Sgr,
}

/// One source's output, mid-line and mid-sequence.
#[derive(Debug, Default)]
pub struct Filter {
    /// Undecoded bytes: the tail of a chunk that cut a UTF-8 character in half.
    tail: Vec<u8>,
    /// The line being written, as the cursor has left it.
    line: Vec<Cell>,
    /// Where the next character lands. A carriage return puts it back to nought.
    cursor: usize,
    scan: Scan,
    /// The style in force, which outlives the chunk that set it.
    sgr: Sgr,
    /// Parameters of the sequence being scanned, until its final byte says what they were for.
    params: String,
}

/// Where in a sequence the last chunk ran out.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum Scan {
    #[default]
    Text,
    /// An escape whose next byte decides what it is.
    Escape,
    /// CSI parameters, up to a final byte in `0x40..=0x7e`.
    Csi,
    /// An OSC string, terminated by BEL or by `ESC \`.
    Osc { after_escape: bool },
    /// An escape with an intermediate byte — charset designation and its like — takes one more.
    Intermediate,
}

impl Filter {
    pub fn new() -> Filter {
        Filter::default()
    }

    /// The lines `bytes` completed, in order. The line still being written stays here until its
    /// newline arrives, however many chunks that takes.
    pub fn absorb(&mut self, bytes: &[u8]) -> Vec<Line> {
        let text = self.decode(bytes);
        let mut lines = Vec::new();
        for character in text.chars() {
            match self.scan {
                Scan::Text => match character {
                    '\u{1b}' => self.scan = Scan::Escape,
                    '\n' => lines.push(self.finish()),
                    '\r' => self.cursor = 0,
                    '\t' => self.place_all("    "),
                    control if control.is_control() => {}
                    character => self.place(character),
                },
                Scan::Escape => {
                    self.scan = match character {
                        '[' => {
                            self.params.clear();
                            Scan::Csi
                        }
                        ']' => Scan::Osc {
                            after_escape: false,
                        },
                        intermediate if ('\u{20}'..='\u{2f}').contains(&intermediate) => {
                            Scan::Intermediate
                        }
                        _ => Scan::Text,
                    }
                }
                Scan::Csi => {
                    if ('\u{40}'..='\u{7e}').contains(&character) {
                        // Only `m` says anything about paint. The rest move the cursor or ask the
                        // terminal something, and neither is content.
                        if character == 'm' {
                            let params = std::mem::take(&mut self.params);
                            self.sgr.apply(&params);
                        }
                        self.scan = Scan::Text;
                    } else {
                        self.params.push(character);
                    }
                }
                Scan::Osc { after_escape } => {
                    self.scan = match character {
                        '\u{7}' => Scan::Text,
                        '\\' if after_escape => Scan::Text,
                        character => Scan::Osc {
                            after_escape: character == '\u{1b}',
                        },
                    }
                }
                Scan::Intermediate => self.scan = Scan::Text,
            }
            if self.line.len() >= LONGEST_LINE {
                lines.push(self.finish());
            }
        }
        lines
    }

    /// The line still being written, for a source that has ended without a newline after it. A crash
    /// message and a prompt both arrive that way.
    pub fn rest(&mut self) -> Option<Line> {
        match self.line.is_empty() {
            true => None,
            false => Some(self.finish()),
        }
    }

    /// Everything of the previous generation, half a line and half a sequence included. A log is
    /// truncated at spawn, so what was mid-flight belongs to a run that is over.
    pub fn reset(&mut self) {
        *self = Filter::new();
    }

    /// The cells collapsed into one run per style, which is what a reader draws.
    fn finish(&mut self) -> Line {
        self.cursor = 0;
        let mut segments: Vec<Segment> = Vec::new();
        for cell in self.line.drain(..) {
            match segments.last_mut() {
                Some(last) if last.sgr == cell.sgr => last.text.push(cell.character),
                _ => segments.push(Segment {
                    text: String::from(cell.character),
                    sgr: cell.sgr,
                }),
            }
        }
        Line { segments }
    }

    fn place(&mut self, character: char) {
        let cell = Cell {
            character,
            sgr: self.sgr,
        };
        match self.line.get_mut(self.cursor) {
            Some(overwritten) => *overwritten = cell,
            None => self.line.push(cell),
        }
        self.cursor += 1;
    }

    fn place_all(&mut self, text: &str) {
        for character in text.chars() {
            self.place(character);
        }
    }

    /// As much of `tail + bytes` as decodes, leaving a character the chunk cut in half for the chunk
    /// that finishes it.
    fn decode(&mut self, bytes: &[u8]) -> String {
        self.tail.extend_from_slice(bytes);
        let mut text = String::with_capacity(self.tail.len());
        loop {
            match std::str::from_utf8(&self.tail) {
                Ok(whole) => {
                    text.push_str(whole);
                    self.tail.clear();
                    return text;
                }
                Err(error) => {
                    let valid = error.valid_up_to();
                    text.push_str(&String::from_utf8_lossy(&self.tail[..valid]));
                    match error.error_len() {
                        // Not a character cut in half but bytes no decoder will ever take: spending
                        // them is what keeps the tail from wedging on them for the rest of the run.
                        Some(bad) => {
                            text.push(char::REPLACEMENT_CHARACTER);
                            self.tail.drain(..valid + bad);
                        }
                        None => {
                            self.tail.drain(..valid);
                            return text;
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all(chunks: &[&str]) -> Vec<Line> {
        let mut filter = Filter::new();
        chunks
            .iter()
            .flat_map(|chunk| filter.absorb(chunk.as_bytes()))
            .collect()
    }

    fn one(text: &str) -> Line {
        let lines = all(&[text, "\n"]);
        assert_eq!(lines.len(), 1, "{lines:?}");
        lines.into_iter().next().expect("a line")
    }

    #[test]
    fn the_control_characters_that_are_not_paint_never_reach_the_screen() {
        assert_eq!(one("\u{1b}[32mgreen\u{1b}[0m done"), "green done");
        assert_eq!(one("\u{1b}[2Kcleared"), "cleared");
        assert_eq!(one("\u{1b}]0;a title\u{7}shell"), "shell");
        assert_eq!(one("\u{1b}]0;a title\u{1b}\\shell"), "shell");
        assert_eq!(one("a\tb"), "a    b");
        assert_eq!(one("\u{1b}(Bplain"), "plain");
        assert_eq!(one("bell\u{7}rung"), "bellrung");
    }

    #[test]
    fn colour_is_kept_as_the_style_of_the_run_it_painted() {
        let line = one("\u{1b}[32mgreen\u{1b}[0m plain");
        assert_eq!(line, "green plain");
        assert_eq!(
            line.segments()
                .iter()
                .map(|segment| (segment.text.as_str(), segment.sgr.fg))
                .collect::<Vec<_>>(),
            [("green", Some(Colour::Ansi(2))), (" plain", None)]
        );
    }

    #[test]
    fn the_bright_half_and_the_bigger_palettes_are_read_as_written() {
        assert_eq!(
            one("\u{1b}[92mbright").segments()[0].sgr.fg,
            Some(Colour::Ansi(10))
        );
        assert_eq!(
            one("\u{1b}[38;5;208mindexed").segments()[0].sgr.fg,
            Some(Colour::Indexed(208))
        );
        assert_eq!(
            one("\u{1b}[38;2;255;199;92mrgb").segments()[0].sgr.fg,
            Some(Colour::Rgb(255, 199, 92))
        );
        let both = one("\u{1b}[1;4mboth");
        assert!(both.segments()[0].sgr.bold);
        assert!(both.segments()[0].sgr.underline);
    }

    #[test]
    fn a_rewritten_column_takes_the_paint_of_the_frame_that_won_it() {
        let line = one("\u{1b}[31mred\r\u{1b}[32mgr");
        assert_eq!(line, "grd");
        assert_eq!(
            line.segments()
                .iter()
                .map(|segment| (segment.text.as_str(), segment.sgr.fg))
                .collect::<Vec<_>>(),
            [("gr", Some(Colour::Ansi(2))), ("d", Some(Colour::Ansi(1)))]
        );
    }

    #[test]
    fn a_clipped_line_keeps_every_character_it_kept_painted_as_it_was() {
        let line = one("\u{1b}[34mfour\u{1b}[0mmore").clip(6);
        assert_eq!(line, "fourmo");
        assert_eq!(line.segments().len(), 2);
        assert_eq!(line.segments()[0].sgr.fg, Some(Colour::Ansi(4)));
        assert_eq!(line.segments()[1].sgr.fg, None);
    }

    #[test]
    fn a_carriage_return_returns_the_line_to_its_start_and_what_follows_overwrites_it() {
        assert_eq!(one("bare\rrewrite"), "rewrite");
        assert_eq!(one("[   ] 0%\r[## ] 50%\r[###] 100%"), "[###] 100%");
        // The line ending a terminal writes is CRLF, and the return in it changes nothing.
        assert_eq!(all(&["one\r\ntwo\r\n"]), ["one", "two"]);
    }

    #[test]
    fn a_bar_rewritten_frame_by_frame_reads_as_its_final_state_rather_than_every_frame_at_once() {
        let frames: Vec<String> = (0..=10)
            .map(|tenth| format!("\rdownloading {:>3}%", tenth * 10))
            .collect();
        let mut chunks: Vec<&str> = frames.iter().map(String::as_str).collect();
        chunks.push("\n");
        assert_eq!(all(&chunks), ["downloading 100%"]);
    }

    #[test]
    fn a_line_only_arrives_once_its_newline_does() {
        let mut filter = Filter::new();
        assert!(filter.absorb(b"half a ").is_empty());
        assert_eq!(
            filter.absorb(b"line\nand another\n"),
            ["half a line", "and another"]
        );
    }

    #[test]
    fn a_sequence_cut_in_half_by_a_chunk_boundary_is_still_spent_whole() {
        assert_eq!(
            all(&["\u{1b}[3", "2mgreen\u{1b}", "[0m done\n"]),
            ["green done"]
        );
        assert_eq!(all(&["\u{1b}", "[32mgreen\n"]), ["green"]);
        assert_eq!(all(&["\u{1b}]0;a ti", "tle\u{7}shell\n"]), ["shell"]);
        // OSC's two-character terminator, split between the ESC and the backslash.
        assert_eq!(all(&["\u{1b}]0;a title\u{1b}", "\\shell\n"]), ["shell"]);
        assert_eq!(all(&["\u{1b}", "(Bplain\n"]), ["plain"]);
    }

    #[test]
    fn a_character_cut_in_half_by_a_chunk_boundary_is_held_back_rather_than_mangled() {
        let mut filter = Filter::new();
        let text = "café ✔".as_bytes();
        let (head, tail) = text.split_at(4);
        assert!(filter.absorb(head).is_empty());
        assert_eq!(filter.absorb(tail), Vec::<String>::new());
        assert_eq!(filter.absorb(b"\n"), ["café ✔"]);
    }

    /// A byte no decoder will ever take must not sit in the tail waiting for a chunk that finishes it.
    #[test]
    fn a_byte_that_is_not_utf8_at_all_is_spent_rather_than_held_forever() {
        let mut filter = Filter::new();
        assert_eq!(filter.absorb(b"one \xff two\n"), ["one \u{fffd} two"]);
        assert_eq!(filter.absorb(b"after\n"), ["after"]);
    }

    #[test]
    fn a_source_that_never_writes_a_newline_is_broken_up_rather_than_held_in_memory() {
        let mut filter = Filter::new();
        let lines = filter.absorb("x".repeat(LONGEST_LINE * 2 + 5).as_bytes());
        assert_eq!(lines.len(), 2);
        assert!(lines.iter().all(|line| line.width() == LONGEST_LINE));
        assert_eq!(filter.rest().map(|rest| rest.width()), Some(5));
    }

    #[test]
    fn a_source_that_ended_without_a_newline_still_gives_up_its_last_line() {
        let mut filter = Filter::new();
        assert!(filter.absorb(b"irb(main):001> ").is_empty());
        assert_eq!(filter.rest(), Some(Line::plain("irb(main):001> ")));
        assert_eq!(filter.rest(), None);
    }

    #[test]
    fn a_reset_takes_the_half_line_and_the_half_sequence_of_the_previous_generation_with_it() {
        let mut filter = Filter::new();
        assert!(filter.absorb(b"half a line\x1b[").is_empty());
        filter.reset();
        assert_eq!(filter.absorb(b"32mfresh\n"), ["32mfresh"]);
    }
}
