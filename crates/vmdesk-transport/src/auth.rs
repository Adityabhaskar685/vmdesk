//! User authentication: the configured key file or keychain password, then ssh-agent,
//! then the default keys in `~/.ssh`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use russh::client::{Handle, KeyboardInteractiveAuthResponse};
use russh::keys::agent::AgentIdentity;
use russh::keys::agent::client::AgentClient;
use russh::keys::{PrivateKeyWithHashAlg, load_secret_key};
use vmdesk_core::{CredentialRef, Error, Result, Vm};

use crate::hostkey::Client;

/// Keys `ssh` tries by default, in its order.
const DEFAULT_KEYS: [&str; 3] = ["id_ed25519", "id_ecdsa", "id_rsa"];

pub(crate) async fn authenticate(
    handle: &mut Handle<Client>,
    vm: &Vm,
    use_agent: bool,
) -> Result<()> {
    let user = vm.username.as_str();
    let mut tried = Vec::new();

    match &vm.credential {
        CredentialRef::KeyFile(path) => {
            let path = expand_home(path);
            match key_file(handle, user, &path).await {
                Ok(true) => return Ok(()),
                Ok(false) => tried.push(format!("{} rejected", path.display())),
                Err(e) => tried.push(e),
            }
        }
        CredentialRef::KeyChain(entry) => match password(handle, user, entry).await {
            Ok(true) => return Ok(()),
            Ok(false) => tried.push("keychain password rejected".to_owned()),
            Err(e) => tried.push(e),
        },
        CredentialRef::SshAgent | CredentialRef::CloudProvider(_) => {}
    }

    if use_agent {
        match agent(handle, user).await {
            Ok(true) => return Ok(()),
            Ok(false) => tried.push("ssh-agent keys rejected".to_owned()),
            Err(e) => tried.push(e),
        }
    }

    match &vm.credential {
        CredentialRef::SshAgent => {
            let home = dirs::home_dir().unwrap_or_default();
            for name in DEFAULT_KEYS {
                let path = home.join(".ssh").join(name);
                if !path.exists() {
                    continue;
                }
                match key_file(handle, user, &path).await {
                    Ok(true) => return Ok(()),
                    Ok(false) => tried.push(format!("{} rejected", path.display())),
                    Err(e) => tried.push(e),
                }
            }
        }
        CredentialRef::KeyFile(_) | CredentialRef::KeyChain(_) => {}
        CredentialRef::CloudProvider(_) => {
            tried.push("cloud credentials are not supported yet".to_owned());
        }
    }

    if tried.is_empty() {
        tried.push("no key to try".to_owned());
    }
    Err(Error::Auth(format!(
        "{user}@{}: {}",
        vm.name,
        tried.join("; ")
    )))
}

/// `Ok(false)` when the server rejects the key, `Err` when it cannot be used at all.
async fn key_file(
    handle: &mut Handle<Client>,
    user: &str,
    path: &Path,
) -> std::result::Result<bool, String> {
    let key = load_secret_key(path, None).map_err(|e| {
        format!(
            "{}: {e} (passphrase-protected keys must be added to ssh-agent)",
            path.display()
        )
    })?;
    let hash = handle
        .best_supported_rsa_hash()
        .await
        .map_err(|e| e.to_string())?
        .flatten();
    handle
        .authenticate_publickey(user, PrivateKeyWithHashAlg::new(Arc::new(key), hash))
        .await
        .map(|r| r.success())
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// Password auth, falling back to keyboard-interactive (where most servers ask for it).
async fn password(
    handle: &mut Handle<Client>,
    user: &str,
    entry: &str,
) -> std::result::Result<bool, String> {
    let secret = crate::keychain::get_password(entry)
        .await
        .map_err(|e| e.to_string())?;
    let res = handle
        .authenticate_password(user, secret.as_str())
        .await
        .map_err(|e| format!("password: {e}"))?;
    if res.success() {
        return Ok(true);
    }
    let mut step = handle
        .authenticate_keyboard_interactive_start(user, None)
        .await
        .map_err(|e| format!("keyboard-interactive: {e}"))?;
    // Answer hidden prompts with the password; give up after a few rounds.
    for _ in 0..3 {
        match step {
            KeyboardInteractiveAuthResponse::Success => return Ok(true),
            KeyboardInteractiveAuthResponse::Failure { .. } => return Ok(false),
            KeyboardInteractiveAuthResponse::InfoRequest { prompts, .. } => {
                let answers = prompts
                    .iter()
                    .map(|p| {
                        if p.echo {
                            String::new()
                        } else {
                            secret.clone()
                        }
                    })
                    .collect();
                step = handle
                    .authenticate_keyboard_interactive_respond(answers)
                    .await
                    .map_err(|e| format!("keyboard-interactive: {e}"))?;
            }
        }
    }
    Ok(false)
}

async fn agent(handle: &mut Handle<Client>, user: &str) -> std::result::Result<bool, String> {
    let mut agent = AgentClient::connect_env()
        .await
        .map_err(|e| format!("ssh-agent unavailable: {e}"))?;
    let identities = agent
        .request_identities()
        .await
        .map_err(|e| format!("ssh-agent: {e}"))?;
    let hash = handle
        .best_supported_rsa_hash()
        .await
        .map_err(|e| e.to_string())?
        .flatten();
    for identity in identities {
        let AgentIdentity::PublicKey { key, .. } = identity else {
            continue;
        };
        let res = handle
            .authenticate_publickey_with(user, key, hash, &mut agent)
            .await
            .map_err(|e| format!("ssh-agent: {e}"))?;
        if res.success() {
            return Ok(true);
        }
    }
    Ok(false)
}

fn expand_home(path: &Path) -> PathBuf {
    match (path.strip_prefix("~"), dirs::home_dir()) {
        (Ok(rest), Some(home)) => home.join(rest),
        _ => path.to_owned(),
    }
}
