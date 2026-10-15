//! "Open terminal" (spec section 12, stage 1): writes an SSH config fragment for the VM
//! and starts the user's terminal with `ssh -F <fragment> <vm>`. Jump paths go through
//! `vmdesk proxy`, so any path the transport can build also works for plain `ssh`.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context as _, bail};
use vmdesk_core::{CredentialRef, Hop, Vm};

/// Terminals tried in order when `VMDESK_TERMINAL` is not set, with the arguments that
/// come before the command to run.
const TERMINALS: [(&str, &[&str]); 6] = [
    ("ghostty", &["-e"]),
    ("x-terminal-emulator", &["-e"]),
    ("ptyxis", &["--"]),
    ("gnome-terminal", &["--"]),
    ("kitty", &[]),
    ("xterm", &["-e"]),
];

/// Writes the fragment, launches the terminal and returns a one-line description.
pub fn open(vm: &Vm, all: &[Vm], data_dir: &Path) -> anyhow::Result<String> {
    let dir = data_dir.join("ssh");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let fragment = dir.join(format!("{}.conf", vm.id));
    std::fs::write(&fragment, config(vm, all, &vmdesk_binary()))
        .with_context(|| format!("writing {}", fragment.display()))?;

    let askpass = match &vm.credential {
        CredentialRef::KeyChain(entry) => Some(write_askpass(&dir, vm, entry, &vmdesk_binary())?),
        _ => None,
    };
    let ssh = ssh_command(vm, &fragment, askpass.as_deref());
    let (program, prefix) = terminal()?;
    let mut cmd = Command::new(&program);
    crate::gpu::restore_child_env(&mut cmd);
    cmd.args(&prefix)
        .args(&ssh)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("starting {program}"))?;
    Ok(format!("opened {program} for {}", vm.name))
}

/// `ssh -F <fragment> <alias>` under `env`. The variables are set on the command itself
/// because terminals that reuse a running instance do not pass our environment on.
///
/// `TERM=xterm-256color` does what Ghostty's `ssh-env` shell integration does (that only
/// wraps `ssh` typed in a shell, not one the terminal starts directly): remote machines
/// rarely have terminfo for `xterm-ghostty` or `xterm-kitty`, and every one knows
/// `xterm-256color`. Password VMs also get `SSH_ASKPASS` pointing at the helper.
fn ssh_command(vm: &Vm, fragment: &Path, askpass: Option<&Path>) -> Vec<String> {
    let mut cmd = vec![
        "env".to_owned(),
        "TERM=xterm-256color".to_owned(),
        "COLORTERM=truecolor".to_owned(),
    ];
    if let Some(helper) = askpass {
        cmd.extend([
            format!("SSH_ASKPASS={}", helper.display()),
            "SSH_ASKPASS_REQUIRE=force".to_owned(),
        ]);
    }
    cmd.extend([
        "ssh".to_owned(),
        "-F".to_owned(),
        fragment.display().to_string(),
        host_alias(vm),
    ]);
    cmd
}

/// `SSH_ASKPASS` must name a program without arguments, so each password VM gets a
/// two-line script that runs `vmdesk askpass <entry>` with the prompt.
fn write_askpass(dir: &Path, vm: &Vm, entry: &str, vmdesk: &Path) -> anyhow::Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;

    let path = dir.join(format!("{}.askpass", vm.id));
    let script = format!(
        "#!/bin/sh\nexec '{}' askpass '{}' \"$@\"\n",
        vmdesk.display(),
        entry.replace('\'', ""),
    );
    std::fs::write(&path, script).with_context(|| format!("writing {}", path.display()))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
    Ok(path)
}

/// The `Host` name used inside the fragment; VM names may contain characters ssh treats
/// specially, so the id is used.
fn host_alias(vm: &Vm) -> String {
    format!("vmdesk-{}", vm.id)
}

/// The SSH config for `vm`. Direct VMs get their address; anything with a jump host
/// gets a `ProxyCommand` through `vmdesk proxy`, which checks the jump hosts' keys.
fn config(vm: &Vm, all: &[Vm], vmdesk: &Path) -> String {
    let mut out = format!(
        "# Written by VMDesk for {}\nHost {}\n",
        vm.name,
        host_alias(vm)
    );
    let ep = vm.endpoints.first();
    let _ = writeln!(
        out,
        "  HostName {}",
        ep.map_or(vm.name.as_str(), |e| e.address.as_str())
    );
    let _ = writeln!(out, "  Port {}", ep.map_or(22, |e| e.port));
    let _ = writeln!(out, "  User {}", vm.username);
    // Like Ghostty's ssh-env: offer the colour capability (servers that accept it get it).
    let _ = writeln!(out, "  SetEnv COLORTERM=truecolor");
    if let CredentialRef::KeyFile(key) = &vm.credential {
        let _ = writeln!(out, "  IdentityFile {}", key.display());
    }
    let jumps = vm
        .paths
        .first()
        .is_some_and(|p| p.hops.iter().any(|h| matches!(h, Hop::Jump { .. })));
    if jumps {
        let _ = writeln!(out, "  ProxyCommand {} proxy {}", vmdesk.display(), vm.id);
        // Without this ssh would record the target's key under the alias only.
        let _ = writeln!(out, "  HostKeyAlias {}", vm.name);
    }
    let names: Vec<&str> = vm
        .paths
        .first()
        .map(|p| {
            p.jumps_hosts()
                .filter_map(|id| all.iter().find(|v| v.id == id).map(|v| v.name.as_str()))
                .collect()
        })
        .unwrap_or_default();
    if !names.is_empty() {
        let _ = writeln!(out, "  # via {}", names.join(" -> "));
    }
    out
}

/// `vmdesk` next to this binary (a cargo build puts both in `target/<profile>`), else
/// whatever is on `PATH`.
fn vmdesk_binary() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|d| d.join("vmdesk")))
        .filter(|p| p.is_file())
        .unwrap_or_else(|| PathBuf::from("vmdesk"))
}

/// `VMDESK_TERMINAL` (a command line, e.g. `ghostty -e`), else the first known terminal
/// that is installed.
fn terminal() -> anyhow::Result<(String, Vec<String>)> {
    if let Ok(cmd) = std::env::var("VMDESK_TERMINAL") {
        let mut parts = cmd.split_whitespace().map(str::to_owned);
        if let Some(program) = parts.next() {
            return Ok((program, parts.collect()));
        }
    }
    for (program, prefix) in TERMINALS {
        if which(program) {
            return Ok((
                program.to_owned(),
                prefix.iter().map(|s| (*s).to_owned()).collect(),
            ));
        }
    }
    bail!("no terminal found; set VMDESK_TERMINAL, e.g. \"gnome-terminal --\"")
}

fn which(program: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use vmdesk_core::{
        ConnectionPath, Endpoint, EndpointKind, Lifecycle, Origin, ProviderId, VmId,
    };

    use super::*;

    fn vm(name: &str, addr: &str, paths: Vec<ConnectionPath>) -> Vm {
        Vm {
            id: VmId::new(),
            name: name.into(),
            provider: ProviderId::new(ProviderId::STATIC),
            external_ref: None,
            username: "root".into(),
            credential: CredentialRef::SshAgent,
            endpoints: vec![Endpoint {
                address: addr.into(),
                port: 2222,
                kind: EndpointKind::Private,
                source: Origin::User,
                bind_interface: None,
                last_success: None,
            }],
            paths,
            tags: vec![],
            labels: BTreeMap::new(),
            lifecycle: Lifecycle::Unknown,
            favourite: false,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn direct_and_jump_fragments() {
        let bastion = vm("bastion", "10.30.30.51", vec![]);
        let mut lab = vm(
            "lab",
            "172.16.9.89",
            vec![ConnectionPath::via(&[bastion.id], Origin::User)],
        );
        lab.credential = CredentialRef::KeyFile("/home/a/.ssh/id_lab".into());
        let all = [bastion.clone(), lab.clone()];
        let bin = Path::new("/opt/vmdesk");

        let direct = config(&bastion, &all, bin);
        assert!(direct.contains("HostName 10.30.30.51\n  Port 2222\n  User root\n"));
        assert!(!direct.contains("ProxyCommand"));

        let jumped = config(&lab, &all, bin);
        assert!(jumped.contains(&format!("ProxyCommand /opt/vmdesk proxy {}", lab.id)));
        assert!(jumped.contains("IdentityFile /home/a/.ssh/id_lab"));
        assert!(jumped.contains("HostKeyAlias lab"));
        assert!(jumped.contains("# via bastion"));
    }

    #[test]
    fn password_vms_get_the_askpass_helper() {
        let v = vm("lab", "172.16.9.89", vec![]);
        let frag = Path::new("/d/ssh/x.conf");
        let plain = ssh_command(&v, frag, None);
        assert_eq!(
            &plain[..4],
            ["env", "TERM=xterm-256color", "COLORTERM=truecolor", "ssh"]
        );

        let dir = std::env::temp_dir().join(format!("vmdesk-askpass-{}", v.id));
        std::fs::create_dir_all(&dir).unwrap();
        let helper = write_askpass(&dir, &v, "entry-1", Path::new("/opt/vmdesk")).unwrap();
        let script = std::fs::read_to_string(&helper).unwrap();
        assert_eq!(
            script,
            "#!/bin/sh\nexec '/opt/vmdesk' askpass 'entry-1' \"$@\"\n"
        );
        let cmd = ssh_command(&v, frag, Some(&helper));
        assert_eq!(
            &cmd[3..5],
            [
                &format!("SSH_ASKPASS={}", helper.display()),
                "SSH_ASKPASS_REQUIRE=force"
            ]
        );
        assert_eq!(cmd[5], "ssh");
        std::fs::remove_dir_all(dir).ok();
    }
}
