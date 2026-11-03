//! The SQLite database: inboxes, the PRs they matched, each PR's versions
//! and review comments, and the agent sessions with their transcripts.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use anyhow::{Context, Result, bail};
use grenadine_core::api::{
    AgentBackend, AgentEvent, AgentEventKind, AgentPrs, AgentSession, AgentStatus, AutoReviewEntry,
    AutoReviewState, CiStatus, CleanupItem, CostUnit, Inbox, InboxEdit, Landed, Person, PrDetail,
    PrKey, PrSummary, RepoSettings, ReviewComment, ReviewDecision, ReviewInfo, SessionKind, Stack,
    ThreadCounts, Version, VersionKind,
};
use rusqlite::{Connection, OptionalExtension, params};

enum Migration {
    Sql(&'static str),
    /// Replaces every inbox with the defaults.
    ResetInboxes,
    /// Records the PRs that the sessions' transcripts show they opened, as
    /// open; the next poll looks up their states.
    BackfillAgentPrs,
}

/// Each entry upgrades the schema by one version.
const MIGRATIONS: &[Migration] = &[
    Migration::Sql(
        r#"
    CREATE TABLE inboxes (
        id INTEGER PRIMARY KEY,
        name TEXT NOT NULL,
        filter TEXT NOT NULL,
        position INTEGER NOT NULL,
        error TEXT
    );
    INSERT INTO inboxes (name, filter, position) VALUES
        ('Authored', 'is:open author:@me draft:false', 0),
        ('Drafts', 'is:open author:@me draft:true', 1),
        ('Review requested', 'is:open review-requested:@me draft:false', 2),
        ('Reviewed', 'is:open reviewed-by:@me draft:false', 3),
        ('Involved', 'is:open involves:@me draft:false', 4);

    -- The PRs an inbox's last search found, in GitHub's order.
    CREATE TABLE inbox_prs (
        inbox_id INTEGER NOT NULL REFERENCES inboxes(id) ON DELETE CASCADE,
        repo TEXT NOT NULL,
        number INTEGER NOT NULL,
        rank INTEGER NOT NULL,
        PRIMARY KEY (inbox_id, repo, number)
    );

    CREATE TABLE prs (
        repo TEXT NOT NULL,
        number INTEGER NOT NULL,
        title TEXT NOT NULL,
        body TEXT NOT NULL,
        author TEXT NOT NULL,
        state TEXT NOT NULL,
        is_draft INTEGER NOT NULL,
        url TEXT NOT NULL,
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL,
        base_ref TEXT NOT NULL,
        head_ref TEXT NOT NULL,
        head_oid TEXT NOT NULL,
        approximate INTEGER NOT NULL DEFAULT 0,
        drift TEXT,
        sync_error TEXT,
        -- What updated_at and head_oid were at the last successful sync.
        synced_updated_at TEXT,
        synced_head_oid TEXT,
        PRIMARY KEY (repo, number)
    );

    CREATE TABLE versions (
        repo TEXT NOT NULL,
        number INTEGER NOT NULL,
        idx INTEGER NOT NULL,
        sha TEXT NOT NULL,
        merge_base TEXT,
        kind TEXT NOT NULL,
        pushed_at TEXT,
        missing INTEGER NOT NULL,
        PRIMARY KEY (repo, number, idx)
    );

    CREATE TABLE comments (
        repo TEXT NOT NULL,
        number INTEGER NOT NULL,
        id INTEGER NOT NULL,
        json TEXT NOT NULL,
        PRIMARY KEY (repo, number, id)
    );

    -- Every time recomputing a PR's versions gave a different answer.
    CREATE TABLE drift_log (
        id INTEGER PRIMARY KEY,
        repo TEXT NOT NULL,
        number INTEGER NOT NULL,
        at TEXT NOT NULL,
        old TEXT NOT NULL,
        new TEXT NOT NULL
    );
"#,
    ),
    Migration::Sql(
        r#"
    -- The search's metadata, so a PR shows in its inbox before it has
    -- synced. sync_error holds the error of a PR that has never synced
    -- successfully, because such a PR has no prs row to carry it.
    ALTER TABLE inbox_prs ADD COLUMN title TEXT NOT NULL DEFAULT '';
    ALTER TABLE inbox_prs ADD COLUMN author TEXT NOT NULL DEFAULT '';
    ALTER TABLE inbox_prs ADD COLUMN state TEXT NOT NULL DEFAULT '';
    ALTER TABLE inbox_prs ADD COLUMN is_draft INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE inbox_prs ADD COLUMN url TEXT NOT NULL DEFAULT '';
    ALTER TABLE inbox_prs ADD COLUMN updated_at TEXT NOT NULL DEFAULT '';
    ALTER TABLE inbox_prs ADD COLUMN sync_error TEXT;
"#,
    ),
    Migration::Sql(
        r#"
    -- Sync errors of PRs that have neither a prs row nor an inbox_prs
    -- row, e.g. PRs synced on demand that no inbox covers.
    CREATE TABLE sync_errors (
        repo TEXT NOT NULL,
        number INTEGER NOT NULL,
        error TEXT NOT NULL,
        PRIMARY KEY (repo, number)
    );
"#,
    ),
    Migration::ResetInboxes,
    Migration::Sql(
        r#"
    -- The PR's stack as JSON, NULL until a sync fetched it.
    ALTER TABLE prs ADD COLUMN stack TEXT;
"#,
    ),
    Migration::Sql(
        r#"
    -- pushed_by is the JSON of an api::Person.
    ALTER TABLE versions ADD COLUMN pushed_by TEXT;
    ALTER TABLE versions ADD COLUMN pushed_by_is_guess INTEGER NOT NULL DEFAULT 0;

    -- GitHub users' display names and avatars, so that a sync only looks
    -- up users it hasn't seen recently. name and avatar_url are NULL for
    -- logins that aren't users, e.g. bots.
    CREATE TABLE users (
        login TEXT PRIMARY KEY,
        name TEXT,
        avatar_url TEXT,
        -- Unix seconds.
        fetched_at INTEGER NOT NULL
    );
"#,
    ),
    Migration::Sql(
        r#"
    -- Claude Code sessions started from the UI, each in its own worktree.
    CREATE TABLE agents (
        -- Also Claude Code's session ID.
        id TEXT PRIMARY KEY,
        repo TEXT NOT NULL,
        pr_number INTEGER,
        title TEXT NOT NULL,
        branch TEXT NOT NULL,
        worktree TEXT NOT NULL,
        base_sha TEXT NOT NULL,
        status TEXT NOT NULL,
        -- Whether Claude Code created its session, so that later turns
        -- resume it.
        started INTEGER NOT NULL DEFAULT 0,
        cost_usd REAL NOT NULL DEFAULT 0,
        -- Unix seconds.
        created_at INTEGER NOT NULL
    );

    -- Each session's transcript; event is the JSON of an api::AgentEventKind.
    CREATE TABLE agent_events (
        agent_id TEXT NOT NULL REFERENCES agents (id) ON DELETE CASCADE,
        seq INTEGER NOT NULL,
        event TEXT NOT NULL,
        PRIMARY KEY (agent_id, seq)
    );
"#,
    ),
    Migration::Sql(
        r#"
    -- How full the context window is, NULL until Claude Code reports it.
    ALTER TABLE agents ADD COLUMN context_tokens INTEGER;
    ALTER TABLE agents ADD COLUMN context_window INTEGER;
"#,
    ),
    Migration::Sql(
        r#"
    -- Where wrapping up last landed the session's work, the JSON of an
    -- api::Landed; NULL until then.
    ALTER TABLE agents ADD COLUMN landed TEXT;
"#,
    ),
    Migration::Sql(
        r#"
    -- The model Claude Code runs the session with, NULL until it starts.
    ALTER TABLE agents ADD COLUMN model TEXT;
"#,
    ),
    Migration::Sql(
        r#"
    -- What deleted agent sessions left behind, as scans of their
    -- transcripts found it. The sessions are gone, so their details are
    -- copied rather than referenced.
    CREATE TABLE cleanup_items (
        id INTEGER PRIMARY KEY,
        repo TEXT NOT NULL,
        session_id TEXT NOT NULL,
        session_title TEXT NOT NULL,
        title TEXT NOT NULL,
        detail TEXT NOT NULL,
        -- Unix seconds.
        created_at INTEGER NOT NULL,
        -- The agent session the item was handed off to, if any.
        handed_off_to TEXT
    );
"#,
    ),
    Migration::Sql(
        r#"
    -- Haiku's short summary of the first prompt, NULL until it's written.
    ALTER TABLE agents ADD COLUMN summary TEXT;
"#,
    ),
    Migration::Sql(
        r#"
    -- The PRs each agent session opened, with their state as of the last
    -- poll: OPEN, MERGED or CLOSED.
    CREATE TABLE agent_prs (
        agent_id TEXT NOT NULL REFERENCES agents (id) ON DELETE CASCADE,
        repo TEXT NOT NULL,
        number INTEGER NOT NULL,
        state TEXT NOT NULL,
        PRIMARY KEY (agent_id, repo, number)
    );
"#,
    ),
    Migration::BackfillAgentPrs,
    Migration::Sql(
        r#"
    -- Whether the PR conflicted with its base at the last poll that could
    -- tell.
    ALTER TABLE agent_prs ADD COLUMN conflicting INTEGER NOT NULL DEFAULT 0;
"#,
    ),
    Migration::Sql(
        r#"
    -- The search's review thread counts and the state of the checks on
    -- the PR's head. The counts are NULL until a search fills them; ci is
    -- NULL when the head has no checks.
    ALTER TABLE inbox_prs ADD COLUMN threads_resolved INTEGER;
    ALTER TABLE inbox_prs ADD COLUMN threads_total INTEGER;
    ALTER TABLE inbox_prs ADD COLUMN ci TEXT;
"#,
    ),
    Migration::Sql(
        r#"
    -- Images in sessions' transcripts, which events refer to by id.
    CREATE TABLE agent_images (
        id TEXT PRIMARY KEY,
        agent_id TEXT NOT NULL REFERENCES agents (id) ON DELETE CASCADE,
        mime TEXT NOT NULL,
        data BLOB NOT NULL
    );
"#,
    ),
    Migration::Sql(
        r#"
    -- The PR's reviews, conversation, checks and merge state, the JSON of
    -- an api::ReviewInfo; NULL until a sync fetched them.
    ALTER TABLE prs ADD COLUMN review TEXT;
    -- The state of the checks on the head at the last successful sync;
    -- NULL when it had none.
    ALTER TABLE prs ADD COLUMN synced_ci TEXT;
"#,
    ),
    Migration::Sql(
        r#"
    -- The search's review decision, an api::ReviewDecision as its JSON
    -- string (Approved, ChangesRequested, ...) like in prs.review; NULL
    -- when there's none.
    ALTER TABLE inbox_prs ADD COLUMN review_decision TEXT;
"#,
    ),
    Migration::Sql(
        r#"
    -- Which agent backend runs the session, and the backend's own session
    -- ID once it created the session, when later turns resume it.
    ALTER TABLE agents ADD COLUMN backend TEXT NOT NULL DEFAULT 'claude';
    ALTER TABLE agents ADD COLUMN backend_session_id TEXT;
    UPDATE agents SET backend_session_id = id WHERE started = 1;
    ALTER TABLE agents DROP COLUMN started;

    -- The unit of cost: usd.
    ALTER TABLE agents RENAME COLUMN cost_usd TO cost;
    ALTER TABLE agents ADD COLUMN cost_unit TEXT NOT NULL DEFAULT 'usd';
"#,
    ),
    Migration::Sql(
        r#"
    -- The backend of the session the item was found in.
    ALTER TABLE cleanup_items ADD COLUMN backend TEXT NOT NULL DEFAULT 'claude';
"#,
    ),
    Migration::Sql(
        r#"
    -- Whether the inbox counts in the top bar's total. search_fetched
    -- and search_found record a truncated search: how many hits were
    -- fetched out of how many GitHub found, NULL otherwise.
    ALTER TABLE inboxes ADD COLUMN count_in_total INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE inboxes ADD COLUMN search_fetched INTEGER;
    ALTER TABLE inboxes ADD COLUMN search_found INTEGER;
"#,
    ),
    Migration::ResetInboxes,
    Migration::Sql(
        r#"
    -- Each configured repository's settings, the JSON of an
    -- api::RepoSettings. A repository without a row has the defaults.
    CREATE TABLE repo_settings (
        repo TEXT PRIMARY KEY,
        json TEXT NOT NULL
    );
"#,
    ),
    Migration::Sql(
        r#"
    -- Who started the session, an api::SessionKind.
    ALTER TABLE agents ADD COLUMN kind TEXT NOT NULL DEFAULT 'interactive';

    -- The reviews of the viewer's PRs that the server started. A row
    -- outlives its session, which is deleted once it posted the review,
    -- and keeps the same commit of a PR from being reviewed twice.
    CREATE TABLE auto_reviews (
        id INTEGER PRIMARY KEY,
        repo TEXT NOT NULL,
        number INTEGER NOT NULL,
        head_oid TEXT NOT NULL,
        title TEXT NOT NULL,
        url TEXT NOT NULL,
        -- NULL once the session is deleted.
        agent_id TEXT,
        -- An api::AutoReviewState: running, posted, unposted, failed or
        -- cancelled.
        state TEXT NOT NULL,
        -- Unix seconds.
        started_at INTEGER NOT NULL,
        finished_at INTEGER,
        cost REAL NOT NULL DEFAULT 0,
        cost_unit TEXT NOT NULL DEFAULT 'usd',
        error TEXT,
        UNIQUE (repo, number, head_oid)
    );
"#,
    ),
];

/// Applies migrations up to `target` (a schema version); each runs in
/// its own transaction.
fn migrate(conn: &mut Connection, target: usize) -> Result<()> {
    let current: usize = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    for (i, m) in MIGRATIONS
        .iter()
        .enumerate()
        .skip(current)
        .take(target.saturating_sub(current))
    {
        let tx = conn.transaction()?;
        match m {
            Migration::Sql(sql) => tx.execute_batch(sql)?,
            Migration::ResetInboxes => {
                tx.execute("DELETE FROM inboxes", [])?;
                // count_in_total only exists once its migration has run;
                // on a fresh database this runs at the older position
                // where the column doesn't exist yet.
                let has_count: bool = tx.query_row(
                    "SELECT COUNT(*) > 0 FROM pragma_table_info('inboxes')
                     WHERE name = 'count_in_total'",
                    [],
                    |r| r.get(0),
                )?;
                for (position, inbox) in crate::inboxes::DEFAULT_INBOXES.iter().enumerate() {
                    if has_count {
                        tx.execute(
                            "INSERT INTO inboxes (name, filter, position, count_in_total)
                             VALUES (?, ?, ?, ?)",
                            params![
                                inbox.name,
                                inbox.filter,
                                position as i64,
                                inbox.count_in_total
                            ],
                        )?;
                    } else {
                        tx.execute(
                            "INSERT INTO inboxes (name, filter, position) VALUES (?, ?, ?)",
                            params![inbox.name, inbox.filter, position as i64],
                        )?;
                    }
                }
            }
            Migration::BackfillAgentPrs => backfill_agent_prs(&tx)?,
        }
        tx.pragma_update(None, "user_version", i + 1)?;
        tx.commit()?;
    }
    Ok(())
}

fn backfill_agent_prs(conn: &Connection) -> Result<()> {
    let agents: Vec<(String, String)> = conn
        .prepare("SELECT id, repo FROM agents")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut events =
        conn.prepare("SELECT event FROM agent_events WHERE agent_id = ? ORDER BY seq")?;
    for (id, repo) in agents {
        let mut opened = crate::agent::OpenedPrs::default();
        let mut prs = Vec::new();
        for json in events.query_map([&id], |r| r.get::<_, String>(0))? {
            // Events that no longer parse can't have opened a PR.
            if let Ok(kind) = serde_json::from_str::<AgentEventKind>(&json?) {
                prs.extend(opened.see(&repo, &kind));
            }
        }
        for key in prs {
            conn.execute(
                "INSERT OR IGNORE INTO agent_prs (agent_id, repo, number, state)
                 VALUES (?, ?, ?, 'OPEN')",
                params![id, key.repo, key.number],
            )?;
        }
    }
    Ok(())
}

pub struct Db {
    conn: Mutex<Connection>,
    /// Held for as long as the database is open, so that a second server
    /// can't open it too. A second server would mark the first one's
    /// running sessions as interrupted.
    _lock: Option<std::fs::File>,
}

/// PR metadata from GitHub.
#[derive(Clone, Debug)]
pub struct PrMeta {
    pub key: PrKey,
    pub title: String,
    pub body: String,
    pub author: String,
    pub state: String,
    pub is_draft: bool,
    pub url: String,
    pub created_at: String,
    pub updated_at: String,
    pub base_ref: String,
    pub head_ref: String,
    pub head_oid: String,
    /// The combined state of the checks on the head.
    pub ci: Option<CiStatus>,
    /// `None` keeps the stored one.
    pub review: Option<ReviewInfo>,
    /// Why `review` is `None` although the sync succeeded.
    pub review_error: Option<String>,
}

/// What was true at a PR's last successful sync.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncMark {
    pub updated_at: String,
    pub head_oid: String,
    /// Checks finishing don't change `updated_at`.
    pub ci: Option<CiStatus>,
    /// Whether the sync's own combined state, which leaves out superseded
    /// runs, said checks were running. GitHub's `ci` stays failing while
    /// a superseded run failed, so it doesn't change when they finish.
    pub checks_running: bool,
}

fn kind_str(k: VersionKind) -> &'static str {
    match k {
        VersionKind::Initial => "initial",
        VersionKind::Push => "push",
        VersionKind::ForcePush => "force_push",
    }
}

fn ci_str(c: CiStatus) -> &'static str {
    match c {
        CiStatus::Passing => "passing",
        CiStatus::Failing => "failing",
        CiStatus::Running => "running",
    }
}

fn parse_ci(s: &str) -> Option<CiStatus> {
    match s {
        "passing" => Some(CiStatus::Passing),
        "failing" => Some(CiStatus::Failing),
        "running" => Some(CiStatus::Running),
        _ => None,
    }
}

/// A review decision as its JSON string, the form `prs.review` stores it
/// in, so that queries can compare the two.
fn decision_str(d: ReviewDecision) -> String {
    match serde_json::to_value(d) {
        Ok(serde_json::Value::String(s)) => s,
        _ => unreachable!("a ReviewDecision serializes as a string"),
    }
}

fn parse_decision(s: &str) -> ReviewDecision {
    // Unknown strings become ReviewDecision::Unknown.
    serde_json::from_value(serde_json::Value::String(s.to_owned()))
        .unwrap_or(ReviewDecision::Unknown)
}

fn parse_kind(s: &str) -> VersionKind {
    match s {
        "initial" => VersionKind::Initial,
        "push" => VersionKind::Push,
        _ => VersionKind::ForcePush,
    }
}

/// Locks `<path>.lock`; the lock goes away with the process, however it
/// ends.
fn lock(path: &Path) -> Result<std::fs::File> {
    let mut name = path.as_os_str().to_owned();
    name.push(".lock");
    let lock_path = Path::new(&name);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(lock_path)
        .with_context(|| format!("can't open {}", lock_path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => anyhow::bail!(
            "another server is using {}; give this one its own with --db",
            path.display()
        ),
        Err(std::fs::TryLockError::Error(e)) => {
            Err(e).with_context(|| format!("can't lock {}", lock_path.display()))
        }
    }
}

impl Db {
    pub fn open(path: &Path) -> Result<Db> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let lock = lock(path)?;
        let conn =
            Connection::open(path).with_context(|| format!("can't open {}", path.display()))?;
        Self::init(conn, Some(lock))
    }

    #[cfg(test)]
    pub fn in_memory() -> Db {
        Self::init(Connection::open_in_memory().unwrap(), None).unwrap()
    }

    fn init(mut conn: Connection, lock: Option<std::fs::File>) -> Result<Db> {
        conn.pragma_update(None, "foreign_keys", true)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        migrate(&mut conn, MIGRATIONS.len())?;
        Ok(Db {
            conn: Mutex::new(conn),
            _lock: lock,
        })
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap()
    }

    pub fn inboxes(&self) -> Result<Vec<Inbox>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, name, filter, position, count_in_total FROM inboxes
             ORDER BY position, id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(Inbox {
                id: r.get(0)?,
                name: r.get(1)?,
                filter: r.get(2)?,
                position: r.get(3)?,
                count_in_total: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The repository's settings; the defaults until some are saved.
    pub fn repo_settings(&self, repo: &str) -> Result<RepoSettings> {
        let json: Option<String> = self
            .conn()
            .query_row(
                "SELECT json FROM repo_settings WHERE repo = ?",
                [repo],
                |r| r.get(0),
            )
            .optional()?;
        Ok(match json {
            Some(json) => serde_json::from_str(&json)?,
            None => RepoSettings::default(),
        })
    }

    pub fn set_repo_settings(&self, repo: &str, settings: &RepoSettings) -> Result<()> {
        self.conn().execute(
            "INSERT INTO repo_settings (repo, json) VALUES (?, ?)
             ON CONFLICT (repo) DO UPDATE SET json = excluded.json",
            params![repo, serde_json::to_string(settings)?],
        )?;
        Ok(())
    }

    pub fn inbox_error(&self, id: i64) -> Result<Option<String>> {
        Ok(self
            .conn()
            .query_row("SELECT error FROM inboxes WHERE id = ?", [id], |r| r.get(0))
            .optional()?
            .flatten())
    }

    /// How much of the inbox's last search wasn't fetched, if it was
    /// truncated.
    pub fn inbox_truncation(&self, id: i64) -> Result<Option<grenadine_core::api::Truncation>> {
        let row: Option<(Option<u32>, Option<u32>)> = self
            .conn()
            .query_row(
                "SELECT search_fetched, search_found FROM inboxes WHERE id = ?",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        Ok(row
            .and_then(|(f, n)| f.zip(n))
            .map(|(fetched, found)| grenadine_core::api::Truncation { fetched, found }))
    }

    pub fn create_inbox(&self, edit: &InboxEdit) -> Result<i64> {
        let conn = self.conn();
        let position = match edit.position {
            Some(p) => p,
            None => conn.query_row(
                "SELECT COALESCE(MAX(position) + 1, 0) FROM inboxes",
                [],
                |r| r.get(0),
            )?,
        };
        conn.execute(
            "INSERT INTO inboxes (name, filter, position, count_in_total) VALUES (?, ?, ?, ?)",
            params![edit.name, edit.filter, position, edit.count_in_total],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Returns false when there is no such inbox.
    pub fn update_inbox(&self, id: i64, edit: &InboxEdit) -> Result<bool> {
        let n = self.conn().execute(
            "UPDATE inboxes SET name = ?, filter = ?, position = COALESCE(?, position),
                count_in_total = ?, error = NULL WHERE id = ?",
            params![
                edit.name,
                edit.filter,
                edit.position,
                edit.count_in_total,
                id
            ],
        )?;
        Ok(n > 0)
    }

    pub fn delete_inbox(&self, id: i64) -> Result<bool> {
        Ok(self
            .conn()
            .execute("DELETE FROM inboxes WHERE id = ?", [id])?
            > 0)
    }

    /// Records the outcome of an inbox's search. The rows reflect exactly
    /// the latest search, in rank order; a PR that is listed again keeps
    /// its sync_error. `truncated` says how much of the search GitHub
    /// found that wasn't fetched; a failed search keeps the old rows and
    /// the old truncation.
    pub fn set_inbox_results(
        &self,
        id: i64,
        result: std::result::Result<&[crate::github::Hit], &str>,
        truncated: Option<grenadine_core::api::Truncation>,
    ) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        match result {
            Ok(hits) => {
                for (rank, h) in hits.iter().enumerate() {
                    tx.execute(
                        "INSERT INTO inbox_prs
                            (inbox_id, repo, number, rank, title, author, state, is_draft, url, updated_at,
                             threads_resolved, threads_total, ci, review_decision)
                         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                         ON CONFLICT (inbox_id, repo, number) DO UPDATE SET
                            rank = excluded.rank, title = excluded.title, author = excluded.author,
                            state = excluded.state, is_draft = excluded.is_draft, url = excluded.url,
                            updated_at = excluded.updated_at,
                            threads_resolved = excluded.threads_resolved,
                            threads_total = excluded.threads_total, ci = excluded.ci,
                            review_decision = excluded.review_decision",
                        params![
                            id,
                            h.key.repo,
                            h.key.number,
                            rank,
                            h.title,
                            h.author,
                            h.state,
                            h.is_draft,
                            h.url,
                            h.updated_at,
                            h.threads.resolved,
                            h.threads.total,
                            h.ci.map(ci_str),
                            h.review_decision.map(decision_str),
                        ],
                    )?;
                }
                // Rows for PRs the search no longer reports go away.
                let mut sql = String::from("DELETE FROM inbox_prs WHERE inbox_id = ?");
                if !hits.is_empty() {
                    sql += " AND (repo, number) NOT IN (VALUES ";
                    sql += &vec!["(?, ?)"; hits.len()].join(",");
                    sql += ")";
                }
                let mut args: Vec<&dyn rusqlite::ToSql> = vec![&id];
                for h in hits {
                    args.push(&h.key.repo);
                    args.push(&h.key.number);
                }
                tx.execute(&sql, rusqlite::params_from_iter(args))?;
                tx.execute(
                    "UPDATE inboxes SET error = NULL, search_fetched = ?, search_found = ?
                     WHERE id = ?",
                    params![truncated.map(|t| t.fetched), truncated.map(|t| t.found), id],
                )?;
            }
            // Keep the previous results; a transient error shouldn't empty
            // the inbox.
            Err(e) => {
                tx.execute("UPDATE inboxes SET error = ? WHERE id = ?", params![e, id])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn inbox_prs(&self, id: i64) -> Result<Vec<PrSummary>> {
        let conn = self.conn();
        // Rows that still have the migration's empty defaults take their
        // fields from prs until the next poll rewrites them. The review
        // decision comes from the PR's last sync once one read the
        // reviews: it's what the PR's page shows, and a resync after the
        // viewer reviewed it updates it before the next search does. So
        // does the state of the checks, which leaves out the runs that
        // later runs superseded, but only while the search still reports
        // what GitHub said at that sync; otherwise the search is newer.
        let mut stmt = conn.prepare(
            "SELECT i.repo, i.number,
                    CASE WHEN i.title = '' THEN COALESCE(p.title, '') ELSE i.title END,
                    CASE WHEN i.title = '' THEN COALESCE(p.author, '') ELSE i.author END,
                    CASE WHEN i.title = '' THEN COALESCE(p.state, '') ELSE i.state END,
                    CASE WHEN i.title = '' THEN COALESCE(p.is_draft, 0) ELSE i.is_draft END,
                    CASE WHEN i.title = '' THEN COALESCE(p.updated_at, '') ELSE i.updated_at END,
                    CASE WHEN i.title = '' THEN COALESCE(p.url, '') ELSE i.url END,
                    (SELECT COUNT(*) FROM versions v WHERE v.repo = i.repo AND v.number = i.number),
                    p.repo IS NOT NULL,
                    CASE WHEN p.repo IS NULL THEN i.sync_error END,
                    p.stack, i.threads_resolved, i.threads_total,
                    CASE WHEN p.review IS NOT NULL AND p.synced_ci IS i.ci
                         THEN lower(json_extract(p.review, '$.ci')) ELSE i.ci END,
                    CASE WHEN p.review IS NULL THEN i.review_decision
                         ELSE json_extract(p.review, '$.decision') END
             FROM inbox_prs i LEFT JOIN prs p ON p.repo = i.repo AND p.number = i.number
             WHERE i.inbox_id = ? ORDER BY i.rank",
        )?;
        let rows = stmt.query_map([id], |r| {
            let stack = parse_stack(r.get(11)?);
            let mut summary = summary_from_row(r, r.get(9)?, r.get(10)?, stack.as_ref())?;
            let (resolved, total): (Option<u32>, Option<u32>) = (r.get(12)?, r.get(13)?);
            summary.threads = resolved
                .zip(total)
                .map(|(resolved, total)| ThreadCounts { resolved, total });
            summary.ci = r
                .get::<_, Option<String>>(14)?
                .as_deref()
                .and_then(parse_ci);
            summary.review_decision = r.get::<_, Option<String>>(15)?.map(|s| parse_decision(&s));
            Ok(summary)
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn sync_mark(&self, key: &PrKey) -> Result<Option<SyncMark>> {
        type Row = (Option<String>, Option<String>, Option<String>, bool);
        let row: Option<Row> = self
            .conn()
            .query_row(
                "SELECT synced_updated_at, synced_head_oid, synced_ci,
                        COALESCE(json_extract(review, '$.ci') = 'Running', 0)
                 FROM prs WHERE repo = ? AND number = ?",
                params![key.repo, key.number],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        Ok(match row {
            Some((Some(updated_at), Some(head_oid), ci, checks_running)) => Some(SyncMark {
                updated_at,
                head_oid,
                ci: ci.as_deref().and_then(parse_ci),
                checks_running,
            }),
            _ => None,
        })
    }

    pub fn versions(&self, key: &PrKey) -> Result<Vec<Version>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT idx, sha, merge_base, kind, pushed_at, missing, pushed_by, pushed_by_is_guess
             FROM versions WHERE repo = ? AND number = ? ORDER BY idx",
        )?;
        let rows = stmt.query_map(params![key.repo, key.number], |r| {
            Ok(Version {
                number: r.get(0)?,
                sha: r.get(1)?,
                merge_base: r.get(2)?,
                kind: parse_kind(&r.get::<_, String>(3)?),
                pushed_at: r.get(4)?,
                missing: r.get(5)?,
                pushed_by: r
                    .get::<_, Option<String>>(6)?
                    .and_then(|j| serde_json::from_str(&j).ok()),
                pushed_by_is_guess: r.get(7)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The cached users among `logins` that were fetched at or after
    /// `since` (Unix seconds). A login that isn't a user maps to `None`.
    pub fn cached_users(
        &self,
        logins: &[String],
        since: i64,
    ) -> Result<BTreeMap<String, Option<Person>>> {
        let conn = self.conn();
        let mut stmt =
            conn.prepare("SELECT name, avatar_url FROM users WHERE login = ? AND fetched_at >= ?")?;
        let mut out = BTreeMap::new();
        for login in logins {
            let row: Option<(Option<String>, Option<String>)> = stmt
                .query_row(params![login, since], |r| Ok((r.get(0)?, r.get(1)?)))
                .optional()?;
            if let Some((name, avatar_url)) = row {
                let known = name.is_some() || avatar_url.is_some();
                out.insert(
                    login.clone(),
                    known.then(|| Person {
                        login: Some(login.clone()),
                        name,
                        avatar_url,
                    }),
                );
            }
        }
        Ok(out)
    }

    /// Caches the outcome of looking up `logins` at `at` (Unix seconds).
    /// Logins missing from `found` are cached as not being users.
    pub fn store_users(
        &self,
        logins: &[String],
        found: &BTreeMap<String, Person>,
        at: i64,
    ) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        for login in logins {
            let p = found.get(login);
            tx.execute(
                "INSERT INTO users (login, name, avatar_url, fetched_at) VALUES (?, ?, ?, ?)
                 ON CONFLICT (login) DO UPDATE SET
                    name = excluded.name, avatar_url = excluded.avatar_url,
                    fetched_at = excluded.fetched_at",
                params![
                    login,
                    p.and_then(|p| p.name.as_deref()),
                    p.and_then(|p| p.avatar_url.as_deref()),
                    at
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Stores the result of a successful sync. A `stack` of `None` keeps
    /// the stored stack, and so does a `meta.review` of `None` the stored
    /// review; `meta.review_error` becomes the PR's sync error.
    pub fn store_sync(
        &self,
        meta: &PrMeta,
        versions: &[Version],
        comments: &[ReviewComment],
        stack: Option<&Stack>,
        approximate: bool,
        drift: Option<(&str, &str)>,
    ) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let key = &meta.key;
        let drift_text = drift
            .map(|(old, new)| format!("Versions changed on recomputation: was {old}, now {new}"));
        let stack_json = stack.map(serde_json::to_string).transpose()?;
        let review_json = meta
            .review
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;
        tx.execute(
            "INSERT INTO prs (repo, number, title, body, author, state, is_draft, url, created_at, updated_at,
                              base_ref, head_ref, head_oid, approximate, drift, sync_error,
                              synced_updated_at, synced_head_oid, stack, review, synced_ci)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?19, ?10, ?13, ?16, ?17, ?18)
             ON CONFLICT (repo, number) DO UPDATE SET
                title = ?3, body = ?4, author = ?5, state = ?6, is_draft = ?7, url = ?8, created_at = ?9,
                updated_at = ?10, base_ref = ?11, head_ref = ?12, head_oid = ?13, approximate = ?14,
                drift = COALESCE(?15, drift), sync_error = ?19,
                synced_updated_at = ?10, synced_head_oid = ?13, stack = COALESCE(?16, stack),
                review = COALESCE(?17, review), synced_ci = ?18",
            params![
                key.repo,
                key.number,
                meta.title,
                meta.body,
                meta.author,
                meta.state,
                meta.is_draft,
                meta.url,
                meta.created_at,
                meta.updated_at,
                meta.base_ref,
                meta.head_ref,
                meta.head_oid,
                approximate,
                drift_text,
                stack_json,
                review_json,
                meta.ci.map(ci_str),
                meta.review_error,
            ],
        )?;
        // The error a not-yet-synced listing carried is now obsolete.
        tx.execute(
            "UPDATE inbox_prs SET sync_error = NULL WHERE repo = ? AND number = ?",
            params![key.repo, key.number],
        )?;
        tx.execute(
            "DELETE FROM sync_errors WHERE repo = ? AND number = ?",
            params![key.repo, key.number],
        )?;
        if let Some((old, new)) = drift {
            tx.execute(
                "INSERT INTO drift_log (repo, number, at, old, new) VALUES (?, ?, datetime('now'), ?, ?)",
                params![key.repo, key.number, old, new],
            )?;
        }
        tx.execute(
            "DELETE FROM versions WHERE repo = ? AND number = ?",
            params![key.repo, key.number],
        )?;
        for v in versions {
            let pushed_by = v
                .pushed_by
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?;
            tx.execute(
                "INSERT INTO versions (repo, number, idx, sha, merge_base, kind, pushed_at, missing,
                                       pushed_by, pushed_by_is_guess)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                params![
                    key.repo,
                    key.number,
                    v.number,
                    v.sha,
                    v.merge_base,
                    kind_str(v.kind),
                    v.pushed_at,
                    v.missing,
                    pushed_by,
                    v.pushed_by_is_guess
                ],
            )?;
        }
        tx.execute(
            "DELETE FROM comments WHERE repo = ? AND number = ?",
            params![key.repo, key.number],
        )?;
        for c in comments {
            tx.execute(
                "INSERT INTO comments (repo, number, id, json) VALUES (?, ?, ?, ?)",
                params![key.repo, key.number, c.id, serde_json::to_string(c)?],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// The stored title, `None` before the PR's first sync.
    pub fn pr_title(&self, key: &PrKey) -> Result<Option<String>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT title FROM prs WHERE repo = ? AND number = ?",
                params![key.repo, key.number],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Records a failed sync. The error goes wherever the PR is known; a
    /// PR that has never synced and isn't in any inbox only has its
    /// sync_errors row.
    pub fn store_sync_error(&self, key: &PrKey, error: &str) -> Result<()> {
        let conn = self.conn();
        conn.execute(
            "UPDATE prs SET sync_error = ? WHERE repo = ? AND number = ?",
            params![error, key.repo, key.number],
        )?;
        conn.execute(
            "UPDATE inbox_prs SET sync_error = ? WHERE repo = ? AND number = ?",
            params![error, key.repo, key.number],
        )?;
        conn.execute(
            "INSERT INTO sync_errors (repo, number, error) VALUES (?, ?, ?)
             ON CONFLICT (repo, number) DO UPDATE SET error = excluded.error",
            params![key.repo, key.number, error],
        )?;
        Ok(())
    }

    /// The last sync error of a PR, wherever it was recorded.
    pub fn pr_sync_error(&self, key: &PrKey) -> Result<Option<String>> {
        self.conn()
            .query_row(
                "SELECT COALESCE(
                     (SELECT sync_error FROM prs WHERE repo = ? AND number = ?),
                     (SELECT error FROM sync_errors WHERE repo = ? AND number = ?),
                     (SELECT sync_error FROM inbox_prs
                      WHERE repo = ? AND number = ? AND sync_error IS NOT NULL LIMIT 1))",
                params![
                    key.repo, key.number, key.repo, key.number, key.repo, key.number
                ],
                |r| r.get(0),
            )
            .map_err(Into::into)
    }

    /// Whether a PR is in some inbox's latest results.
    pub fn in_any_inbox(&self, key: &PrKey) -> Result<bool> {
        self.conn()
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM inbox_prs WHERE repo = ? AND number = ?)",
                params![key.repo, key.number],
                |r| r.get(0),
            )
            .map_err(Into::into)
    }

    pub fn pr(&self, key: &PrKey) -> Result<Option<PrDetail>> {
        let row = {
            let conn = self.conn();
            conn.query_row(
                "SELECT p.repo, p.number, p.title, p.author, p.state, p.is_draft, p.updated_at, p.url,
                        (SELECT COUNT(*) FROM versions v WHERE v.repo = p.repo AND v.number = p.number),
                        p.body, p.base_ref, p.head_ref, p.approximate, p.drift, p.sync_error, p.stack,
                        p.review
                 FROM prs p WHERE p.repo = ? AND p.number = ?",
                params![key.repo, key.number],
                |r| {
                    let stack = parse_stack(r.get(15)?);
                    Ok((
                        summary_from_row(r, true, None, stack.as_ref())?,
                        r.get::<_, String>(9)?,
                        r.get::<_, String>(10)?,
                        r.get::<_, String>(11)?,
                        r.get::<_, bool>(12)?,
                        r.get::<_, Option<String>>(13)?,
                        r.get::<_, Option<String>>(14)?,
                        stack,
                        r.get::<_, Option<String>>(16)?
                            .and_then(|j| serde_json::from_str(&j).ok()),
                    ))
                },
            )
            .optional()?
        };
        let Some((
            summary,
            body,
            base_ref,
            head_ref,
            approximate,
            drift,
            sync_error,
            stack,
            review,
        )) = row
        else {
            return Ok(None);
        };
        Ok(Some(PrDetail {
            summary,
            body,
            base_ref,
            head_ref,
            versions: self.versions(key)?,
            comments: self.comments(key)?,
            stack,
            approximate,
            drift,
            sync_error,
            review,
        }))
    }

    /// Whether `thread` is the GraphQL ID of one of the PR's review
    /// threads, as of its last sync.
    pub fn has_thread(&self, key: &PrKey, thread: &str) -> Result<bool> {
        Ok(self
            .comments(key)?
            .iter()
            .any(|c| c.thread_id.as_deref() == Some(thread)))
    }

    /// The viewer's pending comments on the PR, as of its last sync.
    pub fn pending_comments(&self, key: &PrKey) -> Result<Vec<ReviewComment>> {
        let mut comments = self.comments(key)?;
        comments.retain(|c| c.pending);
        Ok(comments)
    }

    /// Whether `comment` is the GraphQL ID of one of the viewer's pending
    /// comments on the PR, as of its last sync.
    pub fn has_pending_comment(&self, key: &PrKey, comment: &str) -> Result<bool> {
        Ok(self
            .pending_comments(key)?
            .iter()
            .any(|c| c.node_id.as_deref() == Some(comment)))
    }

    fn comments(&self, key: &PrKey) -> Result<Vec<ReviewComment>> {
        let conn = self.conn();
        let mut stmt =
            conn.prepare("SELECT json FROM comments WHERE repo = ? AND number = ? ORDER BY id")?;
        let rows = stmt.query_map(params![key.repo, key.number], |r| r.get::<_, String>(0))?;
        rows.map(|json| Ok(serde_json::from_str(&json?)?)).collect()
    }

    pub fn create_agent(&self, a: &AgentSession) -> Result<()> {
        self.conn().execute(
            "INSERT INTO agents
                (id, backend, repo, pr_number, title, branch, worktree, base_sha, status,
                 cost, cost_unit, context_tokens, context_window, model, created_at, kind)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                a.id,
                a.backend.as_str(),
                a.repo,
                a.pr,
                a.title,
                a.branch,
                a.worktree,
                a.base_sha,
                status_str(a.status),
                a.cost,
                a.cost_unit.as_str(),
                a.context_tokens,
                a.context_window,
                a.model,
                a.created_at,
                a.kind.as_str()
            ],
        )?;
        Ok(())
    }

    /// Every agent session, newest first. Sessions whose backend or cost
    /// unit this build doesn't know are left out with a warning.
    pub fn agents(&self) -> Result<Vec<AgentSession>> {
        Ok(self
            .agents_unchecked()?
            .into_iter()
            .filter_map(|(session, known)| {
                if !known {
                    tracing::warn!(
                        "agent {}: its backend or cost unit is unknown to this build",
                        session.id
                    );
                }
                known.then_some(session)
            })
            .collect())
    }

    /// Every agent session, newest first, with whether this build knows
    /// its backend and cost unit; unknown ones read as the defaults. Only
    /// for what doesn't run the session, like removing what it owns.
    pub fn agents_unchecked(&self) -> Result<Vec<(AgentSession, bool)>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {AGENT_COLUMNS} FROM agents ORDER BY created_at DESC, id"
        ))?;
        let rows = stmt.query_map([], agent_from_row_unchecked)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The session; an error when its backend or cost unit is one this
    /// build doesn't know, e.g. one a newer grenadine wrote, rather than
    /// passing for another.
    pub fn agent(&self, id: &str) -> Result<Option<AgentSession>> {
        Ok(match self.agent_unchecked(id)? {
            Some((session, true)) => Some(session),
            Some((_, false)) => {
                bail!("agent {id}: its backend or cost unit is unknown to this build")
            }
            None => None,
        })
    }

    /// Stores a backend and cost unit that may be unknown to this build,
    /// like a newer grenadine would.
    #[cfg(test)]
    pub fn set_agent_backend_raw(&self, id: &str, backend: &str, cost_unit: &str) {
        self.conn()
            .execute(
                "UPDATE agents SET backend = ?, cost_unit = ? WHERE id = ?",
                params![backend, cost_unit, id],
            )
            .unwrap();
    }

    /// The session like `agents_unchecked` has it.
    pub fn agent_unchecked(&self, id: &str) -> Result<Option<(AgentSession, bool)>> {
        Ok(self
            .conn()
            .query_row(
                &format!("SELECT {AGENT_COLUMNS} FROM agents WHERE id = ?"),
                [id],
                agent_from_row_unchecked,
            )
            .optional()?)
    }

    pub fn set_agent_status(&self, id: &str, status: AgentStatus) -> Result<()> {
        self.conn().execute(
            "UPDATE agents SET status = ? WHERE id = ?",
            params![status_str(status), id],
        )?;
        Ok(())
    }

    /// The backend's own session ID once it created the session, when
    /// later turns resume it.
    pub fn backend_session(&self, id: &str) -> Result<Option<String>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT backend_session_id FROM agents WHERE id = ?",
                [id],
                |r| r.get(0),
            )
            .optional()?
            .flatten())
    }

    pub fn set_backend_session(&self, id: &str, backend_session_id: &str) -> Result<()> {
        self.conn().execute(
            "UPDATE agents SET backend_session_id = ? WHERE id = ?",
            params![backend_session_id, id],
        )?;
        Ok(())
    }

    pub fn add_agent_cost(&self, id: &str, cost: f64) -> Result<()> {
        self.conn().execute(
            "UPDATE agents SET cost = cost + ? WHERE id = ?",
            params![cost, id],
        )?;
        Ok(())
    }

    /// Records how full the context window is; `None` leaves a value
    /// unchanged.
    pub fn set_agent_usage(
        &self,
        id: &str,
        tokens: Option<u64>,
        window: Option<u64>,
    ) -> Result<()> {
        self.conn().execute(
            "UPDATE agents SET context_tokens = COALESCE(?, context_tokens),
                context_window = COALESCE(?, context_window) WHERE id = ?",
            params![tokens, window, id],
        )?;
        Ok(())
    }

    pub fn set_agent_summary(&self, id: &str, summary: &str) -> Result<()> {
        self.conn().execute(
            "UPDATE agents SET summary = ? WHERE id = ?",
            params![summary, id],
        )?;
        Ok(())
    }

    pub fn set_agent_model(&self, id: &str, model: &str) -> Result<()> {
        self.conn().execute(
            "UPDATE agents SET model = ? WHERE id = ?",
            params![model, id],
        )?;
        Ok(())
    }

    /// Records where wrapping up landed the session's work.
    pub fn set_agent_landed(&self, id: &str, landed: &Landed) -> Result<()> {
        self.conn().execute(
            "UPDATE agents SET landed = ? WHERE id = ?",
            params![serde_json::to_string(landed)?, id],
        )?;
        Ok(())
    }

    /// Records that the session opened the PR `key`, which is in `state`,
    /// unless it's recorded already.
    pub fn add_agent_pr(&self, id: &str, key: &PrKey, state: &str) -> Result<()> {
        self.conn().execute(
            "INSERT OR IGNORE INTO agent_prs (agent_id, repo, number, state) VALUES (?, ?, ?, ?)",
            params![id, key.repo, key.number, state],
        )?;
        Ok(())
    }

    /// The PRs that sessions opened that were open at the last poll.
    pub fn open_agent_prs(&self) -> Result<Vec<PrKey>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT DISTINCT repo, number FROM agent_prs WHERE state = 'OPEN' ORDER BY repo, number",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(PrKey {
                repo: r.get(0)?,
                number: r.get(1)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Records the state of a PR that sessions opened and whether it
    /// conflicts with its base; `None` keeps what's recorded. Returns
    /// whether either changed.
    pub fn set_agent_pr_state(
        &self,
        key: &PrKey,
        state: &str,
        conflicting: Option<bool>,
    ) -> Result<bool> {
        Ok(self.conn().execute(
            "UPDATE agent_prs SET state = ?1, conflicting = COALESCE(?4, conflicting)
             WHERE repo = ?2 AND number = ?3
               AND (state != ?1 OR conflicting != COALESCE(?4, conflicting))",
            params![state, key.repo, key.number, conflicting],
        )? > 0)
    }

    /// Moves the commit that the session's own work starts after.
    pub fn set_agent_base(&self, id: &str, base_sha: &str) -> Result<()> {
        self.conn().execute(
            "UPDATE agents SET base_sha = ? WHERE id = ?",
            params![base_sha, id],
        )?;
        Ok(())
    }

    /// Returns false when there is no such session.
    pub fn delete_agent(&self, id: &str) -> Result<bool> {
        Ok(self
            .conn()
            .execute("DELETE FROM agents WHERE id = ?", [id])?
            > 0)
    }

    /// Marks the sessions that were working when the server stopped as
    /// interrupted, since their Claude Code processes are gone.
    pub fn interrupt_agents(&self) -> Result<()> {
        self.conn().execute(
            "UPDATE agents SET status = ? WHERE status IN (?, ?)",
            params![
                status_str(AgentStatus::Interrupted),
                status_str(AgentStatus::Running),
                status_str(AgentStatus::AwaitingApproval)
            ],
        )?;
        Ok(())
    }

    /// Ends the reviews whose sessions were working when the server
    /// stopped, as `interrupt_agents` does to the sessions.
    pub fn interrupt_auto_reviews(&self, now: i64) -> Result<()> {
        self.conn().execute(
            "UPDATE auto_reviews SET state = 'failed', finished_at = ?,
                error = 'The server stopped during the review.'
             WHERE state = 'running'",
            [now],
        )?;
        Ok(())
    }

    /// Records a review of `key` at `head_oid` starting. Returns its id,
    /// or `None` when that commit already had a review.
    pub fn start_auto_review(
        &self,
        key: &PrKey,
        head_oid: &str,
        title: &str,
        url: &str,
        now: i64,
    ) -> Result<Option<i64>> {
        let conn = self.conn();
        let n = conn.execute(
            "INSERT INTO auto_reviews (repo, number, head_oid, title, url, state, started_at)
             VALUES (?, ?, ?, ?, ?, 'running', ?)
             ON CONFLICT (repo, number, head_oid) DO NOTHING",
            params![key.repo, key.number, head_oid, title, url, now],
        )?;
        Ok((n > 0).then(|| conn.last_insert_rowid()))
    }

    pub fn set_auto_review_session(&self, id: i64, agent_id: &str) -> Result<()> {
        self.conn().execute(
            "UPDATE auto_reviews SET agent_id = ? WHERE id = ?",
            params![agent_id, id],
        )?;
        Ok(())
    }

    /// Records how the review `id` ended, unless it already had.
    pub fn finish_auto_review(
        &self,
        id: i64,
        state: AutoReviewState,
        error: Option<&str>,
        now: i64,
    ) -> Result<()> {
        self.conn().execute(
            "UPDATE auto_reviews SET state = ?, error = ?, finished_at = ?
             WHERE id = ? AND state = 'running'",
            params![auto_review_state_str(state), error, now, id],
        )?;
        Ok(())
    }

    /// The review that session `agent_id` is doing, if it's doing one.
    pub fn auto_review_of(&self, agent_id: &str) -> Result<Option<AutoReviewEntry>> {
        Ok(self
            .conn()
            .query_row(
                &format!("SELECT {AUTO_REVIEW_COLUMNS} WHERE r.agent_id = ?"),
                [agent_id],
                auto_review_from_row,
            )
            .optional()?)
    }

    /// Cancels the review that session `agent_id` is doing, if it's still
    /// running, as the session is about to be deleted.
    pub fn cancel_auto_review(&self, agent_id: &str, now: i64) -> Result<()> {
        self.conn().execute(
            "UPDATE auto_reviews SET state = 'cancelled', finished_at = ?
             WHERE agent_id = ? AND state = 'running'",
            params![now, agent_id],
        )?;
        Ok(())
    }

    /// Detaches the review of session `agent_id` from it as the session is
    /// deleted, keeping the session's cost.
    pub fn auto_review_session_deleted(&self, agent_id: &str) -> Result<()> {
        self.conn().execute(
            "UPDATE auto_reviews SET
                cost = COALESCE((SELECT cost FROM agents WHERE id = ?1), cost),
                cost_unit = COALESCE((SELECT cost_unit FROM agents WHERE id = ?1), cost_unit),
                agent_id = NULL
             WHERE agent_id = ?1",
            [agent_id],
        )?;
        Ok(())
    }

    /// Every review, newest first.
    pub fn auto_reviews(&self) -> Result<Vec<AutoReviewEntry>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {AUTO_REVIEW_COLUMNS} ORDER BY r.started_at DESC, r.id DESC"
        ))?;
        let rows = stmt.query_map([], auto_review_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The reviews of `key`, newest first.
    pub fn auto_reviews_of(&self, key: &PrKey) -> Result<Vec<AutoReviewEntry>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {AUTO_REVIEW_COLUMNS} WHERE r.repo = ? AND r.number = ?
             ORDER BY r.started_at DESC, r.id DESC"
        ))?;
        let rows = stmt.query_map(params![key.repo, key.number], auto_review_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Forgets the reviews that ended before `before` and whose sessions
    /// are gone. Returns whether there were any.
    pub fn prune_auto_reviews(&self, before: i64) -> Result<bool> {
        Ok(self.conn().execute(
            "DELETE FROM auto_reviews
             WHERE agent_id IS NULL AND finished_at IS NOT NULL AND finished_at < ?",
            [before],
        )? > 0)
    }

    /// When the PR was opened, as GitHub has it, once it synced.
    pub fn pr_created_at(&self, key: &PrKey) -> Result<Option<String>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT created_at FROM prs WHERE repo = ? AND number = ?",
                params![key.repo, key.number],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Appends an event to a session's transcript and returns its `seq`.
    pub fn append_agent_event(&self, id: &str, kind: &AgentEventKind) -> Result<u64> {
        let conn = self.conn();
        let seq: u64 = conn.query_row(
            "INSERT INTO agent_events (agent_id, seq, event)
             SELECT ?1, COALESCE(MAX(seq) + 1, 1), ?2 FROM agent_events WHERE agent_id = ?1
             RETURNING seq",
            params![id, serde_json::to_string(kind)?],
            |r| r.get(0),
        )?;
        Ok(seq)
    }

    /// Stores an image of a session's transcript under `image`, a new id.
    pub fn add_agent_image(&self, id: &str, image: &str, mime: &str, data: &[u8]) -> Result<()> {
        self.conn().execute(
            "INSERT INTO agent_images (id, agent_id, mime, data) VALUES (?, ?, ?, ?)",
            params![image, id, mime, data],
        )?;
        Ok(())
    }

    /// An image's MIME type and bytes.
    pub fn agent_image(&self, image: &str) -> Result<Option<(String, Vec<u8>)>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT mime, data FROM agent_images WHERE id = ?",
                [image],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
    }

    /// A session's transcript in order.
    pub fn agent_events(&self, id: &str) -> Result<Vec<AgentEvent>> {
        let conn = self.conn();
        let mut stmt =
            conn.prepare("SELECT seq, event FROM agent_events WHERE agent_id = ? ORDER BY seq")?;
        let rows = stmt.query_map([id], |r| Ok((r.get::<_, u64>(0)?, r.get::<_, String>(1)?)))?;
        rows.map(|row| {
            let (seq, json) = row?;
            Ok(AgentEvent {
                seq: Some(seq),
                kind: serde_json::from_str(&json)?,
            })
        })
        .collect()
    }

    /// The cleanup items, newest first.
    pub fn cleanup_items(&self) -> Result<Vec<CleanupItem>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, repo, session_id, session_title, title, detail, created_at, handed_off_to, backend
             FROM cleanup_items ORDER BY created_at DESC, id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(CleanupItem {
                id: r.get(0)?,
                repo: r.get(1)?,
                session_id: r.get(2)?,
                session_title: r.get(3)?,
                title: r.get(4)?,
                detail: r.get(5)?,
                created_at: r.get(6)?,
                handed_off_to: r.get(7)?,
                backend: AgentBackend::parse(&r.get::<_, String>(8)?).unwrap_or_default(),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Records what `session` left behind, as `(title, detail)` pairs.
    pub fn add_cleanup_items(
        &self,
        session: &AgentSession,
        found: &[(String, String)],
        created_at: i64,
    ) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        for (title, detail) in found {
            tx.execute(
                "INSERT INTO cleanup_items (repo, session_id, session_title, title, detail, created_at, backend)
                 VALUES (?, ?, ?, ?, ?, ?, ?)",
                params![session.repo, session.id, session.title, title, detail, created_at,
                        session.backend.as_str()],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Returns false when there is no such item.
    pub fn delete_cleanup_item(&self, id: i64) -> Result<bool> {
        Ok(self
            .conn()
            .execute("DELETE FROM cleanup_items WHERE id = ?", [id])?
            > 0)
    }

    pub fn set_cleanup_handed_off(&self, ids: &[i64], session_id: &str) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        for id in ids {
            tx.execute(
                "UPDATE cleanup_items SET handed_off_to = ? WHERE id = ?",
                params![session_id, id],
            )?;
        }
        tx.commit()?;
        Ok(())
    }
}

const AGENT_COLUMNS: &str = "id, backend, repo, pr_number, title, branch, worktree, base_sha, \
     status, created_at, cost, cost_unit, context_tokens, context_window, landed, model, summary, \
     (SELECT COUNT(*) FROM agent_prs p WHERE p.agent_id = agents.id AND p.state = 'MERGED'), \
     (SELECT COUNT(*) FROM agent_prs p WHERE p.agent_id = agents.id AND p.state != 'CLOSED'), \
     (SELECT COUNT(*) FROM agent_prs p WHERE p.agent_id = agents.id), \
     (SELECT COUNT(*) FROM agent_prs p \
      WHERE p.agent_id = agents.id AND p.state = 'OPEN' AND p.conflicting), \
     kind";

/// The session in `r`, and whether this build knows its backend and cost
/// unit; unknown ones read as the defaults.
fn agent_from_row_unchecked(r: &rusqlite::Row) -> rusqlite::Result<(AgentSession, bool)> {
    let backend = AgentBackend::parse(&r.get::<_, String>(1)?);
    let cost_unit = CostUnit::parse(&r.get::<_, String>(11)?);
    let known = backend.is_some() && cost_unit.is_some();
    let session = AgentSession {
        id: r.get(0)?,
        backend: backend.unwrap_or_default(),
        repo: r.get(2)?,
        pr: r.get(3)?,
        title: r.get(4)?,
        branch: r.get(5)?,
        worktree: r.get(6)?,
        base_sha: r.get(7)?,
        status: parse_status(&r.get::<_, String>(8)?),
        created_at: r.get(9)?,
        cost: r.get(10)?,
        cost_unit: cost_unit.unwrap_or_default(),
        context_tokens: r.get(12)?,
        context_window: r.get(13)?,
        landed: r
            .get::<_, Option<String>>(14)?
            .and_then(|j| serde_json::from_str(&j).ok()),
        model: r.get(15)?,
        summary: r.get(16)?,
        prs: (r.get::<_, u32>(19)? > 0).then_some(AgentPrs {
            merged: r.get(17)?,
            total: r.get(18)?,
            conflicting: r.get(20)?,
        }),
        // The listing looks at the worktree for these.
        unstaged: 0,
        kind: SessionKind::parse(&r.get::<_, String>(21)?),
    };
    Ok((session, known))
}

const AUTO_REVIEW_COLUMNS: &str = "r.id, r.repo, r.number, r.title, r.url, r.head_oid, r.state, \
     r.agent_id, a.status, r.started_at, r.finished_at, COALESCE(a.cost, r.cost), \
     COALESCE(a.cost_unit, r.cost_unit), r.error \
     FROM auto_reviews r LEFT JOIN agents a ON a.id = r.agent_id";

fn auto_review_from_row(r: &rusqlite::Row) -> rusqlite::Result<AutoReviewEntry> {
    Ok(AutoReviewEntry {
        id: r.get(0)?,
        key: PrKey {
            repo: r.get(1)?,
            number: r.get(2)?,
        },
        title: r.get(3)?,
        url: r.get(4)?,
        head_oid: r.get(5)?,
        state: parse_auto_review_state(&r.get::<_, String>(6)?),
        session: r.get(7)?,
        status: r.get::<_, Option<String>>(8)?.map(|s| parse_status(&s)),
        started_at: r.get(9)?,
        finished_at: r.get(10)?,
        cost: r.get(11)?,
        cost_unit: CostUnit::parse(&r.get::<_, String>(12)?).unwrap_or_default(),
        error: r.get(13)?,
    })
}

fn auto_review_state_str(s: AutoReviewState) -> &'static str {
    match s {
        AutoReviewState::Running => "running",
        AutoReviewState::Posted => "posted",
        AutoReviewState::Unposted => "unposted",
        AutoReviewState::Failed => "failed",
        AutoReviewState::Cancelled => "cancelled",
    }
}

fn parse_auto_review_state(s: &str) -> AutoReviewState {
    match s {
        "running" => AutoReviewState::Running,
        "posted" => AutoReviewState::Posted,
        "unposted" => AutoReviewState::Unposted,
        "cancelled" => AutoReviewState::Cancelled,
        _ => AutoReviewState::Failed,
    }
}

fn status_str(s: AgentStatus) -> &'static str {
    match s {
        AgentStatus::Idle => "idle",
        AgentStatus::Running => "running",
        AgentStatus::AwaitingApproval => "awaiting_approval",
        AgentStatus::Interrupted => "interrupted",
        AgentStatus::Failed => "failed",
    }
}

fn parse_status(s: &str) -> AgentStatus {
    match s {
        "idle" => AgentStatus::Idle,
        "running" => AgentStatus::Running,
        "awaiting_approval" => AgentStatus::AwaitingApproval,
        "interrupted" => AgentStatus::Interrupted,
        _ => AgentStatus::Failed,
    }
}

/// A stack that no longer parses is treated like one that was never
/// fetched; the next sync replaces it.
fn parse_stack(json: Option<String>) -> Option<Stack> {
    serde_json::from_str(&json?).ok()
}

fn summary_from_row(
    r: &rusqlite::Row,
    synced: bool,
    sync_error: Option<String>,
    stack: Option<&Stack>,
) -> rusqlite::Result<PrSummary> {
    let number = r.get(1)?;
    Ok(PrSummary {
        key: PrKey {
            repo: r.get(0)?,
            number,
        },
        title: r.get(2)?,
        author: r.get(3)?,
        state: r.get(4)?,
        is_draft: r.get(5)?,
        updated_at: r.get(6)?,
        url: r.get(7)?,
        version_count: r.get(8)?,
        synced,
        sync_error,
        stack: stack.and_then(|s| s.summary(number)),
        threads: None,
        ci: None,
        review_decision: None,
    })
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use grenadine_core::api::{
        Check, MergeInfo, MergeMethod, MergeState, Review, ReviewState, Side,
    };

    use super::*;

    #[test]
    fn review_decisions_are_stored_like_in_prs_review() {
        for d in [
            ReviewDecision::Approved,
            ReviewDecision::ChangesRequested,
            ReviewDecision::ReviewRequired,
            ReviewDecision::Unknown,
        ] {
            assert_eq!(
                serde_json::to_string(&d).unwrap(),
                format!("\"{}\"", decision_str(d))
            );
            assert_eq!(parse_decision(&decision_str(d)), d);
        }
        assert_eq!(parse_decision("Later"), ReviewDecision::Unknown);
    }

    #[test]
    fn a_second_open_fails_until_the_first_closes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("grenadine.db");
        let db = Db::open(&path).unwrap();
        let err = Db::open(&path).err().unwrap();
        assert!(err.to_string().contains("another server"), "{err:#}");
        drop(db);
        // A process that another test spawns gets a copy of the lock's fd
        // and holds the lock until it execs, so the release can lag the drop.
        let deadline = Instant::now() + Duration::from_secs(5);
        while let Err(err) = Db::open(&path) {
            assert!(Instant::now() < deadline, "{err:#}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn hit(key: &PrKey) -> crate::github::Hit {
        crate::github::Hit {
            key: key.clone(),
            title: "hit title".into(),
            author: "ha".into(),
            state: "OPEN".into(),
            is_draft: false,
            url: "hu".into(),
            updated_at: "2026-01-02T00:00:00Z".into(),
            head_oid: "h".into(),
            threads: ThreadCounts {
                resolved: 1,
                total: 3,
            },
            ci: Some(CiStatus::Failing),
            review_decision: Some(ReviewDecision::ChangesRequested),
            facts: Default::default(),
        }
    }

    fn meta(key: &PrKey) -> PrMeta {
        PrMeta {
            key: key.clone(),
            title: "t".into(),
            body: "b".into(),
            author: "a".into(),
            state: "OPEN".into(),
            is_draft: false,
            url: "u".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-02T00:00:00Z".into(),
            base_ref: "main".into(),
            head_ref: "pr".into(),
            head_oid: "h".into(),
            ci: None,
            review: None,
            review_error: None,
        }
    }

    #[test]
    fn default_inboxes() {
        let db = Db::in_memory();
        let inboxes = db.inboxes().unwrap();
        assert_eq!(inboxes.len(), crate::inboxes::DEFAULT_INBOXES.len());
        for (position, (inbox, default)) in inboxes
            .iter()
            .zip(crate::inboxes::DEFAULT_INBOXES.iter())
            .enumerate()
        {
            assert_eq!(
                (
                    inbox.name.as_str(),
                    inbox.filter.as_str(),
                    inbox.count_in_total
                ),
                (default.name, default.filter, default.count_in_total)
            );
            assert_eq!(inbox.position, position as i64);
        }
    }

    #[test]
    fn inbox_crud() {
        let db = Db::in_memory();
        let edit = InboxEdit {
            name: "Mine".into(),
            filter: "label:x".into(),
            position: None,
            count_in_total: true,
        };
        let id = db.create_inbox(&edit).unwrap();
        let inbox = db.inboxes().unwrap().pop().unwrap();
        assert_eq!((inbox.id, inbox.position), (id, 14));
        assert!(inbox.count_in_total);
        assert!(
            db.update_inbox(
                id,
                &InboxEdit {
                    position: Some(-1),
                    ..edit
                }
            )
            .unwrap()
        );
        assert_eq!(db.inboxes().unwrap()[0].id, id);
        assert!(db.delete_inbox(id).unwrap());
        assert!(!db.delete_inbox(id).unwrap());
    }

    #[test]
    fn sync_round_trip() {
        let db = Db::in_memory();
        let key = PrKey {
            repo: "o/n".into(),
            number: 7,
        };
        db.set_inbox_results(1, Ok(std::slice::from_ref(&hit(&key))), None)
            .unwrap();
        // Listed, but not synced yet: the search's metadata shows.
        let prs = db.inbox_prs(1).unwrap();
        assert_eq!(prs.len(), 1);
        assert!(!prs[0].synced);
        assert_eq!(prs[0].title, "hit title");
        assert_eq!(prs[0].version_count, 0);
        assert_eq!(
            prs[0].threads,
            Some(ThreadCounts {
                resolved: 1,
                total: 3
            })
        );
        assert_eq!(prs[0].ci, Some(CiStatus::Failing));
        assert_eq!(
            prs[0].review_decision,
            Some(ReviewDecision::ChangesRequested)
        );

        let versions = vec![
            Version {
                number: 1,
                sha: "g".into(),
                merge_base: Some("m".into()),
                kind: VersionKind::Initial,
                pushed_at: None,
                pushed_by: None,
                pushed_by_is_guess: false,
                missing: false,
            },
            Version {
                number: 2,
                sha: "h".into(),
                merge_base: Some("m".into()),
                kind: VersionKind::Push,
                pushed_at: Some("2026-01-02T00:00:00+00:00".into()),
                pushed_by: Some(Person {
                    login: None,
                    name: Some("Git Author".into()),
                    avatar_url: None,
                }),
                pushed_by_is_guess: true,
                missing: false,
            },
        ];
        let comments = vec![ReviewComment {
            id: 3,
            in_reply_to: None,
            author: "r".into(),
            body: "nit".into(),
            path: "f".into(),
            original_commit: "h".into(),
            original_line: Some(1),
            original_start_line: None,
            line: Some(1),
            start_line: None,
            side: Side::Right,
            on_file: false,
            created_at: "c".into(),
            url: "u".into(),
            resolved: true,
            resolved_by: Some(Person {
                login: Some("o".into()),
                name: None,
                avatar_url: None,
            }),
            thread_id: Some("T3".into()),
            can_resolve: true,
            pending: false,
            node_id: None,
        }];
        // One of the viewer's pending comments, which only has a GraphQL ID.
        let mut comments = [comments.clone(), comments].concat();
        comments[1].id = 4;
        comments[1].thread_id = None;
        comments[1].pending = true;
        comments[1].node_id = Some("C4".into());
        let review = ReviewInfo {
            viewer: "me".into(),
            decision: Some(ReviewDecision::Approved),
            reviews: vec![Review {
                author: Person {
                    login: Some("o".into()),
                    name: None,
                    avatar_url: None,
                },
                state: ReviewState::Approved,
                body: String::new(),
                submitted_at: "s".into(),
                url: "u".into(),
            }],
            requested: vec!["team".into()],
            comments: vec![],
            checks: vec![Check {
                name: "test".into(),
                status: Some(CiStatus::Passing),
                url: None,
            }],
            ci: Some(CiStatus::Passing),
            pending_review: None,
            merge: MergeInfo {
                conflicting: Some(false),
                state: MergeState::Clean,
                methods: vec![MergeMethod::Squash],
                default_method: Some(MergeMethod::Squash),
                delete_branch_on_merge: true,
                can_merge_as_admin: false,
            },
        };
        let mut with_review = meta(&key);
        with_review.review = Some(review.clone());
        with_review.ci = Some(CiStatus::Running);
        db.store_sync(&with_review, &versions, &comments, None, false, None)
            .unwrap();
        let pr = db.pr(&key).unwrap().unwrap();
        assert_eq!(pr.versions, versions);
        assert_eq!(pr.comments, comments);
        assert!(db.has_thread(&key, "T3").unwrap());
        assert!(!db.has_thread(&key, "T4").unwrap());
        assert!(db.has_pending_comment(&key, "C4").unwrap());
        assert!(!db.has_pending_comment(&key, "C3").unwrap());
        assert_eq!(pr.review.as_ref(), Some(&review));
        assert_eq!(pr.summary.version_count, 2);
        assert_eq!(db.pr_title(&key).unwrap().as_deref(), Some("t"));
        assert!(db.inbox_prs(1).unwrap()[0].synced);
        // The sync's decision is fresher than the search's.
        assert_eq!(
            db.inbox_prs(1).unwrap()[0].review_decision,
            Some(ReviewDecision::Approved)
        );
        assert_eq!(
            db.sync_mark(&key).unwrap(),
            Some(SyncMark {
                updated_at: "2026-01-02T00:00:00Z".into(),
                head_oid: "h".into(),
                ci: Some(CiStatus::Running),
                checks_running: false,
            })
        );
        // The search reports something other than what GitHub said at the
        // sync, so it's newer.
        assert_eq!(db.inbox_prs(1).unwrap()[0].ci, Some(CiStatus::Failing));
        // Once they agree, the sync's state counts: unlike GitHub's, it
        // leaves out the runs that later runs superseded.
        let mut agreed = with_review.clone();
        agreed.ci = Some(CiStatus::Failing);
        db.store_sync(&agreed, &versions, &comments, None, false, None)
            .unwrap();
        assert_eq!(db.inbox_prs(1).unwrap()[0].ci, Some(CiStatus::Passing));
        agreed.review.as_mut().unwrap().ci = Some(CiStatus::Running);
        db.store_sync(&agreed, &versions, &comments, None, false, None)
            .unwrap();
        assert_eq!(db.inbox_prs(1).unwrap()[0].ci, Some(CiStatus::Running));
        assert!(db.sync_mark(&key).unwrap().unwrap().checks_running);
        db.store_sync(&with_review, &versions, &comments, None, false, None)
            .unwrap();

        // A sync that couldn't read the reviews keeps the stored ones and
        // says why.
        let mut failed = meta(&key);
        failed.review_error = Some("FORBIDDEN".into());
        db.store_sync(&failed, &versions, &[], None, false, Some(("a", "b")))
            .unwrap();
        let pr = db.pr(&key).unwrap().unwrap();
        assert!(pr.drift.is_some());
        assert!(pr.comments.is_empty());
        assert_eq!(pr.review, Some(review));
        assert_eq!(pr.sync_error.as_deref(), Some("FORBIDDEN"));
        assert_eq!(db.sync_mark(&key).unwrap().unwrap().ci, None);

        db.store_sync_error(&key, "boom").unwrap();
        assert_eq!(
            db.pr(&key).unwrap().unwrap().sync_error.as_deref(),
            Some("boom")
        );

        db.set_inbox_results(1, Err("bad filter"), None).unwrap();
        assert_eq!(db.inbox_error(1).unwrap().as_deref(), Some("bad filter"));
        assert_eq!(db.inbox_prs(1).unwrap().len(), 1);
    }

    #[test]
    fn sync_error_shows_on_a_never_synced_pr() {
        let db = Db::in_memory();
        let key = PrKey {
            repo: "o/n".into(),
            number: 7,
        };
        db.set_inbox_results(1, Ok(std::slice::from_ref(&hit(&key))), None)
            .unwrap();
        db.store_sync_error(&key, "boom").unwrap();
        assert_eq!(
            db.inbox_prs(1).unwrap()[0].sync_error.as_deref(),
            Some("boom")
        );

        // Re-listing the same PR keeps the error.
        db.set_inbox_results(1, Ok(std::slice::from_ref(&hit(&key))), None)
            .unwrap();
        assert_eq!(
            db.inbox_prs(1).unwrap()[0].sync_error.as_deref(),
            Some("boom")
        );

        db.store_sync(&meta(&key), &[], &[], None, false, None)
            .unwrap();
        let pr = db.inbox_prs(1).unwrap().remove(0);
        assert!(pr.synced);
        assert_eq!(pr.sync_error, None);
        assert_eq!(db.pr_sync_error(&key).unwrap(), None);
    }

    #[test]
    fn sync_error_works_without_any_pr_row() {
        let db = Db::in_memory();
        let key = PrKey {
            repo: "o/n".into(),
            number: 7,
        };
        // A PR that was requested directly and failed to sync is in no
        // inbox and has no prs row; its error is still kept.
        db.store_sync_error(&key, "boom").unwrap();
        assert_eq!(db.pr_sync_error(&key).unwrap().as_deref(), Some("boom"));
        db.store_sync_error(&key, "boom 2").unwrap();
        assert_eq!(db.pr_sync_error(&key).unwrap().as_deref(), Some("boom 2"));

        db.store_sync(&meta(&key), &[], &[], None, false, None)
            .unwrap();
        assert_eq!(db.pr_sync_error(&key).unwrap(), None);
    }

    #[test]
    fn stack_round_trip() {
        let db = Db::in_memory();
        let key = PrKey {
            repo: "o/n".into(),
            number: 7,
        };
        db.set_inbox_results(1, Ok(std::slice::from_ref(&hit(&key))), None)
            .unwrap();
        db.store_sync(&meta(&key), &[], &[], None, false, None)
            .unwrap();
        assert_eq!(db.pr(&key).unwrap().unwrap().stack, None);

        let pr = |number, parent| grenadine_core::api::StackPr {
            number,
            title: "t".into(),
            state: "OPEN".into(),
            is_draft: false,
            url: "u".into(),
            updated_at: "u".into(),
            head_oid: "h".into(),
            parent,
        };
        let stack = Stack {
            base_ref: "main".into(),
            prs: vec![pr(7, Some(6)), pr(6, None)],
            more_ancestors: false,
            more_descendants: false,
        };
        db.store_sync(&meta(&key), &[], &[], Some(&stack), false, None)
            .unwrap();
        // A sync that couldn't fetch the stack keeps the stored one.
        db.store_sync(&meta(&key), &[], &[], None, false, None)
            .unwrap();
        let detail = db.pr(&key).unwrap().unwrap();
        assert_eq!(detail.stack.as_ref(), Some(&stack));
        let summary = detail.summary.stack.unwrap();
        assert_eq!((summary.position, summary.length), (2, 2));
        assert_eq!(
            db.inbox_prs(1).unwrap()[0]
                .stack
                .as_ref()
                .map(|s| s.position),
            Some(2)
        );
    }

    #[test]
    fn in_any_inbox_covers_all_inboxes() {
        let db = Db::in_memory();
        let key = PrKey {
            repo: "o/n".into(),
            number: 7,
        };
        assert!(!db.in_any_inbox(&key).unwrap());
        db.set_inbox_results(2, Ok(std::slice::from_ref(&hit(&key))), None)
            .unwrap();
        assert!(db.in_any_inbox(&key).unwrap());
        // A search that no longer reports the PR uncovers it again.
        db.set_inbox_results(2, Ok(&[]), None).unwrap();
        assert!(!db.in_any_inbox(&key).unwrap());
    }

    #[test]
    fn migrates_a_v1_database() {
        let mut conn = Connection::open_in_memory().unwrap();
        let Migration::Sql(sql) = MIGRATIONS[0] else {
            panic!("migration 1 is SQL")
        };
        conn.execute_batch(sql).unwrap();
        conn.pragma_update(None, "foreign_keys", true).unwrap();
        conn.pragma_update(None, "user_version", 1).unwrap();
        conn.execute(
            "INSERT INTO prs (repo, number, title, body, author, state, is_draft, url,
                              created_at, updated_at, base_ref, head_ref, head_oid)
             VALUES ('o/n', 7, 'old title', 'b', 'au', 'OPEN', 0, 'u', 'c', 'u', 'main', 'pr', 'h')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO inbox_prs (inbox_id, repo, number, rank) VALUES (1, 'o/n', 7, 0)",
            [],
        )
        .unwrap();
        // Skip the inbox resets, which would replace the inboxes and
        // cascade away the inbox_prs row, but apply the rest that the
        // listing's query needs.
        migrate(&mut conn, 3).unwrap();
        conn.pragma_update(None, "user_version", 4).unwrap();
        let reset = last_reset();
        migrate(&mut conn, reset).unwrap();
        conn.pragma_update(None, "user_version", reset + 1).unwrap();
        migrate(&mut conn, MIGRATIONS.len()).unwrap();
        let db = Db {
            conn: Mutex::new(conn),
            _lock: None,
        };
        // Until the next poll rewrites the row, the listing takes the
        // fields from prs.
        let pr = &db.inbox_prs(1).unwrap()[0];
        assert_eq!(pr.title, "old title");
        assert_eq!(pr.author, "au");
        assert!(pr.synced);
    }

    #[test]
    fn repo_settings_round_trip() {
        let db = Db::in_memory();
        assert_eq!(db.repo_settings("o/n").unwrap(), RepoSettings::default());
        db.set_repo_settings("o/n", &RepoSettings::default())
            .unwrap();
        db.set_repo_settings("o/n", &RepoSettings::default())
            .unwrap();
        assert_eq!(db.repo_settings("o/n").unwrap(), RepoSettings::default());
        // Fields that a later version dropped are ignored.
        db.conn()
            .execute(
                "UPDATE repo_settings SET json = '{\"gone\": 1}' WHERE repo = 'o/n'",
                [],
            )
            .unwrap();
        assert_eq!(db.repo_settings("o/n").unwrap(), RepoSettings::default());
    }

    #[test]
    fn users_cache() {
        let db = Db::in_memory();
        let logins = ["alice".to_owned(), "bot[bot]".to_owned()];
        assert!(db.cached_users(&logins, 0).unwrap().is_empty());

        let found = BTreeMap::from([(
            "alice".to_owned(),
            Person {
                login: Some("alice".into()),
                name: Some("Alice A.".into()),
                avatar_url: Some("https://a".into()),
            },
        )]);
        db.store_users(&logins, &found, 100).unwrap();
        let cached = db.cached_users(&logins, 100).unwrap();
        assert_eq!(cached.get("alice"), Some(&found.get("alice").cloned()));
        // A login that isn't a user is cached as such.
        assert_eq!(cached.get("bot[bot]"), Some(&None));
        // Entries fetched before `since` are stale.
        assert!(db.cached_users(&logins, 101).unwrap().is_empty());
    }

    #[test]
    fn comments_stored_before_resolved_existed_load() {
        let db = Db::in_memory();
        let key = PrKey {
            repo: "o/n".into(),
            number: 7,
        };
        db.store_sync(&meta(&key), &[], &[], None, false, None)
            .unwrap();
        db.conn()
            .execute(
                "INSERT INTO comments (repo, number, id, json) VALUES ('o/n', 7, 1, ?)",
                [
                    r#"{"id":1,"in_reply_to":null,"author":"a","body":"b","path":"f",
                     "original_commit":"h","original_line":1,"original_start_line":null,
                     "line":1,"start_line":null,"side":"Right","on_file":false,
                     "created_at":"c","url":"u"}"#,
                ],
            )
            .unwrap();
        let pr = db.pr(&key).unwrap().unwrap();
        assert!(!pr.comments[0].resolved);
        assert!(pr.comments[0].resolved_by.is_none());
    }

    #[test]
    fn truncation_round_trip() {
        use grenadine_core::api::Truncation;
        let db = Db::in_memory();
        assert_eq!(db.inbox_truncation(1).unwrap(), None);
        let t = Truncation {
            fetched: 100,
            found: 250,
        };
        db.set_inbox_results(1, Ok(&[]), Some(t)).unwrap();
        assert_eq!(db.inbox_truncation(1).unwrap(), Some(t));
        // A failed search keeps the truncation, like the rows.
        db.set_inbox_results(1, Err("boom"), None).unwrap();
        assert_eq!(db.inbox_truncation(1).unwrap(), Some(t));
        // A complete search clears it.
        db.set_inbox_results(1, Ok(&[]), None).unwrap();
        assert_eq!(db.inbox_truncation(1).unwrap(), None);
    }

    /// The index of the last migration that resets the inboxes.
    fn last_reset() -> usize {
        MIGRATIONS
            .iter()
            .rposition(|m| matches!(m, Migration::ResetInboxes))
            .unwrap()
    }

    #[test]
    fn reset_inboxes_on_an_old_database() {
        // A database from before the count_in_total migration picks up
        // the new defaults, flags included.
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", true).unwrap();
        migrate(&mut conn, last_reset() - 1).unwrap();
        let db = Db::init(conn, None).unwrap();
        let inboxes = db.inboxes().unwrap();
        assert_eq!(inboxes.len(), crate::inboxes::DEFAULT_INBOXES.len());
        for (inbox, default) in inboxes.iter().zip(crate::inboxes::DEFAULT_INBOXES.iter()) {
            assert_eq!(
                (
                    inbox.name.as_str(),
                    inbox.filter.as_str(),
                    inbox.count_in_total
                ),
                (default.name, default.filter, default.count_in_total)
            );
        }
    }

    #[test]
    fn migration_4_replaces_inboxes() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", true).unwrap();
        migrate(&mut conn, 3).unwrap();
        conn.execute(
            "INSERT INTO inboxes (name, filter, position) VALUES ('Custom', 'label:x', 9)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO inbox_prs (inbox_id, repo, number, rank) VALUES (1, 'o/n', 7, 0)",
            [],
        )
        .unwrap();
        let db = Db::init(conn, None).unwrap();
        let inboxes = db.inboxes().unwrap();
        assert_eq!(inboxes.len(), crate::inboxes::DEFAULT_INBOXES.len());
        for (inbox, default) in inboxes.iter().zip(crate::inboxes::DEFAULT_INBOXES.iter()) {
            assert_eq!(
                (
                    inbox.name.as_str(),
                    inbox.filter.as_str(),
                    inbox.count_in_total
                ),
                (default.name, default.filter, default.count_in_total)
            );
        }
        let listed: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM inbox_prs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(listed, 0);
    }

    /// The backends migration moves `started` to `backend_session_id`
    /// and renames `cost_usd`, keeping the rows' data.
    #[test]
    fn migrates_agents_to_backends() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", true).unwrap();
        // Stop before the backends migration, which moves `started`.
        let backends = MIGRATIONS
            .iter()
            .position(|m| matches!(m, Migration::Sql(sql) if sql.contains("DROP COLUMN started")))
            .unwrap();
        migrate(&mut conn, backends).unwrap();
        for (id, started) in [("started", 1), ("fresh", 0)] {
            conn.execute(
                "INSERT INTO agents (id, repo, title, branch, worktree, base_sha, status,
                                     started, cost_usd, created_at)
                 VALUES (?, 'o/n', 't', 'b', '/w', 's', 'idle', ?, 1.5, 0)",
                params![id, started],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO cleanup_items (repo, session_id, session_title, title, detail, created_at)
             VALUES ('o/n', 's', 't', 'x', 'd', 0)",
            [],
        )
        .unwrap();
        let db = Db::init(conn, None).unwrap();
        assert_eq!(db.cleanup_items().unwrap()[0].backend, AgentBackend::Claude);
        let started = db.agent("started").unwrap().unwrap();
        assert_eq!(started.backend, AgentBackend::Claude);
        assert_eq!(started.cost, 1.5);
        assert_eq!(started.cost_unit, CostUnit::Usd);
        assert_eq!(
            db.backend_session("started").unwrap().as_deref(),
            Some("started")
        );
        assert_eq!(db.backend_session("fresh").unwrap(), None);
    }

    fn agent(id: &str, status: AgentStatus) -> AgentSession {
        AgentSession {
            pr: Some(7),
            title: "t".into(),
            branch: "b".into(),
            worktree: "/w".into(),
            base_sha: "s".into(),
            status,
            created_at: 1,
            ..AgentSession::fixture(id, "o/n")
        }
    }

    /// A row whose backend or cost unit this build doesn't know fails to
    /// load instead of passing for Claude's or USD, and the listing leaves
    /// it out.
    #[test]
    fn unknown_backends_fail_to_load() {
        let db = Db::in_memory();
        db.create_agent(&agent("a", AgentStatus::Idle)).unwrap();
        db.create_agent(&agent("b", AgentStatus::Idle)).unwrap();
        db.set_agent_backend_raw("a", "hal", "usd");
        db.set_agent_backend_raw("b", "claude", "gold");
        db.create_agent(&agent("c", AgentStatus::Idle)).unwrap();
        for id in ["a", "b"] {
            let e = db.agent(id).unwrap_err();
            assert!(format!("{e:#}").contains("unknown to this build"), "{e:#}");
            let (session, known) = db.agent_unchecked(id).unwrap().unwrap();
            assert_eq!((session.id.as_str(), known), (id, false));
        }
        let listed: Vec<_> = db.agents().unwrap().into_iter().map(|s| s.id).collect();
        assert_eq!(listed, ["c"]);
        assert_eq!(db.agents_unchecked().unwrap().len(), 3);
    }

    #[test]
    fn agents_round_trip() {
        let db = Db::in_memory();
        db.create_agent(&agent("a", AgentStatus::Running)).unwrap();
        db.create_agent(&agent("b", AgentStatus::AwaitingApproval))
            .unwrap();
        db.create_agent(&agent("c", AgentStatus::Failed)).unwrap();
        assert_eq!(
            db.agent("a").unwrap(),
            Some(agent("a", AgentStatus::Running))
        );
        assert_eq!(db.agent("x").unwrap(), None);

        assert_eq!(db.backend_session("a").unwrap(), None);
        db.set_backend_session("a", "session-a").unwrap();
        assert_eq!(
            db.backend_session("a").unwrap().as_deref(),
            Some("session-a")
        );
        db.add_agent_cost("a", 0.25).unwrap();
        db.add_agent_cost("a", 0.5).unwrap();
        assert_eq!(db.agent("a").unwrap().unwrap().cost, 0.75);
        db.set_agent_usage("a", None, Some(200_000)).unwrap();
        db.set_agent_usage("a", Some(1_000), None).unwrap();
        let a = db.agent("a").unwrap().unwrap();
        assert_eq!(
            (a.context_tokens, a.context_window),
            (Some(1_000), Some(200_000))
        );
        db.set_agent_summary("a", "Fix the flaky test").unwrap();
        assert_eq!(
            db.agent("a").unwrap().unwrap().summary.as_deref(),
            Some("Fix the flaky test")
        );
        db.set_agent_model("a", "claude-opus-4-6").unwrap();
        assert_eq!(
            db.agent("a").unwrap().unwrap().model.as_deref(),
            Some("claude-opus-4-6")
        );
        let landed = Landed::PullRequest {
            number: 3,
            url: "https://github.com/o/n/pull/3".into(),
        };
        db.set_agent_landed("a", &landed).unwrap();
        db.set_agent_base("a", "t").unwrap();
        let a = db.agent("a").unwrap().unwrap();
        assert_eq!((a.landed, a.base_sha.as_str()), (Some(landed), "t"));
        let pr = |number| grenadine_core::api::LandedPr {
            number,
            url: format!("https://github.com/o/n/pull/{number}"),
        };
        let landed = Landed::Stack {
            prs: vec![pr(3), pr(4)],
        };
        db.set_agent_landed("a", &landed).unwrap();
        assert_eq!(db.agent("a").unwrap().unwrap().landed, Some(landed));

        db.interrupt_agents().unwrap();
        let statuses: Vec<_> = db.agents().unwrap().iter().map(|a| a.status).collect();
        assert_eq!(
            statuses,
            [
                AgentStatus::Interrupted,
                AgentStatus::Interrupted,
                AgentStatus::Failed
            ]
        );

        let text = AgentEventKind::Text("hi".into());
        assert_eq!(db.append_agent_event("a", &text).unwrap(), 1);
        assert_eq!(db.append_agent_event("b", &text).unwrap(), 1);
        assert_eq!(
            db.append_agent_event("a", &AgentEventKind::Error("e".into()))
                .unwrap(),
            2
        );
        assert_eq!(
            db.agent_events("a").unwrap(),
            [
                AgentEvent {
                    seq: Some(1),
                    kind: text.clone()
                },
                AgentEvent {
                    seq: Some(2),
                    kind: AgentEventKind::Error("e".into())
                },
            ]
        );
        // Events need their session.
        assert!(db.append_agent_event("x", &text).is_err());

        db.add_agent_image("a", "i", "image/png", b"png").unwrap();
        assert_eq!(
            db.agent_image("i").unwrap(),
            Some(("image/png".into(), b"png".to_vec()))
        );
        assert_eq!(db.agent_image("j").unwrap(), None);

        assert!(db.delete_agent("a").unwrap());
        assert!(!db.delete_agent("a").unwrap());
        assert_eq!(db.agent_events("a").unwrap(), []);
        assert_eq!(db.agent_image("i").unwrap(), None);
    }

    #[test]
    fn agent_prs() {
        let db = Db::in_memory();
        db.create_agent(&agent("a", AgentStatus::Idle)).unwrap();
        db.create_agent(&agent("b", AgentStatus::Idle)).unwrap();
        let prs = |id| db.agent(id).unwrap().unwrap().prs;
        assert_eq!(prs("a"), None);

        let key = |number| PrKey {
            repo: "o/n".into(),
            number,
        };
        for number in [1, 2, 3] {
            db.add_agent_pr("a", &key(number), "OPEN").unwrap();
        }
        // Again, as a stale state, which changes nothing.
        db.add_agent_pr("a", &key(1), "MERGED").unwrap();
        db.add_agent_pr("b", &key(2), "OPEN").unwrap();
        let counts = |merged, total, conflicting| {
            Some(AgentPrs {
                merged,
                total,
                conflicting,
            })
        };
        assert_eq!(prs("a"), counts(0, 3, 0));
        assert_eq!(db.open_agent_prs().unwrap(), [key(1), key(2), key(3)]);

        // PR 2 conflicts, and it still does while GitHub can't tell.
        assert!(db.set_agent_pr_state(&key(2), "OPEN", Some(true)).unwrap());
        assert!(!db.set_agent_pr_state(&key(2), "OPEN", None).unwrap());
        assert_eq!(prs("a"), counts(0, 3, 1));
        assert_eq!(prs("b"), counts(0, 1, 1));
        assert!(db.set_agent_pr_state(&key(2), "OPEN", Some(false)).unwrap());
        assert_eq!(prs("a"), counts(0, 3, 0));
        assert!(db.set_agent_pr_state(&key(2), "OPEN", Some(true)).unwrap());

        assert!(db.set_agent_pr_state(&key(1), "MERGED", None).unwrap());
        assert!(!db.set_agent_pr_state(&key(1), "MERGED", None).unwrap());
        // Only open PRs count as conflicting.
        assert!(db.set_agent_pr_state(&key(2), "MERGED", None).unwrap());
        // A PR closed without merging counts in neither number.
        assert!(db.set_agent_pr_state(&key(3), "CLOSED", None).unwrap());
        assert_eq!(prs("a"), counts(2, 2, 0));
        assert_eq!(prs("b"), counts(1, 1, 0));
        assert_eq!(db.open_agent_prs().unwrap(), []);

        db.delete_agent("a").unwrap();
        let rows: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM agent_prs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 1);
    }

    #[test]
    fn backfills_agent_prs() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", true).unwrap();
        let backfill = MIGRATIONS
            .iter()
            .position(|m| matches!(m, Migration::BackfillAgentPrs))
            .unwrap();
        migrate(&mut conn, backfill).unwrap();
        conn.execute(
            "INSERT INTO agents (id, repo, title, branch, worktree, base_sha, status, created_at)
             VALUES ('a', 'o/n', 't', 'b', '/w', 's', 'idle', 0)",
            [],
        )
        .unwrap();
        let db = Db {
            conn: Mutex::new(conn),
            _lock: None,
        };
        let events = [
            AgentEventKind::ToolUse {
                id: "t".into(),
                name: "Bash".into(),
                input: r#"{"command":"gh pr create --fill"}"#.into(),
                parent: None,
            },
            AgentEventKind::ToolResult {
                id: "t".into(),
                content: "https://github.com/o/n/pull/4".into(),
                is_error: false,
                patch: Vec::new(),
                images: Vec::new(),
            },
        ];
        for e in &events {
            db.append_agent_event("a", e).unwrap();
        }
        migrate(&mut db.conn(), MIGRATIONS.len()).unwrap();
        assert_eq!(
            db.open_agent_prs().unwrap(),
            [PrKey {
                repo: "o/n".into(),
                number: 4
            }]
        );
    }

    #[test]
    fn cleanup_items_round_trip() {
        let db = Db::in_memory();
        let session = agent("a", AgentStatus::Idle);
        db.add_cleanup_items(&session, &[("old".into(), "d1".into())], 1)
            .unwrap();
        db.add_cleanup_items(
            &session,
            &[("new".into(), "d2".into()), ("newer".into(), "d3".into())],
            2,
        )
        .unwrap();
        let items = db.cleanup_items().unwrap();
        let titles: Vec<_> = items.iter().map(|i| i.title.as_str()).collect();
        assert_eq!(titles, ["new", "newer", "old"]);
        assert_eq!(
            items[2],
            CleanupItem {
                id: items[2].id,
                repo: "o/n".into(),
                session_id: "a".into(),
                session_title: "t".into(),
                title: "old".into(),
                detail: "d1".into(),
                created_at: 1,
                handed_off_to: None,
                backend: AgentBackend::Claude,
            }
        );

        db.set_cleanup_handed_off(&[items[0].id, items[2].id], "s")
            .unwrap();
        let handed: Vec<_> = db
            .cleanup_items()
            .unwrap()
            .into_iter()
            .map(|i| i.handed_off_to)
            .collect();
        assert_eq!(handed, [Some("s".into()), None, Some("s".into())]);

        assert!(db.delete_cleanup_item(items[1].id).unwrap());
        assert!(!db.delete_cleanup_item(items[1].id).unwrap());
        assert_eq!(db.cleanup_items().unwrap().len(), 2);
    }
}
