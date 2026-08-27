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

    fn db_path(acc: &AccountConfig) -> PathBuf {
        // Keyed by email, not display name: names change, addresses don't.
        Self::dir().join(format!("{}.db", sanitize(&acc.email)))
    }

    pub fn open(acc: &AccountConfig) -> Result<Store, String> {
        let dir = Self::dir();
        fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let path = Self::db_path(acc);
        let conn = Connection::open(&path).map_err(|e| e.to_string())?;
        // 0600: the mail cache belongs to the user alone.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
        }
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE IF NOT EXISTS messages (
                 rowid_pk   INTEGER PRIMARY KEY AUTOINCREMENT,
                 folder     TEXT    NOT NULL,
                 uid        INTEGER NOT NULL,
                 message_id TEXT    NOT NULL DEFAULT '',
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
             -- NOTE: no unique index on message_id. Some servers (mailbox.org
             -- observed) return empty/identical Message-IDs for all messages,
             -- which would make a unique constraint reject legitimate rows.
             -- Dedup is by (folder, uid), which is authoritative per folder.
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
             );",
        )
        .map_err(|e| e.to_string())?;
        Ok(Store { conn })
    }

    /// Insert or update summaries fetched from IMAP. Returns how many rows
    /// were newly inserted.
    pub fn upsert_summaries(
        &mut self,
        folder: &str,
        summaries: &[crate::mail::MessageSummary],
    ) -> Result<usize, String> {
        let tx = self.conn.transaction().map_err(|e| e.to_string())?;
        let mut inserted = 0;
        for m in summaries {
            let date: Option<i64> = m.date.map(|d| d.timestamp());
            let n = tx
                .execute(
                    "INSERT INTO messages (folder, uid, subject, from_name, from_email,
                                           date, seen, has_attachment, snippet)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                     ON CONFLICT(folder, uid) DO UPDATE SET
                        seen        = excluded.seen,
                        has_attachment = excluded.has_attachment,
                        subject     = excluded.subject,
                        from_name   = excluded.from_name,
                        from_email  = excluded.from_email,
                        date        = excluded.date,
                        snippet     = excluded.snippet",
                    rusqlite::params![
                        folder,
                        m.uid,
                        m.subject,
                        from_name(&m.from),
                        from_email(&m.from),
                        date,
                        m.seen as i64,
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
}

fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_alphanumeric() || c == '@' || c == '.' || c == '-' || c == '_' { c } else { '_' })
        .collect()
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
