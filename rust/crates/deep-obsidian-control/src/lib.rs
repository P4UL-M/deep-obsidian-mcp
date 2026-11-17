//! The authoritative store for application users, separate from vault data.
use anyhow::{bail, Result};
use argon2::{
    password_hash::{PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use rusqlite::{params, Connection, OptionalExtension};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::UNIX_EPOCH,
};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Reader,
    Editor,
    Owner,
}
impl Role {
    pub fn permits(self, action: Action) -> bool {
        match action {
            Action::Read => true,
            Action::Write => self != Self::Reader,
            Action::Manage => self == Self::Owner,
        }
    }
    fn text(self) -> &'static str {
        match self {
            Self::Reader => "reader",
            Self::Editor => "editor",
            Self::Owner => "owner",
        }
    }
    fn parse(s: &str) -> rusqlite::Result<Self> {
        match s {
            "reader" => Ok(Self::Reader),
            "editor" => Ok(Self::Editor),
            "owner" => Ok(Self::Owner),
            _ => Err(rusqlite::Error::InvalidQuery),
        }
    }
}
#[derive(Clone, Copy, Debug)]
pub enum Action {
    Read,
    Write,
    Manage,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct User {
    pub id: String,
    pub name: String,
    pub default_mount: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Principal {
    pub user_id: String,
    pub name: String,
    pub credential_version: u64,
    pub default_mount: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Grant {
    pub mount_id: String,
    pub role: Role,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InitialUser {
    pub name: String,
    pub grants: Vec<Grant>,
    pub default_mount: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AuditEvent {
    pub id: u64,
    pub timestamp: u64,
    pub actor: String,
    pub mount_id: Option<String>,
    pub operation: String,
}
#[derive(Debug)]
pub struct GeneratedUser {
    pub user: User,
    pub password: SecretString,
}
#[derive(Clone, Debug)]
pub struct ControlStore(Arc<Mutex<Connection>>);

impl ControlStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(p) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(p)?;
        }
        // Create privately before SQLite opens the file, avoiding a readable creation window.
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options.open(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
        let c = Connection::open(path)?;
        c.busy_timeout(std::time::Duration::from_secs(5))?;
        c.execute_batch("PRAGMA foreign_keys=ON;
          CREATE TABLE IF NOT EXISTS control_meta(key TEXT PRIMARY KEY, value INTEGER NOT NULL);
          INSERT OR IGNORE INTO control_meta VALUES('enabled',0);
          INSERT OR IGNORE INTO control_meta VALUES('revision',0);
          CREATE TABLE IF NOT EXISTS control_users(id TEXT PRIMARY KEY, name TEXT NOT NULL UNIQUE, hash TEXT NOT NULL, generation INTEGER NOT NULL DEFAULT 1, default_mount TEXT);
          CREATE TABLE IF NOT EXISTS control_grants(user_id TEXT NOT NULL REFERENCES control_users(id) ON DELETE CASCADE, mount_id TEXT NOT NULL, role TEXT NOT NULL CHECK(role IN ('reader','editor','owner')), PRIMARY KEY(user_id,mount_id));
          CREATE TABLE IF NOT EXISTS control_sessions(digest TEXT PRIMARY KEY, user_id TEXT NOT NULL REFERENCES control_users(id) ON DELETE CASCADE, generation INTEGER NOT NULL, expires INTEGER NOT NULL);
          CREATE TABLE IF NOT EXISTS control_audit(id INTEGER PRIMARY KEY AUTOINCREMENT, timestamp INTEGER NOT NULL, actor TEXT NOT NULL, mount_id TEXT, operation TEXT NOT NULL);")?;
        Ok(Self(Arc::new(Mutex::new(c))))
    }
    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>> {
        self.0
            .lock()
            .map_err(|_| anyhow::anyhow!("control store lock poisoned"))
    }
    /// Record authorization metadata only: callers must never pass credentials or note bodies.
    pub fn record_event(&self, actor: &str, mount: Option<&str>, operation: &str) -> Result<()> {
        if actor.is_empty()
            || actor.len() > 256
            || operation.is_empty()
            || operation.len() > 128
            || actor.contains(['\n', '\r'])
            || operation.contains(['\n', '\r'])
        {
            bail!("invalid audit metadata");
        }
        self.lock()?.execute(
            "INSERT INTO control_audit(timestamp,actor,mount_id,operation) VALUES(?1,?2,?3,?4)",
            params![now(), actor, mount, operation],
        )?;
        Ok(())
    }
    /// Most recent events first, bounded to keep diagnostics predictable.
    pub fn audit_events(&self, limit: usize) -> Result<Vec<AuditEvent>> {
        let c = self.lock()?;
        let mut query = c.prepare("SELECT id,timestamp,actor,mount_id,operation FROM control_audit ORDER BY id DESC LIMIT ?1")?;
        let rows = query.query_map([limit.min(1000) as u64], |r| {
            Ok(AuditEvent {
                id: r.get(0)?,
                timestamp: r.get(1)?,
                actor: r.get(2)?,
                mount_id: r.get(3)?,
                operation: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
    pub fn enabled(&self) -> Result<bool> {
        Ok(self.lock()?.query_row(
            "SELECT value FROM control_meta WHERE key='enabled'",
            [],
            |r| r.get::<_, i64>(0),
        )? != 0)
    }
    pub fn revision(&self) -> Result<u64> {
        Ok(self.lock()?.query_row(
            "SELECT value FROM control_meta WHERE key='revision'",
            [],
            |r| r.get::<_, u64>(0),
        )?)
    }
    pub fn list_users(&self) -> Result<Vec<User>> {
        let c = self.lock()?;
        let mut q = c.prepare("SELECT id,name,default_mount FROM control_users ORDER BY name")?;
        let rows = q.query_map([], user_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
    pub fn get_user(&self, name_or_id: &str) -> Result<Option<User>> {
        lookup_user(&*self.lock()?, name_or_id)
    }
    /// Creates an application user only; never provisions an external account or vault.
    pub fn add_user(&self, name: &str) -> Result<GeneratedUser> {
        validate_name(name)?;
        let generated = generated_user(name)?;
        let mut c = self.lock()?;
        let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        insert_generated(&tx, &generated)?;
        bump(&tx)?;
        tx.commit()?;
        Ok(generated)
    }
    /// Atomically activates the feature and creates every explicitly assigned initial user.
    pub fn enable(
        &self,
        users: &[InitialUser],
        valid_mounts: &[String],
    ) -> Result<Vec<GeneratedUser>> {
        if users.is_empty() {
            bail!("multiuser requires at least one user");
        }
        let mut generated = Vec::new();
        for initial in users {
            validate_name(&initial.name)?;
            validate_initial(initial, valid_mounts)?;
            let mut g = generated_user(&initial.name)?;
            g.user.default_mount = initial.default_mount.clone();
            generated.push(g);
        }
        // Every exposed vault needs an owner; unassigned configured mounts remain hidden.
        for initial in users {
            for grant in &initial.grants {
                if !users.iter().any(|u| {
                    u.grants
                        .iter()
                        .any(|g| g.mount_id == grant.mount_id && g.role == Role::Owner)
                }) {
                    bail!("mount {} requires an owner", grant.mount_id);
                }
            }
        }
        let mut c = self.lock()?;
        let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if enabled_in(&tx)? {
            bail!("multiuser is already enabled");
        }
        // Disabled stores are expected empty, but activation never merges stale accounts.
        tx.execute("DELETE FROM control_users", [])?;
        for (initial, g) in users.iter().zip(&generated) {
            insert_generated(&tx, g)?;
            for grant in &initial.grants {
                tx.execute(
                    "INSERT INTO control_grants VALUES(?1,?2,?3)",
                    params![g.user.id, grant.mount_id, grant.role.text()],
                )?;
            }
        }
        tx.execute("UPDATE control_meta SET value=1 WHERE key='enabled'", [])?;
        bump(&tx)?;
        tx.commit()?;
        Ok(generated)
    }
    /// Enables pre-created users, validating all currently granted mounts and ownership.
    pub fn enable_existing(&self, valid_mounts: &[String]) -> Result<()> {
        let mut c = self.lock()?;
        let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let count: i64 = tx.query_row("SELECT count(*) FROM control_users", [], |r| r.get(0))?;
        if count == 0 {
            bail!("multiuser requires at least one user");
        }
        let mut q = tx.prepare("SELECT DISTINCT mount_id FROM control_grants")?;
        let mounts = q
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(q);
        for mount in mounts {
            validate_mount(&mount, valid_mounts)?;
            if owner_count(&tx, &mount)? == 0 {
                bail!("mount {mount} requires an owner");
            }
        }
        tx.execute("UPDATE control_meta SET value=1 WHERE key='enabled'", [])?;
        bump(&tx)?;
        tx.commit()?;
        Ok(())
    }
    /// Explicit destructive opt-out: remove all application users, grants and sessions.
    pub fn disable(&self) -> Result<()> {
        let mut c = self.lock()?;
        let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute("DELETE FROM control_sessions", [])?;
        tx.execute("DELETE FROM control_users", [])?;
        tx.execute("UPDATE control_meta SET value=0 WHERE key='enabled'", [])?;
        bump(&tx)?;
        tx.commit()?;
        Ok(())
    }
    pub fn grant(
        &self,
        name_or_id: &str,
        mount: &str,
        role: Role,
        valid_mounts: &[String],
    ) -> Result<()> {
        validate_mount(mount, valid_mounts)?;
        self.change_membership(None, name_or_id, mount, Some(role))
    }
    pub fn revoke(&self, name_or_id: &str, mount: &str) -> Result<()> {
        self.change_membership(None, name_or_id, mount, None)
    }
    /// Check the live owner grant and apply the membership in the same transaction.
    /// Ordinary users cannot create accounts or change another vault's membership.
    pub fn change_membership(
        &self,
        actor: Option<&Principal>,
        name_or_id: &str,
        mount: &str,
        role: Option<Role>,
    ) -> Result<()> {
        let mut c = self.lock()?;
        let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if let Some(actor) = actor {
            if !enabled_in(&tx)?
                || !principal_valid_in(&tx, actor)?
                || role_in(&tx, &actor.user_id, mount)? != Some(Role::Owner)
            {
                bail!("owner permission required");
            }
        }
        let user = required_user(&tx, name_or_id)?;
        let old = role_in(&tx, &user.id, mount)?;
        if enabled_in(&tx)?
            && old == Some(Role::Owner)
            && role != Some(Role::Owner)
            && owner_count(&tx, mount)? <= 1
        {
            bail!("cannot remove or demote the last owner of mount {mount}");
        }
        if let Some(role) = role {
            if enabled_in(&tx)? && role != Role::Owner && owner_count(&tx, mount)? == 0 {
                bail!("mount {mount} requires an owner");
            }
            tx.execute("INSERT INTO control_grants VALUES(?1,?2,?3) ON CONFLICT(user_id,mount_id) DO UPDATE SET role=excluded.role",params![user.id,mount,role.text()])?;
        } else {
            tx.execute(
                "DELETE FROM control_grants WHERE user_id=?1 AND mount_id=?2",
                params![user.id, mount],
            )?;
            tx.execute(
                "UPDATE control_users SET default_mount=NULL WHERE id=?1 AND default_mount=?2",
                params![user.id, mount],
            )?;
        }
        bump(&tx)?;
        tx.commit()?;
        Ok(())
    }
    pub fn grants(&self, name_or_id: &str) -> Result<Vec<Grant>> {
        let c = self.lock()?;
        let user = required_user(&c, name_or_id)?;
        let mut q = c.prepare(
            "SELECT mount_id,role FROM control_grants WHERE user_id=?1 ORDER BY mount_id",
        )?;
        let rows = q.query_map([user.id], |r| {
            Ok(Grant {
                mount_id: r.get(0)?,
                role: Role::parse(&r.get::<_, String>(1)?)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
    pub fn role(&self, user_id: &str, mount: &str) -> Result<Option<Role>> {
        role_in(&*self.lock()?, user_id, mount)
    }
    pub fn permits(&self, principal: &Principal, mount: &str, action: Action) -> Result<bool> {
        let c = self.lock()?;
        if !enabled_in(&c)? || !principal_valid_in(&c, principal)? {
            return Ok(false);
        }
        Ok(role_in(&c, &principal.user_id, mount)?.is_some_and(|r| r.permits(action)))
    }
    /// Reconcile memberships after an operator removes a configured mount. This
    /// removes only control metadata, never vault storage or note data.
    pub fn prune_mounts(&self, valid_mounts: &[String]) -> Result<()> {
        let mut c = self.lock()?;
        let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let mut query = tx.prepare("SELECT DISTINCT mount_id FROM control_grants")?;
        let known = query
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(query);
        let mut changed = false;
        for mount in known {
            if !valid_mounts.contains(&mount) {
                tx.execute("DELETE FROM control_grants WHERE mount_id=?1", [&mount])?;
                tx.execute(
                    "UPDATE control_users SET default_mount=NULL WHERE default_mount=?1",
                    [&mount],
                )?;
                changed = true;
            }
        }
        if changed {
            bump(&tx)?;
        }
        tx.commit()?;
        Ok(())
    }
    pub fn set_default_mount(&self, name_or_id: &str, mount: Option<&str>) -> Result<()> {
        let mut c = self.lock()?;
        let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let user = required_user(&tx, name_or_id)?;
        if let Some(m) = mount {
            if role_in(&tx, &user.id, m)?.is_none() {
                bail!("user has no access to mount {m}");
            }
        }
        tx.execute(
            "UPDATE control_users SET default_mount=?2 WHERE id=?1",
            params![user.id, mount],
        )?;
        bump(&tx)?;
        tx.commit()?;
        Ok(())
    }
    pub fn rotate_password(&self, name_or_id: &str) -> Result<GeneratedUser> {
        let password = SecretString::new(random_secret());
        let hash = hash_password(password.expose_secret())?;
        let mut c = self.lock()?;
        let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let user = required_user(&tx, name_or_id)?;
        tx.execute(
            "UPDATE control_users SET hash=?2,generation=generation+1 WHERE id=?1",
            params![user.id, hash],
        )?;
        tx.execute("DELETE FROM control_sessions WHERE user_id=?1", [&user.id])?;
        bump(&tx)?;
        tx.commit()?;
        Ok(GeneratedUser { user, password })
    }
    pub fn remove_user(&self, name_or_id: &str) -> Result<()> {
        let mut c = self.lock()?;
        let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let user = required_user(&tx, name_or_id)?;
        if enabled_in(&tx)? {
            let count: i64 =
                tx.query_row("SELECT count(*) FROM control_users", [], |r| r.get(0))?;
            if count <= 1 {
                bail!("cannot remove the last active user");
            }
            let mut q = tx
                .prepare("SELECT mount_id FROM control_grants WHERE user_id=?1 AND role='owner'")?;
            let mounts = q
                .query_map([&user.id], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            drop(q);
            for mount in mounts {
                if owner_count(&tx, &mount)? <= 1 {
                    bail!("cannot remove the last owner of mount {mount}");
                }
            }
        }
        tx.execute("DELETE FROM control_users WHERE id=?1", [user.id])?;
        bump(&tx)?;
        tx.commit()?;
        Ok(())
    }
    pub fn authenticate(&self, name: &str, password: &str) -> Result<Option<Principal>> {
        let c = self.lock()?;
        if !enabled_in(&c)? {
            return Ok(None);
        }
        let row = c
            .query_row(
                "SELECT id,name,default_mount,hash,generation FROM control_users WHERE name=?1",
                [name],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<String>>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, u64>(4)?,
                    ))
                },
            )
            .optional()?;
        // Unknown users do the same password-hash work to avoid an enumeration shortcut.
        let Some((id, name, default_mount, hash, generation)) = row else {
            let _ = hash_password(password)?;
            return Ok(None);
        };
        if !verify_password(password, &hash) {
            return Ok(None);
        }
        Ok(Some(Principal {
            user_id: id,
            name,
            credential_version: generation,
            default_mount,
        }))
    }
    pub fn principal_valid(&self, principal: &Principal) -> Result<bool> {
        let c = self.lock()?;
        Ok(enabled_in(&c)? && principal_valid_in(&c, principal)?)
    }
    /// Refresh stored public fields and validate the credential generation.
    pub fn principal(&self, user_id: &str, version: u64) -> Result<Option<Principal>> {
        let c = self.lock()?;
        if !enabled_in(&c)? {
            return Ok(None);
        }
        let value=c.query_row("SELECT id,name,generation,default_mount FROM control_users WHERE id=?1 AND generation=?2",params![user_id,version],|r|Ok(Principal{user_id:r.get(0)?,name:r.get(1)?,credential_version:r.get(2)?,default_mount:r.get(3)?})).optional()?;
        Ok(value)
    }
    pub fn create_session(&self, principal: &Principal, ttl_seconds: u64) -> Result<SecretString> {
        if ttl_seconds == 0 || ttl_seconds > 2_592_000 {
            bail!("invalid session lifetime");
        }
        let token = SecretString::new(random_secret());
        let digest = token_digest(token.expose_secret());
        let expires = now()
            .checked_add(ttl_seconds)
            .ok_or_else(|| anyhow::anyhow!("invalid session expiry"))?;
        let c = self.lock()?;
        if !enabled_in(&c)? || !principal_valid_in(&c, principal)? {
            bail!("invalid user authorization");
        }
        c.execute(
            "INSERT INTO control_sessions VALUES(?1,?2,?3,?4)",
            params![
                digest,
                principal.user_id,
                principal.credential_version,
                expires
            ],
        )?;
        Ok(token)
    }
    pub fn session(&self, token: &str) -> Result<Option<Principal>> {
        let c = self.lock()?;
        if !enabled_in(&c)? {
            return Ok(None);
        }
        let value=c.query_row("SELECT u.id,u.name,u.generation,u.default_mount FROM control_sessions s JOIN control_users u ON u.id=s.user_id AND u.generation=s.generation WHERE s.digest=?1 AND s.expires>?2",params![token_digest(token),now()],|r|Ok(Principal{user_id:r.get(0)?,name:r.get(1)?,credential_version:r.get(2)?,default_mount:r.get(3)?})).optional()?;
        Ok(value)
    }
    pub fn revoke_session(&self, token: &str) -> Result<()> {
        self.lock()?.execute(
            "DELETE FROM control_sessions WHERE digest=?1",
            [token_digest(token)],
        )?;
        Ok(())
    }
}

fn user_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<User> {
    Ok(User {
        id: r.get(0)?,
        name: r.get(1)?,
        default_mount: r.get(2)?,
    })
}
fn lookup_user(c: &Connection, key: &str) -> Result<Option<User>> {
    Ok(c.query_row("SELECT id,name,default_mount FROM control_users WHERE name=?1 OR id=?1 ORDER BY CASE WHEN name=?1 THEN 0 ELSE 1 END LIMIT 1",[key],user_row).optional()?)
}
fn required_user(c: &Connection, key: &str) -> Result<User> {
    lookup_user(c, key)?.ok_or_else(|| anyhow::anyhow!("user not found"))
}
fn enabled_in(c: &Connection) -> Result<bool> {
    Ok(c.query_row(
        "SELECT value FROM control_meta WHERE key='enabled'",
        [],
        |r| r.get::<_, i64>(0),
    )? != 0)
}
fn principal_valid_in(c: &Connection, p: &Principal) -> Result<bool> {
    Ok(c.query_row(
        "SELECT EXISTS(SELECT 1 FROM control_users WHERE id=?1 AND generation=?2)",
        params![p.user_id, p.credential_version],
        |r| r.get(0),
    )?)
}
fn role_in(c: &Connection, user: &str, mount: &str) -> Result<Option<Role>> {
    Ok(c.query_row(
        "SELECT role FROM control_grants WHERE user_id=?1 AND mount_id=?2",
        params![user, mount],
        |r| Role::parse(&r.get::<_, String>(0)?),
    )
    .optional()?)
}
fn owner_count(c: &Connection, mount: &str) -> Result<i64> {
    Ok(c.query_row(
        "SELECT count(*) FROM control_grants WHERE mount_id=?1 AND role='owner'",
        [mount],
        |r| r.get(0),
    )?)
}
fn bump(c: &Connection) -> Result<()> {
    c.execute(
        "UPDATE control_meta SET value=value+1 WHERE key='revision'",
        [],
    )?;
    Ok(())
}
fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
    {
        bail!("username must be 1–64 ASCII letters, digits, dots, underscores or hyphens");
    }
    Ok(())
}
fn validate_mount(mount: &str, valid: &[String]) -> Result<()> {
    if !valid.iter().any(|v| v == mount) {
        bail!("unknown mount {mount}");
    }
    Ok(())
}
fn validate_initial(initial: &InitialUser, valid: &[String]) -> Result<()> {
    let mut seen = std::collections::HashSet::new();
    for g in &initial.grants {
        validate_mount(&g.mount_id, valid)?;
        if !seen.insert(&g.mount_id) {
            bail!("duplicate mount assignment");
        }
    }
    if let Some(default) = &initial.default_mount {
        if !initial.grants.iter().any(|g| &g.mount_id == default) {
            bail!("default mount requires a grant");
        }
    }
    Ok(())
}
fn generated_user(name: &str) -> Result<GeneratedUser> {
    Ok(GeneratedUser {
        user: User {
            id: format!("usr_{}", random_secret()),
            name: name.to_owned(),
            default_mount: None,
        },
        password: SecretString::new(random_secret()),
    })
}
fn insert_generated(c: &Connection, g: &GeneratedUser) -> Result<()> {
    let hash = hash_password(g.password.expose_secret())?;
    c.execute(
        "INSERT INTO control_users(id,name,hash,generation,default_mount) VALUES(?1,?2,?3,1,?4)",
        params![g.user.id, g.user.name, hash, g.user.default_mount],
    )?;
    Ok(())
}
fn random_secret() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn hash_password(password: &str) -> Result<String> {
    let salt = SaltString::generate(&mut rand::rngs::OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|_| anyhow::anyhow!("password hashing failed"))
}
fn verify_password(password: &str, hash: &str) -> bool {
    argon2::PasswordHash::new(hash).ok().is_some_and(|h| {
        Argon2::default()
            .verify_password(password.as_bytes(), &h)
            .is_ok()
    })
}
fn token_digest(token: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(token.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (tempfile::TempDir, ControlStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = ControlStore::open(dir.path().join("control.sqlite")).unwrap();
        (dir, store)
    }
    fn mounts() -> Vec<String> {
        vec!["private".into(), "shared".into(), "archive".into()]
    }
    fn initial(name: &str, grants: &[(&str, Role)]) -> InitialUser {
        InitialUser {
            name: name.into(),
            grants: grants
                .iter()
                .map(|(m, r)| Grant {
                    mount_id: (*m).into(),
                    role: *r,
                })
                .collect(),
            default_mount: grants.first().map(|(m, _)| (*m).into()),
        }
    }
    fn activate(store: &ControlStore) -> Vec<GeneratedUser> {
        store
            .enable(
                &[
                    initial("paul", &[("private", Role::Owner), ("shared", Role::Owner)]),
                    initial("laura", &[("shared", Role::Reader)]),
                ],
                &mounts(),
            )
            .unwrap()
    }
    fn login(store: &ControlStore, g: &GeneratedUser) -> Principal {
        store
            .authenticate(&g.user.name, g.password.expose_secret())
            .unwrap()
            .unwrap()
    }

    #[test]
    fn enable_is_atomic_and_rejects_invalid_users_or_mounts() {
        let (_d, s) = fixture();
        assert!(!s.enabled().unwrap());
        assert!(s.enable(&[], &mounts()).is_err());
        assert!(s
            .enable(&[initial("paul", &[("unknown", Role::Owner)])], &mounts())
            .is_err());
        assert!(s
            .enable(&[initial("paul", &[("shared", Role::Reader)])], &mounts())
            .is_err());
        assert!(s
            .enable(
                &[
                    initial("same", &[("shared", Role::Owner)]),
                    initial("same", &[])
                ],
                &mounts()
            )
            .is_err());
        assert!(!s.enabled().unwrap());
        assert!(s.list_users().unwrap().is_empty());
        let users = activate(&s);
        assert_eq!(users.len(), 2);
        assert!(s.enabled().unwrap());
        assert!(s.enable(&[initial("extra", &[])], &mounts()).is_err());
        assert_eq!(s.list_users().unwrap().len(), 2);
    }
    #[test]
    fn grants_filter_private_content_and_update_existing_authorizations() {
        let (_d, s) = fixture();
        let users = activate(&s);
        let paul = login(&s, &users[0]);
        let laura = login(&s, &users[1]);
        assert!(s.permits(&paul, "private", Action::Read).unwrap());
        assert!(!s.permits(&laura, "private", Action::Read).unwrap());
        assert!(s.permits(&laura, "shared", Action::Read).unwrap());
        assert!(!s.permits(&laura, "shared", Action::Write).unwrap());
        s.grant("laura", "shared", Role::Editor, &mounts()).unwrap();
        assert!(s.permits(&laura, "shared", Action::Write).unwrap());
        s.grant("laura", "shared", Role::Reader, &mounts()).unwrap();
        assert!(!s.permits(&laura, "shared", Action::Write).unwrap());
        let rev = s.revision().unwrap();
        s.revoke("laura", "shared").unwrap();
        assert!(s.revision().unwrap() > rev);
        assert!(!s.permits(&laura, "shared", Action::Read).unwrap());
        assert_eq!(s.get_user("laura").unwrap().unwrap().default_mount, None);
    }
    #[test]
    fn rotation_invalidates_password_sessions_and_generation() {
        let (_d, s) = fixture();
        let users = activate(&s);
        let p = login(&s, &users[1]);
        let token = s.create_session(&p, 3600).unwrap();
        assert!(s.session(token.expose_secret()).unwrap().is_some());
        let rotated = s.rotate_password("laura").unwrap();
        assert_ne!(
            users[1].password.expose_secret(),
            rotated.password.expose_secret()
        );
        assert!(s
            .authenticate("laura", users[1].password.expose_secret())
            .unwrap()
            .is_none());
        assert!(s.session(token.expose_secret()).unwrap().is_none());
        assert!(!s.principal_valid(&p).unwrap());
        assert!(!s.permits(&p, "shared", Action::Read).unwrap());
        assert!(s
            .principal(&p.user_id, p.credential_version)
            .unwrap()
            .is_none());
        let fresh = login(&s, &rotated);
        assert!(fresh.credential_version > p.credential_version);
        assert!(s.permits(&fresh, "shared", Action::Read).unwrap());
        assert!(s.authenticate("laura", "incorrect").unwrap().is_none());
        assert!(s.authenticate("unknown", "incorrect").unwrap().is_none());
    }
    #[test]
    fn owners_and_last_user_are_protected_but_disable_is_explicit() {
        let (_d, s) = fixture();
        let users = activate(&s);
        assert!(s.remove_user("paul").is_err());
        assert!(s.grant("paul", "private", Role::Editor, &mounts()).is_err());
        assert!(s.revoke("paul", "private").is_err());
        s.grant("laura", "private", Role::Owner, &mounts()).unwrap();
        s.grant("laura", "shared", Role::Owner, &mounts()).unwrap();
        s.remove_user("paul").unwrap();
        assert!(s.remove_user("laura").is_err());
        let p = login(&s, &users[1]);
        let token = s.create_session(&p, 3600).unwrap();
        s.disable().unwrap();
        assert!(!s.enabled().unwrap());
        assert!(s.list_users().unwrap().is_empty());
        assert!(s.session(token.expose_secret()).unwrap().is_none());
        assert!(!s.principal_valid(&p).unwrap());
        assert!(s
            .authenticate("laura", users[1].password.expose_secret())
            .unwrap()
            .is_none());
        let new = s
            .enable(
                &[initial("new-owner", &[("private", Role::Owner)])],
                &mounts(),
            )
            .unwrap();
        assert_eq!(new.len(), 1);
        assert_eq!(s.list_users().unwrap()[0].name, "new-owner");
    }
    #[test]
    fn persists_private_verifiers_without_plaintext_passwords_or_tokens() {
        let (d, s) = fixture();
        let users = activate(&s);
        let p = login(&s, &users[0]);
        let token = s.create_session(&p, 3600).unwrap();
        let c = Connection::open(d.path().join("control.sqlite")).unwrap();
        let hash: String = c
            .query_row(
                "SELECT hash FROM control_users WHERE name='paul'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(hash.starts_with("$argon2id$"));
        assert!(!hash.contains(users[0].password.expose_secret()));
        let digest: String = c
            .query_row("SELECT digest FROM control_sessions", [], |r| r.get(0))
            .unwrap();
        assert_ne!(&digest, token.expose_secret());
        assert_eq!(digest.len(), 64);
        let reopened = ControlStore::open(d.path().join("control.sqlite")).unwrap();
        assert!(reopened.enabled().unwrap());
        assert!(reopened.session(token.expose_secret()).unwrap().is_some());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(d.path().join("control.sqlite"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        let sentinel = d.path().join("vault-note.md");
        std::fs::write(&sentinel, "keep data").unwrap();
        reopened.disable().unwrap();
        assert_eq!(std::fs::read_to_string(sentinel).unwrap(), "keep data");
        for table in ["control_users", "control_grants", "control_sessions"] {
            let count: i64 = c
                .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
                .unwrap();
            assert_eq!(count, 0);
        }
    }
    #[test]
    fn staged_users_require_ownership_and_default_mount_membership() {
        let (_d, s) = fixture();
        let first = s.add_user("first").unwrap();
        assert!(s
            .authenticate("first", first.password.expose_secret())
            .unwrap()
            .is_none());
        assert!(s.add_user("first").is_err());
        assert!(s.add_user("bad name").is_err());
        s.grant("first", "shared", Role::Reader, &mounts()).unwrap();
        assert!(s.enable_existing(&mounts()).is_err());
        s.grant("first", "shared", Role::Owner, &mounts()).unwrap();
        s.enable_existing(&mounts()).unwrap();
        assert!(s.set_default_mount("first", Some("private")).is_err());
        s.set_default_mount("first", Some("shared")).unwrap();
        assert_eq!(
            s.get_user("first").unwrap().unwrap().default_mount,
            Some("shared".into())
        );
        assert!(s
            .grant("first", "private", Role::Reader, &mounts())
            .is_err());
        assert!(s
            .grant("first", "nonexistent", Role::Owner, &mounts())
            .is_err());
        let p = login(&s, &first);
        assert!(s.create_session(&p, 0).is_err());
        assert!(s.create_session(&p, 2_592_001).is_err());
        let t = s.create_session(&p, 60).unwrap();
        s.revoke_session(t.expose_secret()).unwrap();
        assert!(s.session(t.expose_secret()).unwrap().is_none());
    }
    #[test]
    fn audit_records_are_bounded_and_contain_only_metadata() {
        let (_d, s) = fixture();
        assert!(s.record_event("", None, "users.list").is_err());
        assert!(s.record_event("operator", None, "bad\noperation").is_err());
        s.record_event("operator", Some("shared"), "users.grant")
            .unwrap();
        s.record_event("user-id", Some("private"), "read_file")
            .unwrap();
        let events = s.audit_events(1).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].actor, "user-id");
        assert_eq!(events[0].mount_id.as_deref(), Some("private"));
        assert_eq!(events[0].operation, "read_file");
        assert!(s.audit_events(0).unwrap().is_empty());
        assert_eq!(s.audit_events(100).unwrap().len(), 2);
    }
    #[test]
    fn removed_mounts_drop_only_obsolete_grants_and_defaults() {
        let (_d, s) = fixture();
        activate(&s);
        s.set_default_mount("paul", Some("private")).unwrap();
        s.prune_mounts(&["shared".to_owned()]).unwrap();
        let paul = s.get_user("paul").unwrap().unwrap();
        assert!(paul.default_mount.is_none());
        assert_eq!(s.grants("paul").unwrap().len(), 1);
        assert_eq!(s.grants("paul").unwrap()[0].mount_id, "shared");
        assert_eq!(s.list_users().unwrap().len(), 2);
    }
    #[test]
    fn separate_connections_serialize_changes() {
        let (d, s) = fixture();
        activate(&s);
        let other = ControlStore::open(d.path().join("control.sqlite")).unwrap();
        let handle = std::thread::spawn(move || other.add_user("racing"));
        let a = s.add_user("racing");
        let b = handle.join().unwrap();
        assert_ne!(a.is_ok(), b.is_ok());
        assert_eq!(s.list_users().unwrap().len(), 3);
    }
}
