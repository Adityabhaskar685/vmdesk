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
| M4 | State collectors, snapshots, timeline | Next |
| M5 | GPUI desktop app (MVP) | Planned |
| M6–M9 | Agent, work sessions, embedded libghostty terminal, first cloud provider | Planned |

## Requirements

- Stable Rust with edition 2024 support (1.85 or later)
- Linux. macOS should work for M1 but is untested.

SQLite is bundled through SQLx, so no system library is needed.

## Build

```sh
cargo build -p vmdesk-daemon          # produces target/debug/vmdesk
cargo build -p vmdesk-daemon --release
```

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
  vmdesk-transport/   SSH transport: path builder, session cache, host keys, auth
  vmdesk-reach/       Route lookup, reachability checks, network-change monitor
  vmdesk-collect/     State collectors (M4, stub)
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
