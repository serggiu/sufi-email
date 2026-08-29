//! Local message storage (SQLite), one database per account.
//!
//! Database path: `<config dir>/sufi-email/storage/<account name>.db`
//!
//! The DB is a *cache* of what the server holds: IMAP stays the source of
//! truth for flags. Dedup is enforced by primary key (folder, uid) plus a
//! unique index on (folder, message_id) so a message moved between folders
//! or renumbered after EXPUNGE isn't stored twice.
//!
//! Attachments: metadata only (filename/mime/size/part id). Bodies are not
//! downloaded to disk; "Save as" streams the part straight from IMAP.

use crate::account::AccountConfig;
use mail_parser::{MessageParser, MimeHeaders};
use rusqlite::Connection;
use std::fs;
use std::path::PathBuf;

pub struct Store {
    conn: Connection,
}

/// Metadata for one attachment of a stored message.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AttachmentMeta {
    pub filename: String,
    #[serde(rename = "contentType")]
    pub content_type: String,
    pub size: i64,
    /// IMAP body part number, used to fetch this part on "Save as".
    pub part_id: String,
}

impl Store {
    pub fn dir() -> PathBuf {
        crate::account::Config::dir().join("storage")
    }

    /// Open (or create) a store in an explicit directory. Used by tests to
    /// keep databases isolated from the user's real cache.
    #[cfg(test)]
    pub fn open_in(dir: &std::path::Path, email: &str) -> Result<Store, String> {
        fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        let path = dir.join(format!("{}.db", sanitize(email)));
        Self::init_connection(Connection::open(&path).map_err(|e| e.to_string())?, &path)
    }

    fn db_path(acc: &AccountConfig) -> PathBuf {
        // Keyed by email, not display name: names change, addresses don't.
        Self::dir().join(format!("{}.db", sanitize(&acc.email)))
    }

    pub fn open(acc: &AccountConfig) -> Result<Store, String> {
        let dir = Self::dir();
        fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let path = Self::db_path(acc);
        let conn = Connection::open(&path).map_err(|e| e.to_string())?;
        Self::init_connection(conn, &path)
    }

    fn init_connection(conn: Connection, path: &std::path::Path) -> Result<Store, String> {
        // Concurrent workers (IDLE watcher, poller, UI commands) each open
        // their own connection; WAL allows one writer at a time, so give
        // writers a generous window instead of failing with "database is
        // locked" on transient contention.
        conn.busy_timeout(std::time::Duration::from_secs(10))
            .map_err(|e| e.to_string())?;
        // 0600: the mail cache belongs to the user alone.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
        }
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE IF NOT EXISTS messages (
                 rowid_pk   INTEGER PRIMARY KEY AUTOINCREMENT,
                 folder     TEXT    NOT NULL,
                 uid        INTEGER NOT NULL,
                 message_id TEXT    NOT NULL DEFAULT '',
                 content_hash TEXT NOT NULL DEFAULT '',
                 subject    TEXT,
                 from_name  TEXT,
                 from_email TEXT,
                 date       INTEGER,
                 seen       INTEGER NOT NULL DEFAULT 0,
                 has_attachment INTEGER NOT NULL DEFAULT 0,
                 snippet    TEXT,
                 body_text  TEXT,
                 body_html  TEXT
             );
             CREATE UNIQUE INDEX IF NOT EXISTS idx_messages_folder_uid
                 ON messages(folder, uid);
             -- Dedup by content: survives UID changes (moves between folders
             -- assign new UIDs). Computed as a hash of from+date+subject.
             CREATE INDEX IF NOT EXISTS idx_messages_folder_hash
                 ON messages(folder, content_hash);
             CREATE INDEX IF NOT EXISTS idx_messages_folder_date
                 ON messages(folder, date DESC);
             CREATE TABLE IF NOT EXISTS folders (
                 name        TEXT PRIMARY KEY,
              	delimiter   TEXT,
                 special_use TEXT,
                 unread      INTEGER NOT NULL DEFAULT 0,
                 sort_order  INTEGER NOT NULL DEFAULT 0
             );
             CREATE TABLE IF NOT EXISTS attachments (
                 msg_row   INTEGER NOT NULL REFERENCES messages(rowid_pk) ON DELETE CASCADE,
                 part_id   TEXT    NOT NULL,
                 filename  TEXT,
                 mime      TEXT,
                 size      INTEGER
             );
             -- UIDs we have already notified the user about (new-mail
             -- notifications), one row per message. Shared by the
             -- background inbox poller and the folder-fetch path, so a
             -- message never gets two notifications for the same folder.
             CREATE TABLE IF NOT EXISTS notified_uids (
                 folder TEXT NOT NULL,
                 uid    INTEGER NOT NULL,
                 PRIMARY KEY (folder, uid)
             );
             -- Whether the first sync of a folder has happened yet. On the
             -- first check we record the existing UIDs as a baseline and do
             -- NOT notify, so an old mailbox with hundreds of messages does
             -- not spam notifications on first launch.
             CREATE TABLE IF NOT EXISTS notified_meta (
                 folder   TEXT PRIMARY KEY,
                 baseline INTEGER NOT NULL DEFAULT 0
             );
             -- Read/unread flag changes made while offline. Applied to the
             -- local cache immediately, flushed to the server once a
             -- connection comes back.
             CREATE TABLE IF NOT EXISTS pending_flags (
                 folder TEXT NOT NULL,
                 uid    INTEGER NOT NULL,
                 seen   INTEGER NOT NULL,
                 PRIMARY KEY (folder, uid)
             );
             -- Messages optimistically deleted from the local cache whose
             -- server-side delete (move to Trash / expunge) is still in
             -- flight. While marked, list refreshes must not re-add them.
             CREATE TABLE IF NOT EXISTS pending_deletes (
                 folder TEXT NOT NULL,
                 uid    INTEGER NOT NULL,
                 PRIMARY KEY (folder, uid)
             );",
        )
        .map_err(|e| e.to_string())?;
        // Migration for databases created before content_hash existed.
        // Errors are ignored: the common case is "column already exists".
        let _ = conn.execute_batch(
            "ALTER TABLE messages ADD COLUMN content_hash TEXT NOT NULL DEFAULT '';",
        );
        Ok(Store { conn })
    }

    /// Insert or update summaries fetched from IMAP. Returns how many rows
    /// were newly inserted.
    pub fn upsert_summaries(
        &mut self,
        folder: &str,
        summaries: &[crate::mail::MessageSummary],
    ) -> Result<usize, String> {
        // Read/unread changes that the server hasn't confirmed yet (an
        // in-flight optimistic mark, or an offline queued one). Their value
        // wins over the server's flags so a list refresh can't flash a
        // message back to stale unread while the STORE is still in flight.
        let pending = self.pending_seen_map(folder)?;
        // Optimistically-deleted messages must not come back while the
        // server delete is still in flight.
        let pending_deletes = self.pending_delete_uids(folder)?;
        let tx = self.conn.transaction().map_err(|e| e.to_string())?;
        let mut inserted = 0;
        for m in summaries {
            if pending_deletes.contains(&m.uid) {
                continue;
            }
            let seen = pending.get(&m.uid).copied().unwrap_or(m.seen);
            let date: Option<i64> = m.date.map(|d| d.timestamp());
            let hash = content_hash(&m.from, date, &m.subject);

            // Does this content already exist in the folder under another uid?
            let existing: Option<u32> = tx
                .query_row(
                    "SELECT uid FROM messages WHERE folder = ?1 AND content_hash = ?2 AND uid != ?3 LIMIT 1",
                    rusqlite::params![folder, hash, m.uid],
                    |r| r.get(0),
                )
                .ok();

            if let Some(old_uid) = existing {
                // Moved message: re-point the existing row to the new uid.
                tx.execute(
                    "UPDATE messages SET uid = ?1, seen = ?2, has_attachment = ?3,
                        subject = ?4, from_name = ?5, from_email = ?6, date = ?7, snippet = ?8
                     WHERE folder = ?9 AND uid = ?10",
                    rusqlite::params![
                        m.uid,
                        seen as i64,
                        m.has_attachment as i64,
                        m.subject,
                        from_name(&m.from),
                        from_email(&m.from),
                        date,
                        m.snippet,
                        folder,
                        old_uid,
                    ],
                )
                .map_err(|e| e.to_string())?;
                continue;
            }

            let n = tx
                .execute(
                    "INSERT INTO messages (folder, uid, content_hash, subject, from_name,
                                           from_email, date, seen, has_attachment, snippet)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                     ON CONFLICT(folder, uid) DO UPDATE SET
                        seen        = excluded.seen,
                        has_attachment = excluded.has_attachment,
                        subject     = excluded.subject,
                        from_name   = excluded.from_name,
                        from_email  = excluded.from_email,
                        date        = excluded.date,
                        snippet     = excluded.snippet,
                        content_hash = excluded.content_hash",
                    rusqlite::params![
                        folder,
                        m.uid,
                        hash,
                        m.subject,
                        from_name(&m.from),
                        from_email(&m.from),
                        date,
                        seen as i64,
                        m.has_attachment as i64,
                        m.snippet,
                    ],
                )
                .map_err(|e| e.to_string())?;
            inserted += n;
        }
        tx.commit().map_err(|e| e.to_string())?;
        Ok(inserted)
    }

    /// Persist full bodies + attachment metadata for one message.
    pub fn store_body(
        &mut self,
        folder: &str,
        uid: u32,
        body_text: Option<&str>,
        body_html: Option<&str>,
        attachments: &[AttachmentMeta],
    ) -> Result<(), String> {
        let tx = self.conn.transaction().map_err(|e| e.to_string())?;
        tx.execute(
            "UPDATE messages SET body_text = ?1, body_html = ?2
             WHERE folder = ?3 AND uid = ?4",
            rusqlite::params![body_text, body_html, folder, uid],
        )
        .map_err(|e| e.to_string())?;
        let row: Option<i64> = tx
            .query_row(
                "SELECT rowid_pk FROM messages WHERE folder = ?1 AND uid = ?2",
                rusqlite::params![folder, uid],
                |r| r.get(0),
            )
            .ok();
        if let Some(row) = row {
            tx.execute("DELETE FROM attachments WHERE msg_row = ?1", [row])
                .map_err(|e| e.to_string())?;
            for a in attachments {
                tx.execute(
                    "INSERT INTO attachments (msg_row, part_id, filename, mime, size)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    rusqlite::params![row, a.part_id, a.filename, a.content_type, a.size],
                )
                .map_err(|e| e.to_string())?;
            }
        }
        tx.commit().map_err(|e| e.to_string())
    }

    /// Cached summaries for a folder, newest first.
    pub fn load_summaries(
        &self,
        folder: &str,
    ) -> Result<Vec<crate::mail::MessageSummary>, String> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT uid, COALESCE(subject,''), COALESCE(from_name,''), COALESCE(from_email,''),
                        date, seen, has_attachment, COALESCE(snippet,'')
                 FROM messages WHERE folder = ?1 ORDER BY date DESC",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([folder], |r| {
                let ts: Option<i64> = r.get(4)?;
                Ok(crate::mail::MessageSummary {
                    uid: r.get::<_, u32>(0)?,
                    subject: r.get(1)?,
                    from: combine_from(r.get::<_, String>(2)?, r.get::<_, String>(3)?),
                    date: ts.map(|t| chrono::DateTime::from_timestamp(t, 0)).flatten(),
                    seen: r.get::<_, i64>(5)? != 0,
                    has_attachment: r.get::<_, i64>(6)? != 0,
                    snippet: r.get(7)?,
                })
            })
            .map_err(|e| e.to_string())?;
        rows.collect::<Result<Vec<_>, _>>().map_err(|e| e.to_string())
    }

    /// Cached bodies for one message; None when we haven't cached it yet.
    /// Cache message bodies harvested while streaming a folder list — the
    /// full bodies are already on the wire for snippets, so persisting them
    /// here makes opening any listed message a cache hit.
    pub fn store_bodies(&mut self, folder: &str, bodies: &[crate::mail::BatchBody]) -> Result<(), String> {
        if bodies.is_empty() {
            return Ok(());
        }
        let tx = self.conn.transaction().map_err(|e| e.to_string())?;
        {
            let mut stmt = tx
                .prepare("UPDATE messages SET body_text = ?1, body_html = ?2 WHERE folder = ?3 AND uid = ?4")
                .map_err(|e| e.to_string())?;
            for b in bodies {
                stmt.execute(rusqlite::params![b.text, b.html, folder, b.uid as i64])
                    .map_err(|e| e.to_string())?;
            }
        }
        tx.commit().map_err(|e| e.to_string())
    }

    pub fn load_body(
        &self,
        folder: &str,
        uid: u32,
    ) -> Result<Option<(Option<String>, Option<String>)>, String> {
        let mut stmt = self
            .conn
            .prepare("SELECT body_text, body_html FROM messages WHERE folder=?1 AND uid=?2")
            .map_err(|e| e.to_string())?;
        let mut rows = stmt.query(rusqlite::params![folder, uid]).map_err(|e| e.to_string())?;
        match rows.next().map_err(|e| e.to_string())? {
            None => Ok(None),
            Some(r) => {
                let text: Option<String> = r.get(0).map_err(|e| e.to_string())?;
                let html: Option<String> = r.get(1).map_err(|e| e.to_string())?;
                match (text, html) {
                    (None, None) => Ok(None),
                    other => Ok(Some(other)),
                }
            }
        }
    }

    /// Attachment metadata for one message.
    pub fn load_attachments(
        &self,
        folder: &str,
        uid: u32,
    ) -> Result<Vec<AttachmentMeta>, String> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT a.filename, a.mime, COALESCE(a.size,0), a.part_id
                 FROM attachments a JOIN messages m ON m.rowid_pk = a.msg_row
                 WHERE m.folder = ?1 AND m.uid = ?2",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(rusqlite::params![folder, uid], |r| {
                Ok(AttachmentMeta {
                    filename: r.get::<_, Option<String>>(0)?.unwrap_or_else(|| "attachment".into()),
                    content_type: r.get::<_, Option<String>>(1)?.unwrap_or_else(|| "application/octet-stream".into()),
                    size: r.get(2)?,
                    part_id: r.get(3)?,
                })
            })
            .map_err(|e| e.to_string())?;
        rows.collect::<Result<Vec<_>, _>>().map_err(|e| e.to_string())
    }

    /// Remove a message from the local cache (after move/delete on server).
    pub fn delete_message(&mut self, folder: &str, uid: u32) -> Result<(), String> {
        // Attachment rows cascade via the foreign key.
        self.conn
            .execute(
                "DELETE FROM messages WHERE folder = ?1 AND uid = ?2",
                rusqlite::params![folder, uid],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Reconciliation: drop cached rows whose UIDs are no longer on the
    /// server (messages deleted or moved away by any client).
    pub fn remove_uids_not_in(
        &mut self,
        folder: &str,
        server_uids: &std::collections::HashSet<u32>,
    ) -> Result<usize, String> {
        let stale: Vec<u32> = {
            let mut stmt = self
                .conn
                .prepare("SELECT uid FROM messages WHERE folder = ?1")
                .map_err(|e| e.to_string())?;
            let rows = stmt
                .query_map([folder], |r| r.get::<_, u32>(0))
                .map_err(|e| e.to_string())?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(|e| e.to_string())?
        };
        let mut removed = 0;
        for uid in stale {
            if !server_uids.contains(&uid) {
                self.delete_message(folder, uid)?;
                removed += 1;
            }
        }
        Ok(removed)
    }

    /// Update only the seen flag locally (after a successful STORE on IMAP).
    pub fn set_seen(&mut self, folder: &str, uid: u32, seen: bool) -> Result<(), String> {
        self.conn
            .execute(
                "UPDATE messages SET seen = ?1 WHERE folder = ?2 AND uid = ?3",
                rusqlite::params![seen as i64, folder, uid],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Update the cached unread count for one folder (after flag changes).
    pub fn set_folder_unread(&mut self, folder: &str, unread: u32) -> Result<(), String> {
        self.conn
            .execute(
                "UPDATE folders SET unread = ?1 WHERE name = ?2",
                rusqlite::params![unread, folder],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Replace the cached folder list (after a successful IMAP LIST).
    pub fn store_folders(&mut self, folders: &[crate::mail::Folder]) -> Result<(), String> {
        let tx = self.conn.transaction().map_err(|e| e.to_string())?;
        tx.execute("DELETE FROM folders", []).map_err(|e| e.to_string())?;
        for (i, f) in folders.iter().enumerate() {
            tx.execute(
                "INSERT INTO folders (name, delimiter, special_use, unread, sort_order)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![f.name, f.delimiter, f.special_use, f.unread, i as i64],
            )
            .map_err(|e| e.to_string())?;
        }
        tx.commit().map_err(|e| e.to_string())
    }

    /// Cached folder list, in the order last received from the server.
    pub fn load_folders(&self) -> Result<Vec<crate::mail::Folder>, String> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT name, COALESCE(delimiter,'/'), special_use, unread
                 FROM folders ORDER BY sort_order",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |r| {
                Ok(crate::mail::Folder {
                    name: r.get(0)?,
                    delimiter: r.get(1)?,
                    special_use: r.get(2)?,
                    unread: r.get(3)?,
                })
            })
            .map_err(|e| e.to_string())?;
        rows.collect::<Result<Vec<_>, _>>().map_err(|e| e.to_string())
    }

    /// Drop every trace of an account's cached mail (used on account delete).
    pub fn remove_account_db(acc: &AccountConfig) {
        let _ = fs::remove_file(Self::db_path(acc));
        let _ = fs::remove_file(Self::db_path(acc).with_extension("db-wal"));
        let _ = fs::remove_file(Self::db_path(acc).with_extension("db-shm"));
    }

    // ---------------------------------------------------- new-mail tracking
    //
    // The notified_uids table records which message UIDs the user has
    // already been notified about, per folder. Both the background inbox
    // poller and the folder-fetch path share it, so a message can never
    // produce two notifications. The notified_meta.baseline flag separates
    // "first ever check of this folder" (record everything, notify
    // nothing) from later checks (notify only genuinely new UIDs) — even
    // when the folder was empty on first check.

    /// UIDs already recorded for a folder.
    pub fn notified_uids(&self, folder: &str) -> Result<std::collections::HashSet<u32>, String> {
        let mut stmt = self
            .conn
            .prepare("SELECT uid FROM notified_uids WHERE folder = ?1")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(rusqlite::params![folder], |r| r.get::<_, i64>(0))
            .map_err(|e| e.to_string())?;
        rows.collect::<Result<std::collections::HashSet<i64>, _>>()
            .map(|s| s.into_iter().map(|u| u as u32).collect())
            .map_err(|e| e.to_string())
    }

    /// Record UIDs as already notified (insert-or-ignore).
    pub fn mark_notified(&mut self, folder: &str, uids: &[u32]) -> Result<(), String> {
        let tx = self.conn.transaction().map_err(|e| e.to_string())?;
        {
            let mut stmt = tx
                .prepare("INSERT OR IGNORE INTO notified_uids (folder, uid) VALUES (?1, ?2)")
                .map_err(|e| e.to_string())?;
            for &uid in uids {
                stmt.execute(rusqlite::params![folder, uid as i64])
                    .map_err(|e| e.to_string())?;
            }
        }
        tx.commit().map_err(|e| e.to_string())
    }

    /// Whether the first check of a folder has happened yet.
    pub fn notified_baseline(&self, folder: &str) -> Result<bool, String> {
        let mut stmt = self
            .conn
            .prepare("SELECT baseline FROM notified_meta WHERE folder = ?1")
            .map_err(|e| e.to_string())?;
        let mut rows = stmt
            .query(rusqlite::params![folder])
            .map_err(|e| e.to_string())?;
        match rows.next().map_err(|e| e.to_string())? {
            Some(row) => Ok(row.get::<_, i64>(0).map_err(|e| e.to_string())? != 0),
            None => Ok(false),
        }
    }

    fn set_notified_baseline(&mut self, folder: &str, baseline: bool) -> Result<(), String> {
        self.conn
            .execute(
                "INSERT INTO notified_meta (folder, baseline) VALUES (?1, ?2)
                 ON CONFLICT(folder) DO UPDATE SET baseline = excluded.baseline",
                rusqlite::params![folder, baseline as i64],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Diff `server_uids` against what we have already notified about.
    ///
    /// The first call for a folder records the existing UIDs as a baseline
    /// and returns nothing (pre-existing mail must not notify). Later calls
    /// return the UIDs present now but not recorded before, and record them,
    /// so the caller can fire exactly one notification per batch of new mail.
    pub fn new_uids_since_last_sync(
        &mut self,
        folder: &str,
        server_uids: &[u32],
    ) -> Result<Vec<u32>, String> {
        if !self.notified_baseline(folder)? {
            self.set_notified_baseline(folder, true)?;
            self.mark_notified(folder, server_uids)?;
            return Ok(Vec::new());
        }
        let known = self.notified_uids(folder)?;
        let new: Vec<u32> = server_uids
            .iter()
            .copied()
            .filter(|u| !known.contains(u))
            .collect();
        if !new.is_empty() {
            self.mark_notified(folder, &new)?;
        }
        Ok(new)
    }

    // ---------------------------------------------------- offline flag sync

    /// Record a read/unread change that the server hasn't seen yet (made
    /// while offline). Replaces any previous pending state for the message.
    pub fn upsert_pending_flag(
        &mut self,
        folder: &str,
        uid: u32,
        seen: bool,
    ) -> Result<(), String> {
        self.conn
            .execute(
                "INSERT INTO pending_flags (folder, uid, seen) VALUES (?1, ?2, ?3)
                 ON CONFLICT(folder, uid) DO UPDATE SET seen = excluded.seen",
                rusqlite::params![folder, uid as i64, seen as i64],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Drop a pending flag once it has been synced to the server.
    pub fn remove_pending_flag(&mut self, folder: &str, uid: u32) -> Result<(), String> {
        self.conn
            .execute(
                "DELETE FROM pending_flags WHERE folder = ?1 AND uid = ?2",
                rusqlite::params![folder, uid as i64],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// All pending flag changes, as (folder, uid, seen).
    pub fn pending_flags(&self) -> Result<Vec<(String, u32, bool)>, String> {
        let mut stmt = self
            .conn
            .prepare("SELECT folder, uid, seen FROM pending_flags")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)? != 0)))
            .map_err(|e| e.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map(|v| v.into_iter().map(|(f, u, s)| (f, u as u32, s)).collect())
            .map_err(|e| e.to_string())
    }

    /// Pending (not-yet-server-confirmed) seen values for one folder, keyed
    /// by UID. These override the server's flags on refresh so an in-flight
    /// or offline mark isn't clobbered by stale server state.
    pub fn pending_seen_map(&self, folder: &str) -> Result<std::collections::HashMap<u32, bool>, String> {
        Ok(self
            .pending_flags()?
            .into_iter()
            .filter(|(f, _, _)| f == folder)
            .map(|(_, uid, seen)| (uid, seen))
            .collect())
    }

    /// Drop pending flags for messages that no longer exist on the server
    /// (they were deleted/moved elsewhere, so there is nothing to sync).
    pub fn remove_pending_uids_not_in(
        &mut self,
        folder: &str,
        server_uids: &std::collections::HashSet<u32>,
    ) -> Result<(), String> {
        let pending = self.pending_flags()?;
        for (pf, uid, _) in pending {
            if pf == folder && !server_uids.contains(&uid) {
                self.remove_pending_flag(&folder, uid)?;
            }
        }
        Ok(())
    }

    // ---------------------------------------------------- pending deletes
    //
    // Optimistic deletes: the row is removed from the cache immediately and
    // marked here so a concurrent list refresh (which may still see the
    // message on the server while the delete is in flight) cannot re-add it.

    pub fn mark_pending_delete(&mut self, folder: &str, uid: u32) -> Result<(), String> {
        self.conn
            .execute(
                "INSERT OR IGNORE INTO pending_deletes (folder, uid) VALUES (?1, ?2)",
                rusqlite::params![folder, uid as i64],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn clear_pending_delete(&mut self, folder: &str, uid: u32) -> Result<(), String> {
        self.conn
            .execute(
                "DELETE FROM pending_deletes WHERE folder = ?1 AND uid = ?2",
                rusqlite::params![folder, uid as i64],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn pending_delete_uids(&self, folder: &str) -> Result<std::collections::HashSet<u32>, String> {
        let mut stmt = self
            .conn
            .prepare("SELECT uid FROM pending_deletes WHERE folder = ?1")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(rusqlite::params![folder], |r| r.get::<_, i64>(0))
            .map_err(|e| e.to_string())?;
        rows.collect::<Result<std::collections::HashSet<i64>, _>>()
            .map(|s| s.into_iter().map(|u| u as u32).collect())
            .map_err(|e| e.to_string())
    }

    /// Drop pending-delete markers for messages that are no longer on the
    /// server (the delete finished, possibly while we weren't watching).
    pub fn clear_pending_deletes_not_in(
        &mut self,
        folder: &str,
        server_uids: &std::collections::HashSet<u32>,
    ) -> Result<(), String> {
        let pending = self.pending_delete_uids(folder)?;
        for uid in pending {
            if !server_uids.contains(&uid) {
                self.clear_pending_delete(folder, uid)?;
            }
        }
        Ok(())
    }
}

fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_alphanumeric() || c == '@' || c == '.' || c == '-' || c == '_' { c } else { '_' })
        .collect()
}

/// Test-only accessor for the sanitizer.
#[cfg(test)]
pub(crate) fn sanitize_for_test(name: &str) -> String {
    sanitize(name)
}

/// Stable content identity for dedup: hash of from + date + subject.
/// We avoid server Message-IDs because some servers (mailbox.org observed)
/// return empty/identical values for every message.
///
/// NOTE: this intentionally does NOT include the body — the summary is all
/// we have at upsert time, and from+date+subject is unique enough in
/// practice (two genuinely identical mails from the same sender at the same
/// second with the same subject are vanishingly rare).
fn content_hash(from: &str, date: Option<i64>, subject: &str) -> String {
    use sha2::Digest;
    use std::fmt::Write as _;
    let mut hasher = sha2::Sha256::new();
    hasher.update(from.as_bytes());
    hasher.update(b"\x1f"); // unit separator
    hasher.update(date.unwrap_or(0).to_le_bytes());
    hasher.update(b"\x1f");
    hasher.update(subject.as_bytes());
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(64);
    for b in digest {
        let _ = write!(hex, "{b:02x}");
    }
    hex
}

/// Test-only accessor.
#[cfg(test)]
pub(crate) fn content_hash_for_test(from: &str, date: Option<i64>, subject: &str) -> String {
    content_hash(from, date, subject)
}

/// One-time cleanup: remove rows that are duplicates of another row in the
/// same folder by content hash (keeps the lowest uid = oldest copy).
/// Needed to clean up caches created before content-hash dedup existed.
pub fn dedupe_existing(acc: &AccountConfig) -> Result<usize, String> {
    let store = Store::open(acc)?;
    let n = store
        .conn
        .execute(
            "DELETE FROM messages WHERE rowid_pk NOT IN (
                 SELECT MIN(rowid_pk) FROM messages GROUP BY folder, content_hash
             )",
            [],
        )
        .map_err(|e| e.to_string())?;
    Ok(n)
}

impl Store {
    /// Backfill content_hash for rows created before hashing existed.
    pub fn backfill_hashes(&mut self) -> Result<usize, String> {
        let mut stmt = self
            .conn
            .prepare("SELECT rowid_pk, COALESCE(from_name,''), COALESCE(from_email,''), date, COALESCE(subject,'') FROM messages WHERE content_hash = ''")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<i64>>(3)?,
                    r.get::<_, String>(4)?,
                ))
            })
            .map_err(|e| e.to_string())?;
        let rows: Vec<(i64, String, String, Option<i64>, String)> =
            rows.collect::<Result<Vec<_>, _>>().map_err(|e| e.to_string())?;
        drop(stmt);

        let mut updated = 0;
        for (rowid, fname, femail, date, subject) in rows {
            let from = if fname.is_empty() {
                femail.clone()
            } else if femail.is_empty() {
                fname.clone()
            } else {
                format!("{fname} <{femail}>")
            };
            let hash = content_hash(&from, date, &subject);
            self.conn
                .execute(
                    "UPDATE messages SET content_hash = ?1 WHERE rowid_pk = ?2",
                    rusqlite::params![hash, rowid],
                )
                .map_err(|e| e.to_string())?;
            updated += 1;
        }
        Ok(updated)
    }
}

fn from_name(from: &str) -> &str {
    match from.find(" <") {
        Some(i) => &from[..i],
        None => from,
    }
}

fn from_email(from: &str) -> &str {
    match (from.find('<'), from.rfind('>')) {
        (Some(a), Some(b)) if b > a => &from[a + 1..b],
        _ => from,
    }
}

fn combine_from(name: String, email: String) -> String {
    if name.is_empty() {
        email
    } else if email.is_empty() {
        name
    } else {
        format!("{name} <{email}>")
    }
}

/// Extract attachment metadata while parsing a raw message.
pub fn extract_attachments(raw: &[u8]) -> Vec<AttachmentMeta> {
    let mut out = Vec::new();
    if let Some(msg) = MessageParser::default().parse(raw) {
        for (i, part) in msg.attachments().enumerate() {
            let mime = part
                .content_type()
                .map(|c| {
                    match &c.c_subtype {
                        Some(sub) => format!("{}/{}", c.ctype(), sub),
                        None => c.ctype().to_string(),
                    }
                })
                .unwrap_or_else(|| "application/octet-stream".to_string());
            out.push(AttachmentMeta {
                filename: part
                    .attachment_name()
                    .unwrap_or("attachment")
                    .to_string(),
                content_type: mime,
                size: part.len() as i64,
                // Position in the attachments iterator; used to locate the
                // part again when the user asks to save it.
                part_id: i.to_string(),
            });
        }
    }
    out
}
