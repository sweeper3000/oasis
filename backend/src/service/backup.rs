//! Portable, encrypted logical database backups. Never execute imported SQL.
use super::app_state::AppState;
use crate::{
    entity::site::Site,
    storage::{
        config::{settings_key, StorageConfig},
        crypto, Workspace,
    },
    util,
};
use anyhow::{anyhow, ensure, Context, Result};
use argon2::{Algorithm, Argon2, Params, Version};
use bip39::{Language, Mnemonic};
use rand::{rngs::OsRng, seq::SliceRandom, RngCore};
use rocket::serde::{Deserialize, Serialize};
use sqlx::{FromRow, Pool, Sqlite};
use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::{
    fs,
    io::{AsyncReadExt, AsyncWriteExt},
};
use zeroize::Zeroizing;

pub const MAX_BACKUP: u64 = 64 * 1024 * 1024;
const MAGIC: &[u8; 8] = b"OASISDB1";
const TTL: Duration = Duration::from_secs(600);

pub enum JobKind {
    Export {
        phrase: Zeroizing<String>,
        positions: [usize; 3],
        attempts: u8,
    },
    Import {
        phrase: Zeroizing<String>,
        workspace: Arc<Workspace>,
    },
    Ready {
        workspace: Arc<Workspace>,
    },
    Working,
}
pub struct Job {
    pub uid: i64,
    expires: Instant,
    pub kind: JobKind,
}

#[derive(Default)]
pub struct BackupManager {
    jobs: Arc<Mutex<HashMap<String, Job>>>,
    pub expensive: Arc<tokio::sync::Mutex<()>>,
}
impl BackupManager {
    pub fn start_reaper(&self) {
        let jobs = self.jobs.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(60));
            loop {
                interval.tick().await;
                jobs.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .retain(|_, j| j.expires > Instant::now());
            }
        });
    }
    pub fn insert(&self, uid: i64, kind: JobKind) -> Result<String> {
        let mut jobs = self
            .jobs
            .lock()
            .map_err(|_| anyhow!("Backup state unavailable"))?;
        jobs.retain(|_, j| j.expires > Instant::now() && j.uid != uid);
        ensure!(jobs.len() < 4, "Too many pending backups. Try again later");
        let id = uuid::Uuid::new_v4().to_string();
        jobs.insert(
            id.clone(),
            Job {
                uid,
                expires: Instant::now() + TTL,
                kind,
            },
        );
        Ok(id)
    }
    pub fn start_import(&self, id: &str, uid: i64) -> Result<Job> {
        let mut jobs = self
            .jobs
            .lock()
            .map_err(|_| anyhow!("Backup state unavailable"))?;
        jobs.retain(|_, j| j.expires > Instant::now());
        ensure!(
            jobs.get(id)
                .map(|j| j.uid == uid && matches!(&j.kind, JobKind::Import { .. }))
                .unwrap_or(false),
            "Backup expired or unavailable"
        );
        let job = jobs.remove(id).unwrap();
        jobs.insert(
            id.into(),
            Job {
                uid,
                expires: job.expires,
                kind: JobKind::Working,
            },
        );
        Ok(job)
    }
    pub fn confirm_export(
        &self,
        id: &str,
        uid: i64,
        answers: &[String],
    ) -> Result<Zeroizing<String>> {
        let mut jobs = self
            .jobs
            .lock()
            .map_err(|_| anyhow!("Backup state unavailable"))?;
        jobs.retain(|_, j| j.expires > Instant::now());
        let job = jobs
            .get_mut(id)
            .filter(|j| j.uid == uid)
            .ok_or_else(|| anyhow!("Backup expired or unavailable"))?;
        let matched = if let JobKind::Export {
            phrase,
            positions,
            attempts,
        } = &mut job.kind
        {
            *attempts += 1;
            let words: Vec<_> = phrase.split_whitespace().collect();
            let matches = answers.len() == 3
                && positions
                    .iter()
                    .zip(answers)
                    .all(|(i, answer)| words[*i] == answer.trim().to_ascii_lowercase());
            if !matches && *attempts < 5 {
                return Err(anyhow!("Recovery words did not match"));
            }
            matches
        } else {
            return Err(anyhow!("Not an export operation"));
        };
        let job = jobs.remove(id).unwrap();
        ensure!(matched, "Recovery words did not match; start a new export");
        if let JobKind::Export { phrase, .. } = job.kind {
            Ok(phrase)
        } else {
            unreachable!()
        }
    }
    pub fn ready(
        &self,
        id: &str,
        uid: i64,
        workspace: Arc<Workspace>,
        expires: Instant,
    ) -> Result<()> {
        ensure!(expires > Instant::now(), "Backup expired or unavailable");
        let mut jobs = self
            .jobs
            .lock()
            .map_err(|_| anyhow!("Backup state unavailable"))?;
        // A cancelled/replaced job must not reappear after a lengthy upload.
        ensure!(
            jobs.get(id)
                .map(|j| j.uid == uid && matches!(&j.kind, JobKind::Working))
                .unwrap_or(false),
            "Import was cancelled or replaced"
        );
        jobs.insert(
            id.into(),
            Job {
                uid,
                expires,
                kind: JobKind::Ready { workspace },
            },
        );
        Ok(())
    }
    pub fn ready_workspace(&self, id: &str, uid: i64) -> Result<Arc<Workspace>> {
        let mut jobs = self
            .jobs
            .lock()
            .map_err(|_| anyhow!("Backup state unavailable"))?;
        jobs.retain(|_, j| j.expires > Instant::now());
        match jobs.get(id).filter(|j| j.uid == uid).map(|j| &j.kind) {
            Some(JobKind::Ready { workspace }) => Ok(workspace.clone()),
            _ => Err(anyhow!("Backup expired or unavailable")),
        }
    }
    pub fn cancel(&self, id: &str, uid: i64) {
        let mut jobs = self.jobs.lock().unwrap_or_else(|e| e.into_inner());
        if jobs.get(id).map(|j| j.uid == uid).unwrap_or(false) {
            jobs.remove(id);
        }
    }
    pub fn clear(&self) {
        self.jobs.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }
}
impl Job {
    pub fn expires(&self) -> Instant {
        self.expires
    }
}

pub fn recovery_phrase() -> Result<(Zeroizing<String>, [usize; 3])> {
    let mut entropy = Zeroizing::new([0u8; 16]);
    OsRng.fill_bytes(&mut *entropy);
    let phrase = Zeroizing::new(Mnemonic::from_entropy(&*entropy)?.to_string());
    let mut positions: Vec<usize> = (0..12).collect();
    positions.shuffle(&mut OsRng);
    let mut chosen = [positions[0], positions[1], positions[2]];
    chosen.sort_unstable();
    Ok((phrase, chosen))
}
pub fn normalize_phrase(value: &str) -> Result<Zeroizing<String>> {
    ensure!(
        value.len() <= 256,
        "Enter the complete 12-word recovery phrase"
    );
    let normalized = Zeroizing::new(
        value
            .split_whitespace()
            .map(str::to_ascii_lowercase)
            .collect::<Vec<_>>()
            .join(" "),
    );
    let mnemonic = Mnemonic::parse_in(Language::English, normalized.as_str())
        .map_err(|_| anyhow!("Invalid recovery phrase"))?;
    ensure!(
        mnemonic.word_count() == 12,
        "Enter the complete 12-word recovery phrase"
    );
    Ok(normalized)
}
async fn backup_key(phrase: Zeroizing<String>, salt: [u8; 16]) -> Result<Zeroizing<[u8; 32]>> {
    tokio::task::spawn_blocking(move || {
        let params = Params::new(65536, 3, 1, Some(32))
            .map_err(|_| anyhow!("Backup encryption unavailable"))?;
        let mut key = Zeroizing::new([0u8; 32]);
        Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
            .hash_password_into(phrase.as_bytes(), &salt, &mut *key)
            .map_err(|_| anyhow!("Could not derive backup key"))?;
        Ok(key)
    })
    .await?
}

pub async fn encrypt(phrase: Zeroizing<String>, workspace: &Workspace) -> Result<()> {
    let mut salt = [0u8; 16];
    OsRng.fill_bytes(&mut salt);
    let key = backup_key(phrase, salt).await?;
    let cipher = workspace.path().join("cipher");
    crypto::encrypt_file(&key, &workspace.path().join("payload"), &cipher).await?;
    fs::remove_file(workspace.path().join("payload")).await?;
    let mut output = fs::File::create(workspace.path().join("archive")).await?;
    output.write_all(MAGIC).await?;
    output.write_all(&salt).await?;
    tokio::io::copy(&mut fs::File::open(&cipher).await?, &mut output).await?;
    output.flush().await?;
    ensure!(
        output.metadata().await?.len() <= MAX_BACKUP,
        "Database backup exceeds 64 MiB"
    );
    fs::remove_file(cipher).await?;
    Ok(())
}
pub async fn decrypt(phrase: Zeroizing<String>, workspace: &Workspace) -> Result<()> {
    let mut input = fs::File::open(workspace.path().join("archive")).await?;
    ensure!(
        input.metadata().await?.len() <= MAX_BACKUP,
        "Database backup exceeds 64 MiB"
    );
    let mut magic = [0u8; 8];
    input
        .read_exact(&mut magic)
        .await
        .context("Invalid database backup")?;
    ensure!(&magic == MAGIC, "Unsupported database backup format");
    let mut salt = [0u8; 16];
    input
        .read_exact(&mut salt)
        .await
        .context("Invalid database backup")?;
    let cipher = workspace.path().join("cipher");
    let mut output = fs::File::create(&cipher).await?;
    tokio::io::copy(&mut input, &mut output).await?;
    output.flush().await?;
    drop(output);
    let key = backup_key(phrase, salt).await?;
    crypto::decrypt_file(&key, &cipher, &workspace.path().join("payload"))
        .await
        .map_err(|_| anyhow!("Incorrect recovery phrase or damaged backup"))?;
    ensure!(
        fs::metadata(workspace.path().join("payload")).await?.len() <= MAX_BACKUP,
        "Database backup exceeds 64 MiB"
    );
    fs::remove_file(cipher).await?;
    fs::remove_file(workspace.path().join("archive")).await?;
    Ok(())
}

#[derive(Serialize, Deserialize, FromRow)]
#[serde(crate = "rocket::serde", deny_unknown_fields)]
pub struct SiteRecord {
    pub site_id: i64,
    pub name: String,
    pub version: String,
    pub storage: String,
    pub language: String,
    pub update_freq: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub allow_guest: i8,
}
#[derive(Serialize, Deserialize, FromRow)]
#[serde(crate = "rocket::serde", deny_unknown_fields)]
pub struct UserRecord {
    pub user_id: i64,
    pub username: String,
    pub password: String,
    pub permission: i8,
    pub created_at: i64,
}
#[derive(Serialize, Deserialize, FromRow)]
#[serde(crate = "rocket::serde", deny_unknown_fields)]
pub struct HiddenRecord {
    pub path: String,
    pub least_permission: i8,
}
#[derive(Serialize, Deserialize)]
#[serde(crate = "rocket::serde", deny_unknown_fields)]
pub struct Snapshot {
    pub format_version: u32,
    pub exported_at: i64,
    pub site: SiteRecord,
    pub users: Vec<UserRecord>,
    pub hidden: Vec<HiddenRecord>,
    pub storage: StorageConfig,
}
impl Snapshot {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.format_version == 1,
            "Unsupported database backup version"
        );
        ensure!(
            self.site.site_id > 0 && !self.site.name.is_empty(),
            "Invalid site record"
        );
        ensure!(
            ["en", "cn"].contains(&self.site.language.as_str())
                && [0, 1].contains(&self.site.allow_guest),
            "Invalid site settings"
        );
        ensure!(
            self.users.iter().any(|u| u.permission == 9),
            "Backup must contain an administrator"
        );
        let mut ids = HashSet::new();
        let mut names = HashSet::new();
        for user in &self.users {
            ensure!(
                user.user_id > 0
                    && ids.insert(user.user_id)
                    && user.username.len() >= 2
                    && names.insert(&user.username)
                    && (1..=9).contains(&user.permission),
                "Invalid or duplicate account record"
            );
            let hash: bcrypt::HashParts = user
                .password
                .parse()
                .map_err(|_| anyhow!("Invalid account password hash"))?;
            ensure!(
                (4..=16).contains(&hash.get_cost()),
                "Unsupported account password hashing cost"
            );
        }
        let mut paths = HashSet::new();
        for hidden in &self.hidden {
            crate::storage::normalize(&hidden.path)
                .map_err(|_| anyhow!("Invalid visibility path"))?;
            ensure!(
                paths.insert(&hidden.path) && (0..=9).contains(&hidden.least_permission),
                "Invalid visibility rule"
            );
        }
        Ok(())
    }
    pub fn destination(&mut self, local: &str) -> Result<()> {
        ensure!(
            Path::new(local).is_absolute(),
            "Select an absolute local storage directory"
        );
        self.storage
            .validate(local)
            .context("Check the destination directory and original storage encryption keys")?;
        self.site.storage = local.into();
        Ok(())
    }
}

pub async fn snapshot(pool: &Pool<Sqlite>) -> Result<Snapshot> {
    let mut tx = pool.begin().await?;
    let site: SiteRecord = sqlx::query_as("SELECT site_id,name,version,storage,language,update_freq,created_at,updated_at,allow_guest FROM site").fetch_one(&mut tx).await?;
    let (secret,): (String,) = sqlx::query_as("SELECT secret FROM site")
        .fetch_one(&mut tx)
        .await?;
    let users = sqlx::query_as(
        "SELECT user_id,username,password,permission,created_at FROM user ORDER BY user_id",
    )
    .fetch_all(&mut tx)
    .await?;
    let hidden = sqlx::query_as("SELECT path,least_permission FROM hidden ORDER BY path")
        .fetch_all(&mut tx)
        .await?;
    let stored: Option<(String,)> =
        sqlx::query_as("SELECT sealed_config FROM storage_config WHERE id=1")
            .fetch_optional(&mut tx)
            .await?;
    let storage = match stored {
        Some((sealed,)) => {
            serde_json::from_slice(&crypto::unseal(&settings_key(&secret), &sealed)?)?
        }
        None => StorageConfig::default(),
    };
    tx.commit().await?;
    let result = Snapshot {
        format_version: 1,
        exported_at: util::get_utc_seconds(),
        site,
        users,
        hidden,
        storage,
    };
    result.validate()?;
    Ok(result)
}
pub async fn read_snapshot(workspace: &Workspace) -> Result<Snapshot> {
    ensure!(
        fs::metadata(workspace.path().join("payload")).await?.len() <= MAX_BACKUP,
        "Database backup exceeds 64 MiB"
    );
    let bytes = Zeroizing::new(fs::read(workspace.path().join("payload")).await?);
    let snapshot: Snapshot =
        serde_json::from_slice(&bytes).map_err(|_| anyhow!("Backup contains invalid records"))?;
    snapshot.validate()?;
    Ok(snapshot)
}
pub async fn verify_password(state: &AppState, uid: i64, password: &str) -> Result<()> {
    ensure!(password.len() <= 1024, "Password verification failed");
    let record: Option<(String, i8)> =
        sqlx::query_as("SELECT password,permission FROM user WHERE user_id=?")
            .bind(uid)
            .fetch_optional(&state.pool)
            .await?;
    let (hash, permission) = record.ok_or_else(|| anyhow!("Password verification failed"))?;
    ensure!(permission == 9, "Password verification failed");
    let password = Zeroizing::new(password.to_owned());
    let valid = tokio::task::spawn_blocking(move || {
        bcrypt::verify(password.as_bytes(), &hash).unwrap_or(false)
    })
    .await?;
    ensure!(valid, "Password verification failed");
    Ok(())
}

pub async fn restore(
    state: AppState,
    uid: i64,
    id: String,
    password: Zeroizing<String>,
    local: String,
    access_token: String,
) -> Result<()> {
    let _gate = state.operations.write().await;
    use super::token::{AccessToken, Token};
    let token = AccessToken::decode(&access_token, &state.get_secret()?)
        .map_err(|_| anyhow!("Installation changed; sign in again"))?;
    ensure!(
        token.uid == uid && token.permission == 9,
        "Installation changed; sign in again"
    );
    verify_password(&state, uid, &password).await?;
    ensure!(
        state.get_upload_tasks()?.is_empty()
            && crate::entity::copy_move_task::CopyMoveTask::allow_new_task(),
        "Wait for uploads and copy/move operations to finish before restoring"
    );
    let workspace = state.backups.ready_workspace(&id, uid)?;
    let mut backup = read_snapshot(&workspace).await?;
    backup.storage.prepare_keys(false).await?;
    backup.destination(&local)?;
    let secret = util::generate_secret_key(32);
    let sealed = crypto::seal(
        &settings_key(&secret),
        &serde_json::to_vec(&backup.storage)?,
    )?;
    let site = Site {
        site_id: backup.site.site_id,
        name: backup.site.name.clone(),
        version: util::get_version_constant(),
        storage: local,
        secret,
        language: backup.site.language.clone(),
        update_freq: backup.site.update_freq.clone(),
        created_at: backup.site.created_at,
        updated_at: util::get_utc_seconds(),
        allow_guest: backup.site.allow_guest,
    };
    let mut tx = state.pool.begin().await?;
    for table in ["reset", "hidden", "user", "site", "storage_config"] {
        // Identifiers are fixed server constants; imported values are always bound.
        sqlx::query(&format!("DELETE FROM {}", table))
            .execute(&mut tx)
            .await?;
    }
    sqlx::query("INSERT INTO site(site_id,name,version,storage,secret,language,update_freq,created_at,updated_at,allow_guest) VALUES(?,?,?,?,?,?,?,?,?,?)")
        .bind(site.site_id).bind(&site.name).bind(&site.version).bind(&site.storage).bind(&site.secret).bind(&site.language)
        .bind(&site.update_freq).bind(site.created_at).bind(site.updated_at).bind(site.allow_guest).execute(&mut tx).await?;
    for user in &backup.users {
        sqlx::query(
            "INSERT INTO user(user_id,username,password,permission,created_at) VALUES(?,?,?,?,?)",
        )
        .bind(user.user_id)
        .bind(&user.username)
        .bind(&user.password)
        .bind(user.permission)
        .bind(user.created_at)
        .execute(&mut tx)
        .await?;
    }
    for hidden in &backup.hidden {
        sqlx::query("INSERT INTO hidden(path,least_permission) VALUES(?,?)")
            .bind(&hidden.path)
            .bind(hidden.least_permission)
            .execute(&mut tx)
            .await?;
    }
    sqlx::query("INSERT INTO storage_config(id,sealed_config) VALUES(1,?)")
        .bind(sealed)
        .execute(&mut tx)
        .await?;
    tx.commit().await?;
    // No fallible work or await points after commit. The exclusive request gate
    // remains held until both caches have been replaced.
    state.file_cache.clear();
    *state.site.lock().unwrap_or_else(|e| e.into_inner()) = site;
    state.site.clear_poison();
    *state.storage.lock().unwrap_or_else(|e| e.into_inner()) = backup.storage;
    state.storage.clear_poison();
    *crate::COPY_MOVE_TASK
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = None;
    state.set_first_run(false);
    state.backups.clear();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::token::{AccessToken, Token};

    #[test]
    fn recovery_words_and_jobs_require_confirmation_ownership_and_freshness() {
        let manager = BackupManager::default();
        let (phrase, positions) = recovery_phrase().unwrap();
        assert_eq!(phrase.split_whitespace().count(), 12);
        assert!(positions[0] < positions[1] && positions[1] < positions[2] && positions[2] < 12);
        assert_eq!(
            *normalize_phrase(&phrase.to_ascii_uppercase().replace(' ', "  \n")).unwrap(),
            *phrase
        );
        assert!(normalize_phrase("only three words").is_err());
        let answers: Vec<_> = positions
            .iter()
            .map(|i| phrase.split_whitespace().nth(*i).unwrap().to_owned())
            .collect();
        let id = manager
            .insert(
                1,
                JobKind::Export {
                    phrase: phrase.clone(),
                    positions,
                    attempts: 0,
                },
            )
            .unwrap();
        assert!(manager.confirm_export(&id, 2, &answers).is_err());
        assert_eq!(*manager.confirm_export(&id, 1, &answers).unwrap(), *phrase);
        assert!(manager.confirm_export(&id, 1, &answers).is_err());
        let id = manager
            .insert(
                1,
                JobKind::Export {
                    phrase: phrase.clone(),
                    positions,
                    attempts: 0,
                },
            )
            .unwrap();
        for _ in 0..5 {
            assert!(manager
                .confirm_export(&id, 1, &vec!["wrong".into(); 3])
                .is_err());
        }
        assert!(manager.confirm_export(&id, 1, &answers).is_err());
        let id = manager
            .insert(
                1,
                JobKind::Export {
                    phrase,
                    positions,
                    attempts: 0,
                },
            )
            .unwrap();
        manager.jobs.lock().unwrap().get_mut(&id).unwrap().expires = Instant::now();
        assert!(manager.confirm_export(&id, 1, &answers).is_err());
    }

    #[tokio::test]
    async fn encrypted_backups_reject_wrong_phrases_tampering_and_truncation() {
        let workspace = Workspace::new().unwrap();
        let content = vec![37u8; crypto::CHUNK + 100];
        fs::write(workspace.path().join("payload"), &content)
            .await
            .unwrap();
        let (phrase, _) = recovery_phrase().unwrap();
        encrypt(phrase.clone(), &workspace).await.unwrap();
        assert!(!workspace.path().join("payload").exists());
        let original = fs::read(workspace.path().join("archive")).await.unwrap();
        assert!(original.starts_with(MAGIC));
        let imported = Workspace::new().unwrap();
        fs::write(imported.path().join("archive"), &original)
            .await
            .unwrap();
        decrypt(phrase.clone(), &imported).await.unwrap();
        assert_eq!(
            fs::read(imported.path().join("payload")).await.unwrap(),
            content
        );
        for kind in ["wrong", "corrupt", "short", "version"] {
            let input = Workspace::new().unwrap();
            let mut bytes = original.clone();
            let key = if kind == "wrong" {
                recovery_phrase().unwrap().0
            } else {
                phrase.clone()
            };
            if kind == "corrupt" {
                *bytes.last_mut().unwrap() ^= 1;
            }
            if kind == "short" {
                bytes.pop();
            }
            if kind == "version" {
                bytes[7] = b'2';
            }
            fs::write(input.path().join("archive"), bytes)
                .await
                .unwrap();
            assert!(decrypt(key, &input).await.is_err());
        }
    }

    #[test]
    fn cancelled_and_replaced_imports_cannot_reappear_and_clean_staging() {
        let manager = BackupManager::default();
        let workspace = Arc::new(Workspace::new().unwrap());
        let path = workspace.path().to_owned();
        let id = manager
            .insert(
                1,
                JobKind::Import {
                    phrase: recovery_phrase().unwrap().0,
                    workspace: workspace.clone(),
                },
            )
            .unwrap();
        drop(workspace);
        let job = manager.start_import(&id, 1).unwrap();
        manager.cancel(&id, 2);
        assert!(manager.jobs.lock().unwrap().contains_key(&id));
        manager.cancel(&id, 1);
        let expires = job.expires();
        if let JobKind::Import { workspace, .. } = job.kind {
            assert!(manager.ready(&id, 1, workspace, expires).is_err());
        }
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn restore_is_atomic_drains_handlers_and_reseals_settings() {
        let directory = tempfile::tempdir().unwrap();
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate::Migrator::new(Path::new("./assets/migrations"))
            .await
            .unwrap()
            .run(&pool)
            .await
            .unwrap();
        let password = "backup-test-password";
        let hashed = bcrypt::hash(password, 4).unwrap();
        sqlx::query("INSERT INTO site(site_id,name,version,storage,secret,language,update_freq,created_at,updated_at,allow_guest) VALUES(1,'Before','0.2.6',?,'original-secret','en','monthly',1,1,0)")
            .bind(directory.path().to_str().unwrap()).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO user(user_id,username,password,permission,created_at) VALUES(1,'before',?,9,1)").bind(hashed).execute(&pool).await.unwrap();
        let mut conn = pool.acquire().await.unwrap();
        let site = Site::read(&mut conn).await.unwrap().unwrap();
        drop(conn);
        let state = AppState::new(Some(site), pool);
        let mut snapshot = snapshot(&state.pool).await.unwrap();
        snapshot.format_version = 2;
        assert!(snapshot.validate().is_err());
        snapshot.format_version = 1;
        snapshot.users[0].permission = 1;
        assert!(snapshot.validate().is_err());
        snapshot.users[0].permission = 9;
        let good_hash = std::mem::replace(&mut snapshot.users[0].password, "invalid hash".into());
        assert!(snapshot.validate().is_err());
        snapshot.users[0].password = good_hash;
        snapshot.hidden.push(HiddenRecord {
            path: "../outside".into(),
            least_permission: 1,
        });
        assert!(snapshot.validate().is_err());
        snapshot.hidden.clear();
        snapshot.site.name = "After".into();
        snapshot.users[0].username = "after".into();
        snapshot.hidden.push(HiddenRecord {
            path: "private.txt".into(),
            least_permission: 1,
        });
        snapshot
            .storage
            .providers
            .push(crate::storage::config::Provider {
                id: "archive".into(),
                name: "Archive".into(),
                root: "bucket".into(),
                password: "retained-secret".into(),
                ..Default::default()
            });
        snapshot.validate().unwrap();
        let workspace = Arc::new(Workspace::new().unwrap());
        fs::write(
            workspace.path().join("payload"),
            serde_json::to_vec(&snapshot).unwrap(),
        )
        .await
        .unwrap();
        let id = state
            .backups
            .insert(1, JobKind::Ready { workspace })
            .unwrap();
        let token = AccessToken::new(1, 9).encode("original-secret").unwrap();
        sqlx::query("CREATE TRIGGER reject_restore BEFORE INSERT ON hidden BEGIN SELECT RAISE(ABORT,'injected failure'); END;").execute(&state.pool).await.unwrap();
        assert!(restore(
            state.clone(),
            1,
            id.clone(),
            Zeroizing::new(password.into()),
            directory.path().to_str().unwrap().into(),
            token.clone()
        )
        .await
        .is_err());
        let (name,): (String,) = sqlx::query_as("SELECT name FROM site")
            .fetch_one(&state.pool)
            .await
            .unwrap();
        assert_eq!(name, "Before");
        assert_eq!(state.get_secret().unwrap(), "original-secret");
        sqlx::query("DROP TRIGGER reject_restore")
            .execute(&state.pool)
            .await
            .unwrap();
        let read = state.operations.read().await;
        let task_state = state.clone();
        let local = directory.path().to_str().unwrap().to_owned();
        let task = tokio::spawn(async move {
            restore(
                task_state,
                1,
                id,
                Zeroizing::new(password.into()),
                local,
                token,
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!task.is_finished());
        drop(read);
        task.await.unwrap().unwrap();
        assert_eq!(state.get_site().unwrap().name, "After");
        let secret = state.get_secret().unwrap();
        assert_ne!(secret, "original-secret");
        let stored = StorageConfig::load(&state.pool, &secret).await.unwrap();
        assert_eq!(stored.providers[0].password, "retained-secret");
        assert!(StorageConfig::load(&state.pool, "original-secret")
            .await
            .is_err());
    }
}
