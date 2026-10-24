# Telltale: spec

Terms are defined in [GLOSSARY.md](GLOSSARY.md). This file is about how Telltale works; the glossary is about what its words mean.

Six gauges, each its own executable ([ADR 0001](docs/adr/0001-one-executable-per-gauge.md)):

| executable        | watches                                                 |
| ----------------- | ------------------------------------------------------- |
| `telltale-net`    | traffic on the interface that carries the default route |
| `telltale-mem`    | memory in use                                           |
| `telltale-disk`   | the fullest local filesystem                            |
| `telltale-units`  | failed systemd units, system and user                   |
| `telltale-inbox`  | messages in an IMAP inbox you keep empty                |
| `telltale-claude` | Claude Code sessions, and whether one is waiting on you |

Telltale knows nothing about waybar. Waybar is one consumer of the `waybar` format; see [Using Telltale from waybar](#using-telltale-from-waybar) at the end.

## Principles

**Honest.** Every number is defined. A reading's detail says what was measured, over what window and how fresh it is. A gauge that cannot measure gives an unknown reading; it never repeats its last value as if it were current. Where the kernel's own figure is misleading on this machine (iowait, load average, IO pressure), it is not used.

**Cheap.** Watching costs no process spawns in steady state (except `units`, see its section). Rates are computed over the whole interval with no gaps.

**Quiet when normal.** Levels are chosen so that most readings, most of the time, are quiet or normal.

**Opinionated.** Thresholds are constants in the code, chosen for this machine and stated below. There is no config file.

## Command line

Every gauge takes the same options:

```
telltale-<gauge> [--watch] [--format text|waybar] [--floor LEVEL]
```

- No `--watch`: **check**. Take one reading, write it, exit.
- `--watch`: write a reading, then a new one whenever the formatted output changes. Runs until its output is closed.
- `--format text` (default): for a person.
  ```
  attention: 󰋊 92%
  /       92%  34 GiB free of 418 GiB  (ext4, sda3)
  /boot   14%  ...
  ```
  The first line is level and summary, without the summary's trailing padding (see below); the detail follows. When watching, readings are separated by a blank line.
- `--format waybar`: one JSON object per line: `{"text": summary, "class": level, "tooltip": detail}`. The detail is Pango-escaped (`&`, `<`, `>`), because waybar renders tooltips as Pango markup.
- `--floor LEVEL`: a reading below LEVEL is written empty (`text` format: an empty line; `waybar` format: `{"text":""}`, which waybar hides). The order is quiet < normal < attention < critical; unknown is never below the floor.

**Exit status of a check** follows the Nagios plugin convention, so any gauge works as a check in scripts (`telltale-disk || notify-send ...`): 0 quiet or normal, 1 attention, 2 critical, 3 unknown. A watch exits 0 when its output is closed. Bad arguments exit 64 (`EX_USAGE`).

`telltale-inbox` takes extra options; see its section.

Summaries use Nerd Font glyphs (`󰋊`, `󰇮`, …) and assume a terminal or bar font that has them. Every value is left-aligned in a fixed width, wide enough for its usual values, so a summary keeps its width and the padding falls after the value, not between it and its icon: right-aligned, `󰇮   8` read as part of the segment to its right. A value that outgrows its width (a 4-digit inbox, `1000G` of traffic) widens the summary rather than being cut. The padding is for the bar; the `text` format drops it.

## Design

```
src/
  lib.rs            re-exports; nothing else
  reading.rs        Level, Reading
  format.rs         text and waybar formats, floor
  run.rs            run(gauge): arguments, check/watch, exit status
  gauge/
    mod.rs          the Gauge trait
    net.rs mem.rs disk.rs units.rs claude.rs
    inbox/          mod.rs (the gauge), imap.rs (the protocol)
  bin/
    telltale-net.rs …   each one line: telltale::run(Net::new())
```

**`Gauge`** is the main seam, and it has six adapters:

```rust
pub trait Gauge {
    /// Blocks until the next reading is due, then returns it. The first call
    /// returns as soon as an honest reading exists (for a rate, after one
    /// window). I/O failures become unknown readings; this never panics.
    fn next(&mut self) -> Reading;
}
```

A gauge decides its own pace: `net` sleeps out its 1 s window, `inbox` blocks on the IMAP connection. `run` neither knows nor cares, so check and watch are the same loop with a different stopping rule.

**`run`** is a deep module: argument parsing, check versus watch, removing duplicate output, exit status, writing, stopping on EPIPE, and `PR_SET_PDEATHSIG(SIGTERM)` while watching (so a crashed parent leaves no orphans). It is all behind one function, used identically by all six executables.

**`format`** is pure: `(reading, format, floor) -> String`.

**Inside each gauge** there is an internal seam between I/O and judgement. Parsing (`/proc/net/dev` text into counters) and judging (two samples and an elapsed time into a reading) are pure functions. The I/O wrapper around them is a few lines. Tests:

- Parsers are tested against text captured from this machine.
- Judgement is tested with hand-built samples at every level boundary.
- `format` is tested directly.
- `imap` is tested against a scripted in-memory transcript (real socket vs in-memory: two adapters, so a real seam).
- Each executable gets one integration test: run the real binary as a check and assert on the exit status and output shape.

The I/O wrappers are verified by running them on this machine, not mocked.

**Dependencies:** `serde_json`, `libc`; `rustls` (with `ring`) and `rustls-native-certs` for `inbox` only. Argument parsing is hand-written (three shared options). No async runtime.

**Units:** SI for rates (1 K = 1000 B), binary for memory and disk sizes (GiB). The detail names the unit.

## Cost of watching

Measured on this machine (353 processes, 16 threads) unless marked _est._

| gauge  | wakes    | steady cost                                                                            | replaces                                   |
| ------ | -------- | -------------------------------------------------------------------------------------- | ------------------------------------------ |
| mem    | 1/2 s    | 1 file read                                                                            | `sh` + `free` + `awk` every 5 s            |
| net    | 1/s      | 1 file read                                                                            | `sh` + `routebps` every 1 s                |
| inbox  | on event | idle TCP connection, ~0 CPU                                                            | TLS handshake + 2 sops decrypts every 60 s |
| units  | 1/30 s   | 2 `systemctl` spawns, ~5 ms                                                            | (new)                                      |
| disk   | 1/60 s   | 3 `statvfs` calls                                                                      | (new)                                      |
| claude | on event | inotify watch + a pidfd per session; per change, the bytes appended to each transcript | (new)                                      |

Before: about 3 process spawns per second, including the dropped `cpubusy` (waybar runs each `exec` through `sh -c`). After: none in steady state apart from `units`.

## Findings that shaped the spec

1. **iowait is ~40% with no disk traffic.** `vmstat` shows 10 blocked tasks and `wa` 35–40% while `bi`/`bo` are ~0 and no device has IO in flight. PSI reports IO stalled 82–90% of the time. The best guess is that some waiting with no IO pending is counted as iowait (the one blocked thread seen was vesktop). So load average (inflated to ~10) and IO pressure are not used anywhere; here they would be permanent false alarms.
2. **Traffic leaves through `wg0-mullvad` by policy routing**, but the main table's default route is `eno1`. Every byte crosses `eno1` once (encrypted while the VPN is up), so `net` reads `eno1`.
3. **One unit has already failed:** `app-com.mitchellh.ghostty.service` (Result: protocol, killed by TERM at 16:15). `units` reports it from day one; it gets fixed at its cause.
4. **`df` and `findmnt` disagree** about `/` (57% vs 54%). `findmnt` divides by total size, including ext4's 5% root reserve; `df` divides by what you can actually write. `disk` uses the `df` definition.

---

## `telltale-mem`

**Measures:** `MemTotal − MemAvailable` from `/proc/meminfo`, as a % of `MemTotal`; what `free` calls "used". Page cache is excluded because the kernel can reclaim it. `MemAvailable` is the kernel's own estimate of what could be handed out without swapping. Every 2 s.

**Summary:** `󰍛 ` + the percentage left-aligned in 4 (`7%  `, `100%`)

**Levels:**

- quiet < 50%, normal < 80%
- attention ≥ 80%, critical ≥ 90%
- also critical when memory PSI `full avg10` > 1% (every task stalled on memory, i.e. thrashing), whatever the percentage

**Detail:**

```
Used 6.5 GiB of 31.2 GiB (available 24.7 GiB)
Swap 0.0 GiB of 40.0 GiB
Every task stalled on memory 0.00% of the last 10 s
```

The level is judged on the same rounded percentage the summary shows, so `80%` is never shown in the colour of 79%. No per-app breakdown. The only per-app figure that adds up honestly is PSS (RSS counts shared pages in every process that maps them), and a PSS scan costs 66 ms of kernel time. That's a process monitor's job.

## `telltale-net`

**Measures:** the interface of the main table's default route in `/proc/net/route`, re-resolved every 10 s or when its counters vanish (today `eno1`). Not the VPN interface: the physical interface carries everything, so the number stays continuous when the VPN toggles. It includes WireGuard's small per-packet overhead (what is actually on the wire) and LAN traffic.

`rx_bytes`/`tx_bytes` deltas from one read of `/proc/net/dev` per 1 s window, divided by the measured elapsed time (monotonic clock). No smoothing. If a counter goes backwards (interface reset), that window is dropped rather than reported as a huge or negative rate.

**Summary:** `󰁅 ` + rate + ` 󰁝 ` + rate, each rate left-aligned in 4 characters:

| bytes/s   | shown               |
| --------- | ------------------- |
| < 999.5   | `0B  `, `12B `, `512B` |
| < 9.95 K  | `1.2K`              |
| < 999.5 K | `34K `              |
| < 9.95 M  | `1.2M`              |
| < 999.5 M | `340M`              |
| < 9.95 G  | `1.2G`              |
| < 999.5 G | `125G`              |

**Levels:** quiet when both directions are < 10 K/s, otherwise normal. Critical, with summary `󰁅 off  󰁝 off `, when there is no default route or its interface's `operstate` is not `up`. No attention level: high throughput isn't a problem.

**Detail:**

```
eno1, 1000 Mb/s link (K = 1000 B)
↓ 1,234,567 B/s   ↑ 34,012 B/s over 1.00 s
VPN payload (wg0-mullvad): ↓ 1.18 MB/s  ↑ 31 kB/s
Since boot: ↓ 21.4 GB  ↑ 3.2 GB
```

## `telltale-disk`

**Measures:** every mount in `/proc/self/mounts` on a block device (ext4/btrfs/xfs/vfat/exfat/f2fs/ntfs3), deduplicated by device. Today: `/`, `/boot`, `/data/shared`. `/boot` matters: a full ESP makes kernel updates fail. Used % is `statvfs` with the `df` definition, `(blocks − bfree) / (blocks − bfree + bavail)`, rounded up as `df` rounds it, so the number always matches `df`'s `Use%`. Every 60 s. A device mounted twice (bind mounts, btrfs subvolumes) is listed once, under its shortest path. If any filesystem cannot be measured, the reading is unknown: "the fullest" is then not knowable.

**Summary:** `󰋊 ` + the percentage of the fullest filesystem, left-aligned in 4.

**Levels:** normal < 90%, attention ≥ 90%, critical ≥ 95%, from the fullest filesystem.

**Detail:** one line per filesystem, fullest first: `/  57%  172 GiB free of 418 GiB  (ext4, sda3)`.

## `telltale-units`

**Measures:** `systemctl --failed -o json` and `systemctl --user --failed -o json`, every 30 s (~5 ms for both). A D-Bus subscription would react instantly, but it needs `zbus` (a large dependency tree), and systemd has no "failed count changed" signal to subscribe to. A 30 s delay is fine here.

**Summary:** `󰀦 ` + count, left-aligned in 2.

**Levels:** quiet at 0, critical at 1 or more. A unit systemd restarts on failure never stays failed, so it never shows here; what does show is a unit systemd gave up on, which stays down until someone acts. In the 30 days to 2026-10-19 waybar failed 62 times (killed at login by an RT signal it was not yet catching, since fixed) and forgejo-runner 25, all restarted; the only unit that stayed failed was ghostty (15 of 47 logins, since fixed).

**Detail:**

```
1 failed unit
user  app-com.mitchellh.ghostty.service  protocol  since 16:15
```

No ignore-list: that's how real failures get missed.

## `telltale-inbox`

An inbox you keep empty: every message in it is a todo. The gauge counts messages, not unread ones.

**Options**, on top of the shared ones:

```
--server imap://HOST:PORT | imaps://HOST:PORT   (required)
--mailbox NAME                                   (default INBOX)
--user-env VAR --password-env VAR                (required)
--insecure                                       (loopback servers only)
```

- `imap://` uses STARTTLS, `imaps://` implicit TLS. Plain text is not offered.
- Credentials are read from the named environment variables, so Telltale itself contains no secret store and no provider-specific names.
- Certificates are verified against the system roots. `--insecure` skips verification, for local bridges with self-signed certificates, and is refused for anything but a loopback address. The alternative, pinning Proton Bridge's certificate, breaks every time Bridge regenerates it, and only protects against an attacker who already runs code as you.

**Measures:** `EXAMINE` (read-only, so flags are never touched); the `* n EXISTS` response is the count. Then `IDLE`, if the server advertises it (Proton Bridge does, checked). `EXISTS`, `EXPUNGE` and `FETCH` updates trigger `DONE`, a re-query (`SEARCH UNSEEN`, `FETCH 1 (INTERNALDATE)`), a new reading, and `IDLE` again. `IDLE` is re-issued every 25 min (RFC 2177 says before 29). If the server is silent for 30 min, the connection is treated as dead and reopened. Without `IDLE`, it falls back to checking every 60 s. A check (no `--watch`) connects, reads, logs out.

**Freshness:** our side reacts at once, but a bridge relays a remote server with its own delay. Proton Bridge polls Proton for events, so new mail appears when Bridge notices it.

**Summary:** `󰇮 ` + count, left-aligned in 3; `󰇮 ?  ` when unknown.

**Levels:** quiet at 0, attention above 0. Unknown when the server is unreachable or login fails. While watching, it retries with backoff from 5 s up to 60 s.

**Detail:**

```
257 in INBOX, 203 unread
Oldest arrived 79 days ago (01-Aug-2026)
Connected to imap://127.0.0.1:1143 since 19:05; the server pushes changes (IDLE)
```

A check keeps nothing current, so its last line is when it read: `Read from imap://127.0.0.1:1143 at 19:05`. Unknown:

```
Cannot measure since 19:04: imap://127.0.0.1:1143: Connection refused (os error 111)
Last seen: 257 at 19:03
```

Times are local clock times, not ages: the detail is written once and read later, and "3 min ago" would go stale on screen. A time that is not today gets its date (`2026-10-17 16:15`). `units` does the same. A missing credential variable is an unknown reading (`$VAR is not set`), not a usage error, so it shows on a bar instead of the gauge silently exiting.

## `telltale-claude`

Claude Code sessions, and whether one is waiting on you. Desktop notifications already say when a session finishes; this is what stays on screen afterwards, and what a dismissed notification no longer says.

**Measures:** Claude Code keeps one file per running session, `~/.claude/sessions/<pid>.json`, and rewrites it when the session's status changes. This is Claude Code's own bookkeeping, not a documented interface, so the gauge reads it strictly: a file it cannot parse, or a status it does not know, makes the reading unknown rather than quietly wrong. The fields read, as observed with Claude Code 2.1.306 on 2026-10-22:

| field                 | meaning                                                                                                                                                                                                           |
| --------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `status`              | `busy` working (also while a `!` shell command runs); `idle` the turn is over; `waiting` blocked on you; `shell` known to Claude Code but not seen, read as working; absent for a moment after the session starts |
| `waitingFor`          | why it waits: `permission prompt`, `input needed` (a question)                                                                                                                                                    |
| `statusUpdatedAt`     | when the status was last written (ms since the epoch)                                                                                                                                                             |
| `pid`, `procStart`    | the process, and its start time in clock ticks (`/proc/<pid>/stat` field 22)                                                                                                                                      |
| `name`, `cwd`, `kind` | the session's short name, its directory, and `interactive`, `bg`, `daemon` or `daemon-worker`                                                                                                                     |
| `sessionId`           | names its transcript                                                                                                                                                                                              |

**Titles** come from the session's transcript, `~/.claude/projects/<cwd>/<sessionId>.jsonl`, where `<cwd>` has every character but letters and digits turned into `-`. Claude Code appends a title record every turn or so: `{"type":"ai-title","aiTitle":…}` for the title it chose, `{"type":"custom-title","customTitle":…}` after `/rename`. The last one is the title, as for the dunst hook, so the tooltip and the window titles agree. Only lines that start as a title record count, so a title quoted in a message is not one. Transcripts run to 25 MB here, so the gauge remembers how far it has read each one and reads only what was appended since; a line without its newline yet is left for next time, and a transcript shorter than what was read is read again from the start. A check reads them whole (3 sessions, 4.3 MB: 2 ms). A title is an extra, like `units`' why and when: a transcript that cannot be read leaves the session under its short name, not the reading unknown. A new title shows with the session's next status change, usually the end of the turn that made it.

A session killed outright (`kill -9`, a crashed terminal) leaves its file behind until Claude Code sweeps it, so a file counts only while its `pid` is alive and its start time matches `procStart` (a reused pid does not). The gauge never removes files; that is Claude Code's job. `kind: daemon` is Claude Code's own supervisor, not a session, and is not counted.

Watching: inotify on the directory, reading on `close_write`, `moved_to`, `moved_from` and `delete` (a new file is written in place and is empty at `create`), plus a pidfd per live session, which becomes readable when the process exits, for the deaths that leave no trace in the directory. A file that does not parse is read again after 50 ms, up to three times, before the reading turns unknown: it may be half-written. If the directory cannot be watched, the reading is unknown and the watch is retried every 60 s.

**Summary:** `󰚩 ` + the sessions working (busy or shell), then `󰒲 ` + the rest, each left-aligned in 2 and the working count followed by a space, so `10` does not touch `󰒲`: `󰚩 1  󰒲 2 `, `󰚩 10 󰒲 2 `. The rest are idle, waiting or starting; in all of them the next move is yours. `󰚩 ?  󰒲 ? ` when unknown.

**Levels:**

- quiet: no sessions
- normal: at least one open, working or idle
- attention: at least one waiting on you

An idle session is normal, not quiet: it is waiting for your next message, and writing it is often what you do next, so a bar that hides the floor below normal still shows it. It is not attention either: the turn is over, the notification said so, and a session left open would keep the bar peach all day. Waiting is: the session cannot go on without you.

**Detail:** a light table, one row per session, marked with the bar's icons (`󰏤` for waiting on you):

```
   Session                                 Directory         Since
───────────────────────────────────────────────────────────────────────────────────────
󰏤  Upstream sync for local fixes           ~/projects/mango  01:58  permission prompt
󰚩  Claude Code in Waybar                   ~/desktop         01:53
󰒲  Cargo-nextest --locked install failure  ~/desktop         01:54
```

Waiting first, then working, then idle; within each, newest first (the session that just finished is the one you are most likely to answer), and by title when two changed in the same second, so rows never swap places between readings. Since is when the session's status last changed. Why a session waits trails its row rather than getting a column: it is rare, and an empty column would widen every tooltip. The rule is as wide as the widest line. No count line: the rows are the count. Titles are cut to 40 characters (`…`), so one long title does not widen every row; a session without a title yet shows its short name (`desktop-a1`). Each icon is one cell in a Nerd Font, so the columns line up under their names. No sessions: `No Claude Code sessions`. Unknown: `Cannot measure: ` + why, naming the file.

---

## Using Telltale from waybar

Telltale doesn't know about any of this; it's the waybar side of the contract.

```jsonc
"custom/mem":   { "exec": "telltale-mem --watch --format waybar",  "return-type": "json" },
"custom/net":   { "exec": "telltale-net --watch --format waybar",  "return-type": "json" },
"custom/inbox": { "exec": "~/.local/bin/shims/telltale-inbox --watch --format waybar --server imap://127.0.0.1:1143 --insecure --user-env PROTON_BRIDGE_IMAP_USER --password-env PROTON_BRIDGE_IMAP_PASS",
                  "return-type": "json", "on-click": "xdg-open https://mail.proton.me/" },
"custom/units": { "exec": "telltale-units --watch --format waybar --floor attention", "return-type": "json",
                  "on-click": "ghostty -e sh -c 'systemctl --failed; systemctl --user --failed; read'",
                  "on-click-right": "systemctl --user reset-failed" },
"custom/disk":  { "exec": "telltale-disk --watch --format waybar --floor attention",  "return-type": "json" },
"custom/claude": { "exec": "telltale-claude --watch --format waybar --floor normal", "return-type": "json",
                   "on-click": "claude-focus" },
```

- **Shim:** add the manifest line `telltale-inbox  ~/.cargo/bin/telltale-inbox  PROTON_BRIDGE_IMAP_USER PROTON_BRIDGE_IMAP_PASS` and a `telltale-inbox` symlink in `shims/`. Secrets are decrypted once per waybar start instead of twice a minute. They live in the process environment for its lifetime, readable only by your own user: the same exposure as before, for longer.
- **CSS:** classes are the level names (`quiet`, `normal`, `attention`, `critical`, `unknown`): one palette for every gauge. Quiet is dim grey, attention is peach, critical and unknown are red.
- **Other bar changes** (not Telltale):
  - **mullvad-status:** stream from `mullvad status -j listen`, show the 2-letter country code (`󰌾 SE `, from the relay hostname), red `󰌿 off` when disconnected. Classes are level names.
  - **volume:** `󰕾 30% ` / `󰖁 --  ` (four left-aligned cells, so `100%` fits).
  - **dunst-status:** `󰂚` / `󰂛` instead of emoji, peach when paused.
  - **whitetemp-status:** `󰖔` instead of 🌙.
  - **clock:** `{:%a %d %H:%M}`; the tooltip has the calendar, ISO date and week.
  - **claude-focus** (dotfiles): focuses the Ghostty window of the top row of the tooltip that is not working (the newest waiting session, or else the one that finished last), matching the window title to the session title as the dunst hook does.
  - **Order:** mem net │ inbox claude vpn volume │ units disk │ voxtype desktop whitetemp idle dunst │ clock │ tray.
  - **Padding:** 8 → 6 px.
  - **Segments:** each sits in a recessed pill (`#181825`, a shade darker than the bar, 6 px radius); the icon-only toggles (voxtype desktop whitetemp idle dunst) share one, as a waybar group.

## Decisions

- No CPU gauge. CPU is a rate, not a resource that fills up: at 100% work takes longer, but nothing breaks, unlike memory or disk. The only CPU fault worth catching is a cooling fault, which shows as thermal throttling. The kernel journal (back to 31 Jul 2026, ~160 boots) has no throttle messages, so that is checked on demand, not watched: `journalctl -k | grep -iE 'clock throttled|temperature above threshold'`, plus `/sys/devices/system/cpu/cpu*/thermal_throttle/*_count` for the current boot (these also count brief events the log omits).
- `units` has no ignore-list.
- `inbox` does not get more urgent with age.
- `mem` has no per-app breakdown.
- Gauges always report; hiding is the floor, chosen by the display.
- `claude`'s detail has no line saying how it keeps up (`Watching … (inotify)`) or when a check read: the sessions and their clock times are what matters there, and the line read as jargon. It is the one gauge whose detail does not say how fresh it is.
