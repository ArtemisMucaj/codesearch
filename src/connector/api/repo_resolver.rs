//! Auto-resolution of a repository's indexing context from the global metadata.
//!
//! The `repositories` and `namespace_config` tables are global (one per DuckDB
//! file, not namespace-scoped), so a single read-only query can map a working
//! directory to the namespace it was indexed under — and to the embedding
//! configuration that namespace requires — without the caller having to know
//! either up front.
//!
//! Matching prefers the git remote (a stable, clone-independent key) and falls
//! back to the canonical on-disk path.

use std::path::{Path, PathBuf};
use std::thread::sleep;
use std::time::Duration;

use duckdb::{params, AccessMode, Config, Connection};
use tracing::debug;

use crate::application::git_remote::detect_remote;
use crate::connector::adapter::NamespaceEmbeddingConfig;
use crate::connector::api::container::is_lock_conflict;

/// Maximum number of retry attempts when the read-only open is blocked by a
/// concurrent writer (e.g. an in-progress `codesearch index`). Mirrors the
/// retry budget used when the container itself opens the database read-only, so
/// resolution lands on the right namespace instead of silently falling back to
/// the default once the writer releases the lock.
const LOCK_RETRIES: u32 = 5;

/// Initial backoff for lock-conflict retries; doubles each attempt.
const LOCK_RETRY_INITIAL_MS: u64 = 500;

/// The indexing context resolved for a working directory.
#[derive(Debug, Clone)]
pub struct ResolvedContext {
    /// Namespace the repository was indexed under.
    pub namespace: String,
    /// Embedding backend recorded for that namespace (`"onnx"` / `"api"`).
    pub embedding_target: Option<String>,
    /// Embedding model recorded for that namespace.
    pub embedding_model: Option<String>,
    /// Embedding dimensionality recorded for that namespace.
    pub embedding_dimensions: Option<usize>,
    /// Human-readable repository name (for log messages).
    pub repository_name: String,
    /// UUID primary key of the repository row.
    pub repository_id: String,
    /// How the repository was matched: `"git remote"` or `"path"`.
    pub matched_by: &'static str,
}

/// Resolve the indexing context for `repo_root` from the database at `db_path`.
///
/// Returns `None` when the database does not exist, the repository has not been
/// indexed, or it carries no namespace. All failures degrade to `None` so the
/// caller can simply fall back to defaults — resolution is a convenience, never
/// a hard requirement.
pub fn resolve(db_path: &Path, repo_root: &Path) -> Option<ResolvedContext> {
    if !db_path.exists() {
        return None;
    }

    let conn = open_read_only_with_retry(db_path)?;

    // Prefer the git remote; fall back to the canonical path.
    let remote = detect_remote(repo_root);
    let (namespace, repository_name, repository_id, matched_by) = remote
        .as_deref()
        .and_then(|r| find_by_remote(&conn, r).map(|(ns, name, id)| (ns, name, id, "git remote")))
        .or_else(|| {
            canonical(repo_root)
                .and_then(|p| find_by_path(&conn, &p))
                .map(|(ns, name, id)| (ns, name, id, "path"))
        })?;

    let (embedding_target, embedding_model, embedding_dimensions) =
        find_namespace_config(&conn, &namespace).unwrap_or((None, None, None));

    Some(ResolvedContext {
        namespace,
        embedding_target,
        embedding_model,
        embedding_dimensions,
        repository_name,
        repository_id,
        matched_by,
    })
}

/// Read the stored embedding configuration for `namespace`, if it exists.
///
/// This is the source of truth written by `codesearch create` (or by the
/// first index run into an uncreated namespace); commands consult it so the
/// embedding setup never has to be re-specified on the command line.  All
/// failures degrade to `None` and the caller falls back to defaults.
pub fn namespace_embedding_config(
    db_path: &Path,
    namespace: &str,
) -> Option<NamespaceEmbeddingConfig> {
    if !db_path.exists() {
        return None;
    }
    let conn = open_read_only_with_retry(db_path)?;
    let schema = namespace.trim();
    let schema_name = if schema.is_empty() { "main" } else { schema };
    match find_namespace_config(&conn, schema_name)? {
        (Some(embedding_target), Some(embedding_model), Some(dimensions)) => {
            Some(NamespaceEmbeddingConfig {
                embedding_target,
                embedding_model,
                dimensions,
            })
        }
        _ => None,
    }
}

fn open_read_only(db_path: &Path) -> Result<Connection, duckdb::Error> {
    let config = Config::default().access_mode(AccessMode::ReadOnly)?;
    Connection::open_with_flags(db_path, config)
}

/// Open the database read-only, retrying with exponential backoff while a
/// concurrent writer holds the lock. Non-lock failures (e.g. the database does
/// not exist yet) return `None` immediately. A persistent lock after all
/// retries also yields `None` — the container open that follows runs its own
/// retry and will surface a clear error to the user.
fn open_read_only_with_retry(db_path: &Path) -> Option<Connection> {
    let mut delay_ms = LOCK_RETRY_INITIAL_MS;
    for attempt in 0..=LOCK_RETRIES {
        match open_read_only(db_path) {
            Ok(conn) => return Some(conn),
            Err(e) if attempt < LOCK_RETRIES && is_lock_conflict(&e.to_string()) => {
                if attempt == 0 {
                    debug!("repo resolver: database locked by another process; waiting…");
                }
                sleep(Duration::from_millis(delay_ms));
                delay_ms *= 2;
            }
            Err(e) => {
                debug!("repo resolver: could not open {}: {}", db_path.display(), e);
                return None;
            }
        }
    }
    None
}

fn canonical(path: &Path) -> Option<PathBuf> {
    std::fs::canonicalize(path).ok()
}

/// Find the namespace + name + id for a repository by its normalised git remote.
/// When several repositories share a remote, the most recently updated wins.
fn find_by_remote(conn: &Connection, remote: &str) -> Option<(String, String, String)> {
    query_repo(
        conn,
        "SELECT namespace, name, id FROM repositories \
         WHERE git_remote = ?1 AND namespace IS NOT NULL \
         ORDER BY updated_at DESC LIMIT 1",
        remote,
    )
}

/// Find the namespace + name + id for a repository by its canonical path.
fn find_by_path(conn: &Connection, path: &Path) -> Option<(String, String, String)> {
    query_repo(
        conn,
        "SELECT namespace, name, id FROM repositories \
         WHERE path = ?1 AND namespace IS NOT NULL LIMIT 1",
        &path.to_string_lossy(),
    )
}

fn query_repo(conn: &Connection, sql: &str, key: &str) -> Option<(String, String, String)> {
    let mut stmt = conn.prepare(sql).ok()?;
    stmt.query_row(params![key], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })
    .ok()
}

/// Resolve the memory project for a working directory.
///
/// Resolution order, most stable identifier first; each step falls through to
/// the next when it cannot produce a confident, stable key:
///
/// 1. **Indexed under a named namespace** (direct git-remote/path match) → the
///    namespace. Repositories the user deliberately indexed together are
///    correlated — they work together — so their sessions share one memory
///    pool.
/// 2. **Has a git remote** → the normalized remote (e.g. `github.com/owner/repo`).
///    The remote survives clones, moves, and renames, and is the same key
///    indexing matches on — so memories written *before* a repo is indexed
///    still line up with sessions run *after*, instead of being orphaned.
/// 3. **Namespace inferred from the directory tree** → when the session ran in
///    a directory that is an ancestor *or* a descendant of an indexed repo, and
///    every such repo (in a user-created namespace) belongs to the *same*
///    namespace, attribute the session to it. A conflict — indexed repos from
///    two different namespaces along that path — is ambiguous, so it infers
///    nothing.
/// 4. **Nothing stable to key on** → `None` (global). A bare directory name is
///    a weak, collision-prone key that also breaks the moment the directory is
///    indexed, so an un-inferable location contributes global memories rather
///    than a throwaway project.
///
/// `db_path` is the metadata database, when one is available. It is optional
/// because some callers (e.g. parsing a transcript file directly) have no
/// database to match against; those simply skip the database-backed steps (1
/// and 3) and rely on the git remote alone. Routing every caller through this
/// one function keeps the fallback chain — and the "global when nothing is
/// stable" decision — defined in a single place.
///
/// All resolution failures (missing database, lock timeouts) degrade to a later
/// step and, ultimately, to `None`.
pub fn resolve_memory_project(db_path: Option<&Path>, cwd: &str) -> Option<String> {
    // 1. Direct match → the namespace the repo was indexed under.
    if let Some(ctx) = db_path.and_then(|db| resolve(db, Path::new(cwd))) {
        if ctx.namespace != crate::cli::DEFAULT_NAMESPACE {
            debug!(
                "memory project for '{}' resolved to namespace '{}' (matched by {})",
                cwd, ctx.namespace, ctx.matched_by
            );
            return Some(ctx.namespace);
        }
    }
    // 2. Git remote — stable across indexing, so a repo's memories keep the same
    //    project whether or not it has been indexed yet. Needs no database.
    if let Some(remote) = detect_remote(Path::new(cwd)) {
        debug!(
            "memory project for '{}' resolved to remote '{}'",
            cwd, remote
        );
        return Some(remote);
    }
    // 3. Infer the namespace from indexed repos along this path (ancestor or
    //    descendant), when they agree on one namespace.
    if let Some(namespace) = db_path.and_then(|db| infer_namespace_from_tree(db, cwd)) {
        debug!(
            "memory project for '{}' inferred namespace '{}' from the directory tree",
            cwd, namespace
        );
        return Some(namespace);
    }
    // 4. Nothing stable to key on → global.
    None
}

/// Infer a namespace for `cwd` from indexed repositories whose canonical path
/// is an ancestor or descendant of `cwd`, restricted to user-created
/// namespaces. Returns the namespace when every matching repo agrees on it, and
/// `None` when nothing matches or the matches span more than one namespace
/// (ambiguous — the directory relates to several unrelated pools).
fn infer_namespace_from_tree(db_path: &Path, cwd: &str) -> Option<String> {
    if !db_path.exists() {
        return None;
    }
    let cwd_canonical = canonical(Path::new(cwd))?;
    let cwd_str = cwd_canonical.to_string_lossy().into_owned();
    let conn = open_read_only_with_retry(db_path)?;

    // Path is stored canonical-absolute, so "on the same branch of the tree"
    // is: the repo path is a prefix of the cwd (repo is an ancestor), or the
    // cwd is a prefix of the repo path (repo is a descendant). A prefix must
    // end at a path boundary so `/a/repo` never matches `/a/repose`.
    let sep = std::path::MAIN_SEPARATOR;
    let cwd_prefix = format!("{cwd_str}{sep}");
    let mut stmt = conn
        .prepare(
            "SELECT DISTINCT namespace FROM repositories \
             WHERE namespace IS NOT NULL AND namespace <> ?1 \
             AND (path = ?2 OR path LIKE ?3 || '%' OR ?2 LIKE path || ?4)",
        )
        .ok()?;
    let namespaces: Vec<String> = stmt
        .query_map(
            params![
                crate::cli::DEFAULT_NAMESPACE,
                cwd_str,
                cwd_prefix,
                format!("{sep}%")
            ],
            |row| row.get::<_, String>(0),
        )
        .ok()?
        .filter_map(Result::ok)
        .collect();

    match namespaces.as_slice() {
        [only] => Some(only.clone()),
        // Zero matches, or a conflict across namespaces → infer nothing.
        _ => None,
    }
}

#[allow(clippy::type_complexity)]
fn find_namespace_config(
    conn: &Connection,
    namespace: &str,
) -> Option<(Option<String>, Option<String>, Option<usize>)> {
    let mut stmt = conn
        .prepare(
            "SELECT embedding_target, embedding_model, dimensions \
             FROM namespace_config WHERE namespace = ?1",
        )
        .ok()?;
    stmt.query_row(params![namespace], |row| {
        let target: String = row.get(0)?;
        let model: String = row.get(1)?;
        let dims: i64 = row.get(2)?;
        Ok((Some(target), Some(model), Some(dims as usize)))
    })
    .ok()
}
