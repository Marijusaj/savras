//! Whether the terminal window has your attention.
//!
//! The panel stays quiet about the session in the working pane — but only
//! while you are actually looking at it. In another application you would
//! never see that session ask, and silence there is the one failure a panel
//! like this cannot afford. So Savras asks the terminal to report focus
//! (DEC mode 1004) and treats *unfocused, or unknown*, as "tell me".
//!
//! Enabling 1004 makes the terminal send `ESC [ I` and `ESC [ O` into our
//! stdin, which is the same stream we forward to the child verbatim. A shell
//! that never asked for focus reporting would print those as stray characters,
//! so they are stripped unless the child asked for them itself — the same
//! mirror-what-the-child-wanted rule the mouse already follows. vt100 does not
//! track mode 1004, so the child's request is found by reading its output.

/// Ask the terminal to report focus, and stop asking.
pub const ENABLE: &str = "\x1b[?1004h";
pub const DISABLE: &str = "\x1b[?1004l";

const FOCUS_IN: &[u8] = b"\x1b[I";
const FOCUS_OUT: &[u8] = b"\x1b[O";

/// What the terminal last said about focus, if it said anything in this chunk.
/// The last event wins: a buffer holding both is someone alt-tabbing faster
/// than we read.
pub fn event(bytes: &[u8]) -> Option<bool> {
    let mut latest = None;
    for i in 0..bytes.len().saturating_sub(2) {
        match &bytes[i..i + 3] {
            FOCUS_IN => latest = Some(true),
            FOCUS_OUT => latest = Some(false),
            _ => {}
        }
    }
    latest
}

/// The same bytes with focus events removed, for a child that never asked for
/// them.
pub fn strip(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes.len() - i >= 3 && matches!(&bytes[i..i + 3], FOCUS_IN | FOCUS_OUT) {
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

/// Reads a child's output for its own `ESC [ ? … 1004 h` / `l`, which vt100
/// discards. Keeps the tail of each chunk, because a sequence split across two
/// reads is otherwise a mode change we never see.
#[derive(Default)]
pub struct Watcher {
    carry: Vec<u8>,
    wanted: bool,
}

/// The longest `ESC [ ? … h` we bother to reassemble across a chunk boundary.
const CARRY: usize = 32;

impl Watcher {
    /// Feed a chunk of the child's output; returns whether it wants focus
    /// events now.
    pub fn feed(&mut self, chunk: &[u8]) -> bool {
        let mut bytes = std::mem::take(&mut self.carry);
        bytes.extend_from_slice(chunk);

        let mut i = 0;
        while let Some(start) = find(&bytes[i..], b"\x1b[?") {
            let params = i + start + 3;
            match terminator(&bytes[params..]) {
                Some((end, set)) => {
                    if has_1004(&bytes[params..params + end]) {
                        self.wanted = set;
                    }
                    i = params + end + 1;
                }
                // Truncated: keep it for the next chunk rather than losing it.
                None => {
                    self.carry = bytes[i + start..].to_vec();
                    self.carry.truncate(CARRY);
                    return self.wanted;
                }
            }
        }

        // Only a trailing partial escape is worth remembering.
        let tail = bytes.len().saturating_sub(CARRY).max(i);
        if let Some(start) = find(&bytes[tail..], b"\x1b") {
            self.carry = bytes[tail + start..].to_vec();
        }
        self.wanted
    }
}

/// The end of a private-mode sequence, and whether it sets or resets.
fn terminator(bytes: &[u8]) -> Option<(usize, bool)> {
    for (i, b) in bytes.iter().enumerate() {
        match b {
            b'h' => return Some((i, true)),
            b'l' => return Some((i, false)),
            b'0'..=b'9' | b';' => {}
            // Not a private mode set after all; stop rather than run on.
            _ => return None,
        }
    }
    None
}

fn has_1004(params: &[u8]) -> bool {
    params.split(|b| *b == b';').any(|p| p == b"1004")
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn focus_events_are_read_off_the_input_stream() {
        assert_eq!(event(b"\x1b[I"), Some(true));
        assert_eq!(event(b"\x1b[O"), Some(false));
        assert_eq!(event(b"hello"), None);
        // Typing arriving in the same read as the focus event.
        assert_eq!(event(b"\x1b[Ils -la\r"), Some(true));
        // Away and back before we looked: the last one is the truth.
        assert_eq!(event(b"\x1b[O\x1b[I"), Some(true));
        assert_eq!(event(b"\x1b[I\x1b[O"), Some(false));
    }

    #[test]
    fn stripping_leaves_everything_else_untouched() {
        assert_eq!(strip(b"\x1b[Ils\r"), b"ls\r");
        assert_eq!(strip(b"a\x1b[Ob"), b"ab");
        // An arrow key is ESC [ A — near neighbours must survive.
        assert_eq!(strip(b"\x1b[A\x1b[B"), b"\x1b[A\x1b[B");
        assert_eq!(strip(b"plain"), b"plain");
    }

    #[test]
    fn a_child_asking_for_focus_reporting_is_noticed() {
        let mut w = Watcher::default();
        assert!(!w.feed(b"just some output"), "nothing has asked yet");
        assert!(w.feed(b"\x1b[?1004h"));
        // ...and it stays wanted until it is turned off again.
        assert!(w.feed(b"more output"));
        assert!(!w.feed(b"\x1b[?1004l"));
    }

    #[test]
    fn focus_reporting_asked_for_alongside_other_modes_still_counts() {
        // Terminals accept several private modes in one sequence, and TUIs
        // send them that way.
        let mut w = Watcher::default();
        assert!(w.feed(b"\x1b[?1049;1004;2004h"));
        assert!(!w.feed(b"\x1b[?1049;1004l"));
    }

    #[test]
    fn other_modes_are_not_mistaken_for_it() {
        let mut w = Watcher::default();
        assert!(!w.feed(b"\x1b[?1000h\x1b[?1006h\x1b[?2004h"));
        // 10040 is not 1004.
        assert!(!w.feed(b"\x1b[?10040h"));
    }

    #[test]
    fn a_sequence_split_across_two_reads_is_still_seen() {
        // The pty hands us whatever happened to be in the buffer, and a mode
        // change lost at a chunk boundary is a bug you would never reproduce.
        let mut w = Watcher::default();
        assert!(!w.feed(b"hello\x1b[?10"));
        assert!(w.feed(b"04h"));
    }

    #[test]
    fn ordinary_output_does_not_accumulate() {
        let mut w = Watcher::default();
        w.feed(b"a lot of perfectly ordinary output with no escapes in it");
        assert!(w.carry.is_empty());
        w.feed(b"\x1b[31mred\x1b[0m");
        assert!(w.carry.len() <= CARRY);
    }
}
