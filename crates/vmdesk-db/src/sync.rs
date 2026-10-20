//! Applies a provider listing to the inventory.

use std::collections::{HashMap, HashSet};

use serde::Serialize;
use sqlx::SqliteConnection;
use vmdesk_core::{ALIASES_LABEL, Hop, Origin, ProviderId, ProviderVm, VmId};

use crate::error::is_unique_violation;
use crate::inventory::{
    check_paths, insert_endpoints, now_ms, parse_id, replace_paths, validate_name,
    validate_username,
};

use crate::{Db, Error, Result};

/// What a sync changed, by VM name.
#[derive(Debug, Default, Clone, Serialize)]
pub struct SyncReport {
    pub added: Vec<String>,
    pub refreshed: Vec<String>,
    pub terminated: Vec<String>,
    /// Records not imported, with the reason.
    pub skipped: Vec<String>,
    /// Imported, but something needs attention (usually an unresolved jump host).
    pub warnings: Vec<String>,
}

struct Synced {
    id: VmId,
    name: String,
    jump_via: Vec<String>,
}

impl Db {
    /// Provider-owned data (endpoints, labels, lifecycle, provider paths) is
    /// replaced, user-owned data (name, username, credential, tags, user
    /// endpoints and paths) is never touchec. Records missing from the listing
    /// are marked terminated, and their history is kept.
    pub async fn sync_provider(
        &self,
        provider: &ProviderId,
        listing: Vec<ProviderVm>,
        default_user: &str,
    ) -> Result<SyncReport> {
        let mut tx = self.write().await?;
        let known: Option<i64> = sqlx::query_scalar("SELECT 1 FROM providers WHERE id = ?")
            .bind(&provider.0)
            .fetch_optional(&mut *tx)
            .await?;
        if known.is_none() {
            return Err(Error::Invalid(format!("unknown provider {provider}")));
        }

        let existing: HashMap<String, (VmId, String)> = sqlx::query_as::<_, (String, String, String)>(
            "SELECT external_ref, id, name FROM vms WHERE provider_id = ? AND external_ref IS NOT NULL"
        )
        .bind(&provider.0)
        .fetch_all(&mut *tx)
        .await?
        .into_iter()
        .map(|(r, id, name)| Ok((r, (parse_id(&id)?, name))))
        .collect::<Result<_>>()?;

        let mut report = SyncReport::default();
        let mut seen = HashSet::new();
        let mut synced = Vec::new();
        let now = now_ms();

        for pvm in listing {
            if !seen.insert(pvm.external_ref.clone()) {
                report
                    .warnings
                    .push(format!("{}: listed twice, first entry used", pvm.name));
                continue;
            }
            let (id, name) = if let Some((id, name)) = existing.get(&pvm.external_ref) {
                refresh(&mut tx, *id, &pvm, now).await?;
                report.refreshed.push(name.clone());
                (*id, name.clone())
            } else {
                match insert(&mut tx, provider, &pvm, default_user, now).await? {
                    Ok((id, name)) => {
                        report.added.push(name.clone());
                        (id, name)
                    }
                    Err(reason) => {
                        report.skipped.push(format!("{}: {reason}", pvm.name));
                        continue;
                    }
                }
            };
            let endpoints: Vec<_> = pvm
                .endpoints
                .into_iter()
                .filter(|e| !e.address.trim().is_empty() && e.port != 0)
                .map(|mut e| {
                    e.source = Origin::Provider;
                    e
                })
                .collect();
            insert_endpoints(&mut tx, id, &endpoints).await?;
            for (key, value) in &pvm.labels {
                sqlx::query(
                    "INSERT INTO vm_labels (vm_id, key, value, source) VALUES (?, ?, ?, 'provider') \
                    ON CONFLICT (vm_id, key) DO NOTHING" 
                )
                .bind(id.to_string())
                .bind(key)
                .bind(value)
                .execute(&mut *tx)
                .await?;
            }
            synced.push(Synced {
                id,
                name,
                jump_via: pvm.jump_via,
            });
        }

        for (ext, (id, name)) in &existing {
            if seen.contains(ext) {
                continue;
            }
            let res = sqlx::query(
                "UPDATE vms SET lifecycle = 'terminated', updated_at = ? \
                WHERE id = ? AND lifecycle != 'terminated'",
            )
            .bind(now)
            .bind(id.to_string())
            .execute(&mut *tx)
            .await?;
            if res.rows_affected() > 0 {
                report.terminated.push(name.clone());
            }
        }

        // Second pass: bastions my be listed after the VMs that use them.
        link_jump_paths(&mut tx, provider, synced, &mut report).await?;

        tx.commit().await?;
        Ok(report)
    }
}

/// Turns each VM's `jump_via` names into a provider path, or a warning.
async fn link_jump_paths(
    conn: &mut SqliteConnection,
    provider: &ProviderId,
    synced: Vec<Synced>,
    report: &mut SyncReport,
) -> Result<()> {
    for s in synced {
        let vias = match resolve_jumps(conn, provider, &s.jump_via).await? {
            Ok(vias) => vias,
            Err(missing) => {
                report.warnings.push(format!(
                    "{}: jump host {missing:?} is not in the inventory, no path created",
                    s.name
                ));
                replace_paths(conn, s.id, Origin::Provider, &[]).await?;
                continue;
            }
        };
        let mut hops: Vec<Hop> = vias.into_iter().map(|via| Hop::Jump { via }).collect();
        hops.push(Hop::Direct);
        let paths = [hops];
        match check_paths(conn, s.id, Origin::Provider, &paths).await {
            Ok(()) => replace_paths(conn, s.id, Origin::Provider, &paths).await?,
            Err(Error::Conflict(_) | Error::Invalid(_)) => {
                report.warnings.push(format!(
                    "{}: jump chain {:?} loops back to itself, no path created",
                    s.name, s.jump_via
                ));
                replace_paths(conn, s.id, Origin::Provider, &[]).await?;
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

async fn refresh(conn: &mut SqliteConnection, id: VmId, pvm: &ProviderVm, now: i64) -> Result<()> {
    sqlx::query("UPDATE vms SET lifecycle = ?, updated_at = ? WHERE id = ?")
        .bind(pvm.lifecycle.as_str())
        .bind(now)
        .bind(id.to_string())
        .execute(&mut *conn)
        .await?;
    for table in ["endpoints", "vm_labels"] {
        sqlx::query(&format!(
            "DELETE FROM {table} WHERE vm_id = ? AND source = 'provider'"
        ))
        .bind(id.to_string())
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// Inserts a new provider VM. The inner `Err` is a skip reason, not a failure.
async fn insert(
    conn: &mut SqliteConnection,
    provider: &ProviderId,
    pvm: &ProviderVm,
    default_user: &str,
    now: i64,
) -> Result<std::result::Result<(VmId, String), String>> {
    let name = match validate_name(&pvm.name) {
        Ok(n) => n,
        Err(e) => return Ok(Err(e.to_string())),
    };
    let username = match validate_username(pvm.username.as_deref().unwrap_or(default_user)) {
        Ok(u) => u,
        Err(e) => return Ok(Err(e.to_string())),
    };
    let id = VmId::new();
    let res = sqlx::query(
        "INSERT INTO vms(id, provider_id, external_ref, name, username, credential_ref, \
        lifecycle, favourite, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, 0, ?, ?)",
    )
    .bind(id.to_string())
    .bind(&provider.0)
    .bind(&pvm.external_ref)
    .bind(&name)
    .bind(&username)
    .bind(serde_json::to_string(
        &pvm.credential.clone().unwrap_or_default(),
    )?)
    .bind(pvm.lifecycle.as_str())
    .bind(now)
    .bind(now)
    .execute(&mut *conn)
    .await;

    match res {
        Ok(_) => Ok(Ok((id, name))),
        Err(e) if is_unique_violation(&e) => {
            Ok(Err("a VM with this name already exists".to_owned()))
        }
        Err(e) => Err(e.into()),
    }
}

/// Resolves jump-host names by external-ref, then alias, then VM name.
/// The inner `Err` carries the first name that could not be resolved.
async fn resolve_jumps(
    conn: &mut SqliteConnection,
    provider: &ProviderId,
    names: &[String],
) -> Result<std::result::Result<Vec<VmId>, String>> {
    let mut out = Vec::new();
    for name in names {
        let mut id: Option<String> =
            sqlx::query_scalar("SELECT id FROM vms WHERE provider_id = ? AND external_ref = ?")
                .bind(&provider.0)
                .bind(name)
                .fetch_optional(&mut *conn)
                .await?;
        if id.is_none() {
            id = sqlx::query_scalar(
                "SELECT l.vm_id FROM vm_labels l JOIN vms v on v.id = l.vm_id \
                WHERE v.provider_id = ? AND l.key = ? \
                AND instr(',' || l.value || ',', ',' || ? || ',') > 0 LIMIT 1",
            )
            .bind(&provider.0)
            .bind(ALIASES_LABEL)
            .bind(name)
            .fetch_optional(&mut *conn)
            .await?;
        }
        if id.is_none() {
            id = sqlx::query_scalar(
                "SELECT id FROM vms WHERE provider_id = ? AND name = ? COLLATE NOCASE",
            )
            .bind(&provider.0)
            .bind(name)
            .fetch_optional(&mut *conn)
            .await?;
        }
        match id {
            Some(id) => out.push(parse_id(&id)?),
            None => return Ok(Err(name.clone())),
        }
    }
    Ok(Ok(out))
}
