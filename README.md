# VMDesk

A local-first, single-user tool for keeping track of the development VMs you work on: where they
are, how to reach them, and (in later milestones) what you were doing on each one.

VMDesk is a native desktop app built with
[GPUI](https://github.com/zed-industries/zed/tree/main/crates/gpui) (Zed's UI framework). Everything
runs in one process on your laptop: state lives in a local SQLite database and every VM is reached
over SSH, directly or through jump hosts. The `vmdesk` CLI and a local HTTP API sit beside the app
as the machine interface, so coding agents and scripts can find and reach VMs without knowing IPs,
users or jump hosts. Nothing is installed on the VMs.

## Status

| Milestone | Scope | State |
| --- | --- | --- |
| M0 | Workspace, shared types, core traits | Done |
| M1 | Inventory: static + SSH config providers, SQLite, CRUD API | Done |
| M2 | Transport: Russh over connection paths (direct + jump), `vmdesk exec`, `vmdesk proxy` | Done |
| M3 | Reachability: route inspection, status signals, network-change monitor | Done |
| M4 | SSH collector, snapshots, timeline | Done |
| M5 | GPUI desktop app (MVP) | Done |
| M6 | On-VM agent: offline history | Done |
| M7 | Work sessions, current-work summary, event search | Next |
| M8–M9 | Embedded libghostty terminal, first cloud provider | Planned |

## Requirements

- Stable Rust (edition 2024; built and tested with 1.97)
- Linux with a Vulkan GPU driver for the desktop app. macOS should work for the CLI but is
  untested.

SQLite is bundled through SQLx, so the CLI needs no system library. The desktop app uses
[GPUI](https://github.com/zed-industries/zed/tree/main/crates/gpui) from Zed's `v1.24.2` release
tag and needs these development packages on Debian/Ubuntu:

```sh
sudo apt install libxkbcommon-x11-dev libwayland-dev libx11-xcb-dev libfontconfig-dev clang cmake
```

## Build

```sh
cargo build --release -p vmdesk-daemon -p vmdesk-app
# target/release/vmdesk      the CLI and API server
# target/release/vmdesk-app  the desktop app
```

Build both: the app's "Open terminal" runs `vmdesk proxy` and `vmdesk askpass` from the same
directory. The first build of the app takes several minutes (GPUI and its dependencies come from
the Zed repository). Use release builds for daily use; a debug build of the app uses about twice
the memory.

## Desktop app

```sh
target/release/vmdesk-app            # or: cargo run --release -p vmdesk-app
target/release/vmdesk-app lab-07     # open with a VM selected (name, alias or id)
```

The left side lists every VM with a status dot (green online, amber SSH failing, red no route,
grey not checked) and the network interface that reaches it. Under that is what you were doing
there, the current-work line that `vmdesk show` prints, when the VM's last snapshot shows a
command or tmux session of yours. Type anywhere to filter the list, use ↑/↓ to move through it
and Esc to clear the filter.

The right side shows the selected VM. Its header has the address and path, a status pill
(`Online + agent`, `SSH failing`, ...) with the SSH result, route and check time, and the agent's
state. Below it are three tabs:

- **Overview** starts with **Current work**: what you were doing on the VM, from its last snapshot
  and work session (see [What you were doing](#what-you-were-doing-and-where-you-ran-something)).
  It shows the command (or tmux session), the repo and branch it runs in with its uncommitted
  changes, its tmux session and since when, under the newest note on the latest session. The
  latest session follows: `started 16:40 · 55m · 3 repos` and `ongoing` while more activity
  would still extend it, else when it ran, such as `yesterday 16:40 – 17:38 · 58m`.
  **Add note** opens a field: Enter pins the note to the latest session, as `vmdesk note` does,
  and Esc cancels. Under the card, the last snapshot follows as cards: system (uptime, load,
  memory and disks), tmux sessions, containers, listening ports, git repositories and processes.
- **Sessions** lists the VM's work sessions, newest first, under the day each started. Each one
  has its title (`Untitled session` until you name it), start and end, duration, the repos it
  touched with their branches, its most-run commands and its notes. Click a title to rename the
  session: Enter saves (an empty title clears it) and Esc cancels. The × beside a note deletes
  it.
- **Timeline** tells each recorded event as a sentence under a day header (`Today`, `Yesterday`,
  `Thu 15 Oct`), at its local time, with where it came from (`agent`, `ssh` or `laptop`). For
  example, `Started python3 -m http.server 18766 in /tmp`, `SSH lost (timeout) via wlo1`, or
  `Not observed for 5h 32m`. `Agent connected and recording` marks the first sync after an install.
  Identical events in the same minute, such as many processes exiting at once, show as one row
  with a count.

**Search events**, at the foot of the list, or Ctrl+K, searches the events of every VM
the way `vmdesk search` does. Each word must match the start of a word in the event, so `upf`
finds `pytest upf-tests/n4.py`. The search runs once typing pauses. Results come newest first
under their day, each with the VM it happened on and the event as the timeline tells it (the
newest 100 at most). Click one to open that VM's Timeline. Esc closes the search.

Managing VMs:

- **Add VM** opens a form: name, address, port, user, how to log in (ssh-agent and default keys,
  a key file, or a password kept in the OS keychain) and an optional jump host. Tab moves between
  fields, Ctrl+V pastes, Enter saves, Esc cancels. The new VM is checked right away.
- The **⋯** (More) button beside **Open terminal** opens a menu with **Edit VM…**, **Remove VM…**
  and the agent action. Esc or a click outside closes it.
- **Edit VM…** opens the same form filled in. Leave the password empty to keep the stored one; the
  address and jump host are only saved if you change them, so editing an imported VM's user
  leaves its imported connection alone.
- **Remove VM…** deletes the VM from VMDesk after a confirmation, with its history, host keys and
  stored password. The VM itself is not touched. A VM that others jump through cannot be removed.

Working with a VM:

- **Check** re-runs the route and SSH check. **Refresh** collects a new snapshot; the open VM is
  also re-collected every 2 minutes while it is reachable.
- **Open terminal** writes an SSH config for the VM under `<data dir>/ssh/` and starts your terminal
  with `ssh` in it, over the same path VMDesk uses (jump hosts through `vmdesk proxy`).
  - The terminal is `$VMDESK_TERMINAL` if set (a command that takes the program to run, e.g.
    `ghostty -e`), else the first installed of Ghostty, `x-terminal-emulator`, Ptyxis,
    GNOME Terminal, kitty and xterm.
  - `ssh` runs with `TERM=xterm-256color` and `COLORTERM=truecolor`, as Ghostty's `ssh-env`
    integration would set them: VMs rarely have terminfo for `xterm-ghostty` or `xterm-kitty`.
  - For a VM with a stored password, `ssh` gets it from the keychain through `vmdesk askpass` (an
    `SSH_ASKPASS` helper), so you are not asked. Only the first password prompt is answered; other
    prompts, such as accepting a new host key or a retry after a wrong password, are shown to you.
- The [on-VM agent](#on-vm-agent) has its own item next to the status: `Agent 0.1.0 · recording ·
  synced 2m ago`, `Agent 0.0.9 · outdated · synced 2m ago`, `Agent not running · the next poll
  starts it`, `Agent not answering properly · collecting over SSH`, or `No agent`. While the agent
  collects, the list and the header say `online + agent`, as `vmdesk list` does. The More menu
  has one agent action:
  - **Install agent** when the VM has none, or the binary is gone from the VM. It runs the same
    steps as `vmdesk agent install`, with the build from `VMDESK_AGENT_BIN`, else
    `vmdesk-agent-x86_64` next to `vmdesk-app`.
  - **Upgrade agent** when the agent is outdated: the same steps with this build.
  - **Remove agent…** otherwise. It asks first, then stops and deletes the agent on the VM. The
    VM's history on this laptop is kept.
- **Review host key…** appears when a VM's host key is new or has changed. A new key is shown with
  its fingerprint and whether it matches your `~/.ssh/known_hosts`; **Trust this key** stores
  exactly that key. A changed key can be forgotten (only after a reinstall) and the new one
  reviewed.

Text is set in Noto Sans and Noto Sans Mono (the `fonts-noto-core` and `fonts-noto-mono` packages
on Ubuntu). GPUI cannot pick bold or medium weights from a variable font such as Ubuntu's own, so
headings would lose their weight in it. Without Noto, another installed font is used.

The window draws its own title bar, since GNOME does not decorate Wayland windows: drag it to
move, double-click to maximise, right-click for the window menu, use the minimise / maximise /
close buttons, and resize from the edges.

The app runs the same reachability monitor and snapshot poller as `vmdesk serve`, in the same
process, on the same database as the CLI. Nothing else needs to be running.

Memory: a release build uses about 70 MB on an Intel laptop. GPUI's renderer would otherwise load
every installed graphics driver, including Mesa's GL stack and software renderer with `libLLVM`
(about 55 MB more). So at start the app re-executes itself once with the graphics loaders limited
to the Vulkan drivers of the GPUs actually present and no GL probe; programs it starts, such as the
terminal, get the normal environment back. Set `VMDESK_ALL_GPU_DRIVERS=1` to keep every driver
(needed only on a machine without a hardware Vulkan driver).

## Usage

```sh
# Preview what would be imported from ~/.ssh/config
vmdesk import --dry-run

# Import it (re-running refreshes hosts; removed hosts are marked terminated)
vmdesk import
vmdesk import --ssh-config ./other-config

# Show the inventory
vmdesk list
vmdesk list --tag regal
vmdesk list bastion            # match on name, address or tag

# Run the local API on 127.0.0.1:7878
vmdesk serve
vmdesk serve --listen 127.0.0.1:9000
```

### Running commands and connecting

VMs are named by name, alias or id. Each is reached over its connection paths in order, through
any jump hosts, so you never need its IP, user or bastion:

```sh
vmdesk exec regal-dev-07 uname -a          # exits with the remote command's status
vmdesk exec regal-dev-07 -- ls -la /tmp    # arguments are joined and run by the remote shell
vmdesk exec -l root regal-dev-07 id        # another user for this run only

# Add a VM by hand, here behind a jump host (repeat --via for a chain)
vmdesk add lab-07 172.16.9.89 -l root --via bastion-01

# Log in with a password kept in the OS keychain (prompts, or reads a line from stdin)
vmdesk set-password lab-07

# Remove a VM, with its history, host keys and stored password
vmdesk remove lab-07

# Change the stored login (re-importing never overwrites it)
vmdesk edit regal-dev-07 --user root
vmdesk edit regal-dev-07 --key ~/.ssh/id_lab   # or --agent to go back to ssh-agent

# Plain ssh, scp or VS Code Remote over the same path
ssh -o ProxyCommand='vmdesk proxy %n' regal-dev-07
```

Authentication tries the VM's key file or keychain password, then ssh-agent, then
`~/.ssh/id_ed25519`, `id_ecdsa` and `id_rsa`. Passphrase-protected keys must be loaded in
ssh-agent. Passwords live in the OS keychain (Secret Service, e.g. GNOME Keyring); the database
only stores the entry name.

Host keys are checked on every hop and stored in VMDesk's database (`ssh` checks the target
itself under `proxy`):

- An unknown key is rejected with its fingerprint. Check it, then rerun with
  `--accept-new-host-keys` to trust it.
- A changed key always blocks the connection. If the VM was reinstalled, run
  `vmdesk forget-host-key <vm>` and connect again with `--accept-new-host-keys`.

Logs go to stderr (`RUST_LOG`), so stdout carries only the command output or proxied stream.

### Reachability

```sh
vmdesk status                       # check every VM now
vmdesk status lab-07 bastion-01     # or just these
vmdesk status --watch lab-07        # keep watching and print changes
```

```text
NAME         STATE                  SSH      AGENT    DETAIL
bastion-01   online (wlo1)          ok       unknown
lab-07       online + agent (wlo1)  ok       ok
old-box      ssh failing            timeout  unknown  timed out: old-box: 10.1.2.3:22: timed out
```

Each check looks up which laptop interface routes to the VM (or its first jump host), then logs
in over the VM's path with a 3 s timeout per hop. The result is one of `ok`, `auth_failed`,
`host_key`, `refused`, `timeout`, `no_route` or `error`, summarised as *online*, *ssh failing*,
*no route*, *stopped* or *never checked*, plus *+ agent* while the VM's agent records it (see
[On-VM agent](#on-vm-agent)). `vmdesk list` shows the last result.

`status --watch` and `vmdesk serve` keep checking: every VM when the laptop's addresses change
(VPN up or down, Wi-Fi switch, after a 2 s settle), favourites every minute and the rest every five
minutes, at most 32 at a time. In the background, VMs whose last login failed are only checked for
an open SSH port, so they are not hit with failing logins; `vmdesk status` retries the login.
Status changes are recorded on each VM's timeline.

### What is on a VM, and what happened there

```sh
vmdesk show lab-07              # collect now: system, tmux, containers, ports, repos, processes
vmdesk show lab-07 --cached     # the last snapshot, without connecting
vmdesk timeline lab-07 -n 100   # events, oldest first
```

`show` runs one shell script over a single SSH exec; nothing is installed on the VM. Missing
tools (no Docker, no tmux, Docker permission denied) are reported, not fatal. Processes are
limited to the login user's own work: kernel threads, idle shells, container internals and system
services are left out, and command lines are redacted (`--password=`, `token=`, AWS keys).
Repos are found from process and tmux working directories.

Each snapshot is compared with the previous one, and the differences become timeline events:
processes and containers starting and stopping, tmux sessions, git branch changes, commits and
uncommitted changes, ports opening and closing, reboots, and gaps when the VM was not observed for
45 minutes. A process only counts if it ran 30 s, is on a watchlist (`pytest`, `make`, `cargo`,
`go`, `kubectl`, `helm`, ...) or runs in a tmux pane. `vmdesk serve` collects every 15 minutes
from VMs whose last status check succeeded. Snapshots are kept compressed: all from the last
48 hours, then hourly for 30 days, then daily.

### What you were doing, and where you ran something

`vmdesk show` (with or without `--cached`) starts with a one-line summary of the current work,
under the note you left on the VM's latest work session, if any:

```text
note: waiting on CI for the group2 fix
pytest tests/group2 in regal @ group2-fix (3 modified) · tmux nightly-debug · since 21:07
```

The line comes from the latest snapshot. Its subject is the first of these that exists, the
newest first within each:

1. A command running in an attached tmux session.
2. An attached tmux session.
3. A command running in any tmux pane.
4. A command started from a login shell, as in a plain SSH session.
5. Any other process that the timeline records.
6. A detached tmux session.

Login shells and tmux itself are never the subject. The repo is the one holding the command's
working directory (else the directory is shown), with its branch and uncommitted changes. Running
containers whose compose project or name is the repo's name are listed. "since" is when the
current work session started, else when the command or tmux session started.

A work session is a stretch of activity on one VM: every timeline event except status changes and
gaps. An idle stretch of more than 2 hours starts a new session. Changes that a poll noticed only
after a gap do not count, because they happened at some unknown time inside the gap. Sessions
are kept up to date as events arrive, including agent history that arrives late, which can merge
two sessions into one. A merged session keeps the older one's id, title and notes.

```sh
vmdesk sessions lab-07 -n 20                           # newest first
vmdesk sessions rename lab-07 01a153da nightly debug   # by id or its first characters
vmdesk note lab-07 waiting on CI for the group2 fix    # pinned to the latest session
vmdesk note lab-07 flaky N4 test --session 01a13ee1    # or to another one
vmdesk notes lab-07
vmdesk search upf-tests --since 7d -n 20               # every VM, newest first
```

```text
ID        STARTED      ENDED    DURATION  TITLE          REPOS                 COMMANDS
01a153da  10-19 16:40  ongoing  55m       nightly debug  regal @ group2-fix    pytest tests/group2 ×3 · make build
01a13ee1  10-15 14:55  17:15    2h19m                    5G_Test_Suites        claude · tmux a -t uegnb-16
```

A session id's first 8 characters tell it apart from the VM's other sessions. A session lists the
repos it touched (with the branches in its git events), the commands started in it (most runs
first), its containers and its tmux sessions.

`search` looks through the command lines, directories, repos, branches, commit subjects,
containers, tmux sessions, and listening ports and processes of every event. Each word must match the start
of a word in the event: `upf-tests` finds `pytest upf-tests/n4.py`, and `http.ser` finds
`python3 -m http.server`. Quotes and operators such as `OR` are ordinary words. `--since` takes an
age such as `30m`, `12h`, `7d` or `2w`.

### On-VM agent

The SSH collector only sees what exists at each poll. The agent runs on the VM as your user,
records events into a local log, and hands the backlog over when the laptop connects again, so
what happened while the laptop was off or asleep shows up on reconnect. It opens no port: the
laptop talks to it over the VM's SSH path.

```sh
vmdesk agent install lab-07 --binary target/x86_64-unknown-linux-musl/agent/vmdesk-agent
vmdesk agent status            # sync every VM that has an agent and show its state
vmdesk agent sync lab-07       # fetch new events and a snapshot now
vmdesk agent upgrade lab-07    # the same steps as install, with this build
vmdesk agent uninstall lab-07  # stop and remove it; the history on this laptop is kept
```

```text
NAME    STATE  VERSION  LAST SEQ  LAST CONTACT  DETAIL
lab-07  ok     0.1.0    1834      0s ago        ok, 12 new event(s)
```

`install` checks that the VM is x86_64, sends the binary through an SSH exec channel's stdin to
`~/.local/share/vmdesk/agent/` (mode `0700`), compares its SHA-256 on the VM with the local
file and runs it once before renaming it into place, then runs `vmdesk-agent install`, which
sets up a `systemd --user` service (or a `nohup` daemon where that is unavailable). Each step
can be repeated, so an interrupted install is fixed by running it again. The binary comes from
`--binary`, else `VMDESK_AGENT_BIN`, else `vmdesk-agent-x86_64` next to the `vmdesk` executable.

Once a VM has an agent, `vmdesk show` and the background poller sync it instead of running the
probe script. Events keep the agent's own sequence numbers, so a repeated or interrupted sync
stores each one once; events the agent rotated out before the laptop read them become a gap on
the timeline. If the agent's daemon is not running, the poll still reads its history up to
when it stopped, starts it, and leaves the span since then to the daemon, which diffs from its
own last snapshot: no span is recorded twice. A missing or misbehaving agent falls back to SSH. The agent state (`ok`,
`outdated`, `not_running`, `absent`, `error`) is in `vmdesk status`. An outdated agent keeps
working; upgrades are never automatic.

### SSH config import

Each concrete `Host` block becomes a VM. Wildcard patterns (`Host *.internal`) are skipped.

- `HostName`, `Port` and `User` become the VM's endpoint and username. Without a `User`, your
  local login name is used, as `ssh` does.
- Extra names on the `Host` line are stored as the `aliases` label.
- `ProxyJump a,b` becomes a connection path that jumps through `a`, then `b`. Jump hosts must
  also be in the inventory; otherwise the VM is imported without a path and a warning is shown.

### Data directory

| Platform | Default |
| --- | --- |
| Linux | `~/.local/share/vmdesk/` |
| macOS | `~/Library/Application Support/vmdesk/` |

Override with `--data-dir <path>` or `VMDESK_DATA_DIR`. The directory holds:

- `vmdesk.db`: the SQLite database. Migrations run automatically on start.
- `api-token`: the API bearer token, created on first `serve` with mode `0600`.
- `ssh/`: what "Open terminal" writes: an SSH config per VM, plus an askpass helper for VMs
  with a stored password.

Passwords are not in the data directory; they live in the OS keychain.

Log verbosity is controlled with `RUST_LOG` (default `info`).

## HTTP API

The API is the interface for coding agents and scripts; the desktop app doesn't use it. It listens
on loopback only (`serve` refuses any other address), and every route except `/api/v1/health`
requires the token from `api-token`:

```sh
TOKEN=$(cat ~/.local/share/vmdesk/api-token)
curl -H "Authorization: Bearer $TOKEN" http://127.0.0.1:7878/api/v1/vms
```

| Method | Route | Description |
| --- | --- | --- |
| `GET` | `/api/v1/health` | Status, version and VM count. No token needed. |
| `GET` | `/api/v1/vms` | List VMs. Filters: `?tag=`, `?provider=`, `?q=` |
| `POST` | `/api/v1/vms` | Create a VM |
| `GET` | `/api/v1/vms/{id}` | Get one VM |
| `PATCH` | `/api/v1/vms/{id}` | Update a VM. Absent fields are left alone. |
| `DELETE` | `/api/v1/vms/{id}` | Delete a VM |
| `POST` | `/api/v1/import/ssh-config` | Import an SSH config. Body: `{"path": "..."}` (optional) |
| `GET` | `/api/v1/status` | Last check result of every VM |
| `GET` | `/api/v1/vms/{id}/status` | Last check result of one VM |
| `POST` | `/api/v1/vms/{id}/check` | Check one VM now and return the result |
| `GET` | `/api/v1/vms/{id}/snapshot` | Latest snapshot (404 if none yet) |
| `POST` | `/api/v1/vms/{id}/snapshot` | Collect a snapshot now (502 if unreachable) |
| `GET` | `/api/v1/vms/{id}/events` | Timeline, newest first. `?limit=` (default 100), `?before=` (RFC 3339) |
| `GET` | `/api/v1/vms/{id}/agent` | The VM's agent: id, version, last seq, contact times and state (404 if none) |
| `GET` | `/api/v1/vms/{id}/sessions` | Work sessions, newest first, with `ongoing`. `?limit=` (default 20, max 500) |
| `PATCH` | `/api/v1/sessions/{id}` | Set a session's title. Body: `{"title": "..."}`; `""` clears it |
| `GET` | `/api/v1/vms/{id}/notes` | Notes, newest first |
| `POST` | `/api/v1/vms/{id}/notes` | Add a note. Body: `{"body": "...", "session": "<id>"}`. Without `session` it goes to the latest session; `null` pins it to none |
| `GET` | `/api/v1/vms/{id}/current-work` | The current-work summary and its `line` (404 before the first snapshot) |
| `GET` | `/api/v1/search` | Events on every VM matching `?q=`, newest first. `?since=` (RFC 3339), `?limit=` (default 50, max 500) |

Create a VM that is reached through another one:

```sh
curl -X POST http://127.0.0.1:7878/api/v1/vms \
  -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d '{
    "name": "regal-dev-07",
    "username": "aditya",
    "credential": { "type": "key_file", "value": "/home/aditya/.ssh/id_ed25519" },
    "endpoints": [{ "address": "10.24.18.32", "port": 22 }],
    "paths": [[{ "kind": "jump", "via": "<bastion-vm-id>" }, { "kind": "direct" }]],
    "tags": ["regal"]
  }'
```

Defaults: `port` 22, `credential` `{"type": "ssh_agent"}`, and a single direct path when `paths`
is omitted.

Errors are always JSON, `{"error": "<code>", "message": "..."}`:

| Status | `error` | When |
| --- | --- | --- |
| 400 | `bad_request` | Malformed JSON, unknown field, bad UUID or query |
| 401 | `unauthorized` | Missing or wrong token |
| 404 | `not found` | No such VM or session |
| 409 | `conflict` | Duplicate name (case-insensitive), jump-host loop, or deleting a VM still used as a jump host |
| 422 | `invalid` | Validation failure, e.g. bad name, no endpoints, unknown jump host, empty note, a session of another VM, a search with no letters or digits |
| 500 | `internal` | Anything else; details go to the server log only |

## Repository layout

```text
crates/
  vmdesk-proto/       Snapshot, event and wire-protocol types shared with the agent
  vmdesk-core/        Domain model and traits: Provider, HopConnector, Collector
  vmdesk-db/          SQLite pool, migrations, inventory CRUD, provider sync, work sessions,
                      notes, event search
  vmdesk-providers/   Static and SSH config providers
  vmdesk-api/         Actix Web routes and token auth
  vmdesk-daemon/      The `vmdesk` binary: CLI and server
  vmdesk-app/         The desktop app (GPUI)
  vmdesk-transport/   SSH transport: path builder, session cache, host keys, auth
  vmdesk-reach/       Route lookup, reachability checks, network-change monitor
  vmdesk-collect/     Probe script and parsers, snapshot diff to events, SSH collector, agent
                      sync and install, poller, current-work summary
  vmdesk-agent/       On-VM agent (M6, stub)
migrations/           SQLx migrations, embedded at compile time
```

## Development

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
```

`cargo build` and `cargo test` without `--workspace` only cover `vmdesk-daemon`, the default
member. The app's tests (`cargo test -p vmdesk-app`) drive a real window headlessly through GPUI's
test platform on an in-memory database; nothing appears on screen.

GPUI is pinned to a stable Zed release tag in the workspace `Cargo.toml`. Bump the tag
deliberately: GPUI's API changes between releases.

Migrations are embedded into the binary, so add a new numbered file under `migrations/` rather
than editing an applied one; changing an applied migration makes existing databases fail with a
checksum error.
