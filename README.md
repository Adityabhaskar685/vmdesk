# VMDesk

A local-first, single-user tool for keeping track of the development VMs you work on: where they
are, how to reach them, and (in later milestones) what you were doing on each one.

VMDesk will be a native desktop app built with
[GPUI](https://github.com/zed-industries/zed/tree/main/crates/gpui) (Zed's UI framework), with an
embedded terminal based on libghostty. Everything runs in one process on your laptop: state lives in
a local SQLite database and every VM is reached over SSH. The `vmdesk` CLI and a local HTTP API sit
beside the app as the machine interface, so coding agents and scripts can find and reach VMs without
knowing IPs, users or jump hosts. The desktop app starts at M5.

## Status

| Milestone | Scope | State |
| --- | --- | --- |
| M0 | Workspace, shared types, core traits | Done |
| M1 | Inventory: static + SSH config providers, SQLite, CRUD API | Done |
| M2 | Transport: Russh over connection paths (direct + jump), `vmdesk exec`, `vmdesk proxy` | Done |
| M3 | Reachability: route inspection, status signals, network-change monitor | Done |
| M4 | SSH collector, snapshots, timeline | Done |
| M5 | GPUI desktop app (MVP) | Done |
| M6 | On-VM agent: offline history | Next |
| M7–M9 | Work sessions, embedded libghostty terminal, first cloud provider | Planned |

## Requirements

- Stable Rust with edition 2024 support (1.85 or later)
- Linux. macOS should work for the CLI but is untested.

SQLite is bundled through SQLx, so the CLI needs no system library. The desktop app uses
[GPUI](https://github.com/zed-industries/zed/tree/main/crates/gpui) from Zed's `v1.24.2` release
tag and needs these development packages on Debian/Ubuntu:

```sh
sudo apt install libxkbcommon-x11-dev libwayland-dev libx11-xcb-dev libfontconfig-dev clang cmake
```

## Build

```sh
cargo build -p vmdesk-daemon          # produces target/debug/vmdesk
cargo build -p vmdesk-daemon --release
```

## Desktop app

```sh
cargo run -p vmdesk-app              # or target/debug/vmdesk-app
cargo run -p vmdesk-app -- lab-07    # open with a VM selected
```

The window lists every VM with its status on the left, and the selected VM on the right: an
overview (system, tmux, containers, ports, repos, processes) and its timeline. Type anywhere to
filter the list, use ↑/↓ to move through it and Esc to clear the filter.

- **+ Add VM** opens a form: name, address, port, user, how to log in (ssh-agent and default keys,
  a key file, or a password kept in the OS keychain) and an optional jump host. Tab moves between
  fields, Enter adds, Esc cancels.
- **Edit** opens the same form filled in. Leave the password empty to keep the stored one;
  the address and jump host are only saved if you change them.
- **Review host key…** appears when a VM's key is new or has changed. A new key is shown with its
  fingerprint and whether it matches your `~/.ssh/known_hosts`; **Trust this key** stores exactly
  that key. A changed key can be forgotten (only do that after a reinstall) and the new one
  reviewed.
- The window draws its own title bar (GNOME does not decorate Wayland windows): drag it to move,
  double-click to maximise, right-click for the window menu, and resize from the edges.
- **Remove** deletes the VM from VMDesk after a confirmation, with its history, host keys and
  stored password. The VM itself is not touched. A VM other VMs jump through cannot be removed.
- **Check** re-runs the route and SSH check, **Refresh** collects a new snapshot, and the open VM
  is re-collected every 2 minutes while it is reachable.
- **Open terminal** writes an SSH config for the VM under `<data dir>/ssh/` and starts your terminal
  with `ssh` in it. Jump hosts go through `vmdesk proxy`, so build the CLI as well. The terminal is
  `$VMDESK_TERMINAL` if set (a command that takes the program to run, e.g. `ghostty -e`), else the
  first of Ghostty, `x-terminal-emulator`, Ptyxis, GNOME Terminal, kitty and xterm.
  `ssh` runs with `TERM=xterm-256color`, as Ghostty's `ssh-env` integration would set it: VMs
  rarely have terminfo for `xterm-ghostty` or `xterm-kitty`.
  For a VM with a stored password, `ssh` gets it from the keychain through `vmdesk askpass` (an
  `SSH_ASKPASS` helper), so you are not asked. Only the first password prompt is answered; other
  prompts, such as accepting a new host key or a retry after a wrong password, are shown to you.

Memory: build with `--release` (about 70 MB on an Intel laptop; a debug build maps a much larger
binary). At start the app re-executes itself once so the graphics loaders only load the Vulkan
driver for the GPUs actually present and skip the GL probe; otherwise Mesa's software renderer and
`libLLVM` add about 55 MB. Set `VMDESK_ALL_GPU_DRIVERS=1` to keep every driver (needed only on a
machine without a hardware Vulkan driver).

The app runs the same reachability monitor and snapshot poller as `vmdesk serve`, in the same
process, on the same database as the CLI. Nothing else needs to be running.

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
NAME         STATE          SSH      DETAIL
bastion-01   online (wlo1)  ok
lab-07       online (wlo1)  ok
old-box      ssh failing    timeout  timed out: old-box: 10.1.2.3:22: timed out
```

Each check looks up which laptop interface routes to the VM (or its first jump host), then logs
in over the VM's path with a 3 s timeout per hop. The result is one of `ok`, `auth_failed`,
`host_key`, `refused`, `timeout`, `no_route` or `error`, summarised as *online*, *ssh failing*,
*no route*, *stopped* or *never checked*. `vmdesk list` shows the last result.

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

Log verbosity is controlled with `RUST_LOG` (default `info`).

## HTTP API

The API is the interface for coding agents and scripts; the desktop app doesn't use it. It listens on loopback only; `serve` refuses any other address. Every route except
`/api/v1/health` requires the token from `api-token`:

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
| 404 | `not found` | No such VM |
| 409 | `conflict` | Duplicate name (case-insensitive), jump-host loop, or deleting a VM still used as a jump host |
| 422 | `invalid` | Validation failure, e.g. bad name, no endpoints, unknown jump host |
| 500 | `internal` | Anything else; details go to the server log only |

## Repository layout

```text
crates/
  vmdesk-proto/       Snapshot, event and wire-protocol types shared with the agent
  vmdesk-core/        Domain model and traits: Provider, HopConnector, Collector
  vmdesk-db/          SQLite pool, migrations, inventory CRUD, provider sync
  vmdesk-providers/   Static and SSH config providers
  vmdesk-api/         Actix Web routes and token auth
  vmdesk-daemon/      The `vmdesk` binary: CLI and server
  vmdesk-app/         The desktop app (GPUI)
  vmdesk-transport/   SSH transport: path builder, session cache, host keys, auth
  vmdesk-reach/       Route lookup, reachability checks, network-change monitor
  vmdesk-collect/     Probe script and parsers, snapshot diff to events, SSH collector, poller
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
member.

Migrations are embedded into the binary, so add a new numbered file under `migrations/` rather
than editing an applied one; changing an applied migration makes existing databases fail with a
checksum error.
