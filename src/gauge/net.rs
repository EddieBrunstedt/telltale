//! Traffic on the interface that carries the default route.
//!
//! The physical interface, not a VPN tunnel: it carries everything, so the
//! number stays continuous when a VPN toggles, and it counts what is actually
//! on the wire. Tunnels are listed in the detail as payload.

use std::fs;
use std::thread;
use std::time::{Duration, Instant};

use crate::gauge::Gauge;
use crate::reading::{Level, Reading};

const WINDOW: Duration = Duration::from_secs(1);
/// Windows between looking up the default route, tunnels and link speed.
const RESOLVE_EVERY: u32 = 10;
/// Bytes/s; below this in both directions the link is quiet.
const QUIET_BELOW: f64 = 10_000.0;

/// From linux/route.h and linux/if.h.
const RTF_UP: u32 = 0x1;
const IFF_UP: u32 = 0x1;
const IFF_LOOPBACK: u32 = 0x8;
const IFF_POINTOPOINT: u32 = 0x10;

pub struct Net {
    /// The interface carrying the main table's default route, if any; or why
    /// the routing table could not be read.
    route: Result<Option<String>, String>,
    speed: Option<u32>,
    tunnels: Vec<String>,
    since_resolve: u32,
    prev: Option<Snapshot>,
    deadline: Option<Instant>,
}

impl Net {
    pub fn new() -> Net {
        Net {
            route: Ok(None),
            speed: None,
            tunnels: Vec::new(),
            since_resolve: RESOLVE_EVERY,
            prev: None,
            deadline: None,
        }
    }

    /// Sleeps to the next window boundary. Boundaries stay on a fixed grid so
    /// windows do not drift; after a long stall (suspend) the grid restarts.
    fn wait(&mut self) {
        let now = Instant::now();
        let start = match self.deadline {
            None => now,
            Some(d) if d > now => {
                thread::sleep(d - now);
                d
            }
            Some(d) if now - d < WINDOW => d,
            Some(_) => now,
        };
        self.deadline = Some(start + WINDOW);
    }

    fn resolve(&mut self) {
        self.since_resolve = 0;
        self.route = fs::read_to_string("/proc/net/route")
            .map(|text| parse_default_route(&text))
            .map_err(|e| format!("/proc/net/route: {e}"));
        let default = self.route.as_ref().ok().and_then(Option::as_deref);
        self.speed = default.and_then(link_speed);
        self.tunnels = tunnels(default);
    }
}

impl Default for Net {
    fn default() -> Net {
        Net::new()
    }
}

impl Gauge for Net {
    fn next(&mut self) -> Reading {
        let mut dropped = false;
        loop {
            self.wait();
            if self.since_resolve >= RESOLVE_EVERY {
                self.resolve();
            }
            self.since_resolve += 1;

            let iface = match &self.route {
                Err(why) => return self.unmeasured(Observation::Unreadable(why.clone())),
                Ok(None) => return self.unmeasured(Observation::NoRoute),
                Ok(Some(iface)) => iface.clone(),
            };
            if is_down(&iface) {
                return self.unmeasured(Observation::Down { iface });
            }
            let counters = match fs::read_to_string("/proc/net/dev") {
                Ok(text) => parse_net_dev(&text),
                Err(e) => {
                    return self.unmeasured(Observation::Unreadable(format!("/proc/net/dev: {e}")));
                }
            };
            let current = Snapshot {
                at: Instant::now(),
                counters,
            };
            let Some(prev) = self.prev.replace(current) else {
                continue; // the first edge of the first window
            };
            let current = self.prev.as_ref().expect("just stored");
            match observe(&prev, current, &iface, self.speed, &self.tunnels) {
                Some(observation) => return assess(&observation),
                // A counter went backwards or the interface vanished: drop
                // the window rather than report a nonsense rate, and look the
                // route up again. Twice in a row means it is not transient.
                None if !dropped => {
                    dropped = true;
                    self.since_resolve = RESOLVE_EVERY;
                }
                None => {
                    return assess(&Observation::Unreadable(format!(
                        "/proc/net/dev: no usable counters for {iface}"
                    )));
                }
            }
        }
    }
}

impl Net {
    /// A reading without a rate; the next rate needs two fresh edges.
    fn unmeasured(&mut self, observation: Observation) -> Reading {
        self.prev = None;
        assess(&observation)
    }
}

#[derive(Debug, PartialEq)]
struct Counters {
    iface: String,
    rx: u64,
    tx: u64,
}

struct Snapshot {
    at: Instant,
    counters: Vec<Counters>,
}

impl Snapshot {
    fn get(&self, iface: &str) -> Option<&Counters> {
        self.counters.iter().find(|c| c.iface == iface)
    }
}

/// Bytes moved over a measured number of seconds.
#[derive(Debug, PartialEq)]
struct Window {
    rx: u64,
    tx: u64,
    secs: f64,
}

impl Window {
    fn rates(&self) -> (f64, f64) {
        (self.rx as f64 / self.secs, self.tx as f64 / self.secs)
    }
}

#[derive(Debug, PartialEq)]
enum Observation {
    Unreadable(String),
    NoRoute,
    Down {
        iface: String,
    },
    Up {
        iface: String,
        speed: Option<u32>,
        link: Window,
        /// Counter values now, i.e. since the interface came up.
        totals: (u64, u64),
        tunnels: Vec<(String, Window)>,
    },
}

/// `None` when the link's counters are missing or went backwards. A tunnel
/// with the same problem is left out of this window.
fn observe(
    prev: &Snapshot,
    current: &Snapshot,
    iface: &str,
    speed: Option<u32>,
    tunnels: &[String],
) -> Option<Observation> {
    let secs = (current.at - prev.at).as_secs_f64();
    let window = |name: &str| {
        let (a, b) = (prev.get(name)?, current.get(name)?);
        Some(Window {
            rx: b.rx.checked_sub(a.rx)?,
            tx: b.tx.checked_sub(a.tx)?,
            secs,
        })
    };
    let link = window(iface)?;
    let now = current.get(iface)?;
    Some(Observation::Up {
        iface: iface.to_owned(),
        speed,
        link,
        totals: (now.rx, now.tx),
        tunnels: tunnels
            .iter()
            .filter_map(|t| Some((t.clone(), window(t)?)))
            .collect(),
    })
}

fn assess(observation: &Observation) -> Reading {
    match observation {
        Observation::Unreadable(why) => Reading {
            level: Level::Unknown,
            summary: summary("   ?", "   ?"),
            detail: vec![format!("Cannot measure: {why}")],
        },
        Observation::NoRoute => Reading {
            level: Level::Critical,
            summary: summary(" off", " off"),
            detail: vec!["No default route in the main routing table.".to_owned()],
        },
        Observation::Down { iface } => Reading {
            level: Level::Critical,
            summary: summary(" off", " off"),
            detail: vec![format!("{iface} carries the default route but is down.")],
        },
        Observation::Up {
            iface,
            speed,
            link,
            totals,
            tunnels,
        } => {
            let (rx, tx) = link.rates();
            let level = if rx < QUIET_BELOW && tx < QUIET_BELOW {
                Level::Quiet
            } else {
                Level::Normal
            };
            let mut detail = vec![
                match speed {
                    Some(mbps) => format!("{iface}, {mbps} Mb/s link (K = 1000 B)"),
                    None => format!("{iface} (K = 1000 B)"),
                },
                format!(
                    "↓ {} B/s   ↑ {} B/s over {:.2} s",
                    grouped(rx),
                    grouped(tx),
                    link.secs
                ),
            ];
            for (name, window) in tunnels {
                let (rx, tx) = window.rates();
                detail.push(format!(
                    "Tunnel payload ({name}): ↓ {}/s  ↑ {}/s",
                    human(rx),
                    human(tx)
                ));
            }
            detail.push(format!(
                "Total since {iface} came up: ↓ {}  ↑ {}",
                human(totals.0 as f64),
                human(totals.1 as f64)
            ));
            Reading {
                level,
                summary: summary(&rate4(rx), &rate4(tx)),
                detail,
            }
        }
    }
}

/// Both directions, each exactly 4 characters, so every reading of this
/// gauge has the same width.
fn summary(down: &str, up: &str) -> String {
    format!("󰁅 {down} 󰁝 {up}")
}

/// A rate in exactly 4 characters: `512B`, `1.2K`, ` 34K`, `1.2M`, `340M`.
fn rate4(bytes_per_sec: f64) -> String {
    let b = bytes_per_sec;
    if b < 999.5 {
        format!("{:>3}B", b.round() as u64)
    } else if b < 9_950.0 {
        format!("{:.1}K", b / 1e3)
    } else if b < 999_500.0 {
        format!("{:>3}K", (b / 1e3).round() as u64)
    } else if b < 9_950_000.0 {
        format!("{:.1}M", b / 1e6)
    } else if b < 999_500_000.0 {
        format!("{:>3}M", (b / 1e6).round() as u64)
    } else if b < 9_950_000_000.0 {
        format!("{:.1}G", b / 1e9)
    } else {
        format!("{:>3}G", (b / 1e9).round() as u64)
    }
}

/// SI with three significant figures: `1.18 MB`, `21.4 GB`, `512 B`.
fn human(bytes: f64) -> String {
    const UNITS: [&str; 5] = ["B", "kB", "MB", "GB", "TB"];
    let mut v = bytes;
    let mut unit = 0;
    while v >= 999.5 && unit < UNITS.len() - 1 {
        v /= 1000.0;
        unit += 1;
    }
    let u = UNITS[unit];
    if unit == 0 {
        format!("{} {u}", v.round() as u64)
    } else if v < 9.995 {
        format!("{v:.2} {u}")
    } else if v < 99.95 {
        format!("{v:.1} {u}")
    } else {
        format!("{v:.0} {u}")
    }
}

/// `1234567.4` → `1,234,567`.
fn grouped(n: f64) -> String {
    let digits = (n.round() as u64).to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// The interface of the lowest-metric default route that is up, from
/// `/proc/net/route` (which shows the main table only).
fn parse_default_route(text: &str) -> Option<String> {
    text.lines()
        .skip(1)
        .filter_map(|line| {
            let f: Vec<&str> = line.split_whitespace().collect();
            let (iface, dest, flags, metric, mask) =
                (f.first()?, f.get(1)?, f.get(3)?, f.get(6)?, f.get(7)?);
            let flags = u32::from_str_radix(flags, 16).ok()?;
            let metric: u32 = metric.parse().ok()?;
            (*dest == "00000000" && *mask == "00000000" && flags & RTF_UP != 0)
                .then(|| (metric, (*iface).to_owned()))
        })
        .min()
        .map(|(_, iface)| iface)
}

/// Byte counters per interface from `/proc/net/dev`. Long names leave no
/// space after the colon (`wg0-mullvad:494844680`), so split on the colon,
/// not on whitespace.
fn parse_net_dev(text: &str) -> Vec<Counters> {
    text.lines()
        .skip(2)
        .filter_map(|line| {
            let (name, rest) = line.split_once(':')?;
            let fields: Vec<u64> = rest
                .split_whitespace()
                .map(|f| f.parse().ok())
                .collect::<Option<_>>()?;
            Some(Counters {
                iface: name.trim().to_owned(),
                rx: *fields.first()?,
                tx: *fields.get(8)?,
            })
        })
        .collect()
}

fn is_tunnel(flags: u32) -> bool {
    flags & IFF_UP != 0 && flags & IFF_POINTOPOINT != 0 && flags & IFF_LOOPBACK == 0
}

/// Point-to-point interfaces that are up (WireGuard, tun), except the one
/// already carrying the default route.
fn tunnels(except: Option<&str>) -> Vec<String> {
    let Ok(dir) = fs::read_dir("/sys/class/net") else {
        return Vec::new();
    };
    let mut found: Vec<String> = dir
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let flags = fs::read_to_string(format!("/sys/class/net/{name}/flags")).ok()?;
            let flags = u32::from_str_radix(flags.trim().trim_start_matches("0x"), 16).ok()?;
            (Some(name.as_str()) != except && is_tunnel(flags)).then_some(name)
        })
        .collect();
    found.sort();
    found
}

/// Mb/s, when the driver knows (virtual and wireless links often report -1).
fn link_speed(iface: &str) -> Option<u32> {
    fs::read_to_string(format!("/sys/class/net/{iface}/speed"))
        .ok()?
        .trim()
        .parse::<i64>()
        .ok()
        .filter(|&s| s > 0)
        .and_then(|s| u32::try_from(s).ok())
}

/// "unknown" counts as up: tunnels and some drivers never report "up".
fn is_down(iface: &str) -> bool {
    matches!(
        fs::read_to_string(format!("/sys/class/net/{iface}/operstate"))
            .as_deref()
            .map(str::trim),
        Ok("down" | "lowerlayerdown" | "notpresent")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROUTE: &str = include_str!("../../tests/fixtures/route");
    const NET_DEV: &str = include_str!("../../tests/fixtures/net_dev");

    #[test]
    fn default_route_from_this_machine_is_eno1() {
        assert_eq!(parse_default_route(ROUTE).as_deref(), Some("eno1"));
    }

    #[test]
    fn default_route_prefers_lowest_metric_and_skips_routes_that_are_down() {
        let header =
            "Iface\tDestination\tGateway\tFlags\tRefCnt\tUse\tMetric\tMask\tMTU\tWindow\tIRTT\n";
        let table = format!(
            "{header}\
             wlan0\t00000000\t0101A8C0\t0003\t0\t0\t600\t00000000\t0\t0\t0\n\
             eth0\t00000000\t0101A8C0\t0003\t0\t0\t100\t00000000\t0\t0\t0\n\
             eth1\t00000000\t0101A8C0\t0002\t0\t0\t50\t00000000\t0\t0\t0\n\
             eth0\t0001A8C0\t00000000\t0001\t0\t0\t0\t00FFFFFF\t0\t0\t0\n"
        );
        assert_eq!(parse_default_route(&table).as_deref(), Some("eth0"));
        assert_eq!(parse_default_route(header), None);
    }

    #[test]
    fn net_dev_from_this_machine_including_a_name_with_no_space_after_the_colon() {
        let counters = parse_net_dev(NET_DEV);
        assert_eq!(counters.len(), 6);
        let get = |name: &str| counters.iter().find(|c| c.iface == name).unwrap();
        assert_eq!((get("eno1").rx, get("eno1").tx), (522_024_680, 63_185_057));
        assert_eq!(
            (get("wg0-mullvad").rx, get("wg0-mullvad").tx),
            (494_844_680, 53_850_372)
        );
    }

    #[test]
    fn tunnels_are_up_point_to_point_and_not_loopback() {
        assert!(is_tunnel(0x91)); // wg0-mullvad on this machine
        assert!(!is_tunnel(0x1003)); // eno1, bridges, docker0
        assert!(!is_tunnel(0x9)); // lo
        assert!(!is_tunnel(0x90)); // a tunnel that is down
    }

    #[test]
    fn rates_are_always_four_characters() {
        let cases = [
            (0.0, "  0B"),
            (512.0, "512B"),
            (999.4, "999B"),
            (999.5, "1.0K"),
            (1_234.0, "1.2K"),
            (9_949.0, "9.9K"),
            (9_950.0, " 10K"),
            (34_012.0, " 34K"),
            (999_499.0, "999K"),
            (999_500.0, "1.0M"),
            (1_234_567.0, "1.2M"),
            (9_950_000.0, " 10M"),
            (340_000_000.0, "340M"),
            (999_500_000.0, "1.0G"),
            (125_000_000_000.0, "125G"),
        ];
        for (rate, shown) in cases {
            assert_eq!(rate4(rate), shown, "rate {rate}");
        }
    }

    #[test]
    fn human_and_grouped() {
        assert_eq!(human(512.0), "512 B");
        assert_eq!(human(1_180_000.0), "1.18 MB");
        assert_eq!(human(21_400_000_000.0), "21.4 GB");
        assert_eq!(human(522_024_680.0), "522 MB");
        assert_eq!(grouped(1_234_567.4), "1,234,567");
        assert_eq!(grouped(999.0), "999");
        assert_eq!(grouped(0.0), "0");
    }

    fn snapshot(at: Instant, counters: &[(&str, u64, u64)]) -> Snapshot {
        Snapshot {
            at,
            counters: counters
                .iter()
                .map(|&(iface, rx, tx)| Counters {
                    iface: iface.to_owned(),
                    rx,
                    tx,
                })
                .collect(),
        }
    }

    fn up(rx: u64, tx: u64, secs: f64) -> Observation {
        Observation::Up {
            iface: "eno1".to_owned(),
            speed: Some(1000),
            link: Window { rx, tx, secs },
            totals: (522_024_680, 63_185_057),
            tunnels: vec![],
        }
    }

    #[test]
    fn rates_divide_by_the_measured_elapsed_time() {
        let t0 = Instant::now();
        let prev = snapshot(t0, &[("eno1", 1_000, 500), ("wg0", 10, 10)]);
        let current = snapshot(
            t0 + Duration::from_millis(1250),
            &[("eno1", 26_000, 3_000), ("wg0", 20, 20)],
        );
        let observation = observe(&prev, &current, "eno1", None, &["wg0".to_owned()]).unwrap();
        let Observation::Up { link, tunnels, .. } = observation else {
            panic!("expected up");
        };
        assert_eq!(link.rates(), (20_000.0, 2_000.0));
        assert_eq!(tunnels.len(), 1);
    }

    #[test]
    fn a_counter_going_backwards_drops_the_window() {
        let t0 = Instant::now();
        let prev = snapshot(t0, &[("eno1", 1_000, 500)]);
        let reset = snapshot(t0 + WINDOW, &[("eno1", 10, 600)]);
        assert_eq!(observe(&prev, &reset, "eno1", None, &[]), None);
        let missing = snapshot(t0 + WINDOW, &[("lo", 10, 10)]);
        assert_eq!(observe(&prev, &missing, "eno1", None, &[]), None);
    }

    #[test]
    fn quiet_only_while_both_directions_are_below_10k() {
        assert_eq!(assess(&up(9_999, 9_999, 1.0)).level, Level::Quiet);
        assert_eq!(assess(&up(10_000, 0, 1.0)).level, Level::Normal);
        assert_eq!(assess(&up(0, 10_000, 1.0)).level, Level::Normal);
        // Rate, not bytes: 15 kB over 2 s is 7.5 kB/s.
        assert_eq!(assess(&up(15_000, 0, 2.0)).level, Level::Quiet);
    }

    #[test]
    fn no_route_or_a_down_link_is_critical_and_unreadable_is_unknown() {
        assert_eq!(assess(&Observation::NoRoute).level, Level::Critical);
        let down = Observation::Down {
            iface: "eno1".to_owned(),
        };
        assert_eq!(assess(&down).level, Level::Critical);
        let unreadable = Observation::Unreadable("/proc/net/dev: gone".to_owned());
        assert_eq!(assess(&unreadable).level, Level::Unknown);
    }

    #[test]
    fn every_summary_has_the_same_width() {
        let width = |o: &Observation| assess(o).summary.chars().count();
        let normal = width(&up(1_234_567, 34_012, 1.0));
        assert_eq!(width(&up(0, 0, 1.0)), normal);
        assert_eq!(width(&Observation::NoRoute), normal);
        assert_eq!(width(&Observation::Unreadable(String::new())), normal);
    }

    #[test]
    fn detail_names_the_link_window_tunnels_and_totals() {
        let observation = Observation::Up {
            iface: "eno1".to_owned(),
            speed: Some(1000),
            link: Window {
                rx: 1_234_567,
                tx: 34_012,
                secs: 1.0,
            },
            totals: (21_400_000_000, 3_200_000_000),
            tunnels: vec![(
                "wg0-mullvad".to_owned(),
                Window {
                    rx: 1_180_000,
                    tx: 31_000,
                    secs: 1.0,
                },
            )],
        };
        assert_eq!(
            assess(&observation).detail,
            [
                "eno1, 1000 Mb/s link (K = 1000 B)",
                "↓ 1,234,567 B/s   ↑ 34,012 B/s over 1.00 s",
                "Tunnel payload (wg0-mullvad): ↓ 1.18 MB/s  ↑ 31.0 kB/s",
                "Total since eno1 came up: ↓ 21.4 GB  ↑ 3.20 GB",
            ]
        );
    }
}
