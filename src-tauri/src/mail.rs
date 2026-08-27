use crate::account::AccountConfig;
use crate::crypto;
use crate::store;
use chrono::{DateTime, Utc};
use mail_parser::MessageParser;
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct Folder {
    pub name: String,
    #[serde(rename = "delimiter")]
    pub delimiter: String,
    #[serde(rename = "specialUse")]
    pub special_use: Option<String>,
    /// Number of messages without the \Seen flag (STATUS UNSEEN).
    pub unread: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct MessageSummary {
    pub uid: u32,
    pub subject: String,
    pub from: String,
    pub date: Option<DateTime<Utc>>,
    pub seen: bool,
    pub has_attachment: bool,
    /// First few hundred characters of the plain-text body.
    pub snippet: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct MessageBody {
    pub uid: u32,
    pub html: Option<String>,
    pub text: Option<String>,
}

fn imap_session(
    acc: &AccountConfig,
) -> Result<imap::Session<Box<dyn imap::ImapConnection>>, String> {
    let password = crypto::unseal_password(&acc.password)?;
    let client = imap::ClientBuilder::new(&acc.imap_host, acc.imap_port)
        .connect()
        .map_err(|e| format!("IMAP connect to {}:{} failed: {e}", acc.imap_host, acc.imap_port))?;
    let session = client
        .login(&acc.username, &password)
        .map_err(|e| e.0.to_string())?;
    Ok(session)
}

/// Open a fresh IMAP session (used by commands outside mail.rs).
pub fn imap_session_pub(
    acc: &AccountConfig,
) -> Result<imap::Session<Box<dyn imap::ImapConnection>>, String> {
    imap_session(acc)
}

/// Verify IMAP credentials by logging in and immediately logging out.
pub async fn test_imap(acc: &AccountConfig) -> Result<(), String> {
    let acc = acc.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let mut session = imap_session(&acc)?;
        session.logout().ok();
        Ok(())
    })
    .await
    .map_err(|e| format!("join error: {e}"))?
}

fn special_use_of(attrs: &[String]) -> Option<String> {
    for f in attrs {
        match f.as_str() {
            "\\Sent" | "Sent" => return Some("sent".into()),
            "\\Drafts" | "Drafts" => return Some("drafts".into()),
            "\\Trash" | "Trash" => return Some("trash".into()),
            "\\Junk" | "Junk" => return Some("junk".into()),
            "\\Archive" | "Archive" => return Some("archive".into()),
            _ => {}
        }
    }
    None
}

// Recursively check a BODYSTRUCTURE for any part with attachment disposition.
fn has_attachments(bs: &imap_proto::types::BodyStructure) -> bool {
    use imap_proto::types::BodyStructure;
    let part_dispositioned = |common: &imap_proto::types::BodyContentCommon| {
        common
            .disposition
            .as_ref()
            .map(|d| d.ty.eq_ignore_ascii_case("attachment"))
            .unwrap_or(false)
    };
    match bs {
        BodyStructure::Basic { common, .. } | BodyStructure::Text { common, .. } => {
            part_dispositioned(common)
        }
        BodyStructure::Message { common, body, .. } => {
            part_dispositioned(common) || has_attachments(body)
        }
        BodyStructure::Multipart { bodies, .. } => bodies.iter().any(has_attachments),
    }
}

/// Count messages without \\Seen via SEARCH UNSEEN (returns UIDs/seqs).
fn count_unseen(
    session: &mut imap::Session<Box<dyn imap::ImapConnection>>,
    folder: &str,
) -> u32 {
    // SELECT the folder first: SEARCH operates on the selected mailbox.
    // Ignore errors - a failed count just means no badge.
    let count = (|| -> Result<u32, String> {
        session
            .select(folder)
            .map_err(|e| e.to_string())?;
        let ids = session
            .search("UNSEEN")
            .map_err(|e| e.to_string())?;
        Ok(ids.len() as u32)
    })();
    count.unwrap_or(0)
}

pub async fn list_folders(acc: &AccountConfig) -> Result<Vec<Folder>, String> {
    let acc = acc.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let mut session = imap_session(&acc)?;
        let names = session
            .list(Some(""), Some("*"))
            .map_err(|e| format!("LIST failed: {e}"))?;
        let mut folders = Vec::new();
        for n in names.iter() {
            // \NoSelect folders can't be STATUSed; skip the query for them.
            let selectable = !n
                .attributes()
                .iter()
                .any(|a| format!("{a:?}").contains("NoSelect"));
            let unread = if selectable {
                // RFC 3501: STATUS UNSEEN reports the sequence number of the
                // FIRST unseen message, not a count. Use SEARCH UNSEEN and
                // count the results instead.
                count_unseen(&mut session, n.name())
            } else {
                0
            };
            folders.push(Folder {
                name: n.name().to_string(),
                delimiter: n.delimiter().unwrap_or("/").to_string(),
                special_use: special_use_of(
                    &n.attributes()
                        .iter()
                        .map(|f| format!("{f:?}"))
                        .collect::<Vec<_>>(),
                ),
                unread,
            });
        }
        folders.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        Ok(folders)
    })
    .await
    .map_err(|e| format!("join error: {e}"))?
}

fn decode_imap_utf8(bytes: &[u8]) -> String {
    // RFC 2047 encoded-words (=?utf-8?B?...?=) are decoded by mail-parser;
    // for raw envelope bytes we just handle UTF-8 lossily.
    String::from_utf8_lossy(bytes).into_owned()
}

/// Strip HTML tags and entities, returning readable plain text.
fn html_to_plain(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut chars = html.chars().peekable();
    let mut in_tag = false;
    let mut in_entity = false;
    let mut entity = String::new();
    while let Some(c) = chars.next() {
        if in_tag {
            if c == '>' {
                in_tag = false;
                // Block-level tags become word separators.
                out.push(' ');
            }
            continue;
        }
        if c == '<' {
            // Heuristic: a real tag starts with a letter, / or ! — otherwise
            // it's literal text like "a < b".
            if matches!(chars.peek(), Some(c) if c.is_alphabetic() || *c == '/' || *c == '!') {
                in_tag = true;
            } else {
                out.push(c);
            }
            continue;
        }
        if c == '&' {
            in_entity = true;
            entity.clear();
            continue;
        }
        if in_entity {
            if c == ';' {
                in_entity = false;
                match entity.to_ascii_lowercase().as_str() {
                    "amp" => out.push('&'),
                    "lt" => out.push('<'),
                    "gt" => out.push('>'),
                    "quot" => out.push('"'),
                    "apos" => out.push('\''),
                    "nbsp" => out.push(' '),
                    _ => {
                        if let Some(num) = entity.strip_prefix('#') {
                            let cp = num
                                .strip_prefix('x')
                                .or_else(|| num.strip_prefix('X'))
                                .map(|h| u32::from_str_radix(h, 16))
                                .unwrap_or_else(|| num.parse::<u32>())
                                .ok();
                            if let Some(ch) = cp.and_then(char::from_u32) {
                                out.push(ch);
                            }
                        } else {
                            // Unknown entity: keep it literally.
                            out.push('&');
                            out.push_str(&entity);
                            out.push(';');
                        }
                    }
                }
            } else if entity.len() > 10 || !(c.is_ascii_alphanumeric() || c == '#') {
                in_entity = false;
                out.push('&');
                out.push_str(&entity);
                out.push(c);
            } else {
                entity.push(c);
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// Collapse runs of whitespace into single spaces and trim.
fn collapse_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Truncate to at most `max` chars on a char boundary, appending an ellipsis.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    format!("{}…", cut.trim_end())
}

/// Build a summary from a single fetched message. `raw` is the full RFC 822
/// message; mail-parser decodes RFC 2047 headers, charsets and transfer
/// encodings, then light cleanup strips any residual HTML.
fn summarize(f: &imap::types::Fetch) -> Option<MessageSummary> {
    let env = f.envelope()?;
    let fallback_subject = env
        .subject
        .as_ref()
        .map(|s| collapse_whitespace(&html_to_plain(&decode_imap_utf8(s))))
        .unwrap_or_else(|| "(no subject)".into());
    let mut from = env
        .from
        .as_ref()
        .and_then(|l| l.first())
        .map(|a| {
            let name = a.name.as_ref().map(|n| String::from_utf8_lossy(n).into_owned());
            let email = a
                .mailbox
                .as_ref()
                .zip(a.host.as_ref())
                .map(|(m, h)| {
                    format!(
                        "{}@{}",
                        String::from_utf8_lossy(m),
                        String::from_utf8_lossy(h)
                    )
                });
            match (name, email) {
                (Some(n), Some(e)) if !n.is_empty() => format!("{n} <{e}>", n = n, e = e),
                (Some(n), _) => n,
                (_, Some(e)) => e,
                _ => String::new(),
            }
        })
        .unwrap_or_default();
    let date = env
        .date
        .as_ref()
        .and_then(|d| {
            chrono::DateTime::parse_from_rfc2822(&String::from_utf8_lossy(d))
                .ok()
                .map(|t| t.with_timezone(&Utc))
        })
        .or_else(|| f.internal_date().map(|t| t.into()));
    let seen = f.flags().iter().any(|fl| fl.to_string() == "\\Seen");
    let has_attachment = f.bodystructure().map(has_attachments).unwrap_or(false);

    // Parse the raw message so mail-parser handles RFC 2047 headers,
    // charsets, and transfer encodings (base64/quoted-printable).
    let parsed = f.body().and_then(|b| MessageParser::default().parse(b));
    let subject = parsed
        .as_ref()
        .and_then(|m| m.subject())
        .map(|s| collapse_whitespace(&html_to_plain(s)))
        .unwrap_or(fallback_subject);
    // Prefer the parsed From header: mail-parser decodes RFC 2047
    // display names, which the raw IMAP envelope does not.
    if let Some(addr) = parsed.as_ref().and_then(|m| m.from()).and_then(|a| a.first()) {
        let name = addr.name().unwrap_or("").trim();
        let email = addr.address().unwrap_or("");
        from = if name.is_empty() {
            email.to_string()
        } else {
            format!("{name} <{email}>")
        };
    }
    let snippet = parsed
        .as_ref()
        .map(|m| {
            // Prefer the plain-text part; fall back to stripped HTML.
            let body = m.body_text(0).map(|t| t.to_string()).unwrap_or_else(|| {
                m.body_html(0)
                    .map(|h| html_to_plain(&h.to_string()))
                    .unwrap_or_default()
            });
            let cleaned: Vec<&str> = body
                .lines()
                .map(|l| l.trim())
                .filter(|l| !l.is_empty() && !l.starts_with('>'))
                .take(6)
                .collect();
            truncate_chars(&cleaned.join(" "), 300)
        })
        .unwrap_or_default();

    Some(MessageSummary {
        uid: f.uid.unwrap_or(f.message),
        subject,
        from,
        date,
        seen,
        has_attachment,
        snippet,
    })
}

/// Stream message summaries to the UI in batches of ~25 (newest first) as
/// they are fetched, instead of one blocking round-trip.
pub async fn list_messages_streamed(
    acc: &AccountConfig,
    folder: &str,
    on_batch: impl Fn(Vec<MessageSummary>) + Send + 'static,
    on_reconcile: impl Fn(std::collections::HashSet<u32>) + Send + 'static,
) -> Result<(), String> {
    const CHUNK: u32 = 25;
    const MAX: u32 = 200;

    let acc = acc.clone();
    let folder = folder.to_string();
    tauri::async_runtime::spawn_blocking(move || {
        let mut session = imap_session(&acc)?;
        // SELECT returns the mailbox info including EXISTS - no STATUS needed.
        let mailbox = session
            .select(&folder)
            .map_err(|e| format!("SELECT {folder} failed: {e}"))?;
        let exists = mailbox.exists;
        if exists == 0 {
            log::debug!("streaming {folder}: empty mailbox");
            return Ok(());
        }

        // Newest sequence numbers first, in chunks.
        let newest = exists;
        let oldest = exists.saturating_sub(MAX - 1).max(1);
        log::debug!("streaming {folder}: exists={exists}, range={oldest}:{newest}");
        let mut top = newest;
        while top >= oldest {
            let bottom = (top.saturating_sub(CHUNK - 1)).max(oldest);
            let seqs = format!("{bottom}:{top}");
            log::debug!("fetching chunk {seqs}");
            let fetches = session
                .fetch(&seqs, "(UID FLAGS ENVELOPE BODYSTRUCTURE BODY.PEEK[])")
                .map_err(|e| format!("FETCH failed: {e}"))?;
            log::debug!("chunk {seqs}: {} raw messages", fetches.len());
            let mut batch: Vec<MessageSummary> = fetches.iter().filter_map(summarize).collect();
            // Within a chunk the server returns ascending sequence numbers;
            // sort so the UI receives newest-first.
            batch.sort_by(|a, b| b.date.cmp(&a.date));
            log::debug!("chunk {seqs}: {} summaries parsed", batch.len());
            if !batch.is_empty() {
                on_batch(batch);
            }
            if bottom == oldest || top == 0 {
                break;
            }
            top = bottom.saturating_sub(1);
        }

        // Reconcile: drop cached rows whose UIDs no longer exist on the
        // server (messages deleted or moved away by any client).
        let server_uids = session
            .uid_search("ALL")
            .map_err(|e| format!("UID SEARCH failed: {e}"))?;
        on_reconcile(server_uids);

        Ok(())
    })
    .await
    .map_err(|e| format!("join error: {e}"))?
}

/// Fetch a message and return attachment metadata (no bodies saved to disk).
pub async fn fetch_attachments_meta(
    acc: &AccountConfig,
    folder: &str,
    uid: u32,
) -> Result<Vec<store::AttachmentMeta>, String> {
    let acc = acc.clone();
    let folder = folder.to_string();
    tauri::async_runtime::spawn_blocking(move || {
        let mut session = imap_session(&acc)?;
        session
            .select(&folder)
            .map_err(|e| format!("SELECT {folder} failed: {e}"))?;
        let seqs = format!("{uid}");
        let fetches = session
            .uid_fetch(seqs, "(BODY.PEEK[])")
            .map_err(|e| format!("FETCH failed: {e}"))?;
        let raw = fetches
            .iter()
            .next()
            .and_then(|f| f.body())
            .ok_or_else(|| format!("message uid {uid} not found"))?;
        Ok(store::extract_attachments(raw))
    })
    .await
    .map_err(|e| format!("join error: {e}"))?
}

/// Set or clear the \Seen flag for a message (by UID).
pub async fn set_seen(
    acc: &AccountConfig,
    folder: &str,
    uid: u32,
    seen: bool,
) -> Result<(), String> {
    let acc = acc.clone();
    let folder = folder.to_string();
    tauri::async_runtime::spawn_blocking(move || {
        let mut session = imap_session(&acc)?;
        session
            .select(&folder)
            .map_err(|e| format!("SELECT {folder} failed: {e}"))?;
        let seqs = format!("{uid}");
        let query = if seen { "+FLAGS.SILENT (\\Seen)" } else { "-FLAGS.SILENT (\\Seen)" };
        session
            .uid_store(seqs, query)
            .map_err(|e| format!("STORE failed: {e}"))?;
        Ok(())
    })
    .await
    .map_err(|e| format!("join error: {e}"))?
}

/// Copy a message to another folder, then mark it \Deleted in the source.
/// The actual expunge is left to the server (or an explicit UID EXPUNGE),
/// which is the standard IMAP move pattern.
pub async fn move_message(
    acc: &AccountConfig,
    folder: &str,
    uid: u32,
    dest_folder: &str,
) -> Result<(), String> {
    let acc = acc.clone();
    let folder = folder.to_string();
    let dest = dest_folder.to_string();
    tauri::async_runtime::spawn_blocking(move || {
        let mut session = imap_session(&acc)?;
        session
            .select(&folder)
            .map_err(|e| format!("SELECT {folder} failed: {e}"))?;
        // RFC 6851 MOVE would be ideal; most servers don't advertise it via
        // this crate, so use the portable COPY + DELETE dance.
        session
            .uid_copy(format!("{uid}"), &dest)
            .map_err(|e| format!("COPY to {dest} failed: {e}"))?;
        session
            .uid_store(format!("{uid}"), "+FLAGS.SILENT (\\Deleted)")
            .map_err(|e| format!("mark deleted failed: {e}"))?;
        // Expunge only messages with \Deleted set. Some servers ignore the
        // UID qualifier and expunge everything - acceptable for a mail client.
        session
            .uid_expunge(format!("{uid}"))
            .or_else(|_| session.expunge())
            .map_err(|e| format!("EXPUNGE failed: {e}"))?;
        Ok(())
    })
    .await
    .map_err(|e| format!("join error: {e}"))?
}

/// Permanently delete a message: \Deleted flag + expunge.
pub async fn delete_message(
    acc: &AccountConfig,
    folder: &str,
    uid: u32,
) -> Result<(), String> {
    let acc = acc.clone();
    let folder = folder.to_string();
    tauri::async_runtime::spawn_blocking(move || {
        let mut session = imap_session(&acc)?;
        session
            .select(&folder)
            .map_err(|e| format!("SELECT {folder} failed: {e}"))?;
        session
            .uid_store(format!("{uid}"), "+FLAGS.SILENT (\\Deleted)")
            .map_err(|e| format!("mark deleted failed: {e}"))?;
        session
            .uid_expunge(format!("{uid}"))
            .or_else(|_| session.expunge())
            .map_err(|e| format!("EXPUNGE failed: {e}"))?;
        Ok(())
    })
    .await
    .map_err(|e| format!("join error: {e}"))?
}

pub async fn fetch_message(acc: &AccountConfig, folder: &str, uid: u32) -> Result<MessageBody, String> {
    let acc = acc.clone();
    let folder = folder.to_string();
    tauri::async_runtime::spawn_blocking(move || {
        let mut session = imap_session(&acc)?;
        session
            .select(&folder)
            .map_err(|e| format!("SELECT {folder} failed: {e}"))?;
        let seqs = format!("{uid}");
        let fetches = session
            .uid_fetch(seqs, "(BODY.PEEK[])")
            .map_err(|e| format!("FETCH failed: {e}"))?;
        let raw = fetches
            .iter()
            .next()
            .and_then(|f| f.body())
            .ok_or_else(|| format!("message uid {uid} not found"))?;
        let msg = MessageParser::default().parse(raw).ok_or("unparseable message")?;
        let html = msg.body_html(0).map(|b| b.to_string());
        let text = msg.body_text(0).map(|b| b.to_string());
        Ok(MessageBody { uid, html, text })
    })
    .await
    .map_err(|e| format!("join error: {e}"))?
}

/// Verify SMTP credentials by connecting, issuing AUTH, and closing.
pub async fn test_smtp(acc: &AccountConfig) -> Result<(), String> {
    use lettre::transport::smtp::authentication::Credentials;
    use lettre::{AsyncSmtpTransport, Tokio1Executor};

    let acc = acc.clone();
    tauri::async_runtime::spawn_blocking(move || ())
        .await
        .map_err(|e| format!("join error: {e}"))?;

    let smtp_user = acc.smtp_username.as_deref().unwrap_or(&acc.username).to_string();
    let smtp_pass = crypto::unseal_password(acc.smtp_password.as_deref().unwrap_or(&acc.password))?;
    let builder = if acc.smtp_starttls {
        AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&acc.smtp_host)
            .map_err(|e| format!("smtp relay {}: {e}", acc.smtp_host))?
    } else {
        AsyncSmtpTransport::<Tokio1Executor>::relay(&acc.smtp_host)
            .map_err(|e| format!("smtp relay {}: {e}", acc.smtp_host))?
    };
    let mailer: AsyncSmtpTransport<Tokio1Executor> = builder
        .credentials(Credentials::new(smtp_user, smtp_pass))
        .build();

    // A connection test performs EHLO + AUTH without sending a message.
    mailer
        .test_connection()
        .await
        .map_err(|e| format!("SMTP connect/auth to {}:{} failed: {e}", acc.smtp_host, acc.smtp_port))?;
    Ok(())
}

pub async fn send_email(
    acc: &AccountConfig,
    to: Vec<String>,
    subject: &str,
    body: &str,
) -> Result<(), String> {
    use lettre::message::Mailbox;
    use lettre::transport::smtp::authentication::Credentials;
    use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};

    let from: Mailbox = format!("{} <{}>", acc.name, acc.email)
        .parse()
        .map_err(|e| format!("bad from address: {e}"))?;
    let mut builder = Message::builder().from(from);
    for t in &to {
        let m: Mailbox = t.parse().map_err(|e| format!("bad to address '{t}': {e}"))?;
        builder = builder.to(m);
    }
    let email = builder
        .subject(subject.to_string())
        .body(body.to_string())
        .map_err(|e| format!("build message: {e}"))?;

    let smtp_user = acc.smtp_username.as_deref().unwrap_or(&acc.username);
    let smtp_pass = crypto::unseal_password(acc.smtp_password.as_deref().unwrap_or(&acc.password))?;
    let builder = if acc.smtp_starttls {
        AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&acc.smtp_host)
            .map_err(|e| format!("smtp relay {}: {e}", acc.smtp_host))?
    } else {
        AsyncSmtpTransport::<Tokio1Executor>::relay(&acc.smtp_host)
            .map_err(|e| format!("smtp relay {}: {e}", acc.smtp_host))?
    };
    let mailer: AsyncSmtpTransport<Tokio1Executor> = builder
        .credentials(Credentials::new(smtp_user.to_string(), smtp_pass))
        .build();

    mailer
        .send(email)
        .await
        .map_err(|e| format!("send failed: {e}"))?;
    Ok(())
}
