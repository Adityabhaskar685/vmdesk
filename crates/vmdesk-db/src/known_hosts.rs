//! Trusted SSH host keys per VM, read by the transport on every handshake.

use async_trait::async_trait;
use vmdesk_core::{HostKey, HostKeyStore, VmId};

use crate::Db;
use crate::inventory::now_ms;

fn core_err(e: &sqlx::Error) -> vmdesk_core::Error {
    vmdesk_core::Error::Other(format!("host key store: {e}"))
}

#[async_trait]
impl HostKeyStore for Db {
    async fn host_keys(&self, vm: VmId) -> vmdesk_core::Result<Vec<HostKey>> {
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT key_type, fingerprint FROM known_hosts WHERE vm_id = ? AND confirmed = 1 \
            ORDER BY key_type",
        )
        .bind(vm.to_string())
        .fetch_all(self.pool())
        .await
        .map_err(|e| core_err(&e))?;
        Ok(rows
            .into_iter()
            .map(|(key_type, fingerprint)| HostKey {
                key_type,
                fingerprint,
            })
            .collect())
    }

    async fn trust_host_key(&self, vm: VmId, key: &HostKey) -> vmdesk_core::Result<()> {
        sqlx::query(
            "INSERT INTO known_hosts (vm_id, key_type, fingerprint, first_seen_at, confirmed) \
            VALUES (?, ?, ?, ?, 1) \
            ON CONFLICT (vm_id, key_type) DO UPDATE SET \
            fingerprint = excluded.fingerprint, first_seen_at = excluded.first_seen_at, confirmed = 1",
        )
        .bind(vm.to_string())
        .bind(&key.key_type)
        .bind(&key.fingerprint)
        .bind(now_ms())
        .execute(self.pool())
        .await
        .map_err(|e| core_err(&e))?;
        Ok(())
    }

    async fn forget_host_keys(&self, vm: VmId) -> vmdesk_core::Result<u64> {
        let res = sqlx::query("DELETE FROM known_hosts WHERE vm_id = ?")
            .bind(vm.to_string())
            .execute(self.pool())
            .await
            .map_err(|e| core_err(&e))?;
        Ok(res.rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use vmdesk_core::HostKeyStore;

    use crate::{Db, EndpointInput, NewVm};

    use super::*;

    #[tokio::test]
    async fn trust_replace_and_forget() {
        let db = Db::open_in_memory().await.unwrap();
        let vm = db
            .create_vm(NewVm {
                name: "box".into(),
                username: "u".into(),
                credential: vmdesk_core::CredentialRef::SshAgent,
                endpoints: vec![EndpointInput {
                    address: "10.0.0.1".into(),
                    port: 22,
                    kind: vmdesk_core::EndpointKind::Private,
                    bind_interface: None,
                }],
                paths: None,
                tags: vec![],
                favourite: false,
            })
            .await
            .unwrap();
        assert!(db.host_keys(vm.id).await.unwrap().is_empty());

        let k1 = HostKey {
            key_type: "ssh-ed25519".into(),
            fingerprint: "SHA256:one".into(),
        };
        db.trust_host_key(vm.id, &k1).await.unwrap();
        assert_eq!(
            db.host_keys(vm.id).await.unwrap(),
            std::slice::from_ref(&k1)
        );

        let k2 = HostKey {
            key_type: "ssh-ed25519".into(),
            fingerprint: "SHA256:two".into(),
        };
        db.trust_host_key(vm.id, &k2).await.unwrap();
        assert_eq!(db.host_keys(vm.id).await.unwrap(), [k2]);

        assert_eq!(db.forget_host_keys(vm.id).await.unwrap(), 1);
        assert!(db.host_keys(vm.id).await.unwrap().is_empty());

        db.trust_host_key(vm.id, &k1).await.unwrap();
        db.delete_vm(vm.id).await.unwrap();
        assert!(db.host_keys(vm.id).await.unwrap().is_empty());
    }
}
