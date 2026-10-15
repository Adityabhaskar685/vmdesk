//! User authentication: configured key file, then ssh-agent, then the default keys in `~/.ssh`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use russh::client::Handle;
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

    if let CredentialRef::KeyFile(path) = &vm.credential {
        let path = expand_home(path);
        match key_file(handle, user, &path).await {
            Ok(true) => return Ok(()),
            Ok(false) => tried.push(format!("{} rejected", path.display())),
            Err(e) => tried.push(e),
        }
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
        CredentialRef::KeyFile(_) => {}
        CredentialRef::KeyChain(_) | CredentialRef::CloudProvider(_) => {
            tried.push("keychain and cloud credentials are not supported yet".to_owned());
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
