//! One-time migration of historical Hermes-local `TraceDecay` session stores.
//!
//! Runtime storage never resolves through Hermes. This module only scans the
//! historical, bounded locations below the user's standard `~/.hermes`
//! directory and copies a provably project-owned store into that project's
//! user-profile shard. Sources are opened read-only and are never deleted.

use std::collections::{BTreeSet, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use libsql::{Connection, Value, params};
use sha2::{Digest, Sha256};

use crate::global_db::GlobalDb;

const LEDGER_DIR: &str = "migration-ledger/hermes-legacy";
const COPIED_TABLES: &[&str] = &[
    "sessions",
    "session_messages",
    "lcm_external_payloads",
    "lcm_raw_messages",
    "lcm_summary_nodes",
    "lcm_summary_sources",
    "lcm_lifecycle_state",
    "lcm_maintenance_debt",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyHermesMigration {
    pub source_db: PathBuf,
    pub target_project: PathBuf,
    pub rows_copied: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyHermesMigrationIssue {
    pub source_db: PathBuf,
    pub reason: String,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct LegacyHermesMigrationReport {
    pub migrated: Vec<LegacyHermesMigration>,
    pub already_migrated: Vec<LegacyHermesMigration>,
    pub unresolved: Vec<LegacyHermesMigrationIssue>,
    pub failed: Vec<LegacyHermesMigrationIssue>,
}

/// Migrates historical stores below the user's standard `~/.hermes` only.
/// `HERMES_HOME` is deliberately ignored: it is not `TraceDecay` storage
/// identity and cannot redirect this migration.
pub async fn migrate_legacy_hermes_stores(user_home: &Path) -> LegacyHermesMigrationReport {
    let Ok(profile_root) = crate::storage::default_profile_root() else {
        return LegacyHermesMigrationReport {
            failed: vec![LegacyHermesMigrationIssue {
                source_db: user_home.join(".hermes/.tracedecay/sessions.db"),
                reason: "could not resolve the TraceDecay user-profile store".to_string(),
            }],
            ..LegacyHermesMigrationReport::default()
        };
    };
    migrate_legacy_hermes_stores_to(user_home, &profile_root).await
}

/// Explicit `TraceDecay` profile-root seam used by migration tests. The source
/// root remains the user's standard home; the second argument controls only
/// the destination `TraceDecay` profile.
pub async fn migrate_legacy_hermes_stores_to(
    user_home: &Path,
    tracedecay_profile_root: &Path,
) -> LegacyHermesMigrationReport {
    migrate_legacy_hermes_stores_inner(user_home, tracedecay_profile_root, None).await
}

async fn migrate_legacy_hermes_stores_inner(
    user_home: &Path,
    tracedecay_profile_root: &Path,
    fail_after_table: Option<&str>,
) -> LegacyHermesMigrationReport {
    let hermes_home = user_home.join(".hermes");
    let mut report = LegacyHermesMigrationReport::default();
    for candidate in legacy_store_candidates(&hermes_home, tracedecay_profile_root) {
        let profile_dir = candidate.profile_dir;
        let source_db = candidate.source_db;
        match migrate_candidate(
            user_home,
            &hermes_home,
            &profile_dir,
            &source_db,
            tracedecay_profile_root,
            fail_after_table,
        )
        .await
        {
            Ok(CandidateOutcome::Migrated(migration)) => report.migrated.push(migration),
            Ok(CandidateOutcome::AlreadyMigrated(migration)) => {
                report.already_migrated.push(migration);
            }
            Err(CandidateError::Unresolved(reason)) => {
                report
                    .unresolved
                    .push(LegacyHermesMigrationIssue { source_db, reason });
            }
            Err(CandidateError::Failed(reason)) => {
                report
                    .failed
                    .push(LegacyHermesMigrationIssue { source_db, reason });
            }
        }
    }
    report
}

struct LegacyStoreCandidate {
    profile_dir: PathBuf,
    source_db: PathBuf,
}

fn legacy_store_candidates(
    hermes_home: &Path,
    tracedecay_profile_root: &Path,
) -> Vec<LegacyStoreCandidate> {
    let profiles = legacy_profile_dirs(hermes_home);
    let mut candidates = profiles
        .iter()
        .filter_map(|profile_dir| {
            let source_db = profile_dir.join(".tracedecay/sessions.db");
            source_db.is_file().then(|| LegacyStoreCandidate {
                profile_dir: profile_dir.clone(),
                source_db,
            })
        })
        .collect::<Vec<_>>();

    // A short-lived historical release could create a user-profile shard
    // whose manifest identified a Hermes profile as the code project. Scan
    // only immediate project shards and accept only exact standard-profile
    // identities; unrelated profile stores are never opened.
    if let Ok(entries) = fs::read_dir(tracedecay_profile_root.join("projects")) {
        for entry in entries.flatten() {
            if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            let shard = entry.path();
            let manifest_path = shard.join(crate::storage::STORE_MANIFEST_FILENAME);
            let Ok(manifest) = crate::storage::read_store_manifest(&manifest_path) else {
                continue;
            };
            let Some(profile_dir) = profiles
                .iter()
                .find(|profile| same_path(profile, &manifest.project_root))
            else {
                continue;
            };
            let source_db = shard.join(crate::storage::SESSIONS_DB_FILENAME);
            if source_db.is_file() {
                candidates.push(LegacyStoreCandidate {
                    profile_dir: profile_dir.clone(),
                    source_db,
                });
            }
        }
    }
    candidates.sort_by(|left, right| left.source_db.cmp(&right.source_db));
    candidates.dedup_by(|left, right| same_path(&left.source_db, &right.source_db));
    candidates
}

fn legacy_profile_dirs(hermes_home: &Path) -> Vec<PathBuf> {
    let mut profiles = vec![hermes_home.to_path_buf()];
    if !hermes_home.is_dir() {
        return profiles;
    }
    if let Ok(entries) = fs::read_dir(hermes_home.join("profiles")) {
        let mut named = entries
            .filter_map(|entry| {
                let entry = entry.ok()?;
                // Do not let a profile symlink turn this bounded scan into an
                // arbitrary filesystem walk.
                entry.file_type().ok()?.is_dir().then(|| entry.path())
            })
            .collect::<Vec<_>>();
        named.sort();
        profiles.extend(named);
    }
    profiles
}

enum CandidateOutcome {
    Migrated(LegacyHermesMigration),
    AlreadyMigrated(LegacyHermesMigration),
}

enum CandidateError {
    Unresolved(String),
    Failed(String),
}

async fn migrate_candidate(
    user_home: &Path,
    hermes_home: &Path,
    profile_dir: &Path,
    source_path: &Path,
    tracedecay_profile_root: &Path,
    fail_after_table: Option<&str>,
) -> Result<CandidateOutcome, CandidateError> {
    let source_db = GlobalDb::open_read_only_at(source_path)
        .await
        .ok_or_else(|| CandidateError::Failed("could not open source read-only".to_string()))?;
    let source = source_db.conn();
    source
        .execute("BEGIN", ())
        .await
        .map_err(|error| CandidateError::Failed(format!("could not snapshot source: {error}")))?;

    let result = migrate_candidate_snapshot(
        user_home,
        hermes_home,
        profile_dir,
        source_path,
        source,
        tracedecay_profile_root,
        fail_after_table,
    )
    .await;
    let finish = source.execute("COMMIT", ()).await;
    match (result, finish) {
        (Ok(outcome), Ok(_)) => Ok(outcome),
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(CandidateError::Failed(format!(
            "could not close source snapshot: {error}"
        ))),
    }
}

async fn migrate_candidate_snapshot(
    user_home: &Path,
    hermes_home: &Path,
    profile_dir: &Path,
    source_path: &Path,
    source: &Connection,
    tracedecay_profile_root: &Path,
    fail_after_table: Option<&str>,
) -> Result<CandidateOutcome, CandidateError> {
    verify_source(source)
        .await
        .map_err(CandidateError::Failed)?;
    let source_schema_version = source_lcm_schema_version(source)
        .await
        .map_err(CandidateError::Failed)?;
    if source_schema_version > crate::sessions::lcm::LCM_SCHEMA_VERSION {
        return Err(CandidateError::Failed(format!(
            "source LCM schema {source_schema_version} is newer than supported schema {}",
            crate::sessions::lcm::LCM_SCHEMA_VERSION
        )));
    }

    let target_project = resolve_target_project(
        source,
        &profile_dir.join("config.yaml"),
        user_home,
        hermes_home,
    )
    .await
    .map_err(CandidateError::Unresolved)?;
    let target_layout = resolve_target_layout(&target_project, tracedecay_profile_root)
        .await
        .map_err(|error| {
            CandidateError::Failed(format!("could not resolve target profile shard: {error}"))
        })?;
    if same_path(source_path, &target_layout.sessions_db_path) {
        return Err(CandidateError::Failed(
            "source and target session databases resolve to the same path".to_string(),
        ));
    }
    let fingerprint = logical_source_fingerprint(source, source_path)
        .await
        .map_err(CandidateError::Failed)?;
    let target_db = GlobalDb::open_at(&target_layout.sessions_db_path)
        .await
        .ok_or_else(|| CandidateError::Failed("could not open target session store".to_string()))?;

    let result = merge_snapshot(
        source,
        source_path,
        target_db.conn(),
        &target_layout.sessions_db_path,
        &target_project,
        &fingerprint,
        source_schema_version,
        fail_after_table,
    )
    .await
    .map_err(CandidateError::Failed)?;
    let migration = LegacyHermesMigration {
        source_db: source_path.to_path_buf(),
        target_project,
        rows_copied: result.rows_copied,
    };
    Ok(if result.already_migrated {
        CandidateOutcome::AlreadyMigrated(migration)
    } else {
        CandidateOutcome::Migrated(migration)
    })
}

async fn resolve_target_layout(
    target_project: &Path,
    tracedecay_profile_root: &Path,
) -> crate::errors::Result<crate::storage::StoreLayout> {
    let production_profile = crate::storage::default_profile_root()
        .is_ok_and(|default| same_path(&default, tracedecay_profile_root));
    if production_profile {
        crate::tracedecay::TraceDecay::resolve_store_layout_for_identity(target_project).await
    } else {
        crate::storage::resolve_layout(target_project, tracedecay_profile_root)
    }
}

async fn verify_source(source: &Connection) -> Result<(), String> {
    let mut rows = source
        .query("PRAGMA quick_check", ())
        .await
        .map_err(|error| format!("source quick_check failed: {error}"))?;
    let result = rows
        .next()
        .await
        .map_err(|error| format!("source quick_check could not be read: {error}"))?
        .and_then(|row| row.get::<String>(0).ok())
        .unwrap_or_default();
    if result == "ok" {
        Ok(())
    } else {
        Err(format!("source quick_check reported: {result}"))
    }
}

async fn source_lcm_schema_version(source: &Connection) -> Result<i64, String> {
    if table_columns(source, "session_schema_migrations")
        .await?
        .is_empty()
    {
        return Ok(0);
    }
    let mut rows = source
        .query(
            "SELECT version FROM session_schema_migrations WHERE name = 'lcm'",
            (),
        )
        .await
        .map_err(|error| format!("could not inspect source schema: {error}"))?;
    match rows
        .next()
        .await
        .map_err(|error| format!("could not read source schema: {error}"))?
    {
        Some(row) => row
            .get(0)
            .map_err(|error| format!("invalid source schema version: {error}")),
        None => Ok(0),
    }
}

async fn resolve_target_project(
    source: &Connection,
    config_path: &Path,
    user_home: &Path,
    hermes_home: &Path,
) -> Result<PathBuf, String> {
    if let Some(pin) = crate::agents::hermes::read_config_pinned_project_root(config_path) {
        return real_project_root(Path::new(&pin), user_home, hermes_home)
            .ok_or_else(|| format!("legacy project pin '{pin}' is not a resolvable code project"));
    }

    let columns = table_columns(source, "sessions").await?;
    if columns.is_empty() {
        return Err("source has no sessions table and no legacy project pin".to_string());
    }
    let path_expr = if columns.iter().any(|column| column == "project_path") {
        "project_path"
    } else {
        "NULL"
    };
    let key_expr = if columns.iter().any(|column| column == "project_key") {
        "project_key"
    } else {
        "NULL"
    };
    let metadata_expr = if columns.iter().any(|column| column == "metadata_json") {
        "metadata_json"
    } else {
        "NULL"
    };
    let sql = format!("SELECT {path_expr}, {key_expr}, {metadata_expr} FROM sessions");
    let mut rows = source
        .query(&sql, ())
        .await
        .map_err(|error| format!("could not read source project metadata: {error}"))?;
    let mut targets = BTreeSet::new();
    while let Some(row) = rows
        .next()
        .await
        .map_err(|error| format!("could not read source project metadata row: {error}"))?
    {
        for candidate in [row.get::<Option<String>>(0), row.get::<Option<String>>(1)]
            .into_iter()
            .flatten()
            .flatten()
        {
            if let Some(root) = real_project_root(Path::new(&candidate), user_home, hermes_home) {
                targets.insert(root);
            }
        }
        if let Ok(Some(metadata)) = row.get::<Option<String>>(2) {
            collect_metadata_project_roots(&metadata, user_home, hermes_home, &mut targets);
        }
    }
    match targets.len() {
        1 => targets
            .into_iter()
            .next()
            .ok_or_else(|| "resolved project target disappeared".to_string()),
        0 => Err("no durable real project path exists in source session metadata".to_string()),
        count => Err(format!(
            "source session metadata maps to {count} projects; refusing an ambiguous migration"
        )),
    }
}

fn collect_metadata_project_roots(
    raw: &str,
    user_home: &Path,
    hermes_home: &Path,
    targets: &mut BTreeSet<PathBuf>,
) {
    let Ok(metadata) = serde_json::from_str::<serde_json::Value>(raw) else {
        return;
    };
    for key in [
        "hermes_session_cwd",
        "hermes_session_worktree",
        "cwd",
        "worktree",
        "project_root",
    ] {
        if let Some(path) = metadata.get(key).and_then(serde_json::Value::as_str)
            && let Some(root) = real_project_root(Path::new(path), user_home, hermes_home)
        {
            targets.insert(root);
        }
    }
}

fn real_project_root(candidate: &Path, user_home: &Path, hermes_home: &Path) -> Option<PathBuf> {
    if !candidate.is_absolute() || !candidate.is_dir() {
        return None;
    }
    let canonical = candidate.canonicalize().ok()?;
    let canonical_user_home = user_home
        .canonicalize()
        .unwrap_or_else(|_| user_home.to_path_buf());
    let canonical_hermes_home = hermes_home
        .canonicalize()
        .unwrap_or_else(|_| hermes_home.to_path_buf());
    if canonical == canonical_user_home || canonical.starts_with(&canonical_hermes_home) {
        return None;
    }
    if let Some(git_root) = crate::worktree::git_worktree_root(&canonical) {
        return Some(git_root);
    }
    crate::config::has_project_database(&canonical).then_some(canonical)
}

fn same_path(left: &Path, right: &Path) -> bool {
    left.canonicalize().unwrap_or_else(|_| left.to_path_buf())
        == right.canonicalize().unwrap_or_else(|_| right.to_path_buf())
}

async fn logical_source_fingerprint(
    source: &Connection,
    source_path: &Path,
) -> Result<String, String> {
    let mut hash = Sha256::new();
    hash.update(b"tracedecay-hermes-legacy-session-store-v1\0");
    hash.update(
        source_path
            .canonicalize()
            .unwrap_or_else(|_| source_path.to_path_buf())
            .to_string_lossy()
            .as_bytes(),
    );
    for table in COPIED_TABLES {
        let columns = table_columns(source, table).await?;
        if columns.is_empty() {
            continue;
        }
        hash.update(b"\0table\0");
        hash.update(table.as_bytes());
        for column in &columns {
            hash.update(b"\0column\0");
            hash.update(column.as_bytes());
        }
        let select = columns
            .iter()
            .map(|column| quote_identifier(column))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT {select} FROM {} ORDER BY rowid",
            quote_identifier(table)
        );
        let mut rows = source
            .query(&sql, ())
            .await
            .map_err(|error| format!("could not fingerprint source table {table}: {error}"))?;
        while let Some(row) = rows
            .next()
            .await
            .map_err(|error| format!("could not fingerprint source row in {table}: {error}"))?
        {
            hash.update(b"\0row\0");
            for index in 0..columns.len() {
                let value = row.get::<Value>(index as i32).map_err(|error| {
                    format!("could not fingerprint source value in {table}: {error}")
                })?;
                hash_sqlite_value(&mut hash, value);
            }
        }
    }
    Ok(hex::encode(hash.finalize()))
}

fn hash_sqlite_value(hash: &mut Sha256, value: Value) {
    match value {
        Value::Null => hash.update(b"n"),
        Value::Integer(value) => {
            hash.update(b"i");
            hash.update(value.to_le_bytes());
        }
        Value::Real(value) => {
            hash.update(b"r");
            hash.update(value.to_bits().to_le_bytes());
        }
        Value::Text(value) => {
            hash.update(b"t");
            hash.update((value.len() as u64).to_le_bytes());
            hash.update(value.as_bytes());
        }
        Value::Blob(value) => {
            hash.update(b"b");
            hash.update((value.len() as u64).to_le_bytes());
            hash.update(value);
        }
    }
}

struct MergeOutcome {
    already_migrated: bool,
    rows_copied: u64,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct MigrationMarker {
    schema_version: u32,
    source_fingerprint: String,
    source_db_path: PathBuf,
    target_project_path: PathBuf,
    source_lcm_schema_version: i64,
    rows_copied: u64,
}

#[allow(clippy::too_many_arguments)]
async fn merge_snapshot(
    source: &Connection,
    source_path: &Path,
    target: &Connection,
    target_path: &Path,
    target_project: &Path,
    fingerprint: &str,
    source_schema_version: i64,
    fail_after_table: Option<&str>,
) -> Result<MergeOutcome, String> {
    if let Some(marker) = read_migration_marker(target_path, fingerprint)? {
        return Ok(MergeOutcome {
            already_migrated: true,
            rows_copied: marker.rows_copied,
        });
    }
    target
        .execute("BEGIN IMMEDIATE", ())
        .await
        .map_err(|error| format!("could not begin target migration: {error}"))?;
    let mut created_payloads = Vec::new();
    let result = merge_snapshot_in_transaction(
        source,
        source_path,
        target,
        target_path,
        target_project,
        fail_after_table,
        &mut created_payloads,
    )
    .await;
    match result {
        Ok(outcome) => {
            if let Err(error) = target.execute("COMMIT", ()).await {
                let _ = target.execute("ROLLBACK", ()).await;
                remove_created_payloads(&created_payloads);
                return Err(format!("could not commit target migration: {error}"));
            }
            write_migration_marker(
                target_path,
                &MigrationMarker {
                    schema_version: 1,
                    source_fingerprint: fingerprint.to_string(),
                    source_db_path: source_path.to_path_buf(),
                    target_project_path: target_project.to_path_buf(),
                    source_lcm_schema_version: source_schema_version,
                    rows_copied: outcome.rows_copied,
                },
            )?;
            Ok(outcome)
        }
        Err(error) => {
            let _ = target.execute("ROLLBACK", ()).await;
            remove_created_payloads(&created_payloads);
            Err(error)
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn merge_snapshot_in_transaction(
    source: &Connection,
    source_path: &Path,
    target: &Connection,
    target_path: &Path,
    target_project: &Path,
    fail_after_table: Option<&str>,
    created_payloads: &mut Vec<PathBuf>,
) -> Result<MergeOutcome, String> {
    copy_external_payload_files(source, source_path, target_path, created_payloads).await?;
    let project = GlobalDb::canonical_project_key(target_project);
    let mut rows_copied = copy_table(source, target, "sessions", &[], |columns, values| {
        for (column, value) in columns.iter().zip(values.iter_mut()) {
            if column == "project_path" || column == "project_key" {
                *value = Value::Text(project.clone());
            }
        }
        Ok(())
    })
    .await?;
    fail_after("sessions", fail_after_table)?;

    rows_copied += copy_table(source, target, "session_messages", &[], |_, _| Ok(())).await?;
    fail_after("session_messages", fail_after_table)?;

    rows_copied += copy_table(source, target, "lcm_external_payloads", &[], |_, _| Ok(())).await?;
    fail_after("lcm_external_payloads", fail_after_table)?;

    let (raw_rows, raw_id_map) = copy_raw_messages(source, target).await?;
    rows_copied += raw_rows;
    fail_after("lcm_raw_messages", fail_after_table)?;

    rows_copied += copy_table(source, target, "lcm_summary_nodes", &[], |_, _| Ok(())).await?;
    fail_after("lcm_summary_nodes", fail_after_table)?;

    rows_copied += copy_table(
        source,
        target,
        "lcm_summary_sources",
        &[],
        |columns, values| remap_summary_source(columns, values, &raw_id_map),
    )
    .await?;
    fail_after("lcm_summary_sources", fail_after_table)?;

    rows_copied += copy_table(
        source,
        target,
        "lcm_lifecycle_state",
        &[],
        |columns, values| {
            remap_store_id_columns(
                columns,
                values,
                &raw_id_map,
                &[
                    "current_frontier_store_id",
                    "last_finalized_frontier_store_id",
                ],
            )
        },
    )
    .await?;
    fail_after("lcm_lifecycle_state", fail_after_table)?;

    rows_copied += copy_table(
        source,
        target,
        "lcm_maintenance_debt",
        &[],
        |columns, values| {
            remap_store_id_columns(
                columns,
                values,
                &raw_id_map,
                &["from_store_id", "to_store_id"],
            )
        },
    )
    .await?;
    fail_after("lcm_maintenance_debt", fail_after_table)?;

    Ok(MergeOutcome {
        already_migrated: false,
        rows_copied,
    })
}

fn marker_path(target_db_path: &Path, fingerprint: &str) -> Result<PathBuf, String> {
    if fingerprint.len() != 64 || !fingerprint.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("invalid legacy-store fingerprint".to_string());
    }
    let data_root = target_db_path
        .parent()
        .ok_or_else(|| "target session DB has no parent directory".to_string())?;
    Ok(data_root
        .join(LEDGER_DIR)
        .join(format!("{fingerprint}.json")))
}

fn read_migration_marker(
    target_db_path: &Path,
    fingerprint: &str,
) -> Result<Option<MigrationMarker>, String> {
    let path = marker_path(target_db_path, fingerprint)?;
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(format!(
                "could not read migration marker '{}': {error}",
                path.display()
            ));
        }
    };
    let marker: MigrationMarker = serde_json::from_slice(&bytes)
        .map_err(|error| format!("migration marker '{}' is invalid: {error}", path.display()))?;
    if marker.schema_version != 1 || marker.source_fingerprint != fingerprint {
        return Err(format!(
            "migration marker '{}' has an unsupported identity",
            path.display()
        ));
    }
    Ok(Some(marker))
}

fn write_migration_marker(target_db_path: &Path, marker: &MigrationMarker) -> Result<(), String> {
    let path = marker_path(target_db_path, &marker.source_fingerprint)?;
    let dir = path
        .parent()
        .ok_or_else(|| "migration marker has no parent directory".to_string())?;
    fs::create_dir_all(dir)
        .map_err(|error| format!("could not create migration ledger directory: {error}"))?;
    let metadata = fs::symlink_metadata(dir)
        .map_err(|error| format!("could not inspect migration ledger directory: {error}"))?;
    if !metadata.file_type().is_dir() {
        return Err("migration ledger path is not a regular directory".to_string());
    }
    if path.exists() {
        read_migration_marker(target_db_path, &marker.source_fingerprint)?;
        return Ok(());
    }
    let bytes = serde_json::to_vec_pretty(marker)
        .map_err(|error| format!("could not encode migration marker: {error}"))?;
    let temp = dir.join(format!(
        ".{}.{}.tmp",
        marker.source_fingerprint,
        std::process::id()
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)
        .map_err(|error| format!("could not create migration marker: {error}"))?;
    let persisted = file
        .write_all(&bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| format!("could not persist migration marker: {error}"));
    drop(file);
    if let Err(error) = persisted {
        let _ = fs::remove_file(&temp);
        return Err(error);
    }
    match fs::hard_link(&temp, &path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            read_migration_marker(target_db_path, &marker.source_fingerprint)?;
        }
        Err(error) => {
            let _ = fs::remove_file(&temp);
            return Err(format!("could not publish migration marker: {error}"));
        }
    }
    let _ = fs::remove_file(&temp);
    File::open(dir)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| format!("could not sync migration ledger directory: {error}"))?;
    Ok(())
}

fn fail_after(table: &str, requested: Option<&str>) -> Result<(), String> {
    if requested == Some(table) {
        Err(format!("injected migration failure after {table}"))
    } else {
        Ok(())
    }
}

async fn copy_table<F>(
    source: &Connection,
    target: &Connection,
    table: &str,
    excluded: &[&str],
    mut transform: F,
) -> Result<u64, String>
where
    F: FnMut(&[String], &mut Vec<Value>) -> Result<(), String>,
{
    let source_columns = table_columns(source, table).await?;
    if source_columns.is_empty() {
        return Ok(0);
    }
    let target_columns = table_columns(target, table).await?;
    if target_columns.is_empty() {
        return Err(format!("target is missing required table {table}"));
    }
    let columns = source_columns
        .into_iter()
        .filter(|column| target_columns.contains(column) && !excluded.contains(&column.as_str()))
        .collect::<Vec<_>>();
    if columns.is_empty() {
        return Ok(0);
    }
    let quoted = columns
        .iter()
        .map(|column| quote_identifier(column))
        .collect::<Vec<_>>()
        .join(", ");
    let placeholders = (1..=columns.len())
        .map(|index| format!("?{index}"))
        .collect::<Vec<_>>()
        .join(", ");
    let select_sql = format!(
        "SELECT {quoted} FROM {} ORDER BY rowid",
        quote_identifier(table)
    );
    let insert_sql = format!(
        "INSERT OR IGNORE INTO {} ({quoted}) VALUES ({placeholders})",
        quote_identifier(table)
    );
    let mut source_rows = source
        .query(&select_sql, ())
        .await
        .map_err(|error| format!("could not read source table {table}: {error}"))?;
    let mut inserted = 0;
    while let Some(row) = source_rows
        .next()
        .await
        .map_err(|error| format!("could not read source row from {table}: {error}"))?
    {
        let mut values = Vec::with_capacity(columns.len());
        for index in 0..columns.len() {
            values.push(
                row.get::<Value>(index as i32).map_err(|error| {
                    format!("could not decode source row from {table}: {error}")
                })?,
            );
        }
        transform(&columns, &mut values)?;
        inserted += target
            .execute(&insert_sql, libsql::params_from_iter(values))
            .await
            .map_err(|error| format!("could not copy source row into {table}: {error}"))?;
    }
    Ok(inserted)
}

async fn copy_raw_messages(
    source: &Connection,
    target: &Connection,
) -> Result<(u64, HashMap<i64, i64>), String> {
    let source_columns = table_columns(source, "lcm_raw_messages").await?;
    if source_columns.is_empty() {
        return Ok((0, HashMap::new()));
    }
    if !source_columns.iter().any(|column| column == "store_id") {
        return Err("source lcm_raw_messages has no store_id".to_string());
    }
    let target_columns = table_columns(target, "lcm_raw_messages").await?;
    let columns = source_columns
        .into_iter()
        .filter(|column| column != "store_id" && target_columns.contains(column))
        .collect::<Vec<_>>();
    let provider_index = columns
        .iter()
        .position(|column| column == "provider")
        .ok_or_else(|| "source raw messages have no provider".to_string())?;
    let message_index = columns
        .iter()
        .position(|column| column == "message_id")
        .ok_or_else(|| "source raw messages have no message_id".to_string())?;
    let quoted = columns
        .iter()
        .map(|column| quote_identifier(column))
        .collect::<Vec<_>>()
        .join(", ");
    let placeholders = (1..=columns.len())
        .map(|index| format!("?{index}"))
        .collect::<Vec<_>>()
        .join(", ");
    let select_sql = format!("SELECT store_id, {quoted} FROM lcm_raw_messages ORDER BY store_id");
    let insert_sql =
        format!("INSERT OR IGNORE INTO lcm_raw_messages ({quoted}) VALUES ({placeholders})");
    let mut rows = source
        .query(&select_sql, ())
        .await
        .map_err(|error| format!("could not read source raw messages: {error}"))?;
    let mut inserted = 0;
    let mut id_map = HashMap::new();
    while let Some(row) = rows
        .next()
        .await
        .map_err(|error| format!("could not read source raw message: {error}"))?
    {
        let source_id: i64 = row
            .get(0)
            .map_err(|error| format!("invalid source raw store_id: {error}"))?;
        let provider: String = row
            .get((provider_index + 1) as i32)
            .map_err(|error| format!("invalid source raw provider: {error}"))?;
        let message_id: String = row
            .get((message_index + 1) as i32)
            .map_err(|error| format!("invalid source raw message_id: {error}"))?;
        let mut values = Vec::with_capacity(columns.len());
        for index in 0..columns.len() {
            values.push(
                row.get::<Value>((index + 1) as i32)
                    .map_err(|error| format!("could not decode source raw message: {error}"))?,
            );
        }
        inserted += target
            .execute(&insert_sql, libsql::params_from_iter(values))
            .await
            .map_err(|error| format!("could not copy source raw message: {error}"))?;
        let mut target_rows = target
            .query(
                "SELECT store_id FROM lcm_raw_messages WHERE provider = ?1 AND message_id = ?2",
                params![provider, message_id],
            )
            .await
            .map_err(|error| format!("could not resolve target raw store_id: {error}"))?;
        let target_id = target_rows
            .next()
            .await
            .map_err(|error| format!("could not read target raw store_id: {error}"))?
            .ok_or_else(|| "copied raw message is absent from target".to_string())?
            .get(0)
            .map_err(|error| format!("invalid target raw store_id: {error}"))?;
        id_map.insert(source_id, target_id);
    }
    Ok((inserted, id_map))
}

fn remap_summary_source(
    columns: &[String],
    values: &mut [Value],
    id_map: &HashMap<i64, i64>,
) -> Result<(), String> {
    let kind_index = columns
        .iter()
        .position(|column| column == "source_kind")
        .ok_or_else(|| "summary source has no source_kind".to_string())?;
    let id_index = columns
        .iter()
        .position(|column| column == "source_id")
        .ok_or_else(|| "summary source has no source_id".to_string())?;
    if matches!(&values[kind_index], Value::Text(kind) if kind == "raw_message") {
        let Value::Text(source_id) = &values[id_index] else {
            return Err("raw summary source has a non-text source_id".to_string());
        };
        let source_id = source_id
            .parse::<i64>()
            .map_err(|_| "raw summary source has an invalid store_id".to_string())?;
        let target_id = id_map
            .get(&source_id)
            .ok_or_else(|| format!("raw summary source {source_id} was not copied"))?;
        values[id_index] = Value::Text(target_id.to_string());
    }
    Ok(())
}

fn remap_store_id_columns(
    columns: &[String],
    values: &mut [Value],
    id_map: &HashMap<i64, i64>,
    remapped_columns: &[&str],
) -> Result<(), String> {
    for (column, value) in columns.iter().zip(values.iter_mut()) {
        if !remapped_columns.contains(&column.as_str()) {
            continue;
        }
        let Value::Integer(source_id) = value else {
            continue;
        };
        let target_id = id_map
            .get(source_id)
            .ok_or_else(|| format!("referenced raw store_id {source_id} was not copied"))?;
        *value = Value::Integer(*target_id);
    }
    Ok(())
}

async fn copy_external_payload_files(
    source: &Connection,
    source_db_path: &Path,
    target_db_path: &Path,
    created: &mut Vec<PathBuf>,
) -> Result<(), String> {
    if table_columns(source, "lcm_external_payloads")
        .await?
        .is_empty()
    {
        return Ok(());
    }
    let source_dir = source_db_path
        .parent()
        .ok_or_else(|| "source session DB has no parent directory".to_string())?
        .join("lcm-payloads");
    let target_dir = target_db_path
        .parent()
        .ok_or_else(|| "target session DB has no parent directory".to_string())?
        .join("lcm-payloads");
    let mut rows = source
        .query(
            "SELECT payload_ref, content_hash FROM lcm_external_payloads ORDER BY payload_ref",
            (),
        )
        .await
        .map_err(|error| format!("could not inspect source payloads: {error}"))?;
    while let Some(row) = rows
        .next()
        .await
        .map_err(|error| format!("could not read source payload: {error}"))?
    {
        let payload_ref: String = row
            .get(0)
            .map_err(|error| format!("invalid source payload ref: {error}"))?;
        let expected_hash: String = row
            .get(1)
            .map_err(|error| format!("invalid source payload hash: {error}"))?;
        crate::sessions::lcm::payload::validate_payload_ref(&payload_ref)
            .map_err(|error| format!("unsafe source payload ref '{payload_ref}': {error}"))?;
        let source_file = source_dir.join(&payload_ref);
        let metadata = fs::symlink_metadata(&source_file).map_err(|error| {
            format!(
                "source payload '{}' is unavailable: {error}",
                source_file.display()
            )
        })?;
        if !metadata.file_type().is_file() {
            return Err(format!(
                "source payload '{}' is not a regular file",
                source_file.display()
            ));
        }
        let bytes = fs::read(&source_file).map_err(|error| {
            format!(
                "could not read source payload '{}': {error}",
                source_file.display()
            )
        })?;
        let actual_hash = hex::encode(Sha256::digest(&bytes));
        if actual_hash != expected_hash {
            return Err(format!(
                "source payload '{}' failed its content hash",
                source_file.display()
            ));
        }
        fs::create_dir_all(&target_dir)
            .map_err(|error| format!("could not create target payload directory: {error}"))?;
        let target_metadata = fs::symlink_metadata(&target_dir)
            .map_err(|error| format!("could not inspect target payload directory: {error}"))?;
        if !target_metadata.file_type().is_dir() {
            return Err("target payload directory is not a regular directory".to_string());
        }
        let target_file = target_dir.join(&payload_ref);
        if target_file.exists() {
            let existing = fs::read(&target_file)
                .map_err(|error| format!("could not read existing target payload: {error}"))?;
            if hex::encode(Sha256::digest(&existing)) != expected_hash {
                return Err(format!(
                    "target payload '{}' conflicts with the legacy source",
                    target_file.display()
                ));
            }
            continue;
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target_file)
            .map_err(|error| format!("could not create target payload: {error}"))?;
        created.push(target_file.clone());
        file.write_all(&bytes)
            .and_then(|()| file.sync_all())
            .map_err(|error| format!("could not persist target payload: {error}"))?;
    }
    Ok(())
}

fn remove_created_payloads(paths: &[PathBuf]) {
    for path in paths.iter().rev() {
        let _ = fs::remove_file(path);
    }
}

async fn table_columns(conn: &Connection, table: &str) -> Result<Vec<String>, String> {
    let sql = format!("PRAGMA table_info({})", quote_identifier(table));
    let mut rows = conn
        .query(&sql, ())
        .await
        .map_err(|error| format!("could not inspect table {table}: {error}"))?;
    let mut columns = Vec::new();
    while let Some(row) = rows
        .next()
        .await
        .map_err(|error| format!("could not read table {table} columns: {error}"))?
    {
        columns.push(
            row.get(1)
                .map_err(|error| format!("invalid table {table} column: {error}"))?,
        );
    }
    Ok(columns)
}

fn quote_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sessions::{SessionMessageRecord, SessionRecord};

    fn mark_real_project(project: &Path) {
        fs::create_dir_all(project.join(".tracedecay")).unwrap();
        fs::write(project.join(".tracedecay/tracedecay.db"), []).unwrap();
    }

    async fn seed_source(path: &Path, sessions: &[(&str, &Path)]) {
        let db = GlobalDb::open_at(path).await.expect("open source");
        for (ordinal, (session_id, project)) in sessions.iter().enumerate() {
            let project = project.to_string_lossy().to_string();
            assert!(
                db.upsert_session(&SessionRecord {
                    provider: "hermes".into(),
                    session_id: (*session_id).into(),
                    project_key: project.clone(),
                    project_path: project,
                    title: Some("legacy".into()),
                    started_at: Some(ordinal as i64 + 1),
                    ended_at: None,
                    transcript_path: None,
                    metadata_json: None,
                    parent_session_id: None,
                    is_subagent: false,
                    agent_id: None,
                    parent_tool_use_id: None,
                })
                .await
            );
            assert!(
                db.upsert_session_message(&SessionMessageRecord {
                    provider: "hermes".into(),
                    message_id: format!("message-{session_id}"),
                    session_id: (*session_id).into(),
                    role: "user".into(),
                    timestamp: Some(ordinal as i64 + 1),
                    ordinal: 0,
                    text: "keep this".into(),
                    kind: None,
                    model: None,
                    tool_names: None,
                    source_path: None,
                    source_offset: None,
                    metadata_json: None,
                })
                .await
            );
        }
    }

    async fn count(conn: &Connection, table: &str) -> i64 {
        let mut rows = conn
            .query(&format!("SELECT COUNT(*) FROM {table}"), ())
            .await
            .unwrap();
        rows.next().await.unwrap().unwrap().get(0).unwrap()
    }

    fn marker_count(target_db_path: &Path) -> usize {
        target_db_path
            .parent()
            .and_then(|root| fs::read_dir(root.join(LEDGER_DIR)).ok())
            .map(|entries| entries.flatten().count())
            .unwrap_or(0)
    }

    #[tokio::test]
    async fn migrates_standard_profile_store_once() {
        let temp = tempfile::tempdir().unwrap();
        let user_home = temp.path().join("home");
        let profile_root = temp.path().join("tracedecay-profile");
        let project = temp.path().join("project");
        mark_real_project(&project);
        let hermes = user_home.join(".hermes");
        let source = hermes.join(".tracedecay/sessions.db");
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        fs::write(
            hermes.join("config.yaml"),
            format!(
                "plugins:\n  tracedecay:\n    project_root: {}\n",
                project.display()
            ),
        )
        .unwrap();
        seed_source(&source, &[("session-1", &project)]).await;

        let first = migrate_legacy_hermes_stores_to(&user_home, &profile_root).await;
        assert_eq!(first.migrated.len(), 1, "{first:?}");
        assert!(first.migrated[0].rows_copied >= 3);
        let layout = crate::storage::resolve_layout(&project, &profile_root).unwrap();
        let target = GlobalDb::open_read_only_at(&layout.sessions_db_path)
            .await
            .unwrap();
        assert_eq!(count(target.conn(), "sessions").await, 1);
        assert_eq!(count(target.conn(), "session_messages").await, 1);
        assert_eq!(count(target.conn(), "lcm_raw_messages").await, 1);
        assert_eq!(marker_count(&layout.sessions_db_path), 1);
        assert_eq!(
            target
                .get_session("hermes", "session-1")
                .await
                .unwrap()
                .project_path,
            GlobalDb::canonical_project_key(&project)
        );
        drop(target);

        let second = migrate_legacy_hermes_stores_to(&user_home, &profile_root).await;
        assert_eq!(second.already_migrated.len(), 1, "{second:?}");
        let target = GlobalDb::open_read_only_at(&layout.sessions_db_path)
            .await
            .unwrap();
        assert_eq!(count(target.conn(), "sessions").await, 1);
        assert_eq!(marker_count(&layout.sessions_db_path), 1);
        let source_after = GlobalDb::open_read_only_at(&source).await.unwrap();
        assert_eq!(count(source_after.conn(), "sessions").await, 1);
    }

    #[tokio::test]
    async fn ambiguous_metadata_is_preserved_and_reported() {
        let temp = tempfile::tempdir().unwrap();
        let user_home = temp.path().join("home");
        let profile_root = temp.path().join("tracedecay-profile");
        let first = temp.path().join("first");
        let second = temp.path().join("second");
        mark_real_project(&first);
        mark_real_project(&second);
        let source = user_home.join(".hermes/.tracedecay/sessions.db");
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        seed_source(&source, &[("first", &first), ("second", &second)]).await;

        let report = migrate_legacy_hermes_stores_to(&user_home, &profile_root).await;
        assert_eq!(report.unresolved.len(), 1, "{report:?}");
        assert!(report.unresolved[0].reason.contains("ambiguous"));
        let source_after = GlobalDb::open_read_only_at(&source).await.unwrap();
        assert_eq!(count(source_after.conn(), "sessions").await, 2);
        assert!(
            !crate::storage::resolve_layout(&first, &profile_root)
                .unwrap()
                .sessions_db_path
                .exists()
        );
    }

    #[tokio::test]
    async fn one_unpinned_metadata_project_is_migrated() {
        let temp = tempfile::tempdir().unwrap();
        let user_home = temp.path().join("home");
        let profile_root = temp.path().join("tracedecay-profile");
        let project = temp.path().join("project");
        mark_real_project(&project);
        let source = user_home.join(".hermes/.tracedecay/sessions.db");
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        seed_source(&source, &[("session", &project)]).await;

        let report = migrate_legacy_hermes_stores_to(&user_home, &profile_root).await;
        assert_eq!(report.migrated.len(), 1, "{report:?}");
        assert_eq!(
            report.migrated[0].target_project,
            project.canonicalize().unwrap()
        );
    }

    #[tokio::test]
    async fn migrates_profile_shard_misidentified_as_hermes_project() {
        let temp = tempfile::tempdir().unwrap();
        let user_home = temp.path().join("home");
        let profile_root = temp.path().join("tracedecay-profile");
        let hermes = user_home.join(".hermes");
        let project = temp.path().join("project");
        mark_real_project(&project);
        let legacy_shard = profile_root.join("projects/legacy-hermes-identity");
        let source = legacy_shard.join(crate::storage::SESSIONS_DB_FILENAME);
        fs::create_dir_all(&legacy_shard).unwrap();
        let manifest = crate::storage::StoreManifest {
            schema_version: crate::storage::STORE_MANIFEST_SCHEMA_VERSION,
            project_id: Some("legacy-hermes-identity".into()),
            store_kind: crate::storage::StoreKind::CodeProject,
            storage_mode: crate::storage::StorageMode::ProfileSharded,
            project_root: hermes.clone(),
            data_root: legacy_shard.clone(),
            graph_db_relpath: PathBuf::from("tracedecay.db"),
            sessions_db_relpath: PathBuf::from(crate::storage::SESSIONS_DB_FILENAME),
            branch_meta_relpath: PathBuf::from(crate::storage::BRANCH_META_FILENAME),
        };
        fs::write(
            legacy_shard.join(crate::storage::STORE_MANIFEST_FILENAME),
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();
        seed_source(&source, &[("session", &project)]).await;

        let report = migrate_legacy_hermes_stores_to(&user_home, &profile_root).await;
        assert_eq!(report.migrated.len(), 1, "{report:?}");
        assert_eq!(report.migrated[0].source_db, source);
        let source_after = GlobalDb::open_read_only_at(&source).await.unwrap();
        assert_eq!(count(source_after.conn(), "sessions").await, 1);
        let target_layout = crate::storage::resolve_layout(&project, &profile_root).unwrap();
        assert_ne!(target_layout.sessions_db_path, source);
        let target = GlobalDb::open_read_only_at(&target_layout.sessions_db_path)
            .await
            .unwrap();
        assert_eq!(count(target.conn(), "sessions").await, 1);
    }

    #[tokio::test]
    async fn migrates_older_source_with_missing_current_columns_and_lcm_tables() {
        let temp = tempfile::tempdir().unwrap();
        let user_home = temp.path().join("home");
        let profile_root = temp.path().join("tracedecay-profile");
        let project = temp.path().join("project");
        mark_real_project(&project);
        let source = user_home.join(".hermes/.tracedecay/sessions.db");
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        let source_handle = libsql::Builder::new_local(&source).build().await.unwrap();
        let source_conn = source_handle.connect().unwrap();
        source_conn
            .execute_batch(
                "CREATE TABLE sessions (
                    provider TEXT NOT NULL,
                    session_id TEXT NOT NULL,
                    project_key TEXT NOT NULL,
                    project_path TEXT NOT NULL,
                    title TEXT,
                    PRIMARY KEY(provider, session_id)
                 );
                 CREATE TABLE session_messages (
                    provider TEXT NOT NULL,
                    message_id TEXT NOT NULL,
                    session_id TEXT NOT NULL,
                    role TEXT NOT NULL,
                    ordinal INTEGER NOT NULL,
                    text TEXT NOT NULL,
                    PRIMARY KEY(provider, message_id)
                 );",
            )
            .await
            .unwrap();
        let project_text = project.to_string_lossy().to_string();
        source_conn
            .execute(
                "INSERT INTO sessions(provider, session_id, project_key, project_path, title)
                 VALUES ('hermes', 'old-session', ?1, ?1, 'old')",
                [project_text],
            )
            .await
            .unwrap();
        source_conn
            .execute(
                "INSERT INTO session_messages(provider, message_id, session_id, role, ordinal, text)
                 VALUES ('hermes', 'old-message', 'old-session', 'user', 0, 'old text')",
                (),
            )
            .await
            .unwrap();
        drop(source_conn);
        drop(source_handle);

        let report = migrate_legacy_hermes_stores_to(&user_home, &profile_root).await;
        assert_eq!(report.migrated.len(), 1, "{report:?}");
        let layout = crate::storage::resolve_layout(&project, &profile_root).unwrap();
        let target = GlobalDb::open_read_only_at(&layout.sessions_db_path)
            .await
            .unwrap();
        assert_eq!(count(target.conn(), "sessions").await, 1);
        assert_eq!(count(target.conn(), "session_messages").await, 1);
        assert_eq!(count(target.conn(), "lcm_raw_messages").await, 0);
    }

    #[tokio::test]
    async fn hermes_profile_path_is_never_a_migration_target() {
        let temp = tempfile::tempdir().unwrap();
        let user_home = temp.path().join("home");
        let profile_root = temp.path().join("tracedecay-profile");
        let hermes = user_home.join(".hermes");
        let source = hermes.join(".tracedecay/sessions.db");
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        fs::write(hermes.join(".tracedecay/tracedecay.db"), []).unwrap();
        seed_source(&source, &[("session", &hermes)]).await;

        let report = migrate_legacy_hermes_stores_to(&user_home, &profile_root).await;
        assert_eq!(report.unresolved.len(), 1, "{report:?}");
        assert!(report.migrated.is_empty());
    }

    #[tokio::test]
    async fn future_source_schema_is_rejected_without_target_changes() {
        let temp = tempfile::tempdir().unwrap();
        let user_home = temp.path().join("home");
        let profile_root = temp.path().join("tracedecay-profile");
        let project = temp.path().join("project");
        mark_real_project(&project);
        let source = user_home.join(".hermes/.tracedecay/sessions.db");
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        seed_source(&source, &[("session", &project)]).await;
        let source_rw = GlobalDb::open_at(&source).await.unwrap();
        source_rw
            .conn()
            .execute(
                "UPDATE session_schema_migrations SET version = ?1 WHERE name = 'lcm'",
                [crate::sessions::lcm::LCM_SCHEMA_VERSION + 1],
            )
            .await
            .unwrap();
        drop(source_rw);

        let report = migrate_legacy_hermes_stores_to(&user_home, &profile_root).await;
        assert_eq!(report.failed.len(), 1, "{report:?}");
        assert!(report.failed[0].reason.contains("newer"));
        assert!(
            !crate::storage::resolve_layout(&project, &profile_root)
                .unwrap()
                .sessions_db_path
                .exists()
        );
    }

    #[tokio::test]
    async fn injected_failure_rolls_back_and_retry_converges() {
        let temp = tempfile::tempdir().unwrap();
        let user_home = temp.path().join("home");
        let profile_root = temp.path().join("tracedecay-profile");
        let project = temp.path().join("project");
        mark_real_project(&project);
        let source = user_home.join(".hermes/.tracedecay/sessions.db");
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        seed_source(&source, &[("session", &project)]).await;
        let layout = crate::storage::resolve_layout(&project, &profile_root).unwrap();
        let target = GlobalDb::open_at(&layout.sessions_db_path).await.unwrap();
        assert_eq!(count(target.conn(), "sessions").await, 0);
        drop(target);

        let failed =
            migrate_legacy_hermes_stores_inner(&user_home, &profile_root, Some("sessions")).await;
        assert_eq!(failed.failed.len(), 1, "{failed:?}");
        let target = GlobalDb::open_read_only_at(&layout.sessions_db_path)
            .await
            .unwrap();
        assert_eq!(count(target.conn(), "sessions").await, 0);
        assert_eq!(marker_count(&layout.sessions_db_path), 0);
        drop(target);
        let source_after = GlobalDb::open_read_only_at(&source).await.unwrap();
        assert_eq!(count(source_after.conn(), "sessions").await, 1);
        drop(source_after);

        let retry = migrate_legacy_hermes_stores_to(&user_home, &profile_root).await;
        assert_eq!(retry.migrated.len(), 1, "{retry:?}");
    }

    #[test]
    fn standard_scan_never_consults_hermes_home_environment() {
        let temp = tempfile::tempdir().unwrap();
        let standard = temp.path().join("home/.hermes");
        let redirected = temp.path().join("redirected");
        fs::create_dir_all(standard.join("profiles/alpha")).unwrap();
        fs::create_dir_all(&redirected).unwrap();
        let profiles = legacy_profile_dirs(&standard);
        assert_eq!(
            profiles,
            vec![standard.clone(), standard.join("profiles/alpha")]
        );
        assert!(!profiles.contains(&redirected));
    }
}
