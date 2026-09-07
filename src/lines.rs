//! Reading lines from somebody else's program.
//!
//! A script and an i3bar provider are both programs dbar did not write, and both are read
//! a line at a time. `BufRead::lines` will grow one line until the machine runs out of
//! memory, so a program that prints without ever printing a newline - a broken loop, a
//! binary opened by mistake - can take the bar down with it. What is read here is bounded
//! instead: a line longer than the limit is cut, the rest of it is thrown away, and the
//! caller is told so it can say which program did it.

use std::io::BufRead;

/// The longest line taken from another program, in bytes.
///
/// A status line is a few hundred bytes and an i3bar update with every block on the bar in
/// it is a few thousand, so this is far more than a working program needs and small enough
/// that a broken one costs nothing worth measuring.
pub const LIMIT: usize = 256 * 1024;

/// One line, and how much of it was over the limit.
pub struct Line {
    pub text: String,
    /// Bytes past the limit that were dropped. Zero for every line a sane program prints.
    pub dropped: usize,
}

/// Lines from a reader, each cut to `LIMIT` bytes.
pub struct Lines<R> {
    reader: R,
    limit: usize,
}

pub fn capped<R: BufRead>(reader: R) -> Lines<R> {
    Lines {
        reader,
        limit: LIMIT,
    }
}

impl<R: BufRead> Iterator for Lines<R> {
    type Item = std::io::Result<Line>;

    fn next(&mut self) -> Option<std::io::Result<Line>> {
        let mut kept: Vec<u8> = Vec::new();
        let mut dropped = 0usize;
        let mut any = false;
        loop {
            let available = match self.reader.fill_buf() {
                Ok(available) => available,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Some(Err(e)),
            };
            // End of the stream: whatever was read without a newline behind it is still a
            // line, and nothing at all is the end of the iterator.
            if available.is_empty() {
                if !any {
                    return None;
                }
                break;
            }
            any = true;
            let (upto, consumed, done) = match available.iter().position(|b| *b == b'\n') {
                Some(at) => (at, at + 1, true),
                None => (available.len(), available.len(), false),
            };
            let room = self.limit.saturating_sub(kept.len());
            let take = room.min(upto);
            kept.extend_from_slice(&available[..take]);
            dropped += upto - take;
            self.reader.consume(consumed);
            if done {
                break;
            }
        }
        if kept.last() == Some(&b'\r') {
            kept.pop();
        }
        let text = String::from_utf8_lossy(&kept).into_owned();
        Some(Ok(Line { text, dropped }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(input: &[u8], limit: usize) -> Vec<(String, usize)> {
        let lines = Lines {
            reader: std::io::BufReader::new(input),
            limit,
        };
        lines
            .map(|l| l.expect("reading from a slice cannot fail"))
            .map(|l| (l.text, l.dropped))
            .collect()
    }

    #[test]
    fn a_line_within_the_limit_arrives_whole() {
        let read = read(b"one\ntwo\r\n", 16);
        assert_eq!(
            read,
            [("one".to_string(), 0), ("two".to_string(), 0)],
            "a carriage return belongs to the line ending, not to the text"
        );
    }

    /// The point of the whole module: a program that prints without ever printing a
    /// newline costs a fixed amount of memory rather than all of it.
    #[test]
    fn a_line_over_the_limit_is_cut_and_the_rest_counted() {
        let read = read(b"abcdefgh\nij\n", 4);
        assert_eq!(read, [("abcd".to_string(), 4), ("ij".to_string(), 0)]);
    }

    #[test]
    fn output_that_stops_without_a_newline_is_still_a_line() {
        assert_eq!(read(b"tail", 16), [("tail".to_string(), 0)]);
        assert!(read(b"", 16).is_empty(), "nothing at all is no lines");
    }
}
