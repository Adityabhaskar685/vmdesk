//! The probe script and the parser for its output. One POSIX shell script gathers every
//! section in a single exec; sections are separated by [`MARKER`] lines, and a section
//! whose tool is missing or fails reports `!unavailable <reason>` instead.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use vmdesk_proto::{
    Commit, Container, Disk, Listener, ProcessInfo, RepoInfo, SNAPSHOT_SCHEMA, Snapshot,
    SystemInfo, TmuxSession,
};

use crate::redact;

/// Marker that starts each section of the probe output.
pub const MARKER: &str = "@@vmdesk@@";

/// Run by `/bin/sh`. Prints `@@vmdesk@@ <section>` before each section.
pub const PROBE_SCRIPT: &str = r#"
m='@@vmdesk@@'
LC_ALL=C; export LC_ALL
sec() { printf '%s %s\n' "$m" "$1"; }
na() { echo "!unavailable $1"; }
lim() { if command -v timeout >/dev/null 2>&1; then timeout 5 "$@"; else "$@"; fi; }
g() { lim git -c safe.directory='*' -C "$@"; }

sec system
date +%s
id -un
hostname 2>/dev/null || cat /proc/sys/kernel/hostname
uname -r
( . /etc/os-release 2>/dev/null && echo "$PRETTY_NAME" ) || echo
cat /proc/uptime
cat /proc/loadavg
nproc 2>/dev/null || getconf _NPROCESSORS_ONLN
grep -E '^(MemTotal|MemAvailable):' /proc/meminfo
grep '^btime' /proc/stat

sec disks
df -kP -x tmpfs -x devtmpfs -x overlay -x squashfs -x efivarfs 2>/dev/null | tail -n +2

sec processes
ps -eo pid=,ppid=,user:32=,etimes=,pcpu=,rss=,args= 2>/dev/null || na "ps failed"

cwds=$(for d in /proc/[0-9]*; do c=$(readlink "$d/cwd" 2>/dev/null) && printf '%s\t%s\n' "${d#/proc/}" "$c"; done)
sec cwds
printf '%s\n' "$cwds"

sec containers
if command -v docker >/dev/null 2>&1; then lim docker ps -a --format '{{json .}}' 2>&1 | head -200; else na "docker not installed"; fi

panes=
sec tmux
if command -v tmux >/dev/null 2>&1; then
  tmux list-sessions -F '#{session_name}<|>#{session_windows}<|>#{session_attached}<|>#{session_created}' 2>/dev/null
  panes=$(tmux list-panes -a -F '#{session_name}<|>#{window_active}<|>#{pane_active}<|>#{pane_current_path}<|>#{pane_current_command}' 2>/dev/null)
  sec tmux_panes
  printf '%s\n' "$panes"
else
  na "tmux not installed"
fi

sec listeners
if command -v ss >/dev/null 2>&1; then ss -Htlnp 2>/dev/null; else na "ss not installed"; fi

sec repos
if command -v git >/dev/null 2>&1; then
  { printf '%s\n' "$cwds" | cut -f2; printf '%s\n' "$panes" | awk -F '<[|]>' '{print $4}'; } \
    | grep -v -e '^/$' -e '^/proc' -e '^/sys' -e '^$' | sort -u | head -200 \
    | while IFS= read -r d; do g "$d" rev-parse --show-toplevel 2>/dev/null; done \
    | sort -u | head -50 \
    | while IFS= read -r r; do
        b=$(g "$r" symbolic-ref --short -q HEAD 2>/dev/null)
        n=$(g "$r" status --porcelain 2>/dev/null | wc -l)
        ab=$(g "$r" rev-list --left-right --count '@{u}...HEAD' 2>/dev/null | tr '\t' ' ')
        c=$(g "$r" log -1 --format='%H%x09%ct%x09%s' 2>/dev/null)
        printf '%s\t%s\t%s\t%s\t%s\n' "$r" "$b" "$n" "$ab" "$c"
      done
else
  na "git not installed"
fi
sec end
"#;

/// The command to exec: the script under `/bin/sh`, whatever the login shell is.
#[must_use]
pub fn probe_command() -> String {
    format!("/bin/sh -c '{}'", PROBE_SCRIPT.replace('\'', r"'\''"))
}

/// Processes started this soon after boot, outside any SSH or tmux session, are taken
/// to be system services rather than something you started.
const BOOT_GRACE_SECS: i64 = 300;
const SHELLS: [&str; 6] = ["bash", "sh", "zsh", "fish", "dash", "ksh"];
const NOISE: [&str; 6] = [
    "sshd:",
    "(sd-pam)",
    "sleep",
    "containerd",
    "dockerd",
    "docker-proxy",
];
/// Binaries here, run by init from `/`, are system services even if restarted later.
/// `/usr/bin` is left out: packaged daemons you deploy (e.g. open5gs) live there.
const SYSTEM_DIRS: [&str; 5] = [
    "/usr/lib/",
    "/usr/libexec/",
    "/usr/sbin/",
    "/sbin/",
    "/lib/",
];
/// Programs whose children count as interactive work, like an SSH session's.
const MULTIPLEXERS: [&str; 3] = ["tmux", "screen", "SCREEN"];

/// Field separator in tmux formats. Not a tab: tmux before 3.1 turns tabs into `_` when
/// the locale is not UTF-8, as it is under the probe's `LC_ALL=C`.
const TMUX_SEP: &str = "<|>";

/// Parses the probe output. `taken_at` is the laptop's time of collection.
#[must_use]
pub fn parse(output: &str, taken_at: DateTime<Utc>) -> Snapshot {
    let sections = split(output);
    let body = |name: &str| sections.get(name).map_or("", String::as_str);
    let mut snap = Snapshot {
        schema_version: SNAPSHOT_SCHEMA,
        taken_at: Some(taken_at),
        ..Snapshot::default()
    };
    // Sections whose tool reported itself unavailable are left empty.
    let mut skip = HashSet::new();
    for name in ["processes", "containers", "tmux", "listeners", "repos"] {
        if let Some(reason) = body(name)
            .lines()
            .find_map(|l| l.strip_prefix("!unavailable "))
        {
            snap.unavailable.push(format!("{name}: {reason}"));
            skip.insert(name);
        }
    }
    let sys = parse_system(body("system"));

    snap.system = sys.info;
    snap.system.disks = parse_disks(body("disks"));
    if !skip.contains("processes") {
        let cwds = parse_cwds(body("cwds"));
        snap.processes = filter_processes(
            &parse_processes(body("processes"), sys.now),
            &cwds,
            &sys.user,
            snap.system.boot_time,
        );
    }
    if !skip.contains("containers") {
        match parse_containers(body("containers")) {
            Ok(c) => snap.containers = c,
            Err(reason) => snap.unavailable.push(format!("containers: {reason}")),
        }
    }
    if !skip.contains("tmux") {
        snap.tmux = parse_tmux_sessions(body("tmux"), body("tmux_panes"));
    }
    if !skip.contains("listeners") {
        snap.listeners = parse_listeners(body("listeners"));
    }
    if !skip.contains("repos") {
        snap.repos = parse_repos(body("repos"));
    }
    snap
}

fn split(output: &str) -> HashMap<String, String> {
    let mut sections = HashMap::new();
    let mut current: Option<(String, String)> = None;
    for line in output.lines() {
        if let Some(name) = line.strip_prefix(MARKER) {
            if let Some((n, b)) = current.take() {
                sections.insert(n, b);
            }
            current = Some((name.trim().to_owned(), String::new()));
        } else if let Some((_, b)) = current.as_mut() {
            b.push_str(line);
            b.push('\n');
        }
    }
    if let Some((n, b)) = current {
        sections.insert(n, b);
    }
    sections
}

struct System {
    info: SystemInfo,
    /// The VM's own clock, Unix seconds; process and tmux times are relative to it.
    now: i64,
    user: String,
}

fn parse_system(body: &str) -> System {
    let lines: Vec<&str> = body.lines().collect();
    let line = |i: usize| lines.get(i).map_or("", |l| l.trim());
    let mut info = SystemInfo {
        hostname: line(2).to_owned(),
        kernel: line(3).to_owned(),
        os: line(4).to_owned(),
        ..SystemInfo::default()
    };
    let now: i64 = line(0).parse().unwrap_or_default();
    let mut avail = None;
    for l in &lines[5.min(lines.len())..] {
        let mut f = l.split_whitespace();
        match f.next() {
            Some("MemTotal:") => info.mem_total_kb = num(f.next()),
            Some("MemAvailable:") => avail = Some(num(f.next())),
            Some("btime") => info.boot_time = f.next().and_then(|v| v.parse().ok()).unwrap_or(0),
            _ => {}
        }
    }
    info.mem_used_kb = avail.map_or(0, |a| info.mem_total_kb.saturating_sub(a));
    // /proc/uptime: "165713.08 648305.01"
    info.uptime_secs = line(5)
        .split(['.', ' '])
        .next()
        .and_then(|u| u.parse().ok())
        .unwrap_or(0);
    // /proc/loadavg: "1.00 1.02 1.04 2/384 153662"
    let loads: Vec<f32> = line(6)
        .split_whitespace()
        .take(3)
        .filter_map(|v| v.parse().ok())
        .collect();
    if let [a, b, c] = loads[..] {
        info.load = [a, b, c];
    }
    info.cpus = line(7).parse().unwrap_or(0);
    System {
        info,
        now,
        user: line(1).to_owned(),
    }
}

fn num(v: Option<&str>) -> u64 {
    v.and_then(|v| v.parse().ok()).unwrap_or(0)
}

fn parse_disks(body: &str) -> Vec<Disk> {
    body.lines()
        .filter_map(|l| {
            // Filesystem 1024-blocks Used Available Capacity Mounted-on
            let f: Vec<&str> = l.split_whitespace().collect();
            (f.len() >= 6).then(|| Disk {
                mount: f[5..].join(" "),
                total_kb: f[1].parse().unwrap_or(0),
                used_kb: f[2].parse().unwrap_or(0),
            })
        })
        .collect()
}

fn parse_processes(body: &str, vm_now: i64) -> Vec<ProcessInfo> {
    body.lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            let pid = f.next()?.parse().ok()?;
            let parent = f.next()?.parse().ok()?;
            let user = f.next()?.to_owned();
            let etimes: i64 = f.next()?.parse().ok()?;
            let cpu_pct = f.next()?.parse().ok()?;
            let rss_kb = f.next()?.parse().ok()?;
            let cmdline = f.collect::<Vec<_>>().join(" ");
            Some(ProcessInfo {
                pid,
                ppid: parent,
                user,
                cmdline,
                cwd: None,
                started_at: vm_now - etimes,
                cpu_pct,
                rss_kb,
            })
        })
        .collect()
}

fn parse_cwds(body: &str) -> HashMap<u32, String> {
    body.lines()
        .filter_map(|l| {
            let (pid, cwd) = l.split_once('\t')?;
            Some((pid.parse().ok()?, cwd.to_owned()))
        })
        .collect()
}

fn program(cmdline: &str) -> &str {
    let first = cmdline.split_whitespace().next().unwrap_or("");
    first
        .rsplit('/')
        .next()
        .unwrap_or(first)
        .trim_start_matches('-')
}

/// Keeps the login user's own work (spec section 9): drops kernel threads, the probe
/// itself, idle shells, container internals and services started at boot.
fn filter_processes(
    all: &[ProcessInfo],
    cwds: &HashMap<u32, String>,
    user: &str,
    boot_time: i64,
) -> Vec<ProcessInfo> {
    let parent: HashMap<u32, u32> = all.iter().map(|p| (p.pid, p.ppid)).collect();
    let by_pid: HashMap<u32, &ProcessInfo> = all.iter().map(|p| (p.pid, p)).collect();
    let has_child: HashSet<u32> = all.iter().map(|p| p.ppid).collect();
    let ancestor = |pid: u32, pred: &dyn Fn(&ProcessInfo) -> bool| {
        let mut cur = pid;
        for _ in 0..64 {
            let Some(&pp) = parent.get(&cur) else {
                return false;
            };
            if pp <= 1 {
                return false;
            }
            if by_pid.get(&pp).is_some_and(|p| pred(p)) {
                return true;
            }
            cur = pp;
        }
        false
    };
    let is_probe = |p: &ProcessInfo| p.cmdline.contains(MARKER);
    let in_session = |p: &ProcessInfo| {
        p.cmdline.starts_with("sshd:")
            || MULTIPLEXERS
                .iter()
                .any(|m| program(&p.cmdline).starts_with(m))
    };
    let system_service = |p: &ProcessInfo| {
        p.ppid == 1
            && cwds.get(&p.pid).is_some_and(|c| c == "/")
            && SYSTEM_DIRS.iter().any(|d| p.cmdline.starts_with(d))
    };
    let in_container = |p: &ProcessInfo| program(&p.cmdline).starts_with("containerd-shim");

    all.iter()
        .filter(|p| {
            let prog = program(&p.cmdline);
            p.user == user
                && p.pid != 2
                && p.ppid != 2
                && !p.cmdline.starts_with('[')
                && !is_probe(p)
                && !ancestor(p.pid, &is_probe)
                && !NOISE.iter().any(|n| p.cmdline.starts_with(n) || prog == *n)
                && !prog.starts_with("containerd-shim")
                && !ancestor(p.pid, &in_container)
                && (!SHELLS.contains(&prog) || has_child.contains(&p.pid))
                && !(prog == "systemd" && p.cmdline.contains("--user"))
                && !system_service(p)
                && (p.started_at - boot_time > BOOT_GRACE_SECS || ancestor(p.pid, &in_session))
        })
        .map(|p| ProcessInfo {
            cmdline: redact(&p.cmdline),
            cwd: cwds.get(&p.pid).cloned(),
            ..p.clone()
        })
        .collect()
}

fn parse_containers(body: &str) -> Result<Vec<Container>, String> {
    let mut out = Vec::new();
    for line in body.lines().filter(|l| !l.trim().is_empty()) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            // e.g. "permission denied while trying to connect to the Docker daemon socket"
            return Err(line.trim().chars().take(160).collect());
        };
        let s = |k: &str| {
            v.get(k)
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_owned()
        };
        let labels = s("Labels");
        out.push(Container {
            id: s("ID"),
            name: s("Names"),
            image: s("Image"),
            state: s("State"),
            status: s("Status"),
            ports: s("Ports"),
            compose_project: labels
                .split(',')
                .find_map(|kv| kv.strip_prefix("com.docker.compose.project="))
                .map(str::to_owned),
        });
    }
    Ok(out)
}

fn parse_tmux_sessions(sessions: &str, panes: &str) -> Vec<TmuxSession> {
    // Active pane of the active window, per session.
    let mut active: HashMap<&str, (&str, &str)> = HashMap::new();
    for l in panes.lines() {
        let f: Vec<&str> = l.split(TMUX_SEP).collect();
        if let [session, "1", "1", cwd, cmd] = f[..] {
            active.insert(session, (cwd, cmd));
        }
    }
    sessions
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split(TMUX_SEP).collect();
            let [name, windows, attached, created] = f[..] else {
                return None;
            };
            let pane = active.get(name);
            Some(TmuxSession {
                name: name.to_owned(),
                windows: windows.parse().ok()?,
                attached: attached != "0",
                created: created.parse().unwrap_or(0),
                pane_cwd: pane.map(|p| p.0.to_owned()),
                pane_command: pane.map(|p| p.1.to_owned()),
            })
        })
        .collect()
}

fn parse_listeners(body: &str) -> Vec<Listener> {
    let mut seen = HashSet::new();
    let mut out: Vec<Listener> = body
        .lines()
        .filter_map(|l| {
            // LISTEN 0 4096 0.0.0.0:22 0.0.0.0:* users:(("sshd",pid=150476,fd=3))
            let f: Vec<&str> = l.split_whitespace().collect();
            let local = f.get(3)?;
            let (addr, port) = local.rsplit_once(':')?;
            let users = f.get(5..).map(|r| r.join(" ")).unwrap_or_default();
            let process = users
                .split("((\"")
                .nth(1)
                .and_then(|r| r.split('"').next())
                .map(str::to_owned);
            let pid = users
                .split("pid=")
                .nth(1)
                .and_then(|r| r.split(|c: char| !c.is_ascii_digit()).next())
                .and_then(|p| p.parse().ok());
            let addr = addr.trim_start_matches('[').trim_end_matches(']');
            let addr = addr.split('%').next().unwrap_or(addr).to_owned();
            let port = port.parse().ok()?;
            seen.insert((addr.clone(), port)).then(|| Listener {
                proto: "tcp".to_owned(),
                addr,
                port,
                pid,
                process,
            })
        })
        .collect();
    out.sort_by(|a, b| (a.port, &a.addr).cmp(&(b.port, &b.addr)));
    out
}

fn parse_repos(body: &str) -> Vec<RepoInfo> {
    body.lines()
        .filter_map(|l| {
            // path, branch, dirty, "behind ahead", sha, time, subject
            let f: Vec<&str> = l.splitn(7, '\t').collect();
            let path = (*f.first()?).to_owned();
            let mut ab = f.get(3).unwrap_or(&"").split_whitespace();
            let behind = ab.next().and_then(|v| v.parse().ok());
            let ahead = ab.next().and_then(|v| v.parse().ok());
            let last_commit = match (f.get(4), f.get(5), f.get(6)) {
                (Some(sha), Some(t), Some(subject)) if !sha.is_empty() => Some(Commit {
                    sha: (*sha).to_owned(),
                    time: t.parse().unwrap_or(0),
                    subject: (*subject).to_owned(),
                }),
                _ => None,
            };
            Some(RepoInfo {
                path,
                branch: f.get(1).filter(|b| !b.is_empty()).map(|b| (*b).to_owned()),
                dirty: f.get(2).and_then(|n| n.trim().parse().ok()).unwrap_or(0),
                ahead,
                behind,
                last_commit,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOOT: i64 = 1_791_891_400;
    const NOW: i64 = BOOT + 165_713;

    fn sample() -> String {
        format!(
            "{MARKER} system
{NOW}
root
lab07
6.8.0-31-generic
Ubuntu 24.04 LTS
165713.08 648305.01
1.00 1.02 1.04 2/384 153662
4
MemTotal:        4009348 kB
MemAvailable:    2854960 kB
btime {BOOT}
{MARKER} disks
/dev/mapper/ubuntu--vg-ubuntu--lv    49222292 11455872  35233660      25% /
/dev/vda2                             1992552   195216   1676096      11% /boot
{MARKER} processes
      1       0 root      165713  0.0 13824 /usr/lib/systemd/systemd --system
      2       0 root      165713  0.0     0 [kthreadd]
    600       1 root      165700  0.0  9000 /usr/lib/systemd/systemd-resolved
 136690       1 root       17316  0.0 11288 sshd: root@pts/6
 136700  136690 root       17300  0.0  5000 -bash
 136800  136700 root       17000 12.5 90000 pytest tests/group2 --token=abc123
 136900  136690 root       17300  0.0  5000 -bash
 152134       1 root        9000  1.0 20000 /usr/bin/open5gs-udrd -c /etc/open5gs/udr.yaml
 152200       1 root        9000  0.1  8000 /usr/bin/containerd-shim-runc-v2 -namespace moby
 152210  152200 root        9000  0.1  8000 ./uegnb config/conf.yaml
 153604  150476 root           0 18.7 10884 sshd: root@notty
 153660  153604 root           0  0.0  3712 /bin/sh -c m='{MARKER}'
 153673  153660 root           0  0.0  2304 ps -eo pid=
 160000       1 alice        900  0.0  2304 vim notes.md
 145470       1 root       12000  0.0  9000 /sbin/multipathd -d -s
 114372       1 root       85000  0.0  3000 SCREEN -S shetty
 114373  114372 root       85000  0.0  4000 /bin/bash
 114431  114373 root       85000  2.0 90000 claude --resume
{MARKER} cwds
136800\t/root/regal
152134\t/
145470\t/
{MARKER} containers
{{\"ID\":\"ff1903ec0d5b\",\"Image\":\"uegnb-ui:1.0.2\",\"Labels\":\"a=b,com.docker.compose.project=uegnb,c=d\",\"Names\":\"uegnb-ui\",\"Ports\":\"\",\"State\":\"running\",\"Status\":\"Up 45 hours\"}}
{MARKER} tmux
nightly<|>2<|>1<|>1791900000
{MARKER} tmux_panes
nightly<|>1<|>1<|>/root/regal<|>pytest
nightly<|>0<|>1<|>/tmp<|>bash
{MARKER} listeners
LISTEN 0      5         127.0.0.20:7777  0.0.0.0:* users:((\"open5gs-udrd\",pid=152134,fd=9))
LISTEN 0      4096         0.0.0.0:22    0.0.0.0:* users:((\"sshd\",pid=150476,fd=3),(\"systemd\",pid=1,fd=166))
LISTEN 0      4096            [::]:22       [::]:* users:((\"sshd\",pid=150476,fd=4))
LISTEN 0      4096   127.0.0.53%lo:53    0.0.0.0:*
{MARKER} repos
/root/regal\tgroup2-fix\t3\t1 2\tabc123\t1791900100\tFix: handle\ttabs in subject
/root/detached\t\t0\t\tdef456\t1791800000\tinit
{MARKER} end
"
        )
    }

    #[test]
    fn parses_every_section() {
        let snap = parse(&sample(), Utc::now());
        let sys = &snap.system;
        assert_eq!(
            (sys.hostname.as_str(), sys.os.as_str()),
            ("lab07", "Ubuntu 24.04 LTS")
        );
        assert_eq!(
            (sys.uptime_secs, sys.boot_time, sys.cpus),
            (165_713, BOOT, 4)
        );
        assert!(
            sys.load
                .iter()
                .zip([1.0, 1.02, 1.04])
                .all(|(a, b)| (a - b).abs() < 1e-6)
        );
        assert_eq!(sys.mem_used_kb, 4_009_348 - 2_854_960);
        assert_eq!(sys.disks.len(), 2);
        assert_eq!(sys.disks[1].mount, "/boot");

        let cmds: Vec<&str> = snap.processes.iter().map(|p| p.cmdline.as_str()).collect();
        assert_eq!(
            cmds,
            [
                "-bash",
                "pytest tests/group2 --token=***",
                "/usr/bin/open5gs-udrd -c /etc/open5gs/udr.yaml",
                "SCREEN -S shetty",
                "/bin/bash",
                "claude --resume",
            ],
            "kept: session work (redacted), a packaged daemon started later, a screen \
             session; dropped: boot services, a restarted system daemon, container internals"
        );
        let pytest = &snap.processes[1];
        assert_eq!(pytest.cwd.as_deref(), Some("/root/regal"));
        assert_eq!(pytest.started_at, NOW - 17_000);

        assert_eq!(snap.containers.len(), 1);
        assert_eq!(snap.containers[0].compose_project.as_deref(), Some("uegnb"));

        assert_eq!(snap.tmux.len(), 1);
        assert_eq!(snap.tmux[0].pane_cwd.as_deref(), Some("/root/regal"));
        assert_eq!(snap.tmux[0].pane_command.as_deref(), Some("pytest"));

        let l: Vec<(&str, u16, Option<&str>)> = snap
            .listeners
            .iter()
            .map(|l| (l.addr.as_str(), l.port, l.process.as_deref()))
            .collect();
        assert_eq!(
            l,
            [
                ("0.0.0.0", 22, Some("sshd")),
                ("::", 22, Some("sshd")),
                ("127.0.0.53", 53, None),
                ("127.0.0.20", 7777, Some("open5gs-udrd")),
            ]
        );
        assert_eq!(snap.listeners[3].pid, Some(152_134));

        let r = &snap.repos[0];
        assert_eq!(
            (r.branch.as_deref(), r.dirty, r.behind, r.ahead),
            (Some("group2-fix"), 3, Some(1), Some(2))
        );
        assert_eq!(
            r.last_commit.as_ref().unwrap().subject,
            "Fix: handle\ttabs in subject"
        );
        assert_eq!(snap.repos[1].branch, None);
        assert!(snap.unavailable.is_empty());
    }

    #[test]
    fn missing_tools_are_reported_not_fatal() {
        let out = format!(
            "{MARKER} system\n{NOW}\nbob\nh\nk\n\n1 1\n0 0 0 1/1 1\n1\n\
             {MARKER} containers\npermission denied while trying to connect to the Docker daemon socket\n\
             {MARKER} tmux\n!unavailable tmux not installed\n\
             {MARKER} listeners\n!unavailable ss not installed\n{MARKER} end\n"
        );
        let snap = parse(&out, Utc::now());
        assert_eq!(snap.system.hostname, "h");
        assert_eq!(
            snap.unavailable,
            [
                "tmux: tmux not installed",
                "listeners: ss not installed",
                "containers: permission denied while trying to connect to the Docker daemon socket"
            ]
        );
    }

    #[test]
    fn command_quotes_the_script() {
        let cmd = probe_command();
        assert!(cmd.starts_with("/bin/sh -c '"));
        assert!(cmd.contains(r"m='\''@@vmdesk@@'\''"));
    }
}
