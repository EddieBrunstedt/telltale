//! Just enough IMAP4rev1 (RFC 3501) and IDLE (RFC 2177) to count a mailbox
//! and wait for it to change. Generic over the byte stream, so tests can play
//! a scripted server.

use std::fmt;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::time::Duration;

/// How long a command may take before the connection counts as dead.
pub(crate) const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

/// A byte stream whose reads can time out.
pub(crate) trait Transport: Read + Write {
    fn set_read_timeout(&self, timeout: Duration) -> io::Result<()>;
}

#[derive(Debug)]
pub(crate) enum Error {
    Io(io::Error),
    /// The server answered NO or BAD, or said BYE. Holds its words.
    Refused(String),
    /// The server said something this client cannot make sense of.
    Protocol(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "{e}"),
            Error::Refused(words) => write!(f, "server refused: {words}"),
            Error::Protocol(what) => write!(f, "unexpected from server: {what}"),
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Error {
        Error::Io(e)
    }
}

pub(crate) struct Session<T: Transport> {
    conn: BufReader<T>,
    next_tag: u32,
}

impl<T: Transport> Session<T> {
    /// Over a fresh connection, before or instead of the greeting (after
    /// STARTTLS the server sends none).
    pub(crate) fn new(transport: T) -> Session<T> {
        Session {
            conn: BufReader::new(transport),
            next_tag: 1,
        }
    }

    pub(crate) fn greeting(&mut self) -> Result<(), Error> {
        let line = self.read_line()?;
        if line.starts_with("* OK") || line.starts_with("* PREAUTH") {
            Ok(())
        } else if let Some(words) = line.strip_prefix("* BYE") {
            Err(Error::Refused(words.trim().to_owned()))
        } else {
            Err(Error::Protocol(format!("greeting {line:?}")))
        }
    }

    /// The transport back, for STARTTLS. Anything the server sent after its
    /// OK would otherwise be read as if it came over TLS (the STARTTLS
    /// injection attack), so that is refused.
    pub(crate) fn into_transport(self) -> Result<T, Error> {
        if self.conn.buffer().is_empty() {
            Ok(self.conn.into_inner())
        } else {
            Err(Error::Protocol(
                "data after the STARTTLS response, before TLS".to_owned(),
            ))
        }
    }

    pub(crate) fn starttls(&mut self) -> Result<(), Error> {
        self.command("STARTTLS").map(drop)
    }

    pub(crate) fn login(&mut self, user: &str, password: &str) -> Result<(), Error> {
        let command = format!("LOGIN {} {}", quote(user)?, quote(password)?);
        self.command(&command).map(drop)
    }

    /// Capability names, upper-cased.
    pub(crate) fn capabilities(&mut self) -> Result<Vec<String>, Error> {
        Ok(self
            .command("CAPABILITY")?
            .iter()
            .filter_map(|line| line.strip_prefix("* CAPABILITY "))
            .flat_map(str::split_whitespace)
            .map(str::to_ascii_uppercase)
            .collect())
    }

    /// Selects the mailbox read-only (so no flag is ever changed) and returns
    /// how many messages it holds.
    pub(crate) fn examine(&mut self, mailbox: &str) -> Result<u32, Error> {
        let lines = self.command(&format!("EXAMINE {}", quote(mailbox)?))?;
        lines
            .iter()
            .filter_map(|line| {
                line.strip_prefix("* ")?
                    .strip_suffix(" EXISTS")?
                    .parse()
                    .ok()
            })
            .next_back()
            .ok_or_else(|| Error::Protocol("EXAMINE without EXISTS".to_owned()))
    }

    /// Messages in the selected mailbox without the \Seen flag.
    pub(crate) fn unseen(&mut self) -> Result<u32, Error> {
        let lines = self.command("SEARCH UNSEEN")?;
        Ok(lines
            .iter()
            .filter_map(|line| line.strip_prefix("* SEARCH"))
            .map(|ids| ids.split_whitespace().count() as u32)
            .sum())
    }

    /// When message 1 (the earliest to arrive) arrived, as the server writes
    /// it: `17-Oct-2026 09:12:33 +0200`.
    pub(crate) fn first_arrival(&mut self) -> Result<String, Error> {
        let lines = self.command("FETCH 1 (INTERNALDATE)")?;
        lines
            .iter()
            .find_map(|line| {
                let after = &line[line.find("INTERNALDATE \"")? + "INTERNALDATE \"".len()..];
                Some(after[..after.find('"')?].to_owned())
            })
            .ok_or_else(|| Error::Protocol("FETCH without INTERNALDATE".to_owned()))
    }

    /// Waits up to `limit` for the selected mailbox to change, then ends the
    /// IDLE. True if it changed (messages came or went, or flags changed).
    pub(crate) fn idle(&mut self, limit: Duration) -> Result<bool, Error> {
        let tag = self.send("IDLE")?;
        loop {
            let line = self.read_line()?;
            if line.starts_with('+') {
                break;
            }
            if let Some(rest) = line.strip_prefix(&format!("{tag} ")) {
                return Err(Error::Refused(rest.to_owned()));
            }
        }
        self.conn.get_ref().set_read_timeout(limit)?;
        let waited = loop {
            match self.read_line() {
                Ok(line) if changes(&line) => break Ok(true),
                Ok(line) if line.starts_with("* BYE") => break Err(Error::Refused(line)),
                Ok(_) => {} // e.g. "* OK still here"
                Err(Error::Io(e)) if timed_out(&e) => break Ok(false),
                Err(e) => break Err(e),
            }
        };
        self.conn.get_ref().set_read_timeout(COMMAND_TIMEOUT)?;
        let changed = waited?;
        self.write("DONE")?;
        self.until_tagged(&tag)?;
        Ok(changed)
    }

    /// Best effort, for leaving politely: no answer is awaited.
    pub(crate) fn logout(&mut self) {
        let _ = self.send("LOGOUT");
    }

    fn command(&mut self, command: &str) -> Result<Vec<String>, Error> {
        let tag = self.send(command)?;
        self.until_tagged(&tag)
    }

    fn send(&mut self, command: &str) -> Result<String, Error> {
        let tag = format!("t{}", self.next_tag);
        self.next_tag += 1;
        self.write(&format!("{tag} {command}"))?;
        Ok(tag)
    }

    fn write(&mut self, line: &str) -> Result<(), Error> {
        let conn = self.conn.get_mut();
        conn.write_all(line.as_bytes())?;
        conn.write_all(b"\r\n")?;
        conn.flush()?;
        Ok(())
    }

    /// The untagged lines up to the tagged completion, if that is OK.
    fn until_tagged(&mut self, tag: &str) -> Result<Vec<String>, Error> {
        let prefix = format!("{tag} ");
        let mut untagged = Vec::new();
        loop {
            let line = self.read_line()?;
            if let Some(rest) = line.strip_prefix(&prefix) {
                return if rest.starts_with("OK") {
                    Ok(untagged)
                } else {
                    Err(Error::Refused(rest.to_owned()))
                };
            }
            if let Some(words) = line.strip_prefix("* BYE") {
                return Err(Error::Refused(format!("BYE{words}")));
            }
            untagged.push(line);
        }
    }

    /// One logical line, without CRLF. A literal (`{12}` at the end of a
    /// line, then exactly that many bytes) is folded into the line.
    fn read_line(&mut self) -> Result<String, Error> {
        let mut line = Vec::new();
        loop {
            let start = line.len();
            if self.conn.read_until(b'\n', &mut line)? == 0 {
                return Err(Error::Io(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "the server closed the connection",
                )));
            }
            // Only this read's line ending: a literal may end in CR or LF.
            while line.len() > start && line.last().is_some_and(|b| *b == b'\n' || *b == b'\r') {
                line.pop();
            }
            match literal(&line[start..]) {
                Some((at, n)) => {
                    // The bytes take the announcement's place.
                    line.truncate(start + at);
                    let mut bytes = vec![0; n];
                    self.conn.read_exact(&mut bytes)?;
                    line.extend_from_slice(&bytes);
                }
                None => return Ok(String::from_utf8_lossy(&line).into_owned()),
            }
        }
    }
}

/// Where a literal announcement `{n}` starts, and `n`, if the line ends with
/// one.
fn literal(line: &[u8]) -> Option<(usize, usize)> {
    let open = line.strip_suffix(b"}")?;
    let at = open.iter().rposition(|b| *b == b'{')?;
    let n = std::str::from_utf8(&open[at + 1..]).ok()?.parse().ok()?;
    Some((at, n))
}

/// Untagged responses that mean the selected mailbox changed.
fn changes(line: &str) -> bool {
    let mut words = line.split_whitespace();
    words.next() == Some("*")
        && words.next().is_some_and(|n| n.parse::<u32>().is_ok())
        && matches!(
            words.next().map(str::to_ascii_uppercase).as_deref(),
            Some("EXISTS" | "EXPUNGE" | "FETCH" | "RECENT")
        )
}

fn timed_out(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

/// An IMAP quoted string. CR, LF and NUL cannot be quoted (they would need a
/// literal), and are refused rather than sent mangled.
fn quote(s: &str) -> Result<String, Error> {
    if s.contains(['\r', '\n', '\0']) {
        return Err(Error::Protocol(
            "a value with a line break or NUL cannot be sent".to_owned(),
        ));
    }
    Ok(format!(
        "\"{}\"",
        s.replace('\\', "\\\\").replace('"', "\\\"")
    ))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::collections::VecDeque;

    /// A scripted server: what it sends, in chunks, with timeouts in between;
    /// and everything the client wrote.
    #[derive(Default)]
    pub(crate) struct Script {
        chunks: VecDeque<Option<Vec<u8>>>,
        pub(crate) written: Vec<u8>,
    }

    impl Script {
        pub(crate) fn new() -> Script {
            Script::default()
        }

        /// The server sends these lines (CRLF added).
        pub(crate) fn send(mut self, lines: &[&str]) -> Script {
            let mut bytes = Vec::new();
            for line in lines {
                bytes.extend_from_slice(line.as_bytes());
                bytes.extend_from_slice(b"\r\n");
            }
            self.chunks.push_back(Some(bytes));
            self
        }

        /// The next read times out.
        pub(crate) fn silence(mut self) -> Script {
            self.chunks.push_back(None);
            self
        }

        pub(crate) fn written(&self) -> String {
            String::from_utf8_lossy(&self.written).into_owned()
        }
    }

    impl Read for Script {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            match self.chunks.front_mut() {
                None => Ok(0),
                Some(None) => {
                    self.chunks.pop_front();
                    Err(io::ErrorKind::WouldBlock.into())
                }
                Some(Some(bytes)) => {
                    let n = buf.len().min(bytes.len());
                    buf[..n].copy_from_slice(&bytes[..n]);
                    bytes.drain(..n);
                    if bytes.is_empty() {
                        self.chunks.pop_front();
                    }
                    Ok(n)
                }
            }
        }
    }

    impl Write for Script {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.written.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Transport for Script {
        fn set_read_timeout(&self, _: Duration) -> io::Result<()> {
            Ok(())
        }
    }

    fn session(script: Script) -> Session<Script> {
        Session::new(script)
    }

    #[test]
    fn a_proton_bridge_style_session() {
        let mut s = session(
            Script::new()
                .send(&["* OK [CAPABILITY IMAP4rev1 IDLE STARTTLS] Proton Mail Bridge ready"])
                .send(&["t1 OK [CAPABILITY IDLE IMAP4rev1 MOVE] Logged in"])
                .send(&[
                    "* CAPABILITY IMAP4rev1 IDLE MOVE UIDPLUS",
                    "t2 OK CAPABILITY",
                ])
                .send(&[
                    "* FLAGS (\\Answered \\Flagged \\Deleted \\Seen \\Draft)",
                    "* 257 EXISTS",
                    "* 0 RECENT",
                    "* OK [UIDVALIDITY 1] UIDs valid",
                    "t3 OK [READ-ONLY] EXAMINE completed",
                ])
                .send(&["* SEARCH 1 4 9", "t4 OK SEARCH completed"])
                .send(&[
                    "* 1 FETCH (INTERNALDATE \"17-Oct-2026 09:12:33 +0200\")",
                    "t5 OK FETCH completed",
                ]),
        );
        s.greeting().unwrap();
        s.login("me@proton.me", "pa\"ss\\word").unwrap();
        assert!(s.capabilities().unwrap().contains(&"IDLE".to_owned()));
        assert_eq!(s.examine("INBOX").unwrap(), 257);
        assert_eq!(s.unseen().unwrap(), 3);
        assert_eq!(s.first_arrival().unwrap(), "17-Oct-2026 09:12:33 +0200");
        assert_eq!(
            s.conn.get_ref().written(),
            "t1 LOGIN \"me@proton.me\" \"pa\\\"ss\\\\word\"\r\n\
             t2 CAPABILITY\r\n\
             t3 EXAMINE \"INBOX\"\r\n\
             t4 SEARCH UNSEEN\r\n\
             t5 FETCH 1 (INTERNALDATE)\r\n"
        );
    }

    #[test]
    fn an_empty_search_is_zero() {
        let mut s = session(Script::new().send(&["* SEARCH", "t1 OK done"]));
        assert_eq!(s.unseen().unwrap(), 0);
    }

    #[test]
    fn idle_reports_a_change_then_ends_cleanly() {
        let mut s = session(
            Script::new()
                .send(&["+ idling"])
                .send(&["* OK still here", "* 258 EXISTS"])
                .send(&["t1 OK IDLE terminated"]),
        );
        assert!(s.idle(Duration::from_secs(1500)).unwrap());
        assert_eq!(s.conn.get_ref().written(), "t1 IDLE\r\nDONE\r\n");
    }

    #[test]
    fn idle_that_times_out_reports_no_change() {
        let mut s = session(
            Script::new()
                .send(&["+ idling"])
                .silence()
                .send(&["t1 OK IDLE terminated"]),
        );
        assert!(!s.idle(Duration::from_secs(1500)).unwrap());
    }

    #[test]
    fn flag_changes_and_expunges_count_as_changes() {
        assert!(changes("* 5 FETCH (FLAGS (\\Seen))"));
        assert!(changes("* 12 EXPUNGE"));
        assert!(!changes("* OK still here"));
        assert!(!changes("* SEARCH 1 2"));
    }

    #[test]
    fn refusals_and_goodbyes_are_errors() {
        let mut s = session(Script::new().send(&["t1 NO [AUTHENTICATIONFAILED] bad credentials"]));
        let e = s.login("u", "p").unwrap_err().to_string();
        assert_eq!(
            e,
            "server refused: NO [AUTHENTICATIONFAILED] bad credentials"
        );

        let mut s = session(Script::new().send(&["* BYE shutting down"]));
        assert!(matches!(s.examine("INBOX"), Err(Error::Refused(_))));

        let mut s = session(Script::new().send(&["* BYE too many connections"]));
        assert!(matches!(s.greeting(), Err(Error::Refused(_))));
    }

    #[test]
    fn a_closed_connection_is_an_error_not_an_empty_answer() {
        let mut s = session(Script::new().send(&["* 3 EXISTS"]));
        assert!(matches!(s.examine("INBOX"), Err(Error::Io(_))));
    }

    #[test]
    fn literals_are_folded_into_their_line() {
        // The literal's bytes, then the rest of the line (here nothing).
        let mut s = session(Script::new().send(&["* LIST () \"/\" {5}", "INBOX", "t1 OK"]));
        assert_eq!(s.read_line().unwrap(), "* LIST () \"/\" INBOX");
        assert_eq!(s.read_line().unwrap(), "t1 OK");
    }

    #[test]
    fn data_buffered_after_starttls_is_refused() {
        let mut s = session(Script::new().send(&["t1 OK begin TLS", "* injected"]));
        s.starttls().unwrap();
        assert!(s.into_transport().is_err());

        let mut s = session(Script::new().send(&["t1 OK begin TLS"]));
        s.starttls().unwrap();
        assert!(s.into_transport().is_ok());
    }

    #[test]
    fn values_with_line_breaks_are_not_sent() {
        let mut s = session(Script::new());
        assert!(s.login("u", "p\r\nt9 DELETE INBOX").is_err());
        assert_eq!(s.conn.get_ref().written(), "");
    }
}
