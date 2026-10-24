//! Claude Code sessions, and whether one is waiting on you.
//!
//! Claude Code keeps a file per running session in `~/.claude/sessions` and
//! rewrites it when the session's status changes. The format is its own and
//! undocumented, so anything this gauge does not recognise makes the reading
//! unknown rather than quietly wrong. A session killed outright leaves its
//! file behind, so a file counts only while its process is alive and is the
//! one that wrote it.
//!
//! Watching costs nothing between changes: inotify on the directory, and a
//! pidfd per live session for the deaths that leave no trace in it.
//!
//! Titles come from each session's transcript, where Claude Code appends the
//! current title every turn or so. A transcript runs to tens of MB, so only
//! what was appended since the last reading is read.

use std::collections::HashMap;
use std::fs;
use std::io::{self, BufRead, BufReader, Seek, SeekFrom};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use crate::gauge::Gauge;
use crate::gauge::clock::clock;
use crate::gauge::inotify;
use crate::reading::{Level, Reading};
use crate::run::{Extra, Given};

/// How long to wait before trying again to watch a directory that could not
/// be watched.
const RETRY_EVERY: Duration = Duration::from_secs(60);
/// A file that does not parse may be half-written: read it again this much
/// later, this many times, before the reading turns unknown.
const REREAD_AFTER: Duration = Duration::from_millis(50);
const REREADS: usize = 3;
/// Titles are cut to this many characters, so one long title does not
/// widen every row.
const TITLE_WIDTH: usize = 40;

/// No options of its own, but a check must not set up a watch, so it needs
/// to know whether it is being watched.
pub const OPTIONS: Extra = Extra {
    usage: "",
    help: "",
    flags: &[],
};

pub struct Claude {
    home: Option<String>,
    watching: bool,
    inotify: Option<OwnedFd>,
    /// One per live session; each becomes readable when its process exits.
    pidfds: Vec<OwnedFd>,
    /// A session died between being read and being followed, so the next
    /// reading must not wait for a change that has already happened.
    reread: bool,
    tried: bool,
    titles: Titles,
}

impl Claude {
    pub fn from_options(given: &Given) -> Result<Claude, String> {
        Ok(Claude {
            home: std::env::var("HOME").ok(),
            watching: given.watching(),
            inotify: None,
            pidfds: Vec::new(),
            reread: false,
            tried: false,
            titles: Titles::default(),
        })
    }

    fn dir(&self) -> Result<PathBuf, String> {
        let home = self.home.as_ref().ok_or("$HOME is not set")?;
        Ok(Path::new(home).join(".claude/sessions"))
    }

    fn home(&self) -> String {
        self.home.clone().unwrap_or_default()
    }

    /// Starts watching, or after a failure, waits and tries again.
    fn watch(&mut self) -> Result<(), String> {
        if self.tried {
            thread::sleep(RETRY_EVERY);
        }
        self.tried = true;
        self.inotify = Some(watch(&self.dir()?)?);
        Ok(())
    }

    /// Blocks until the directory changes or a session's process exits.
    fn wait(&mut self) {
        if std::mem::take(&mut self.reread) {
            return;
        }
        let Some(inotify) = &self.inotify else { return };
        let mut fds: Vec<libc::pollfd> = std::iter::once(inotify)
            .chain(&self.pidfds)
            .map(|fd| libc::pollfd {
                fd: fd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            })
            .collect();
        loop {
            // SAFETY: fds is a live, correctly sized array of pollfd.
            let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) };
            if n >= 0 {
                break;
            }
            if io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                // Not expected; start over rather than spin.
                self.inotify = None;
                return;
            }
        }
        if fds[0].revents != 0 && !inotify::drain(inotify) {
            // The directory went away; the watch went with it.
            self.inotify = None;
        }
    }

    /// Reads every session, again a little later if a file is half-written,
    /// and gives each its title.
    fn read(&mut self) -> Result<Vec<Session>, String> {
        let dir = self.dir()?;
        let home = self.home();
        let mut rereads = 0;
        let mut sessions = loop {
            match read_sessions(&dir, &home) {
                Err(_) if rereads < REREADS => {
                    rereads += 1;
                    thread::sleep(REREAD_AFTER);
                }
                result => break result?,
            }
        };
        for s in &mut sessions {
            if let Some(id) = &s.id {
                s.title = self.titles.title(&transcript(&home, &s.cwd, id), id);
            }
        }
        self.titles.keep(&sessions);
        Ok(sessions)
    }

    /// Opens a pidfd for each live session, so its exit wakes the watch.
    fn follow(&mut self, sessions: &[Session]) {
        self.pidfds = sessions
            .iter()
            .filter_map(|s| {
                let fd = pidfd_open(s.pid);
                if fd.is_none() {
                    self.reread = true;
                }
                fd
            })
            .collect();
    }
}

impl Gauge for Claude {
    fn next(&mut self) -> Reading {
        if self.watching {
            if self.inotify.is_some() {
                self.wait();
            } else if let Err(why) = self.watch() {
                return assess(Err(why), &self.home());
            }
        }
        let sessions = self.read();
        if self.watching {
            self.follow(sessions.as_deref().unwrap_or_default());
        }
        assess(sessions, &self.home())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Status {
    // In the order the detail lists them.
    Waiting,
    Busy,
    Shell,
    Idle,
    /// No status yet: the file is written once before the first status.
    Starting,
}

impl Status {
    fn parse(status: &serde_json::Value) -> Result<Status, String> {
        match status {
            serde_json::Value::Null => Ok(Status::Starting),
            serde_json::Value::String(s) => match s.as_str() {
                "waiting" => Ok(Status::Waiting),
                "busy" => Ok(Status::Busy),
                "shell" => Ok(Status::Shell),
                "idle" => Ok(Status::Idle),
                other => Err(format!("status '{other}' is not one this gauge knows")),
            },
            other => Err(format!("status {other} is not a string")),
        }
    }

    /// As the bar counts it: working by the robot, the rest by the sleep
    /// sign, except that waiting on you gets the pause sign.
    fn icon(self) -> &'static str {
        match self {
            Status::Waiting => "󰏤",
            Status::Busy | Status::Shell => "󰚩",
            Status::Idle | Status::Starting => "󰒲",
        }
    }
}

#[derive(Debug, PartialEq)]
struct Session {
    pid: i32,
    status: Status,
    /// Why it waits: `permission prompt`, `input needed`.
    waiting_for: Option<String>,
    /// When the status was last written, in seconds since the epoch.
    since: Option<i64>,
    /// Claude Code's short name for it (`desktop-a1`), shown until it has a
    /// title.
    name: String,
    cwd: String,
    id: Option<String>,
    /// From the transcript, filled in after parsing.
    title: Option<String>,
}

/// One session file. `Ok(None)` for a file to leave out: its process is gone
/// (`alive` is asked with the pid and start time), or it is Claude Code's
/// supervisor rather than a session. Liveness is settled before anything
/// else, so a leftover file in an older format is not an error.
fn parse(
    text: &str,
    alive: impl FnOnce(i32, Option<u64>) -> bool,
) -> Result<Option<Session>, String> {
    let v: serde_json::Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
    let pid = v["pid"]
        .as_i64()
        .and_then(|p| i32::try_from(p).ok())
        .ok_or("no pid")?;
    let proc_start = v["procStart"].as_str().and_then(|t| t.parse().ok());
    if !alive(pid, proc_start) || v["kind"] == "daemon" {
        return Ok(None);
    }
    let text = |key: &str| v[key].as_str().map(str::to_owned);
    let cwd = text("cwd").unwrap_or_else(|| "?".to_owned());
    Ok(Some(Session {
        pid,
        status: Status::parse(&v["status"])?,
        waiting_for: text("waitingFor"),
        since: v["statusUpdatedAt"].as_i64().map(|ms| ms / 1000),
        name: text("name").unwrap_or_else(|| cwd.rsplit('/').next().unwrap_or("?").to_owned()),
        cwd,
        id: text("sessionId"),
        title: None,
    }))
}

/// Where Claude Code keeps a session's transcript: under its directory with
/// every character but letters and digits turned into `-`.
fn transcript(home: &str, cwd: &str, id: &str) -> PathBuf {
    let project: String = cwd
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    Path::new(home).join(format!(".claude/projects/{project}/{id}.jsonl"))
}

/// How far each live session's transcript has been read, and the title found
/// so far, by session id.
#[derive(Default)]
struct Titles(HashMap<String, Read>);

#[derive(Default)]
struct Read {
    bytes: u64,
    title: Option<String>,
}

impl Titles {
    /// The latest title in the transcript, reading only what was appended
    /// since the last call. A title is an extra: a transcript that cannot be
    /// read leaves the session untitled, not the reading unknown.
    fn title(&mut self, path: &Path, id: &str) -> Option<String> {
        let read = self.0.entry(id.to_owned()).or_default();
        if let Ok(mut file) = fs::File::open(path) {
            if file.metadata().is_ok_and(|m| m.len() < read.bytes) {
                // Rewritten from the start.
                *read = Read::default();
            }
            if file.seek(SeekFrom::Start(read.bytes)).is_ok() {
                if let Ok((bytes, title)) = scan(BufReader::new(file)) {
                    read.bytes += bytes;
                    read.title = title.or(read.title.take());
                }
            }
        }
        read.title.clone()
    }

    /// Forgets the sessions that are gone.
    fn keep(&mut self, sessions: &[Session]) {
        self.0
            .retain(|id, _| sessions.iter().any(|s| s.id.as_deref() == Some(id)));
    }
}

/// The bytes in whole lines, and the last title among them. A line still
/// being written has no newline yet; it is left for the next read.
fn scan(mut reader: impl BufRead) -> io::Result<(u64, Option<String>)> {
    let (mut bytes, mut title) = (0, None);
    let mut line = Vec::new();
    loop {
        line.clear();
        let n = reader.read_until(b'\n', &mut line)?;
        if line.last() != Some(&b'\n') {
            return Ok((bytes, title));
        }
        bytes += n as u64;
        title = title_in(&line).or(title);
    }
}

/// The title a transcript line sets, if it is a title record: `ai-title` for
/// the one Claude Code chose, `custom-title` for one given with `/rename`.
/// Matched on the line's start, so a title quoted in a message is not one.
fn title_in(line: &[u8]) -> Option<String> {
    let key = if line.starts_with(br#"{"type":"ai-title""#) {
        "aiTitle"
    } else if line.starts_with(br#"{"type":"custom-title""#) {
        "customTitle"
    } else {
        return None;
    };
    let v: serde_json::Value = serde_json::from_slice(line).ok()?;
    v[key].as_str().map(str::to_owned)
}

/// The title, cut to `TITLE_WIDTH` characters, or the short name until there
/// is one.
fn label(s: &Session) -> String {
    let Some(title) = &s.title else {
        return s.name.clone();
    };
    if title.chars().count() <= TITLE_WIDTH {
        return title.clone();
    }
    let cut: String = title.chars().take(TITLE_WIDTH - 1).collect();
    format!("{}…", cut.trim_end())
}

/// The process's start time in clock ticks since boot: field 22 of
/// `/proc/<pid>/stat`. The name in field 2 may hold spaces and parentheses,
/// so fields are counted from the last `)`.
fn start_time(stat: &str) -> Option<u64> {
    let (_, after_name) = stat.rsplit_once(')')?;
    after_name.split_whitespace().nth(22 - 3)?.parse().ok()
}

fn alive(pid: i32, proc_start: Option<u64>) -> bool {
    match fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => proc_start.is_none_or(|t| start_time(&stat) == Some(t)),
        Err(_) => false,
    }
}

fn read_sessions(dir: &Path, home: &str) -> Result<Vec<Session>, String> {
    let shown = |path: &Path| tilde(&path.to_string_lossy(), home);
    let entries = fs::read_dir(dir).map_err(|e| format!("{}: {e}", shown(dir)))?;
    let mut sessions = Vec::new();
    for entry in entries {
        let path = entry.map_err(|e| format!("{}: {e}", shown(dir)))?.path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            // The session ended between listing and reading.
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(format!("{}: {e}", shown(&path))),
        };
        if let Some(session) =
            parse(&text, alive).map_err(|why| format!("{}: {why}", shown(&path)))?
        {
            sessions.push(session);
        }
    }
    Ok(sessions)
}

fn watch(dir: &Path) -> Result<OwnedFd, String> {
    // A new file is written in place and is still empty at IN_CREATE; it is
    // read on IN_CLOSE_WRITE instead.
    let mask = libc::IN_CLOSE_WRITE
        | libc::IN_MOVED_TO
        | libc::IN_MOVED_FROM
        | libc::IN_DELETE
        | libc::IN_DELETE_SELF
        | libc::IN_MOVE_SELF;
    inotify::watch(dir, mask).map_err(|e| format!("{}: {e}", dir.display()))
}

fn pidfd_open(pid: i32) -> Option<OwnedFd> {
    // SAFETY: pidfd_open takes a pid and flags; a non-negative result is a
    // new fd that nothing else owns.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    (fd >= 0).then(|| unsafe { OwnedFd::from_raw_fd(fd as i32) })
}

fn tilde(path: &str, home: &str) -> String {
    match path.strip_prefix(home) {
        Some(rest) if !home.is_empty() && (rest.is_empty() || rest.starts_with('/')) => {
            format!("~{rest}")
        }
        _ => path.to_owned(),
    }
}

fn assess(sessions: Result<Vec<Session>, String>, home: &str) -> Reading {
    let mut sessions = match sessions {
        Ok(s) => s,
        Err(why) => {
            return Reading {
                level: Level::Unknown,
                summary: summary("?", "?"),
                detail: vec![format!("Cannot measure: {why}")],
            };
        }
    };
    // By state, then newest first (a session with no time yet last), then by
    // what the row shows, so equal times never swap places between readings.
    sessions.sort_by_key(|s| (s.status, std::cmp::Reverse(s.since), label(s)));
    let count = |status: Status| sessions.iter().filter(|s| s.status == status).count();
    let (waiting, working) = (
        count(Status::Waiting),
        count(Status::Busy) + count(Status::Shell),
    );
    // An open session is something to see even when idle: it is waiting for
    // your next message, which is often what you do next.
    let level = if waiting > 0 {
        Level::Attention
    } else if sessions.is_empty() {
        Level::Quiet
    } else {
        Level::Normal
    };

    let detail = if sessions.is_empty() {
        vec!["No Claude Code sessions".to_owned()]
    } else {
        table(&sessions, home)
    };
    Reading {
        level,
        // Waiting and starting sessions count as idle: in all three, the
        // next move is yours.
        summary: summary(
            &working.to_string(),
            &(sessions.len() - working).to_string(),
        ),
        detail,
    }
}

/// A light table: column names, a rule as wide as the widest line, and a row
/// per session marked with the bar's icon for it. Why a session waits
/// trails its row; it is rare, so it gets no column.
fn table(sessions: &[Session], home: &str) -> Vec<String> {
    const NAMES: [&str; 3] = ["Session", "Directory", "Since"];
    let rows: Vec<(&str, [String; 3], &str)> = sessions
        .iter()
        .map(|s| {
            let since = s.since.map(clock).unwrap_or_default();
            (
                s.status.icon(),
                [label(s), tilde(&s.cwd, home), since],
                s.waiting_for.as_deref().unwrap_or_default(),
            )
        })
        .collect();
    let width = |i: usize| {
        rows.iter()
            .map(|(_, cells, _)| cells[i].chars().count())
            .chain([NAMES[i].len()])
            .max()
            .unwrap_or(0)
    };
    let (w0, w1, w2) = (width(0), width(1), width(2));
    let line = |icon: &str, cells: [&str; 3], why: &str| {
        format!(
            "{icon}  {:<w0$}  {:<w1$}  {:<w2$}  {why}",
            cells[0], cells[1], cells[2]
        )
        .trim_end()
        .to_owned()
    };
    let mut lines = vec![line(" ", NAMES, "")];
    lines.extend(
        rows.iter()
            .map(|(icon, [a, b, c], why)| line(icon, [a, b, c], why)),
    );
    let rule = "─".repeat(lines.iter().map(|l| l.chars().count()).max().unwrap_or(0));
    lines.insert(1, rule);
    lines
}

/// Working sessions by the robot, idle ones by the sleep sign. Each count is
/// left-aligned in 2 characters, so the summary does not change width and
/// the padding falls after a count, not between it and its icon. A space
/// follows the working count's pad, so `10` does not run into the sleep sign.
fn summary(working: &str, idle: &str) -> String {
    format!("󰚩 {working:<2} 󰒲 {idle:<2}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// This session's own file, while it was working.
    const SESSION: &str = include_str!("../../tests/fixtures/claude_session.json");
    const HOME: &str = "/home/nilo";
    const AT: i64 = 1_792_623_497;

    fn session(status: Status, name: &str, cwd: &str, since: i64) -> Session {
        Session {
            pid: 1,
            status,
            waiting_for: None,
            since: Some(since),
            name: name.to_owned(),
            cwd: cwd.to_owned(),
            id: None,
            title: None,
        }
    }

    fn titled(status: Status, title: &str, cwd: &str, since: i64) -> Session {
        Session {
            title: Some(title.to_owned()),
            ..session(status, "short-a1", cwd, since)
        }
    }

    fn live(_: i32, _: Option<u64>) -> bool {
        true
    }

    #[test]
    fn a_session_file_from_this_machine() {
        let mut asked = None;
        let parsed = parse(SESSION, |pid, start| {
            asked = Some((pid, start));
            true
        });
        assert_eq!(asked, Some((471_542, Some(3_404_602))));
        assert_eq!(
            parsed,
            Ok(Some(Session {
                pid: 471_542,
                status: Status::Busy,
                waiting_for: None,
                since: Some(AT),
                name: "desktop-a1".to_owned(),
                cwd: "/home/nilo/desktop".to_owned(),
                id: Some("59e934af-43c6-4726-a9ca-ea34b7f9483d".to_owned()),
                title: None,
            }))
        );
    }

    #[test]
    fn a_dead_sessions_file_is_left_out_whatever_it_says() {
        assert_eq!(parse(SESSION, |_, _| false), Ok(None));
        assert_eq!(
            parse(r#"{"pid":7,"status":"from-the-future"}"#, |_, _| false),
            Ok(None)
        );
    }

    #[test]
    fn statuses_seen_and_the_moment_before_the_first() {
        let with = |status: &str| {
            parse(&format!(r#"{{"pid":7,"status":{status}}}"#), live).map(|s| s.map(|s| s.status))
        };
        assert_eq!(with(r#""waiting""#), Ok(Some(Status::Waiting)));
        assert_eq!(with(r#""busy""#), Ok(Some(Status::Busy)));
        assert_eq!(with(r#""shell""#), Ok(Some(Status::Shell)));
        assert_eq!(with(r#""idle""#), Ok(Some(Status::Idle)));
        assert_eq!(with("null"), Ok(Some(Status::Starting)));
        assert_eq!(
            parse(r#"{"pid":7}"#, live).map(|s| s.map(|s| s.status)),
            Ok(Some(Status::Starting))
        );
    }

    #[test]
    fn what_it_does_not_know_is_an_error_not_a_guess() {
        assert!(parse("", live).is_err(), "half-written");
        assert!(parse(r#"{"status":"busy"}"#, live).is_err(), "no pid");
        assert!(parse(r#"{"pid":7,"status":"napping"}"#, live).is_err());
        assert!(parse(r#"{"pid":7,"status":3}"#, live).is_err());
    }

    #[test]
    fn the_supervisor_is_not_a_session() {
        assert_eq!(
            parse(r#"{"pid":7,"kind":"daemon","status":"idle"}"#, live),
            Ok(None)
        );
        assert!(
            parse(r#"{"pid":7,"kind":"bg","status":"idle"}"#, live)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn waiting_for_and_a_name_from_the_directory_when_there_is_none() {
        let s = parse(
            r#"{"pid":7,"status":"waiting","waitingFor":"permission prompt","cwd":"/home/nilo/projects/telltale"}"#,
            live,
        )
        .unwrap()
        .unwrap();
        assert_eq!(s.waiting_for.as_deref(), Some("permission prompt"));
        assert_eq!(s.name, "telltale");
        assert_eq!(s.since, None);
    }

    #[test]
    fn start_time_counts_fields_after_the_name() {
        let stat = "479432 (claude) S 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 3479768 20 21";
        assert_eq!(start_time(stat), Some(3_479_768));
        let odd = "12 (a) b (c) S 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 99 20";
        assert_eq!(start_time(odd), Some(99));
        assert_eq!(start_time("12 (short) S 1"), None);
    }

    #[test]
    fn this_process_is_alive_and_a_reused_pid_is_not() {
        let me = std::process::id() as i32;
        let stat = fs::read_to_string("/proc/self/stat").unwrap();
        let started = start_time(&stat).unwrap();
        assert!(alive(me, Some(started)));
        assert!(alive(me, None));
        assert!(!alive(me, Some(started + 1)));
    }

    #[test]
    fn a_transcript_lives_under_its_directory_with_symbols_as_dashes() {
        assert_eq!(
            transcript(HOME, "/tmp/claude-1000/-home-nilo-desktop/x_y.z", "abc"),
            Path::new(
                "/home/nilo/.claude/projects/-tmp-claude-1000--home-nilo-desktop-x-y-z/abc.jsonl"
            )
        );
    }

    #[test]
    fn the_last_title_record_wins_and_quoted_ones_do_not_count() {
        let lines = concat!(
            r#"{"type":"user","message":{"content":"{\"type\":\"ai-title\",\"aiTitle\":\"Quoted\"}"}}"#,
            "\n",
            r#"{"type":"ai-title","aiTitle":"First","sessionId":"s"}"#,
            "\n",
            r#"{"type":"assistant","message":{}}"#,
            "\n",
            r#"{"type":"custom-title","customTitle":"Renamed","sessionId":"s"}"#,
            "\n",
        );
        assert_eq!(
            scan(lines.as_bytes()).unwrap(),
            (lines.len() as u64, Some("Renamed".to_owned()))
        );
        assert_eq!(scan(&b"{\"type\":\"user\"}\n"[..]).unwrap(), (16, None));
    }

    #[test]
    fn a_line_still_being_written_is_left_for_next_time() {
        let done = "{\"type\":\"ai-title\",\"aiTitle\":\"Done\"}\n";
        let text = format!("{done}{{\"type\":\"ai-title\",\"aiTitle\":\"Half");
        assert_eq!(
            scan(text.as_bytes()).unwrap(),
            (done.len() as u64, Some("Done".to_owned()))
        );
    }

    #[test]
    fn titles_read_only_what_was_appended() {
        use std::io::Write;
        let dir = std::env::temp_dir().join(format!("telltale-titles-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.jsonl");
        let line = |title: &str| format!("{{\"type\":\"ai-title\",\"aiTitle\":\"{title}\"}}\n");
        fs::write(&path, line("One")).unwrap();
        let mut titles = Titles::default();
        assert_eq!(titles.title(&path, "s").as_deref(), Some("One"));
        let after_one = titles.0["s"].bytes;
        assert_eq!(after_one, line("One").len() as u64);

        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"{\"type\":\"user\"}\n").unwrap();
        assert_eq!(titles.title(&path, "s").as_deref(), Some("One"), "kept");
        file.write_all(line("Two").as_bytes()).unwrap();
        assert_eq!(titles.title(&path, "s").as_deref(), Some("Two"));

        fs::write(&path, line("Anew")).unwrap();
        assert_eq!(
            titles.title(&path, "s").as_deref(),
            Some("Anew"),
            "rewritten"
        );
        assert_eq!(titles.title(&dir.join("missing"), "t"), None);

        titles.keep(&[Session {
            id: Some("s".to_owned()),
            ..session(Status::Idle, "a", "/tmp", AT)
        }]);
        assert_eq!(titles.0.keys().collect::<Vec<_>>(), ["s"]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_label_is_the_title_cut_or_the_short_name() {
        assert_eq!(
            label(&session(Status::Idle, "desktop-a1", "/", AT)),
            "desktop-a1"
        );
        let exact = "x".repeat(TITLE_WIDTH);
        assert_eq!(label(&titled(Status::Idle, &exact, "/", AT)), exact);
        let long = format!("{} tail", "x".repeat(TITLE_WIDTH));
        let cut = label(&titled(Status::Idle, &long, "/", AT));
        assert_eq!(cut.chars().count(), TITLE_WIDTH);
        assert!(cut.ends_with("x…"));
    }

    #[test]
    fn tilde_only_for_the_home_directory_itself() {
        assert_eq!(tilde("/home/nilo/desktop", HOME), "~/desktop");
        assert_eq!(tilde("/home/nilo", HOME), "~");
        assert_eq!(tilde("/home/nilothing", HOME), "/home/nilothing");
        assert_eq!(tilde("/tmp", ""), "/tmp");
    }

    #[test]
    fn no_sessions_is_quiet() {
        let reading = assess(Ok(vec![]), HOME);
        assert_eq!(reading.level, Level::Quiet);
        assert_eq!(reading.summary, "󰚩 0  󰒲 0 ");
        assert_eq!(reading.detail, ["No Claude Code sessions"]);
    }

    #[test]
    fn open_idle_sessions_are_normal_and_counted_as_idle() {
        let reading = assess(
            Ok(vec![
                session(Status::Idle, "a", "/tmp", AT),
                session(Status::Starting, "b", "/tmp", AT),
            ]),
            HOME,
        );
        assert_eq!(reading.level, Level::Normal);
        assert_eq!(reading.summary, "󰚩 0  󰒲 2 ");
    }

    #[test]
    fn working_and_idle_are_counted_apart() {
        let reading = assess(
            Ok(vec![
                session(Status::Idle, "a", "/tmp", AT),
                session(Status::Busy, "b", "/tmp", AT),
                session(Status::Shell, "c", "/tmp", AT),
            ]),
            HOME,
        );
        assert_eq!(reading.level, Level::Normal);
        assert_eq!(reading.summary, "󰚩 2  󰒲 1 ");
    }

    #[test]
    fn waiting_asks_for_attention_and_comes_first_newest_first() {
        let mut asking = titled(
            Status::Waiting,
            "Claude Code in Waybar",
            "/home/nilo/projects/telltale",
            AT,
        );
        asking.waiting_for = Some("permission prompt".to_owned());
        let reading = assess(
            Ok(vec![
                titled(
                    Status::Idle,
                    "Dunst notification background color",
                    "/home/nilo/desktop",
                    AT - 600,
                ),
                titled(
                    Status::Busy,
                    "Upstream sync for the local fixes to the layout code",
                    "/home/nilo/projects/mango",
                    AT + 60,
                ),
                asking,
                session(Status::Waiting, "notes", "/srv/notes", AT - 60),
            ]),
            HOME,
        );
        assert_eq!(reading.level, Level::Attention);
        assert_eq!(reading.summary, "󰚩 1  󰒲 3 ", "the waiting are idle too");
        let since = |t| format!("{:<w$}", clock(t), w = clock(AT).len());
        let rows = [
            format!(
                "󰏤  Claude Code in Waybar                     ~/projects/telltale  {}  permission prompt",
                since(AT)
            ),
            format!(
                "󰏤  notes                                     /srv/notes           {}",
                since(AT - 60)
            ),
            format!(
                "󰚩  Upstream sync for the local fixes to th…  ~/projects/mango     {}",
                since(AT + 60)
            ),
            format!(
                "󰒲  Dunst notification background color       ~/desktop            {}",
                since(AT - 600)
            ),
        ];
        let rows = rows.map(|r| r.trim_end().to_owned());
        assert_eq!(
            reading.detail[0],
            "   Session                                   Directory            Since"
        );
        assert_eq!(
            reading.detail[1],
            "─".repeat(rows[0].chars().count()),
            "as wide as the widest line"
        );
        assert_eq!(reading.detail[2..], rows);
    }

    #[test]
    fn equal_times_go_by_title_and_no_time_goes_last() {
        let mut untimed = session(Status::Idle, "a-first-by-name", "/", AT);
        untimed.since = None;
        let reading = assess(
            Ok(vec![
                untimed,
                titled(Status::Idle, "Beta", "/", AT),
                titled(Status::Idle, "Alpha", "/", AT),
                titled(Status::Idle, "Older", "/", AT - 60),
            ]),
            HOME,
        );
        let titles: Vec<&str> = reading.detail[2..]
            .iter()
            .map(|row| row.split("  ").nth(1).unwrap())
            .collect();
        assert_eq!(titles, ["Alpha", "Beta", "Older", "a-first-by-name"]);
    }

    #[test]
    fn columns_are_never_narrower_than_their_names() {
        let mut short = session(Status::Starting, "a", "/", AT);
        short.since = None;
        let reading = assess(Ok(vec![short]), HOME);
        assert_eq!(
            reading.detail,
            [
                "   Session  Directory  Since",
                "────────────────────────────",
                "󰒲  a        /",
            ]
        );
    }

    #[test]
    fn cannot_read_is_unknown_says_why_and_keeps_its_width() {
        let reading = assess(
            Err(
                "~/.claude/sessions/9.json: status 'napping' is not one this gauge knows"
                    .to_owned(),
            ),
            HOME,
        );
        assert_eq!(reading.level, Level::Unknown);
        assert_eq!(reading.summary, "󰚩 ?  󰒲 ? ");
        assert_eq!(
            summary("10", "2"),
            "󰚩 10 󰒲 2 ",
            "a 2-digit count keeps its space"
        );
        assert_eq!(
            reading.detail,
            [
                "Cannot measure: ~/.claude/sessions/9.json: status 'napping' is not one this gauge knows"
            ]
        );
    }
}
