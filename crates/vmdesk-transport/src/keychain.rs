//! SSH passwords in the OS keychain (Secret Service on Linux). The database only
//! stores the entry name, as `CredentialRef::KeyChain`.

use vmdesk_core::{Error, Result};

const SERVICE: &str = "vmdesk";

fn entry(name: &str) -> Result<keyring::Entry> {
    keyring::Entry::new(SERVICE, name).map_err(|e| Error::Other(format!("keychain: {e}")))
}

/// Runs a blocking keychain call off the async runtime.
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| Error::Other(format!("keychain: {e}")))?
}

pub async fn get_password(name: &str) -> Result<String> {
    let name = name.to_owned();
    blocking(move || {
        entry(&name)?.get_password().map_err(|e| match e {
            keyring::Error::NoEntry => Error::NotFound(format!("keychain entry {SERVICE}/{name}")),
            e => Error::Other(format!("keychain: {e}")),
        })
    })
    .await
}

pub async fn set_password(name: &str, password: &str) -> Result<()> {
    let (name, password) = (name.to_owned(), password.to_owned());
    blocking(move || {
        entry(&name)?
            .set_password(&password)
            .map_err(|e| Error::Other(format!("keychain: {e}")))
    })
    .await
}

/// Removes the entry; a missing entry is not an error.
pub async fn delete_password(name: &str) -> Result<()> {
    let name = name.to_owned();
    blocking(move || match entry(&name)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(Error::Other(format!("keychain: {e}"))),
    })
    .await
}
