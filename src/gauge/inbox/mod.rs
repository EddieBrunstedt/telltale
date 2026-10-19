//! Messages in an IMAP inbox you keep empty: every message in it is a todo,
//! so this counts messages, not unread ones.
//!
//! One connection for as long as the gauge runs. With IDLE (RFC 2177) the
//! server says when the mailbox changes; without it, the mailbox is checked
//! every minute. A bridge (Proton Mail Bridge) relays a remote server with
//! its own delay, so "at once" means as soon as the bridge knows.

mod imap;

use std::env;
use std::io::{self, Read, Write};
use std::net::{IpAddr, TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{
    ClientConfig, ClientConnection, DigitallySignedStruct, RootCertStore, SignatureScheme,
    StreamOwned,
};

use crate::gauge::Gauge;
use crate::gauge::clock::{clock, now};
use crate::reading::{Level, Reading};
use crate::run::{Extra, Given};
use imap::{COMMAND_TIMEOUT, Session, Transport};

/// RFC 2177: re-issue IDLE before 29 minutes, or the server may drop us.
const IDLE_FOR: Duration = Duration::from_secs(25 * 60);
/// Without IDLE, how often to look.
const POLL_EVERY: Duration = Duration::from_secs(60);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const RETRY_FIRST: Duration = Duration::from_secs(5);
const RETRY_AT_MOST: Duration = Duration::from_secs(60);

pub const OPTIONS: Extra = Extra {
    usage: "--server URL [--mailbox NAME] --user-env VAR --password-env VAR [--insecure]",
    help: "

  --server URL         imap://HOST:PORT (STARTTLS) or imaps://HOST:PORT (TLS)
  --mailbox NAME       the mailbox to count (default INBOX)
  --user-env VAR       read the user name from environment variable VAR
  --password-env VAR   read the password from environment variable VAR
  --insecure           do not verify the server's certificate; only for a
                       server on a loopback address, such as a local bridge
                       with a self-signed certificate",
    flags: &[
        ("--server", true),
        ("--mailbox", true),
        ("--user-env", true),
        ("--password-env", true),
        ("--insecure", false),
    ],
};

#[derive(Clone, Copy, Debug, PartialEq)]
enum Security {
    StartTls,
    Tls,
}

#[derive(Debug, PartialEq)]
struct Server {
    security: Security,
    host: String,
    port: u16,
}

impl Server {
    fn parse(url: &str) -> Result<Server, String> {
        let (security, rest, default_port) = if let Some(rest) = url.strip_prefix("imap://") {
            (Security::StartTls, rest, 143)
        } else if let Some(rest) = url.strip_prefix("imaps://") {
            (Security::Tls, rest, 993)
        } else {
            return Err(format!(
                "--server must start with imap:// or imaps://, not '{url}'"
            ));
        };
        let rest = rest.trim_end_matches('/');
        // [::1]:1143, host:1143, or a bare host.
        let (host, port) = match rest.strip_prefix('[') {
            Some(v6) => {
                let (host, after) = v6
                    .split_once(']')
                    .ok_or_else(|| format!("unclosed '[' in '{url}'"))?;
                (host, after.strip_prefix(':'))
            }
            None => match rest.rsplit_once(':') {
                Some((host, port)) => (host, Some(port)),
                None => (rest, None),
            },
        };
        if host.is_empty() {
            return Err(format!("no host in '{url}'"));
        }
        let port = match port {
            Some(p) => p
                .parse()
                .map_err(|_| format!("bad port '{p}' in '{url}'"))?,
            None => default_port,
        };
        Ok(Server {
            security,
            host: host.to_owned(),
            port,
        })
    }

    fn is_loopback(&self) -> bool {
        self.host == "localhost" || self.host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
    }

    fn url(&self) -> String {
        let scheme = match self.security {
            Security::StartTls => "imap",
            Security::Tls => "imaps",
        };
        if self.host.contains(':') {
            format!("{scheme}://[{}]:{}", self.host, self.port)
        } else {
            format!("{scheme}://{}:{}", self.host, self.port)
        }
    }
}

pub struct Inbox {
    server: Server,
    mailbox: String,
    user_env: String,
    password_env: String,
    tls: Arc<ClientConfig>,
    session: Option<Session<Conn>>,
    /// Whether the server pushes changes; if not, it is polled.
    pushes: bool,
    connected_at: i64,
    last_seen: Option<(u32, i64)>,
    /// When reaching the mailbox started failing, and the latest reason.
    failing: Option<(i64, String)>,
    retry_in: Duration,
}

impl Inbox {
    pub fn from_options(given: &Given) -> Result<Inbox, String> {
        let server = Server::parse(given.required("--server")?)?;
        let insecure = given.has("--insecure");
        if insecure && !server.is_loopback() {
            return Err(format!(
                "--insecure is only allowed for a loopback server, not '{}'",
                server.host
            ));
        }
        Ok(Inbox {
            tls: tls_config(insecure)?,
            server,
            mailbox: given.value("--mailbox").unwrap_or("INBOX").to_owned(),
            user_env: given.required("--user-env")?.to_owned(),
            password_env: given.required("--password-env")?.to_owned(),
            session: None,
            pushes: false,
            connected_at: 0,
            last_seen: None,
            failing: None,
            retry_in: RETRY_FIRST,
        })
    }

    fn connect(&mut self) -> Result<(), String> {
        let secret = |var: &str| env::var(var).map_err(|_| format!("${var} is not set"));
        let (user, password) = (secret(&self.user_env)?, secret(&self.password_env)?);
        let url = self.server.url();
        let failed = |e: &dyn std::fmt::Display| format!("{url}: {e}");

        let tcp = dial(&self.server.host, self.server.port).map_err(|e| failed(&e))?;
        let mut session = match self.server.security {
            Security::Tls => {
                let mut s = Session::new(self.wrap(tcp).map_err(|e| failed(&e))?);
                s.greeting().map_err(|e| failed(&e))?;
                s
            }
            Security::StartTls => {
                let mut s = Session::new(Conn::Plain(tcp));
                s.greeting().map_err(|e| failed(&e))?;
                s.starttls().map_err(|e| failed(&e))?;
                let Conn::Plain(tcp) = s.into_transport().map_err(|e| failed(&e))? else {
                    unreachable!("STARTTLS starts from plain text");
                };
                // No greeting after STARTTLS; the TLS handshake happens on
                // the first command.
                Session::new(self.wrap(tcp).map_err(|e| failed(&e))?)
            }
        };
        session
            .login(&user, &password)
            .map_err(|e| failed(&format!("login: {e}")))?;
        self.pushes = session
            .capabilities()
            .map_err(|e| failed(&e))?
            .iter()
            .any(|c| c == "IDLE");
        self.connected_at = now();
        self.session = Some(session);
        Ok(())
    }

    fn wrap(&self, tcp: TcpStream) -> Result<Conn, String> {
        let name = ServerName::try_from(self.server.host.clone())
            .map_err(|e| format!("'{}' as a TLS name: {e}", self.server.host))?;
        let tls = ClientConnection::new(self.tls.clone(), name).map_err(|e| e.to_string())?;
        Ok(Conn::Tls(Box::new(StreamOwned::new(tls, tcp))))
    }

    /// Counts in the mailbox now.
    fn count(&mut self) -> Result<Counts, String> {
        let session = self.session.as_mut().expect("connected");
        let failed = |e: imap::Error| format!("{}: {e}", self.server.url());
        let total = session.examine(&self.mailbox).map_err(failed)?;
        let unseen = session.unseen().map_err(failed)?;
        let first = if total > 0 {
            Some(session.first_arrival().map_err(failed)?)
        } else {
            None
        };
        Ok(Counts {
            total,
            unseen,
            first,
        })
    }

    /// Blocks until the mailbox may have changed. Ok(false): it did not, but
    /// the IDLE had to be renewed.
    fn wait(&mut self) -> Result<bool, String> {
        if !self.pushes {
            thread::sleep(POLL_EVERY);
            return Ok(true);
        }
        let session = self.session.as_mut().expect("connected");
        session
            .idle(IDLE_FOR)
            .map_err(|e| format!("{}: {e}", self.server.url()))
    }

    fn read(&mut self) -> Reading {
        match self.count() {
            Ok(counts) => {
                self.last_seen = Some((counts.total, now()));
                assess(&counts, &self.situation(), now())
            }
            Err(why) => {
                self.session = None;
                self.unreachable(why)
            }
        }
    }

    fn unreachable(&mut self, why: String) -> Reading {
        let since = self.failing.as_ref().map_or_else(now, |(since, _)| *since);
        self.failing = Some((since, why.clone()));
        unknown(&why, since, self.last_seen)
    }

    fn situation(&self) -> Situation {
        Situation {
            mailbox: self.mailbox.clone(),
            url: self.server.url(),
            since: self.connected_at,
            pushes: self.pushes,
        }
    }
}

impl Gauge for Inbox {
    fn next(&mut self) -> Reading {
        loop {
            if self.session.is_none() {
                if self.failing.is_some() {
                    thread::sleep(self.retry_in);
                    self.retry_in = (self.retry_in * 2).min(RETRY_AT_MOST);
                }
                if let Err(why) = self.connect() {
                    return self.unreachable(why);
                }
                self.failing = None;
                self.retry_in = RETRY_FIRST;
                return self.read();
            }
            match self.wait() {
                Ok(true) => return self.read(),
                Ok(false) => {} // renewed the IDLE; nothing to report
                Err(why) => {
                    // Reconnect after the first retry delay; only if that
                    // fails too does the reading turn unknown.
                    self.session = None;
                    self.failing = Some((now(), why));
                }
            }
        }
    }
}

impl Drop for Inbox {
    fn drop(&mut self) {
        if let Some(session) = self.session.as_mut() {
            session.logout();
        }
    }
}

#[derive(Debug, PartialEq)]
struct Counts {
    total: u32,
    unseen: u32,
    /// When the earliest message arrived, as the server writes it.
    first: Option<String>,
}

/// How the counts were obtained, for the detail.
struct Situation {
    mailbox: String,
    url: String,
    since: i64,
    pushes: bool,
}

fn assess(counts: &Counts, situation: &Situation, now: i64) -> Reading {
    let mut detail = vec![format!(
        "{} in {}, {} unread",
        counts.total, situation.mailbox, counts.unseen
    )];
    if let Some(first) = &counts.first {
        detail.push(match parse_internaldate(first) {
            Some(at) => {
                let date = first.trim().split(' ').next().unwrap_or(first);
                format!("Oldest arrived {} ({date})", days_ago(now - at))
            }
            None => format!("Oldest arrived {first}"),
        });
    }
    detail.push(format!(
        "Connected to {} since {}; {}",
        situation.url,
        clock(situation.since),
        if situation.pushes {
            "the server pushes changes (IDLE)"
        } else {
            "checked every 60 s (the server has no IDLE)"
        }
    ));
    Reading {
        level: if counts.total == 0 {
            Level::Quiet
        } else {
            Level::Attention
        },
        summary: summary(&format!("{:>3}", counts.total)),
        detail,
    }
}

fn unknown(why: &str, since: i64, last_seen: Option<(u32, i64)>) -> Reading {
    let mut detail = vec![format!("Cannot measure since {}: {why}", clock(since))];
    if let Some((total, at)) = last_seen {
        detail.push(format!("Last seen: {total} at {}", clock(at)));
    }
    Reading {
        level: Level::Unknown,
        summary: summary("  ?"),
        detail,
    }
}

/// The count is 3 characters (a 4-digit inbox widens it rather than being
/// cut), so the summary does not change width.
fn summary(count: &str) -> String {
    format!("󰇮 {count}")
}

fn days_ago(secs: i64) -> String {
    match secs.div_euclid(86_400).max(0) {
        0 => "today".to_owned(),
        1 => "1 day ago".to_owned(),
        n => format!("{n} days ago"),
    }
}

/// Seconds since the epoch from an IMAP date-time, `17-Oct-2026 09:12:33
/// +0200` (the day may be space-padded).
fn parse_internaldate(s: &str) -> Option<i64> {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let mut parts = s.split_whitespace();
    let (date, time, zone) = (parts.next()?, parts.next()?, parts.next()?);
    let mut d = date.split('-');
    let day: i64 = d.next()?.parse().ok()?;
    let month = d.next()?;
    let month = MONTHS.iter().position(|m| *m == month)? as i64 + 1;
    let year: i64 = d.next()?.parse().ok()?;
    let mut t = time.split(':').map(|x| x.parse::<i64>().ok());
    let (h, m, sec) = (t.next()??, t.next()??, t.next()??);
    let sign = match zone.as_bytes().first()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let zh: i64 = zone.get(1..3)?.parse().ok()?;
    let zm: i64 = zone.get(3..5)?.parse().ok()?;
    let days = days_from_civil(year, month, day);
    Some(days * 86_400 + h * 3_600 + m * 60 + sec - sign * (zh * 3_600 + zm * 60))
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's
/// `days_from_civil`).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn dial(host: &str, port: u16) -> io::Result<TcpStream> {
    let mut last = io::Error::new(io::ErrorKind::NotFound, format!("{host} has no address"));
    for addr in (host, port).to_socket_addrs()? {
        match TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) {
            Ok(tcp) => {
                tcp.set_read_timeout(Some(COMMAND_TIMEOUT))?;
                tcp.set_write_timeout(Some(COMMAND_TIMEOUT))?;
                return Ok(tcp);
            }
            Err(e) => last = e,
        }
    }
    Err(last)
}

enum Conn {
    Plain(TcpStream),
    Tls(Box<StreamOwned<ClientConnection, TcpStream>>),
}

impl Read for Conn {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Conn::Plain(s) => s.read(buf),
            Conn::Tls(s) => s.read(buf),
        }
    }
}

impl Write for Conn {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Conn::Plain(s) => s.write(buf),
            Conn::Tls(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Conn::Plain(s) => s.flush(),
            Conn::Tls(s) => s.flush(),
        }
    }
}

impl Transport for Conn {
    fn set_read_timeout(&self, timeout: Duration) -> io::Result<()> {
        match self {
            Conn::Plain(s) => s.set_read_timeout(Some(timeout)),
            Conn::Tls(s) => s.sock.set_read_timeout(Some(timeout)),
        }
    }
}

fn tls_config(insecure: bool) -> Result<Arc<ClientConfig>, String> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?;
    let config = if insecure {
        builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AnyCertificate(provider)))
            .with_no_client_auth()
    } else {
        let mut roots = RootCertStore::empty();
        roots.add_parsable_certificates(rustls_native_certs::load_native_certs().certs);
        if roots.is_empty() {
            return Err("no system root certificates found to verify the server".to_owned());
        }
        builder.with_root_certificates(roots).with_no_client_auth()
    };
    Ok(Arc::new(config))
}

/// For `--insecure`: any certificate is accepted, but the handshake is still
/// checked against it, so the connection is encrypted to whoever holds that
/// certificate's key. Only ever used for loopback servers.
#[derive(Debug)]
struct AnyCertificate(Arc<CryptoProvider>);

impl ServerCertVerifier for AnyCertificate {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn situation(pushes: bool) -> Situation {
        Situation {
            mailbox: "INBOX".to_owned(),
            url: "imap://127.0.0.1:1143".to_owned(),
            since: 0,
            pushes,
        }
    }

    #[test]
    fn server_urls() {
        let server = |url| Server::parse(url);
        assert_eq!(
            server("imap://127.0.0.1:1143"),
            Ok(Server {
                security: Security::StartTls,
                host: "127.0.0.1".to_owned(),
                port: 1143
            })
        );
        assert_eq!(server("imaps://mail.example.org").unwrap().port, 993);
        assert_eq!(server("imap://mail.example.org/").unwrap().port, 143);
        let v6 = server("imap://[::1]:1143").unwrap();
        assert_eq!((v6.host.as_str(), v6.port), ("::1", 1143));
        assert_eq!(v6.url(), "imap://[::1]:1143");
        assert!(server("pop3://x").is_err());
        assert!(server("imap://:1143").is_err());
        assert!(server("imap://host:port").is_err());
    }

    #[test]
    fn insecure_only_for_loopback() {
        let loopback = |url| Server::parse(url).unwrap().is_loopback();
        assert!(loopback("imap://127.0.0.1:1143"));
        assert!(loopback("imap://[::1]:1143"));
        assert!(loopback("imap://localhost:1143"));
        assert!(!loopback("imap://192.168.1.17:1143"));
        assert!(!loopback("imaps://imap.example.org"));
    }

    #[test]
    fn internaldates() {
        assert_eq!(parse_internaldate("01-Jan-1970 00:00:00 +0000"), Some(0));
        assert_eq!(
            parse_internaldate("17-Oct-2026 09:12:33 +0200"),
            Some(1_792_221_153)
        );
        assert_eq!(
            parse_internaldate(" 7-Feb-2024 23:59:59 -0500"),
            Some(1_707_368_399)
        );
        assert_eq!(parse_internaldate("17-Okt-2026 09:12:33 +0200"), None);
        assert_eq!(parse_internaldate("garbage"), None);
    }

    #[test]
    fn an_empty_inbox_is_quiet_and_anything_in_it_asks_for_attention() {
        let empty = Counts {
            total: 0,
            unseen: 0,
            first: None,
        };
        let reading = assess(&empty, &situation(true), 0);
        assert_eq!(reading.level, Level::Quiet);
        assert_eq!(reading.summary, summary("  0"));

        let one = Counts {
            total: 1,
            unseen: 0,
            first: Some("17-Oct-2026 09:12:33 +0200".to_owned()),
        };
        assert_eq!(assess(&one, &situation(true), 0).level, Level::Attention);
    }

    #[test]
    fn detail_counts_both_dates_the_oldest_and_says_how_it_knows() {
        let counts = Counts {
            total: 257,
            unseen: 203,
            first: Some("17-Oct-2026 09:12:33 +0200".to_owned()),
        };
        let two_days_later = 1_792_221_153 + 2 * 86_400 + 3_600;
        let detail = assess(&counts, &situation(true), two_days_later).detail;
        assert_eq!(detail[0], "257 in INBOX, 203 unread");
        assert_eq!(detail[1], "Oldest arrived 2 days ago (17-Oct-2026)");
        assert!(detail[2].starts_with("Connected to imap://127.0.0.1:1143 since "));
        assert!(detail[2].ends_with("; the server pushes changes (IDLE)"));
        let polled = assess(&counts, &situation(false), two_days_later).detail;
        assert!(polled[2].ends_with("checked every 60 s (the server has no IDLE)"));
    }

    #[test]
    fn unreachable_is_unknown_says_why_and_what_was_last_seen() {
        let reading = unknown("imap://127.0.0.1:1143: Connection refused", 0, Some((3, 0)));
        assert_eq!(reading.level, Level::Unknown);
        assert!(reading.detail[0].ends_with(": imap://127.0.0.1:1143: Connection refused"));
        assert!(reading.detail[1].starts_with("Last seen: 3 at "));
        assert_eq!(
            reading.summary.chars().count(),
            summary("257").chars().count()
        );
    }

    #[test]
    fn ages_in_days() {
        assert_eq!(days_ago(3_600), "today");
        assert_eq!(days_ago(86_400), "1 day ago");
        assert_eq!(days_ago(41 * 86_400 + 5), "41 days ago");
        assert_eq!(days_ago(-60), "today", "a clock slightly behind the server");
    }
}
