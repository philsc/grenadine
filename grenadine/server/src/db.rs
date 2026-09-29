//! The SQLite database: inboxes, the PRs they matched, and each PR's
//! versions and review comments.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use anyhow::{Context, Result};
use grenadine_core::api::{
    Inbox, InboxEdit, PrDetail, PrKey, PrSummary, ReviewComment, Version, VersionKind,
};
use rusqlite::{Connection, OptionalExtension, params};

/// Each entry upgrades the schema by one version.
const MIGRATIONS: &[&str] = &[r#"
    CREATE TABLE inboxes (
        id INTEGER PRIMARY KEY,
        name TEXT NOT NULL,
        filter TEXT NOT NULL,
        position INTEGER NOT NULL,
        error TEXT
    );
    INSERT INTO inboxes (name, filter, position) VALUES
        ('Authored', 'is:open author:@me', 0),
        ('Review requested', 'is:open review-requested:@me', 1),
        ('Reviewed', 'is:open reviewed-by:@me', 2),
        ('Involved', 'is:open involves:@me', 3);

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
"#];

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
        let current: usize = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        for (i, sql) in MIGRATIONS.iter().enumerate().skip(current) {
            let tx = conn.transaction()?;
            tx.execute_batch(sql)?;
            tx.pragma_update(None, "user_version", i + 1)?;
            tx.commit()?;
        }
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

    /// Records the outcome of an inbox's search.
    pub fn set_inbox_results(
        &self,
        id: i64,
        result: std::result::Result<&[PrKey], &str>,
    ) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        match result {
            Ok(keys) => {
                tx.execute("DELETE FROM inbox_prs WHERE inbox_id = ?", [id])?;
                for (rank, k) in keys.iter().enumerate() {
                    tx.execute(
                        "INSERT OR IGNORE INTO inbox_prs (inbox_id, repo, number, rank) VALUES (?, ?, ?, ?)",
                        params![id, k.repo, k.number, rank],
                    )?;
                }
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
        let mut stmt = conn.prepare(
            "SELECT p.repo, p.number, p.title, p.author, p.state, p.is_draft, p.updated_at, p.url,
                    (SELECT COUNT(*) FROM versions v WHERE v.repo = p.repo AND v.number = p.number)
             FROM inbox_prs i JOIN prs p ON p.repo = i.repo AND p.number = i.number
             WHERE i.inbox_id = ? ORDER BY i.rank",
        )?;
        let rows = stmt.query_map([id], summary_from_row)?;
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

    /// Records a failed sync. Only an already-known PR can carry the error.
    pub fn store_sync_error(&self, key: &PrKey, error: &str) -> Result<()> {
        self.conn().execute(
            "UPDATE prs SET sync_error = ? WHERE repo = ? AND number = ?",
            params![error, key.repo, key.number],
        )?;
        Ok(())
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
                        summary_from_row(r)?,
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

fn summary_from_row(r: &rusqlite::Row) -> rusqlite::Result<PrSummary> {
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
    })
}

#[cfg(test)]
mod tests {
    use grenadine_core::api::Side;

    use super::*;

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
        let names: Vec<_> = db
            .inboxes()
            .unwrap()
            .into_iter()
            .map(|i| i.filter)
            .collect();
        assert_eq!(
            names,
            [
                "is:open author:@me",
                "is:open review-requested:@me",
                "is:open reviewed-by:@me",
                "is:open involves:@me"
            ]
        );
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
        assert_eq!((inbox.id, inbox.position), (id, 4));
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
        db.set_inbox_results(1, Ok(std::slice::from_ref(&key)))
            .unwrap();
        // Listed, but not synced yet.
        assert!(db.inbox_prs(1).unwrap().is_empty());

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
        assert_eq!(db.inbox_prs(1).unwrap().len(), 1);
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
}
