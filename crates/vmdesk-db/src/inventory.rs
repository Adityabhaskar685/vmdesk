//! VM inventory creates, read, update, dalete, plush the path checks shared with sync.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::str::FromStr;

use chrono::{DateTime, Utc};
use serde::Deserialize;
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqliteConnection};
use uuid::Uuid;
use vmdesk_core::{
    ConnectionPath, CredentialRef, Endpoint, EndpointKind, Hop, Origin, ProviderId, Vm, VmId,
};

use crate::error::conflict_on_unique;
use crate::{Db, Error, Result};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EndpointInput {
    pub address: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default)]
    pub kind: EndpointKind,
    #[serde(default)]
    pub bind_interface: Option<String>,
}

fn default_port() -> u16 {
    22
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewVm {
    pub name: String,
    pub username: String,
    #[serde(default)]
    pub credential: CredentialRef,
    pub endpoints: Vec<EndpointInput>,
    /// Hop list, most preferred first, Defaults to own direct path.
    #[serde(default)]
    pub paths: Option<Vec<Vec<Hop>>>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub favourite: bool,
}

/// Partial update. Absent fields are left alone.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VmPatch {
    pub name: Option<String>,
    pub username: Option<String>,
    pub credential: Option<CredentialRef>,
    /// Replace the user-entered endpoints, provider endpoints are kept.
    pub endpoints: Option<Vec<EndpointInput>>,
    /// Replace the user-entered path, provider paths are kept.
    pub paths: Option<Vec<Vec<Hop>>>,
    pub tags: Option<Vec<String>>,
    pub favourite: Option<bool>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ListFilter {
    pub tag: Option<String>,
    pub provider: Option<String>,
    /// Case-insensitive match on name, address or tag.
    pub q: Option<String>,
}

impl ListFilter {
    fn matches(&self, vm: &Vm) -> bool {
        self.tag
            .as_ref()
            .is_none_or(|t| vm.tags.iter().any(|x| x.eq_ignore_ascii_case(t)))
            && self.provider.as_ref().is_none_or(|p| vm.provider.0 == *p)
            && self.q.as_ref().is_none_or(|q| {
                let q = q.to_ascii_lowercase();
                vm.name.to_ascii_lowercase().contains(&q)
                    || vm
                        .endpoints
                        .iter()
                        .any(|e| e.address.to_ascii_lowercase().contains(&q))
                    || vm.tags.iter().any(|t| t.to_ascii_lowercase().contains(&q))
            })
    }
}

impl Db {
    pub async fn list_vms(&self, filter: &ListFilter) -> Result<Vec<Vm>> {
        let mut conn = self.pool.acquire().await?;
        let vms = load_vms(&mut conn, None).await?;
        Ok(vms.into_iter().filter(|vm| filter.matches(vm)).collect())
    }

    pub async fn get_vm(&self, id: VmId) -> Result<Vm> {
        let mut conn = self.pool.acquire().await?;
        get_in(&mut conn, id).await
    }

    /// Looks a VM up by UUID or (case-insensitive) name.
    pub async fn find_vm(&self, name_or_id: &str) -> Result<Vm> {
        let mut conn = self.pool.acquire().await?;
        if let Ok(id) = name_or_id.parse::<VmId>() {
            return get_in(&mut conn, id).await;
        }
        let id: Option<String> =
            sqlx::query_scalar("SELECT id FROM vms WHERE name = ? COLLATE NOCASE")
                .bind(name_or_id)
                .fetch_optional(&mut *conn)
                .await?;
        get_in(&mut conn, parse_id(&id.ok_or(Error::NotFound)?)?).await
    }

    pub async fn create_vm(&self, new: NewVm) -> Result<Vm> {
        let name = validate_name(&new.name)?;
        let username = validate_username(&new.username)?;
        let endpoints = validate_endpoints(new.endpoints)?;
        if endpoints.is_empty() {
            return Err(Error::Invalid("a VM needs at least one endpoint".into()));
        }
        let tags = clean_tags(new.tags)?;
        let paths = new.paths.unwrap_or_else(|| vec![vec![Hop::Direct]]);
        if paths.is_empty() {
            return Err(Error::Invalid(
                "a VM needs at least one connection path".into(),
            ));
        }

        let id = VmId::new();
        let now = now_ms();
        let mut tx = self.write().await?;
        check_paths(&mut tx, id, Origin::User, &paths).await?;
        sqlx::query(
            "INSERT INTO vms (id, provider_id, external_ref, name, username, credential_ref, \
            lifecycle, favourite, created_at, updated_at) \
            VALUES (?, ?, NULL, ?, ?, ?, 'unknown', ?, ?, ?)",
        )
        .bind(id.to_string())
        .bind(ProviderId::STATIC)
        .bind(&name)
        .bind(&username)
        .bind(serde_json::to_string(&new.credential)?)
        .bind(new.favourite)
        .bind(now)
        .bind(now)
        .execute(&mut *tx)
        .await
        .map_err(|e| conflict_on_unique(e, || format!("a VM named {name:?} already exists")))?;

        insert_endpoints(&mut tx, id, &endpoints).await?;
        replace_tags(&mut tx, id, &tags).await?;
        replace_paths(&mut tx, id, Origin::User, &paths).await?;
        let vm = get_in(&mut tx, id).await?;
        tx.commit().await?;
        Ok(vm)
    }

    pub async fn update_vm(&self, id: VmId, patch: VmPatch) -> Result<Vm> {
        let mut tx = self.write().await?;
        let current = get_in(&mut tx, id).await?;
        let name = match &patch.name {
            Some(n) => validate_name(n)?,
            None => current.name,
        };
        let username = match &patch.username {
            Some(u) => validate_username(u)?,
            None => current.username,
        };
        let credential = patch.credential.unwrap_or(current.credential);
        let favourite = patch.favourite.unwrap_or(current.favourite);

        sqlx::query(
            "UPDATE vms SET name = ?, username = ?, credential_ref = ?, favourite = ?, \
            updated_at = ? where id = ?",
        )
        .bind(&name)
        .bind(&username)
        .bind(serde_json::to_string(&credential)?)
        .bind(favourite)
        .bind(now_ms())
        .bind(id.to_string())
        .execute(&mut *tx)
        .await
        .map_err(|e| conflict_on_unique(e, || format!("a VM named {name:?} already exists")))?;

        if let Some(eps) = patch.endpoints {
            let eps = validate_endpoints(eps)?;
            sqlx::query("DELETE FROM endpoints WHERE vm_id = ? and SOURCE = 'user'")
                .bind(id.to_string())
                .execute(&mut *tx)
                .await?;
            insert_endpoints(&mut tx, id, &eps).await?;
            if count(
                &mut tx,
                "SELECT COUNT(*) FROM endpoints where vm_id = ?",
                id,
            )
            .await?
                == 0
            {
                return Err(Error::Invalid("a VM needs at least one enpoint".into()));
            }
        }

        if let Some(tags) = patch.tags {
            replace_tags(&mut tx, id, &clean_tags(tags)?).await?;
        }
        if let Some(paths) = patch.paths {
            check_paths(&mut tx, id, Origin::User, &paths).await?;
            replace_paths(&mut tx, id, Origin::User, &paths).await?;
            let sql = "SELECT COUNT(*) FROM connection_paths WHERE vm_id = ?";
            if count(&mut tx, sql, id).await? == 0 {
                return Err(Error::Invalid(
                    "a VM needs at least one connection path".into(),
                ));
            }
        }
        let vm = get_in(&mut tx, id).await?;
        tx.commit().await?;
        Ok(vm)
    }

    /// Deletes a VM and its history. Refused while other VMs jump through it.
    pub async fn delete_vm(&self, id: VmId) -> Result<()> {
        let mut tx = self.write().await?;
        let dependents: Vec<String> = sqlx::query_scalar(
            "SELECT DISTINCT v.name FROM hops h \
            JOIN connection_paths cp ON cp.id = h.path_id \
            JOIN vms v ON v.id = cp.vm_id \
            WHERE h.kind = 'jump' AND json_extract(h.params_json, '$.via') == ?1 and v.id != ?1 \
            ORDER BY v.name",
        )
        .bind(id.to_string())
        .fetch_all(&mut *tx)
        .await?;
        if !dependents.is_empty() {
            return Err(Error::Conflict(format!(
                "used as a jump host by: {}",
                dependents.join(", ")
            )));
        }
        let res = sqlx::query("DELETE FROM vms where id = ?")
            .bind(id.to_string())
            .execute(&mut *tx)
            .await?;
        if res.rows_affected() == 0 {
            return Err(Error::NotFound);
        }
        tx.commit().await?;
        Ok(())
    }
}

pub(crate) fn validate_name(name: &str) -> Result<String> {
    let n = name.trim();
    let ok = (1..=64).contains(&n.len())
        && n.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if ok {
        Ok(n.to_owned())
    } else {
        Err(Error::Invalid(format!(
            "invalid name {name:?}: use 1-64 letters, digits, '-', '_' or '.'"
        )))
    }
}

pub(crate) fn validate_username(user: &str) -> Result<String> {
    let u = user.trim();
    if (1..=64).contains(&u.len()) && !u.contains(char::is_whitespace) {
        Ok(u.to_owned())
    } else {
        Err(Error::Invalid(format!("invalid username {user:?}")))
    }
}

fn validate_endpoints(input: Vec<EndpointInput>) -> Result<Vec<Endpoint>> {
    input
        .into_iter()
        .map(|e| {
            let address = e.address.trim().to_owned();
            if address.is_empty() || address.len() > 253 || address.contains(char::is_whitespace) {
                return Err(Error::Invalid(format!("invalid address: {:?}", e.address)));
            }
            if e.port == 0 {
                return Err(Error::Invalid("port must be 1 - 65535".into()));
            }
            let bind_interface = e
                .bind_interface
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty());
            Ok(Endpoint {
                address,
                port: e.port,
                kind: e.kind,
                source: Origin::User,
                bind_interface,
                last_success: None,
            })
        })
        .collect()
}

fn clean_tags(tags: Vec<String>) -> Result<Vec<String>> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for t in tags {
        let t = t.trim().to_owned();
        if t.is_empty() || t.len() > 32 || t.contains(char::is_whitespace) {
            return Err(Error::Invalid(format!("invalid tag {t:?}")));
        }
        if seen.insert(t.to_ascii_lowercase()) {
            out.push(t);
        }
    }
    Ok(out)
}

pub(crate) fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

fn idx(i: usize) -> i64 {
    i64::try_from(i).unwrap_or(i64::MAX)
}

async fn count(conn: &mut SqliteConnection, sql: &str, id: VmId) -> Result<i64> {
    Ok(sqlx::query_scalar(sql)
        .bind(id.to_string())
        .fetch_one(&mut *conn)
        .await?)
}

pub(crate) async fn insert_endpoints(
    conn: &mut SqliteConnection,
    vm: VmId,
    endpoints: &[Endpoint],
) -> Result<()> {
    for e in endpoints {
        sqlx::query(
            "INSERT INTO endpoints (id, vm_id, address, port, kind, source, bind_interface) \
            VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(Uuid::now_v7().to_string())
        .bind(vm.to_string())
        .bind(&e.address)
        .bind(i64::from(e.port))
        .bind(e.kind.as_str())
        .bind(e.source.as_str())
        .bind(&e.bind_interface)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

async fn replace_tags(conn: &mut SqliteConnection, vm: VmId, tags: &[String]) -> Result<()> {
    sqlx::query("DELETE FROM vm_tags WHERE vm_id = ?")
        .bind(vm.to_string())
        .execute(&mut *conn)
        .await?;

    for t in tags {
        sqlx::query("INSERT INTO vm_tags (vm_id, tag) VALUES (?, ?)")
            .bind(vm.to_string())
            .bind(t)
            .execute(&mut *conn)
            .await?;
    }
    Ok(())
}

/// Replace this VM's path of one origin. Callers run [`check_paths`] first.
pub(crate) async fn replace_paths(
    conn: &mut SqliteConnection,
    vm: VmId,
    origin: Origin,
    paths: &[Vec<Hop>],
) -> Result<()> {
    sqlx::query("DELETE FROM connection_paths WHERE vm_id = ? and source = ?")
        .bind(vm.to_string())
        .bind(origin.as_str())
        .execute(&mut *conn)
        .await?;

    // User paths are preffered over provider paths.
    let base = if origin == Origin::User { 0 } else { 100 };
    for (i, hops) in paths.iter().enumerate() {
        let path_id = Uuid::now_v7().to_string();
        sqlx::query(
            "INSERT INTO connection_paths (id, vm_id, priority, source) VALUES (?, ?, ?, ?)",
        )
        .bind(&path_id)
        .bind(vm.to_string())
        .bind(base + idx(i))
        .bind(origin.as_str())
        .execute(&mut *conn)
        .await?;

        for (pos, hop) in hops.iter().enumerate() {
            let kind = match hop {
                Hop::Direct => "direct",
                Hop::Jump { .. } => "jump",
            };
            sqlx::query(
                "INSERT INTO hops (path_id, position, kind, params_json) VALUES (?, ?, ?, ?)",
            )
            .bind(&path_id)
            .bind(idx(pos))
            .bind(kind)
            .bind(serde_json::to_string(&hop)?)
            .execute(&mut *conn)
            .await?;
        }
    }
    Ok(())
}

type JumpGraph = HashMap<VmId, Vec<(Origin, VmId)>>;

async fn jump_graph(conn: &mut SqliteConnection) -> Result<JumpGraph> {
    let rows = sqlx::query(
        "SELECT cp.vm_id, cp.source, h.params_json FROM hops h \
        JOIN connection_paths CP on cp.id = h.path_id WHERE h.kind = 'jump'",
    )
    .fetch_all(&mut *conn)
    .await?;

    let mut graph = JumpGraph::new();
    for row in rows {
        let vm = parse_id(&row.try_get::<String, _>("vm_id")?)?;
        let origin = parse_enum::<Origin>(&row.try_get::<String, _>("source")?)?;
        if let Hop::Jump { via } = serde_json::from_str(&row.try_get::<String, _>("params_json")?)?
        {
            graph.entry(vm).or_default().push((origin, via));
        }
    }
    Ok(graph)
}

fn reaches(graph: &JumpGraph, start: VmId, target: VmId) -> bool {
    let next = |n: &VmId| graph.get(n).into_iter().flatten().map(|(_, v)| *v);
    let mut stack: Vec<VmId> = next(&start).collect();
    let mut seen = HashSet::new();
    while let Some(n) = stack.pop() {
        if n == target {
            return true;
        }
        if seen.insert(n) {
            stack.extend(next(&n));
        }
    }
    false
}

/// Validates proposed paths for 'vm': shape, existing jump hosts, and no loops
/// through the jump host's own path.
pub(crate) async fn check_paths(
    conn: &mut SqliteConnection,
    vm: VmId,
    origin: Origin,
    paths: &[Vec<Hop>],
) -> Result<()> {
    let mut graph = jump_graph(conn).await?;
    let mut edges: Vec<(Origin, VmId)> = graph
        .remove(&vm)
        .unwrap_or_default()
        .into_iter()
        .filter(|(o, _)| *o != origin)
        .collect();
    for hops in paths {
        let path = ConnectionPath {
            hops: hops.clone(),
            source: origin,
        };
        path.validate(vm)?;
        for via in path.jumps_hosts() {
            let exists: Option<i64> = sqlx::query_scalar("SELECT 1 FROM vms WHERE Id = ?")
                .bind(via.to_string())
                .fetch_optional(&mut *conn)
                .await?;
            if exists.is_none() {
                return Err(Error::Invalid(format!("jump host {via} does not exist")));
            }
            edges.push((origin, via));
        }
    }
    graph.insert(vm, edges);
    if reaches(&graph, vm, vm) {
        return Err(Error::Conflict(
            "these paths would create a jump-host loop".into(),
        ));
    }
    Ok(())
}

pub(crate) fn parse_id(s: &str) -> Result<VmId> {
    s.parse()
        .map_err(|_| Error::Corrupt(format!("bad VM id {s:?}")))
}

fn parse_enum<T: FromStr<Err = vmdesk_core::Error>>(s: &str) -> Result<T> {
    s.parse()
        .map_err(|e: vmdesk_core::Error| Error::Corrupt(e.to_string()))
}

pub(crate) fn from_ms(ms: i64) -> Result<DateTime<Utc>> {
    DateTime::from_timestamp_millis(ms).ok_or_else(|| Error::Corrupt(format!("bad time {ms}")))
}

pub(crate) async fn get_in(conn: &mut SqliteConnection, id: VmId) -> Result<Vm> {
    load_vms(conn, Some(id)).await?.pop().ok_or(Error::NotFound)
}

fn vm_from_row(row: &SqliteRow) -> Result<Vm> {
    Ok(Vm {
        id: parse_id(&row.try_get::<String, _>("id")?)?,
        provider: ProviderId(row.try_get("provider_id")?),
        external_ref: row.try_get("external_ref")?,
        name: row.try_get("name")?,
        username: row.try_get("username")?,
        credential: serde_json::from_str(&row.try_get::<String, _>("credential_ref")?)?,
        lifecycle: parse_enum(&row.try_get::<String, _>("lifecycle")?)?,
        favourite: row.try_get::<i64, _>("favourite")? != 0,
        created_at: from_ms(row.try_get("created_at")?)?,
        updated_at: from_ms(row.try_get("updated_at")?)?,
        endpoints: Vec::new(),
        paths: Vec::new(),
        tags: Vec::new(),
        labels: BTreeMap::new(),
    })
}

/// Loads one VM (`only`) or all of them, with endpoints, tags, labels and paths.
pub(crate) async fn load_vms(conn: &mut SqliteConnection, only: Option<VmId>) -> Result<Vec<Vm>> {
    let filter = only.map(|id| id.to_string());
    let rows = sqlx::query(
        "SELECT id, provider_id, external_ref, name, username, credential_ref, lifecycle, \
        favourite, created_at, updated_at FROM vms \
        WHERE ?1 IS NULL OR ID = ?1 ORDER BY name COLLATE NOCASE",
    )
    .bind(&filter)
    .fetch_all(&mut *conn)
    .await?;

    let mut vms = rows.iter().map(vm_from_row).collect::<Result<Vec<_>>>()?;
    let index: HashMap<String, usize> = vms
        .iter()
        .enumerate()
        .map(|(i, vm)| (vm.id.to_string(), i))
        .collect();
    let slot = |row: &SqliteRow| -> Result<Option<usize>> {
        Ok(index.get(&row.try_get::<String, _>("vm_id")?).copied())
    };

    let endpoint_rows = sqlx::query(
        "SELECT vm_id, address, port, kind, source, bind_interface, last_success_at \
        FROM endpoints WHERE ?1 IS NULL or vm_id = ?1 \
        ORDER BY CASE source WHEN 'user' THEN 0 ELSE 1 END, rowid",
    )
    .bind(&filter)
    .fetch_all(&mut *conn)
    .await?;

    for row in &endpoint_rows {
        let Some(i) = slot(row)? else { continue };
        let port: i64 = row.try_get("port")?;
        let last: Option<i64> = row.try_get("last_success_at")?;
        vms[i].endpoints.push(Endpoint {
            address: row.try_get("address")?,
            port: u16::try_from(port).map_err(|_| Error::Corrupt(format!("bad port {port}")))?,
            kind: parse_enum(&row.try_get::<String, _>("kind")?)?,
            source: parse_enum(&row.try_get::<String, _>("source")?)?,
            bind_interface: row.try_get("bind_interface")?,
            last_success: last.map(from_ms).transpose()?,
        });
    }

    let tag_rows =
        sqlx::query("SELECT vm_id, tag FROM vm_tags where ?1 is NULL or vm_id = ?1 ORDER BY rowid")
            .bind(&filter)
            .fetch_all(&mut *conn)
            .await?;

    for row in &tag_rows {
        if let Some(i) = slot(row)? {
            vms[i].tags.push(row.try_get("tag")?);
        }
    }

    let label_rows =
        sqlx::query("SELECT vm_id, key, value FROM vm_labels WHERE ?1 IS NULL OR vm_id = ?1")
            .bind(&filter)
            .fetch_all(&mut *conn)
            .await?;

    for row in &label_rows {
        if let Some(i) = slot(row)? {
            vms[i]
                .labels
                .insert(row.try_get("key")?, row.try_get("value")?);
        }
    }

    let hop_rows = sqlx::query(
        "SELECT cp.vm_id, cp.id AS path_id, cp.source, h.params_json \
        FROM connection_paths cp JOIN hops h ON h.path_id = cp.id \
        WHERE ?1 IS NULL OR cp.vm_id = ?1
        ORDER BY cp.vm_id, cp.priority, cp.id, h.position",
    )
    .bind(&filter)
    .fetch_all(&mut *conn)
    .await?;
    let mut current: Option<String> = None;
    for row in &hop_rows {
        let Some(i) = slot(row)? else { continue };
        let path_id: String = row.try_get("path_id")?;
        let hop: Hop = serde_json::from_str(&row.try_get::<String, _>("params_json")?)?;
        if current.as_deref() != Some(path_id.as_str()) {
            let source = parse_enum(&row.try_get::<String, _>("source")?)?;
            vms[i].paths.push(ConnectionPath {
                hops: Vec::new(),
                source,
            });
            current = Some(path_id);
        }
        if let Some(path) = vms[i].paths.last_mut() {
            path.hops.push(hop);
        }
    }
    Ok(vms)
}
