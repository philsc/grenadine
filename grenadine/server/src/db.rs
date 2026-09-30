//! The SQLite database: inboxes, the PRs they matched, and each PR's
//! versions and review comments.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use anyhow::{Context, Result};
use grenadine_core::api::{
    Inbox, InboxEdit, PrDetail, PrKey, PrSummary, ReviewComment, Version, VersionKind,
};
use rusqlite::{Connection, OptionalExtension, params};

enum Migration {
    Sql(&'static str),
    /// Replaces every inbox with the defaults.
    ResetInboxes,
}

/// Each entry upgrades the schema by one version.
const MIGRATIONS: &[Migration] = &[Migration::Sql(r#"
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
"#),
    Migration::Sql(r#"
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
"#),
    Migration::Sql(r#"
    -- Sync errors of PRs that have neither a prs row nor an inbox_prs
    -- row, e.g. PRs synced on demand that no inbox covers.
    CREATE TABLE sync_errors (
        repo TEXT NOT NULL,
        number INTEGER NOT NULL,
        error TEXT NOT NULL,
        PRIMARY KEY (repo, number)
    );
"#),
    Migration::ResetInboxes,
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
                for (position, (name, filter)) in
                    crate::inboxes::DEFAULT_INBOXES.iter().enumerate()
                {
                    tx.execute(
                        "INSERT INTO inboxes (name, filter, position) VALUES (?, ?, ?)",
                        params![name, filter, position as i64],
                    )?;
                }
            }
        }
        tx.pragma_update(None, "user_version", i + 1)?;
        tx.commit()?;
    }
    Ok(())
}

pub struct Db {
    conn: Mutex<Connection>,
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
}

/// What was true at a PR's last successful sync.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncMark {
    pub updated_at: String,
    pub head_oid: String,
}

fn kind_str(k: VersionKind) -> &'static str {
    match k {
        VersionKind::Initial => "initial",
        VersionKind::Push => "push",
        VersionKind::ForcePush => "force_push",
    }
}

fn parse_kind(s: &str) -> VersionKind {
    match s {
        "initial" => VersionKind::Initial,
        "push" => VersionKind::Push,
        _ => VersionKind::ForcePush,
    }
}

impl Db {
    pub fn open(path: &Path) -> Result<Db> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let conn =
            Connection::open(path).with_context(|| format!("can't open {}", path.display()))?;
        Self::init(conn)
    }

    #[cfg(test)]
    pub fn in_memory() -> Db {
        Self::init(Connection::open_in_memory().unwrap()).unwrap()
    }

    fn init(mut conn: Connection) -> Result<Db> {
        conn.pragma_update(None, "foreign_keys", true)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        migrate(&mut conn, MIGRATIONS.len())?;
        Ok(Db {
            conn: Mutex::new(conn),
        })
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap()
    }

    pub fn inboxes(&self) -> Result<Vec<Inbox>> {
        let conn = self.conn();
        let mut stmt =
            conn.prepare("SELECT id, name, filter, position FROM inboxes ORDER BY position, id")?;
        let rows = stmt.query_map([], |r| {
            Ok(Inbox {
                id: r.get(0)?,
                name: r.get(1)?,
                filter: r.get(2)?,
                position: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn inbox_error(&self, id: i64) -> Result<Option<String>> {
        Ok(self
            .conn()
            .query_row("SELECT error FROM inboxes WHERE id = ?", [id], |r| r.get(0))
            .optional()?
            .flatten())
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
            "INSERT INTO inboxes (name, filter, position) VALUES (?, ?, ?)",
            params![edit.name, edit.filter, position],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Returns false when there is no such inbox.
    pub fn update_inbox(&self, id: i64, edit: &InboxEdit) -> Result<bool> {
        let n = self.conn().execute(
            "UPDATE inboxes SET name = ?, filter = ?, position = COALESCE(?, position), error = NULL WHERE id = ?",
            params![edit.name, edit.filter, edit.position, id],
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
    /// its sync_error.
    pub fn set_inbox_results(
        &self,
        id: i64,
        result: std::result::Result<&[crate::github::Hit], &str>,
    ) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        match result {
            Ok(hits) => {
                for (rank, h) in hits.iter().enumerate() {
                    tx.execute(
                        "INSERT INTO inbox_prs
                            (inbox_id, repo, number, rank, title, author, state, is_draft, url, updated_at)
                         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                         ON CONFLICT (inbox_id, repo, number) DO UPDATE SET
                            rank = excluded.rank, title = excluded.title, author = excluded.author,
                            state = excluded.state, is_draft = excluded.is_draft, url = excluded.url,
                            updated_at = excluded.updated_at",
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
                            h.updated_at
                        ],
                    )?;
                }
                // Rows for PRs the search no longer reports go away.
                let mut sql =
                    String::from("DELETE FROM inbox_prs WHERE inbox_id = ?");
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
                tx.execute("UPDATE inboxes SET error = NULL WHERE id = ?", [id])?;
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
        // fields from prs until the next poll rewrites them.
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
                    CASE WHEN p.repo IS NULL THEN i.sync_error END
             FROM inbox_prs i LEFT JOIN prs p ON p.repo = i.repo AND p.number = i.number
             WHERE i.inbox_id = ? ORDER BY i.rank",
        )?;
        let rows = stmt.query_map([id], |r| {
            summary_from_row(r, r.get(9)?, r.get(10)?)
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn sync_mark(&self, key: &PrKey) -> Result<Option<SyncMark>> {
        let row: Option<(Option<String>, Option<String>)> = self
            .conn()
            .query_row(
                "SELECT synced_updated_at, synced_head_oid FROM prs WHERE repo = ? AND number = ?",
                params![key.repo, key.number],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        Ok(match row {
            Some((Some(updated_at), Some(head_oid))) => Some(SyncMark {
                updated_at,
                head_oid,
            }),
            _ => None,
        })
    }

    pub fn versions(&self, key: &PrKey) -> Result<Vec<Version>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT idx, sha, merge_base, kind, pushed_at, missing FROM versions
             WHERE repo = ? AND number = ? ORDER BY idx",
        )?;
        let rows = stmt.query_map(params![key.repo, key.number], |r| {
            Ok(Version {
                number: r.get(0)?,
                sha: r.get(1)?,
                merge_base: r.get(2)?,
                kind: parse_kind(&r.get::<_, String>(3)?),
                pushed_at: r.get(4)?,
                missing: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Stores the result of a successful sync.
    pub fn store_sync(
        &self,
        meta: &PrMeta,
        versions: &[Version],
        comments: &[ReviewComment],
        approximate: bool,
        drift: Option<(&str, &str)>,
    ) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let key = &meta.key;
        let drift_text = drift
            .map(|(old, new)| format!("Versions changed on recomputation: was {old}, now {new}"));
        tx.execute(
            "INSERT INTO prs (repo, number, title, body, author, state, is_draft, url, created_at, updated_at,
                              base_ref, head_ref, head_oid, approximate, drift, sync_error,
                              synced_updated_at, synced_head_oid)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, NULL, ?10, ?13)
             ON CONFLICT (repo, number) DO UPDATE SET
                title = ?3, body = ?4, author = ?5, state = ?6, is_draft = ?7, url = ?8, created_at = ?9,
                updated_at = ?10, base_ref = ?11, head_ref = ?12, head_oid = ?13, approximate = ?14,
                drift = COALESCE(?15, drift), sync_error = NULL,
                synced_updated_at = ?10, synced_head_oid = ?13",
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
            tx.execute(
                "INSERT INTO versions (repo, number, idx, sha, merge_base, kind, pushed_at, missing)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                params![key.repo, key.number, v.number, v.sha, v.merge_base, kind_str(v.kind), v.pushed_at, v.missing],
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
                params![key.repo, key.number, key.repo, key.number, key.repo, key.number],
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
                        p.body, p.base_ref, p.head_ref, p.approximate, p.drift, p.sync_error
                 FROM prs p WHERE p.repo = ? AND p.number = ?",
                params![key.repo, key.number],
                |r| {
                    Ok((
                        summary_from_row(r, true, None)?,
                        r.get::<_, String>(9)?,
                        r.get::<_, String>(10)?,
                        r.get::<_, String>(11)?,
                        r.get::<_, bool>(12)?,
                        r.get::<_, Option<String>>(13)?,
                        r.get::<_, Option<String>>(14)?,
                    ))
                },
            )
            .optional()?
        };
        let Some((summary, body, base_ref, head_ref, approximate, drift, sync_error)) = row else {
            return Ok(None);
        };
        Ok(Some(PrDetail {
            summary,
            body,
            base_ref,
            head_ref,
            versions: self.versions(key)?,
            comments: self.comments(key)?,
            approximate,
            drift,
            sync_error,
        }))
    }

    fn comments(&self, key: &PrKey) -> Result<Vec<ReviewComment>> {
        let conn = self.conn();
        let mut stmt =
            conn.prepare("SELECT json FROM comments WHERE repo = ? AND number = ? ORDER BY id")?;
        let rows = stmt.query_map(params![key.repo, key.number], |r| r.get::<_, String>(0))?;
        rows.map(|json| Ok(serde_json::from_str(&json?)?)).collect()
    }
}

fn summary_from_row(
    r: &rusqlite::Row,
    synced: bool,
    sync_error: Option<String>,
) -> rusqlite::Result<PrSummary> {
    Ok(PrSummary {
        key: PrKey {
            repo: r.get(0)?,
            number: r.get(1)?,
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
    })
}

#[cfg(test)]
mod tests {
    use grenadine_core::api::Side;

    use super::*;

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
        }
    }

    #[test]
    fn default_inboxes() {
        let db = Db::in_memory();
        let inboxes = db.inboxes().unwrap();
        assert_eq!(inboxes.len(), crate::inboxes::DEFAULT_INBOXES.len());
        for (position, (inbox, (name, filter))) in inboxes
            .iter()
            .zip(crate::inboxes::DEFAULT_INBOXES.iter())
            .enumerate()
        {
            assert_eq!((inbox.name.as_str(), inbox.filter.as_str()), (*name, *filter));
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
        };
        let id = db.create_inbox(&edit).unwrap();
        let inbox = db.inboxes().unwrap().pop().unwrap();
        assert_eq!((inbox.id, inbox.position), (id, 9));
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
        db.set_inbox_results(1, Ok(std::slice::from_ref(&hit(&key))))
            .unwrap();
        // Listed, but not synced yet: the search's metadata shows.
        let prs = db.inbox_prs(1).unwrap();
        assert_eq!(prs.len(), 1);
        assert!(!prs[0].synced);
        assert_eq!(prs[0].title, "hit title");
        assert_eq!(prs[0].version_count, 0);

        let versions = vec![Version {
            number: 1,
            sha: "h".into(),
            merge_base: Some("m".into()),
            kind: VersionKind::Initial,
            pushed_at: None,
            missing: false,
        }];
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
        }];
        db.store_sync(&meta(&key), &versions, &comments, false, None)
            .unwrap();
        let pr = db.pr(&key).unwrap().unwrap();
        assert_eq!(pr.versions, versions);
        assert_eq!(pr.comments, comments);
        assert_eq!(pr.summary.version_count, 1);
        assert_eq!(db.pr_title(&key).unwrap().as_deref(), Some("t"));
        assert!(db.inbox_prs(1).unwrap()[0].synced);
        assert_eq!(
            db.sync_mark(&key).unwrap(),
            Some(SyncMark {
                updated_at: "2026-01-02T00:00:00Z".into(),
                head_oid: "h".into()
            })
        );

        db.store_sync(&meta(&key), &versions, &[], false, Some(("a", "b")))
            .unwrap();
        let pr = db.pr(&key).unwrap().unwrap();
        assert!(pr.drift.is_some());
        assert!(pr.comments.is_empty());

        db.store_sync_error(&key, "boom").unwrap();
        assert_eq!(
            db.pr(&key).unwrap().unwrap().sync_error.as_deref(),
            Some("boom")
        );

        db.set_inbox_results(1, Err("bad filter")).unwrap();
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
        db.set_inbox_results(1, Ok(std::slice::from_ref(&hit(&key))))
            .unwrap();
        db.store_sync_error(&key, "boom").unwrap();
        assert_eq!(
            db.inbox_prs(1).unwrap()[0].sync_error.as_deref(),
            Some("boom")
        );

        // Re-listing the same PR keeps the error.
        db.set_inbox_results(1, Ok(std::slice::from_ref(&hit(&key))))
            .unwrap();
        assert_eq!(
            db.inbox_prs(1).unwrap()[0].sync_error.as_deref(),
            Some("boom")
        );

        db.store_sync(&meta(&key), &[], &[], false, None).unwrap();
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

        db.store_sync(&meta(&key), &[], &[], false, None).unwrap();
        assert_eq!(db.pr_sync_error(&key).unwrap(), None);
    }

    #[test]
    fn in_any_inbox_covers_all_inboxes() {
        let db = Db::in_memory();
        let key = PrKey {
            repo: "o/n".into(),
            number: 7,
        };
        assert!(!db.in_any_inbox(&key).unwrap());
        db.set_inbox_results(2, Ok(std::slice::from_ref(&hit(&key))))
            .unwrap();
        assert!(db.in_any_inbox(&key).unwrap());
        // A search that no longer reports the PR uncovers it again.
        db.set_inbox_results(2, Ok(&[])).unwrap();
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
        // Stop before migration 4, which would replace the inboxes and
        // cascade away the inbox_prs row.
        migrate(&mut conn, 3).unwrap();
        let db = Db {
            conn: Mutex::new(conn),
        };
        // Until the next poll rewrites the row, the listing takes the
        // fields from prs.
        let pr = &db.inbox_prs(1).unwrap()[0];
        assert_eq!(pr.title, "old title");
        assert_eq!(pr.author, "au");
        assert!(pr.synced);
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
        let db = Db::init(conn).unwrap();
        let inboxes = db.inboxes().unwrap();
        assert_eq!(inboxes.len(), crate::inboxes::DEFAULT_INBOXES.len());
        for (inbox, (name, filter)) in inboxes
            .iter()
            .zip(crate::inboxes::DEFAULT_INBOXES.iter())
        {
            assert_eq!((inbox.name.as_str(), inbox.filter.as_str()), (*name, *filter));
        }
        let listed: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM inbox_prs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(listed, 0);
    }
}
