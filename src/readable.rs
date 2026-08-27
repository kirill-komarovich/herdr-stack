//! Program output turned into lines a plain reader can be shown, in one place.
//!
//! Everything a terminal would act on is spent here rather than passed on: colour and cursor motion
//! would reach the popup's own screen as commands rather than as content, and the log has three
//! readers — the overlay, the peek and your own shell — of which the last renders nothing at all.
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

/// One source's output, mid-line and mid-sequence.
#[derive(Debug, Default)]
pub struct Filter {
    /// Undecoded bytes: the tail of a chunk that cut a UTF-8 character in half.
    tail: Vec<u8>,
    /// The line being written, as the cursor has left it.
    line: Vec<char>,
    /// Where the next character lands. A carriage return puts it back to nought.
    cursor: usize,
    scan: Scan,
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
    pub fn absorb(&mut self, bytes: &[u8]) -> Vec<String> {
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
                        '[' => Scan::Csi,
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
                        self.scan = Scan::Text;
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
    pub fn rest(&mut self) -> Option<String> {
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

    fn finish(&mut self) -> String {
        self.cursor = 0;
        self.line.drain(..).collect()
    }

    fn place(&mut self, character: char) {
        match self.line.get_mut(self.cursor) {
            Some(overwritten) => *overwritten = character,
            None => self.line.push(character),
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

    fn all(chunks: &[&str]) -> Vec<String> {
        let mut filter = Filter::new();
        chunks
            .iter()
            .flat_map(|chunk| filter.absorb(chunk.as_bytes()))
            .collect()
    }

    fn one(text: &str) -> String {
        let lines = all(&[text, "\n"]);
        assert_eq!(lines.len(), 1, "{lines:?}");
        lines.into_iter().next().expect("a line")
    }

    #[test]
    fn colour_and_the_rest_of_the_control_characters_never_reach_the_screen() {
        assert_eq!(one("\u{1b}[32mgreen\u{1b}[0m done"), "green done");
        assert_eq!(one("\u{1b}]0;a title\u{7}shell"), "shell");
        assert_eq!(one("\u{1b}]0;a title\u{1b}\\shell"), "shell");
        assert_eq!(one("a\tb"), "a    b");
        assert_eq!(one("\u{1b}(Bplain"), "plain");
        assert_eq!(one("bell\u{7}rung"), "bellrung");
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
        assert!(
            lines
                .iter()
                .all(|line| line.chars().count() == LONGEST_LINE)
        );
        assert_eq!(filter.rest().map(|rest| rest.chars().count()), Some(5));
    }

    #[test]
    fn a_source_that_ended_without_a_newline_still_gives_up_its_last_line() {
        let mut filter = Filter::new();
        assert!(filter.absorb(b"irb(main):001> ").is_empty());
        assert_eq!(filter.rest().as_deref(), Some("irb(main):001> "));
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
