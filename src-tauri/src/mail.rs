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
    /// The message's own Message-ID (used to thread our outgoing replies).
    pub message_id: String,
    /// The full References chain (space-joined message-ids), for building
    /// outgoing thread headers.
    pub references: String,
    /// The immediate parent message-id (In-Reply-To), for thread resolution.
    pub in_reply_to: String,
    /// Conversation identifier: the thread root Message-ID (first entry of
    /// References, else In-Reply-To, else own Message-ID, else a
    /// normalized-subject fallback). Stable as the thread grows.
    pub thread_id: String,
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
        .mode(session_connection_mode(&acc.imap_host))
        .connect()
        .map_err(|e| format!("IMAP connect to {}:{} failed: {e}", acc.imap_host, acc.imap_port))?;
    let session = client
        .login(&acc.username, &password)
        .map_err(|e| e.0.to_string())?;
    Ok(session)
}

/// Production always uses TLS (implicit on 993, STARTTLS elsewhere).
#[cfg(not(test))]
fn session_connection_mode(_host: &str) -> imap::ConnectionMode {
    imap::ConnectionMode::AutoTls
}

/// Tests run against the in-process fake IMAP server on 127.0.0.1, which
/// speaks plaintext TCP only; any other host keeps the production TLS path.
#[cfg(test)]
fn session_connection_mode(host: &str) -> imap::ConnectionMode {
    if host == "127.0.0.1" || host == "localhost" {
        imap::ConnectionMode::Plaintext
    } else {
        imap::ConnectionMode::AutoTls
    }
}

/// Open a session against a plaintext (non-TLS) server. Used by tests with
/// the fake IMAP server; production always uses TLS via `imap_session`.
/// Returns the session with a boxed connection, matching production types.
#[cfg(test)]
pub fn imap_session_insecure(
    host: &str,
    port: u16,
    username: &str,
    password: &str,
) -> Result<imap::Session<Box<dyn imap::ImapConnection>>, String> {
    let client = imap::ClientBuilder::new(host, port)
        .mode(imap::ConnectionMode::Plaintext)
        .connect()
        .map_err(|e| format!("connect to {host}:{port} failed: {e}"))?;
    let session = client
        .login(username, password)
        .map_err(|e| e.0.to_string())?;
    Ok(session)
}

/// Open a fresh IMAP session (used by commands outside mail.rs).
pub fn imap_session_pub(
    acc: &AccountConfig,
) -> Result<imap::Session<Box<dyn imap::ImapConnection>>, String> {
    imap_session(acc)
}

/// All UIDs present in a folder, sorted ascending. Used by the background
/// inbox poller to diff against the notified set. Blocking IMAP — callers
/// should run it on a worker thread.
pub fn list_folder_uids(acc: &AccountConfig, folder: &str) -> Result<Vec<u32>, String> {
    let mut session = imap_session(acc)?;
    session
        .select(folder)
        .map_err(|e| format!("SELECT {folder} failed: {e}"))?;
    let uids = session
        .uid_search("ALL")
        .map_err(|e| format!("UID SEARCH failed: {e}"))?;
    let mut uids: Vec<u32> = uids.into_iter().collect();
    uids.sort_unstable();
    Ok(uids)
}

/// Number of messages without \Seen in a folder — the authoritative server
/// count, used to keep the tray icon truthful from the background watchers
/// (which have no live session of their own). Blocking IMAP; callers
/// should run it on a worker thread.
pub fn folder_unseen_count(acc: &AccountConfig, folder: &str) -> Result<u32, String> {
    let mut session = imap_session(acc)?;
    session
        .select(folder)
        .map_err(|e| format!("SELECT {folder} failed: {e}"))?;
    let unseen = session
        .search("UNSEEN")
        .map_err(|e| format!("SEARCH UNSEEN failed: {e}"))?;
    Ok(unseen.len() as u32)
}

/// Sender display + subject of one message, for the new-mail notification.
/// Fetches the header section and parses it with mail-parser — the exact
/// same decoding the message list applies (RFC 2047 names/subjects, HTML
/// stripped, whitespace collapsed), so the toast never shows raw encoded
/// words or entities.
pub fn fetch_envelope_preview(
    session: &mut imap::Session<Box<dyn imap::ImapConnection>>,
    uid: u32,
) -> Result<(String, String), String> {
    let fetches = session
        .uid_fetch(format!("{uid}"), "(BODY.PEEK[HEADER])")
        .map_err(|e| format!("FETCH failed: {e}"))?;
    let raw = fetches
        .iter()
        .next()
        .and_then(|f| f.header())
        .ok_or_else(|| format!("message uid {uid} not found"))?;
    let msg = MessageParser::default().parse(raw).ok_or("unparseable header")?;
    let from = msg
        .from()
        .and_then(|a| a.first())
        .map(address_display)
        .unwrap_or_default();
    let subject = msg
        .subject()
        .map(|s| collapse_whitespace(&html_to_plain(&s.to_string())))
        .unwrap_or_default();
    Ok((from, subject))
}

fn address_display(a: &mail_parser::Addr) -> String {
    let name = a.name().unwrap_or("").trim();
    let email = a.address().unwrap_or("");
    if name.is_empty() {
        email.to_string()
    } else {
        format!("{name} <{email}>")
    }
}

/// Extract the message-ids (angle brackets stripped) from a References /
/// In-Reply-To style header value.
pub(crate) fn extract_message_ids(raw: &str) -> Vec<String> {
    raw.split_whitespace()
        .map(|t| t.trim().trim_start_matches('<').trim_end_matches('>'))
        .filter(|t| !t.is_empty())
        .map(|t| t.to_string())
        .collect()
}

/// Normalize a subject for the no-Message-ID fallback: strip Re:/Fwd:
/// prefixes, lowercase, collapse whitespace.
fn normalize_subject(subject: &str) -> String {
    let mut s = subject.trim().to_lowercase();
    loop {
        let before = s.clone();
        for prefix in ["re:", "fwd:", "fw:", "aw:", "sv:"] {
            if let Some(rest) = s.strip_prefix(prefix) {
                s = rest.trim_start().to_string();
            }
        }
        if s == before {
            break;
        }
    }
    collapse_whitespace(&s)
}

/// The string form of a parsed header value (Text or TextList).
fn header_value_str(v: &mail_parser::HeaderValue) -> String {
    match v {
        mail_parser::HeaderValue::Text(t) => t.to_string(),
        mail_parser::HeaderValue::TextList(list) => list.iter().map(|s| s.to_string()).collect::<Vec<_>>().join(" "),
        _ => String::new(),
    }
}

/// Compute a stable conversation identifier: the thread root Message-ID
/// (first References entry, else In-Reply-To, else the message's own id,
/// else a normalized-subject fallback).
fn compute_thread_id(message_id: &str, in_reply_to: &str, references: &str, subject: &str) -> String {
    if let Some(first) = extract_message_ids(references).first() {
        return first.clone();
    }
    if let Some(first) = extract_message_ids(in_reply_to).first() {
        return first.clone();
    }
    if !message_id.is_empty() {
        return message_id.to_string();
    }
    format!("subj:{}", normalize_subject(subject))
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
    tauri::async_runtime::spawn_blocking(move || list_folders_blocking(&acc))
        .await
        .map_err(|e| format!("join error: {e}"))?
}

/// Blocking core of [`list_folders`], usable from worker threads (e.g. the
/// background inbox poller) that have no async runtime to hand.
pub fn list_folders_blocking(acc: &AccountConfig) -> Result<Vec<Folder>, String> {
    let mut session = imap_session(acc)?;
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

#[cfg(test)]
mod text_tests {
    use super::*;

    // ---- html_to_plain ----

    #[test]
    fn html_tags_are_stripped() {
        // Tags emit spaces (word separators); callers collapse whitespace
        // afterwards, so raw output may contain doubled spaces.
        assert_eq!(html_to_plain("<p>Hello</p>"), " Hello ");
        assert_eq!(collapse_whitespace(&html_to_plain("<b>bold</b> text")), "bold text");
    }

    #[test]
    fn html_block_tags_become_word_separators() {
        let out = html_to_plain("line1<br>line2");
        assert!(out.contains("line1") && out.contains("line2"));
        assert_ne!(out, "line1line2");
    }

    #[test]
    fn common_entities_are_decoded() {
        assert_eq!(html_to_plain("a &amp; b"), "a & b");
        assert_eq!(html_to_plain("&lt;tag&gt;"), "<tag>");
        assert_eq!(html_to_plain("&quot;quoted&quot;"), "\"quoted\"");
        assert_eq!(html_to_plain("a&nbsp;b"), "a b");
        assert_eq!(html_to_plain("&#65;&#x42;"), "AB");
    }

    #[test]
    fn literal_less_than_is_preserved() {
        // "a < b" is not a tag: the char after '<' is a space.
        assert_eq!(html_to_plain("a < b"), "a < b");
    }

    #[test]
    fn unknown_entities_pass_through() {
        assert_eq!(html_to_plain("&nosuch;"), "&nosuch;");
    }

    #[test]
    fn utf8_survives_html_stripping() {
        assert_eq!(html_to_plain("<p>DacÄ ai pÄstrat</p>"), " DacÄ ai pÄstrat ");
    }

    // ---- collapse_whitespace ----

    #[test]
    fn whitespace_runs_collapse_to_single_space() {
        assert_eq!(collapse_whitespace("a \t\n  b"), "a b");
        assert_eq!(collapse_whitespace("  trimmed  "), "trimmed");
    }

    // ---- truncate_chars ----

    #[test]
    fn short_strings_are_untouched() {
        assert_eq!(truncate_chars("short", 300), "short");
    }

    #[test]
    fn long_strings_are_truncated_with_ellipsis() {
        let s = "x".repeat(400);
        let out = truncate_chars(&s, 300);
        assert_eq!(out.chars().count(), 301); // 300 + ellipsis
        assert!(out.ends_with('…'));
    }

    #[test]
    fn truncation_respects_char_boundaries() {
        // Multi-byte characters must not be cut mid-sequence.
        let s = "ä".repeat(400);
        let out = truncate_chars(&s, 300);
        assert!(out.starts_with(&"ä".repeat(300)));
        assert!(out.ends_with('…'));
    }

    // ---- special_use_of ----

    #[test]
    fn special_use_flags_are_recognized() {
        assert_eq!(special_use_of(&["\\Trash".to_string()]).as_deref(), Some("trash"));
        assert_eq!(special_use_of(&["\\Sent".to_string()]).as_deref(), Some("sent"));
        assert_eq!(special_use_of(&["\\Junk".to_string()]).as_deref(), Some("junk"));
        assert_eq!(special_use_of(&["\\Archive".to_string()]).as_deref(), Some("archive"));
        assert_eq!(special_use_of(&["\\Drafts".to_string()]).as_deref(), Some("drafts"));
    }

    #[test]
    fn no_special_use_flag_returns_none() {
        assert_eq!(special_use_of(&[]), None);
        assert_eq!(special_use_of(&["\\HasChildren".to_string()]), None);
    }

    // ---- content_hash (store.rs, tested here for convenience) ----

    #[test]
    fn content_hash_is_stable_for_identical_content() {
        let a = crate::store::content_hash_for_test("a@b.com", Some(123), "Subject");
        let b = crate::store::content_hash_for_test("a@b.com", Some(123), "Subject");
        assert_eq!(a, b);
    }

    #[test]
    fn content_hash_differs_for_different_content() {
        let base = crate::store::content_hash_for_test("a@b.com", Some(123), "Subject");
        assert_ne!(base, crate::store::content_hash_for_test("other@b.com", Some(123), "Subject"));
        assert_ne!(base, crate::store::content_hash_for_test("a@b.com", Some(456), "Subject"));
        assert_ne!(base, crate::store::content_hash_for_test("a@b.com", Some(123), "Other"));
    }

    #[test]
    fn content_hash_handles_missing_date() {
        let with_none = crate::store::content_hash_for_test("a@b.com", None, "S");
        let with_zero = crate::store::content_hash_for_test("a@b.com", Some(0), "S");
        // None and Some(0) hash the same (both map to 0) - documented behavior.
        assert_eq!(with_none, with_zero);
    }
}

/// Plain-text / HTML body extracted while streaming a folder's message
/// list, so the cache can be warmed for free — the full body is already
/// downloaded to build the snippet, so storing it makes later selection a
/// cache hit instead of another IMAP fetch.
#[derive(Debug, Clone)]
pub struct BatchBody {
    pub uid: u32,
    pub text: Option<String>,
    pub html: Option<String>,
    /// Attachment metadata, extracted while parsing the raw message (so the
    /// cache warm-up stores them alongside the bodies).
    pub attachments: Vec<crate::store::AttachmentMeta>,
}

/// Build a summary from a single fetched message, plus the message's plain
/// text and HTML bodies so the caller can cache them. `raw` is the full
/// RFC 822 message; mail-parser decodes RFC 2047 headers, charsets and
/// transfer encodings, then light cleanup strips any residual HTML.
fn summarize(f: &imap::types::Fetch) -> Option<(MessageSummary, BatchBody)> {
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

    let (text, html) = parsed
        .as_ref()
        .map(|m| {
            (
                m.body_text(0).map(|t| t.to_string()),
                m.body_html(0).map(|h| h.to_string()),
            )
        })
        .unwrap_or((None, None));

    // Threading: own Message-ID + References chain + conversation id.
    let message_id = parsed
        .as_ref()
        .and_then(|m| m.message_id())
        .map(|s| s.to_string())
        .unwrap_or_default();
    let references = parsed
        .as_ref()
        .map(|m| header_value_str(m.references()))
        .unwrap_or_default();
    let in_reply_to = parsed
        .as_ref()
        .map(|m| header_value_str(m.in_reply_to()))
        .unwrap_or_default();
    let thread_id = compute_thread_id(&message_id, &in_reply_to, &references, &subject);

    Some((
        MessageSummary {
            uid: f.uid.unwrap_or(f.message),
            subject,
            from,
            date,
            seen,
            has_attachment,
            snippet,
            message_id,
            references,
            in_reply_to,
            thread_id,
        },
        BatchBody {
            uid: f.uid.unwrap_or(f.message),
            text,
            html,
            attachments: parsed
                .as_ref()
                .map(crate::store::extract_attachments_from)
                .unwrap_or_default(),
        },
    ))
}

/// Stream message summaries to the UI in batches of ~25 (newest first) as
/// they are fetched, instead of one blocking round-trip.
/// Blocking core of [`list_messages_streamed`], usable from worker threads
/// (the background IDLE watcher's cache warm-up) that have no async runtime.
pub fn list_messages_streamed_blocking(
    acc: &AccountConfig,
    folder: &str,
    on_batch: impl Fn(Vec<MessageSummary>, Vec<BatchBody>),
    on_reconcile: impl Fn(std::collections::HashSet<u32>),
) -> Result<(), String> {
    const CHUNK: u32 = 25;
    const MAX: u32 = 200;

    let acc = acc.clone();
    let folder = folder.to_string();
        let mut session = imap_session(&acc)?;
        // SELECT returns the mailbox info including EXISTS - no STATUS needed.
        let mailbox = session
            .select(&folder)
            .map_err(|e| format!("SELECT {folder} failed: {e}"))?;
        let exists = mailbox.exists;
        if exists == 0 {
            log::debug!("streaming {folder}: empty mailbox");
            // No summaries to stream, but the local cache may still hold rows
            // for messages that were removed server-side. Reconcile with the
            // (empty) server uid set so stale cache rows are pruned.
            let server_uids = session
                .uid_search("ALL")
                .map_err(|e| format!("UID SEARCH failed: {e}"))?;
            on_reconcile(server_uids);
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
            let mut batch: Vec<MessageSummary> = Vec::new();
            let mut bodies: Vec<BatchBody> = Vec::new();
            for (summary, body) in fetches.iter().filter_map(summarize) {
                batch.push(summary);
                bodies.push(body);
            }
            batch.sort_by(|a, b| b.date.cmp(&a.date));
            log::debug!("chunk {seqs}: {} summaries parsed", batch.len());
            if !batch.is_empty() {
                on_batch(batch, bodies);
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
}

/// Stream message summaries to the UI in batches of ~25 (newest first) as
/// they are fetched, instead of one blocking round-trip.
pub async fn list_messages_streamed(
    acc: &AccountConfig,
    folder: &str,
    on_batch: impl Fn(Vec<MessageSummary>, Vec<BatchBody>) + Send + 'static,
    on_reconcile: impl Fn(std::collections::HashSet<u32>) + Send + 'static,
) -> Result<(), String> {
    let acc = acc.clone();
    let folder = folder.to_string();
    tauri::async_runtime::spawn_blocking(move || {
        list_messages_streamed_blocking(&acc, &folder, on_batch, on_reconcile)
    })
    .await
    .map_err(|e| format!("join error: {e}"))?
}

/// A message's parsed body plus its attachment metadata, fetched in a
/// single IMAP connection and a single FETCH (BODY.PEEK[]) round trip.
/// The attachment list is extracted from the very same raw message that
/// produced the body — no second download.
#[derive(Debug, Clone)]
pub struct FetchedMessage {
    pub body: MessageBody,
    pub attachments: Vec<store::AttachmentMeta>,
}

/// Fetch a message's body and attachment metadata in one connection.
/// Callers (the cache-miss path in the UI command) persist both together.
pub async fn fetch_message_full(
    acc: &AccountConfig,
    folder: &str,
    uid: u32,
) -> Result<FetchedMessage, String> {
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
        let attachments = store::extract_attachments(raw);
        Ok(FetchedMessage {
            body: MessageBody { uid, html, text },
            attachments,
        })
    })
    .await
    .map_err(|e| format!("join error: {e}"))?
}

/// Set or clear the \Seen flag for a message (by UID).
/// Blocking core of [`set_seen`], usable from worker threads (the inbox
/// poller's offline-sync flush) that have no async runtime to hand.
pub fn set_seen_blocking(
    acc: &AccountConfig,
    folder: &str,
    uid: u32,
    seen: bool,
) -> Result<u32, String> {
    let mut session = imap_session(acc)?;
    session
        .select(folder)
        .map_err(|e| format!("SELECT {folder} failed: {e}"))?;
    let seqs = format!("{uid}");
    let query = if seen { "+FLAGS.SILENT (\\Seen)" } else { "-FLAGS.SILENT (\\Seen)" };
    session
        .uid_store(seqs, query)
        .map_err(|e| format!("STORE failed: {e}"))?;
    // SEARCH operates on the selected mailbox; count is the server truth.
    let unseen = session
        .search("UNSEEN")
        .map_err(|e| format!("SEARCH UNSEEN failed: {e}"))?;
    Ok(unseen.len() as u32)
}

/// Set or clear the \Seen flag for a message (by UID). Returns the folder's
/// fresh unread count straight from the server (SEARCH UNSEEN in the same
/// session), so the badge never depends on how complete the local cache is.
pub async fn set_seen(
    acc: &AccountConfig,
    folder: &str,
    uid: u32,
    seen: bool,
) -> Result<u32, String> {
    let acc = acc.clone();
    let folder = folder.to_string();
    tauri::async_runtime::spawn_blocking(move || set_seen_blocking(&acc, &folder, uid, seen))
        .await
        .map_err(|e| format!("join error: {e}"))?
}

/// Copy a message to another folder, then mark it \Deleted in the source.
/// The actual expunge is left to the server (or an explicit UID EXPUNGE),
/// which is the standard IMAP move pattern.
/// Fetch the Message-IDs of the given UIDs in one round-trip (used by the
/// IDLE watcher to tell moved messages apart from genuinely new mail).
/// Results are aligned by UID, not by response position — servers may
/// return FETCH responses in any order.
pub fn fetch_message_ids(
    session: &mut imap::Session<Box<dyn imap::ImapConnection>>,
    uids: &[u32],
) -> Result<Vec<Option<String>>, String> {
    if uids.is_empty() {
        return Ok(Vec::new());
    }
    let set = uids
        .iter()
        .map(|u| u.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let fetches = session
        .uid_fetch(set, "(BODY.PEEK[HEADER])")
        .map_err(|e| format!("FETCH failed: {e}"))?;
    let mut by_uid: std::collections::HashMap<u32, Option<String>> =
        std::collections::HashMap::new();
    for f in fetches.iter() {
        let uid = f.uid.unwrap_or(0);
        let mid = f
            .header()
            .and_then(|h| MessageParser::default().parse_headers(h))
            .and_then(|m| m.message_id().map(|s| s.to_string()));
        by_uid.insert(uid, mid);
    }
    Ok(uids.iter().map(|u| by_uid.get(u).cloned().flatten()).collect())
}

/// Move a message to another folder (COPY + \Deleted + expunge). Returns
/// the copy's new UID in the destination folder when it can be determined
/// (the IMAP crate doesn't expose COPYUID, so the destination is searched
/// by Message-ID); None when the message has no Message-ID.
pub async fn move_message(
    acc: &AccountConfig,
    folder: &str,
    uid: u32,
    dest_folder: &str,
) -> Result<Option<u32>, String> {
    let acc = acc.clone();
    let folder = folder.to_string();
    let dest = dest_folder.to_string();
    tauri::async_runtime::spawn_blocking(move || {
        let mut session = imap_session(&acc)?;
        session
            .select(&folder)
            .map_err(|e| format!("SELECT {folder} failed: {e}"))?;
        // Capture the Message-ID before the move so we can find the copy.
        let message_id = fetch_message_ids(&mut session, &[uid])
            .ok()
            .and_then(|mut v| v.pop().flatten());
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
        let new_uid = match message_id {
            Some(mid) => {
                session
                    .select(&dest)
                    .map_err(|e| format!("SELECT {dest} failed: {e}"))?;
                session
                    .uid_search(format!("HEADER Message-ID \"{mid}\""))
                    .ok()
                    .and_then(|ids| ids.into_iter().max())
            }
            None => None,
        };
        Ok(new_uid)
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

/// Guess a ContentType from a filename extension (octet-stream fallback).
pub fn content_type_for(filename: &str) -> lettre::message::header::ContentType {
    use lettre::message::header::ContentType;
    let ext = std::path::Path::new(filename)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    let mime = match ext.as_str() {
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "txt" | "md" => "text/plain",
        "html" | "htm" => "text/html",
        "csv" => "text/csv",
        "json" => "application/json",
        "zip" => "application/zip",
        "gz" => "application/gzip",
        "tar" => "application/x-tar",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "ppt" => "application/vnd.ms-powerpoint",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "mp3" => "audio/mpeg",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "ogg" => "audio/ogg",
        "wav" => "audio/wav",
        _ => "application/octet-stream",
    };
    ContentType::parse(mime).unwrap_or_else(|_| ContentType::parse("application/octet-stream").unwrap())
}

/// Send an email, optionally with file attachments (paths read from disk).
/// When replying, pass the replied-to message's Message-ID and References
/// chain so the outgoing mail joins the same thread.
pub async fn send_email(
    acc: &AccountConfig,
    to: Vec<String>,
    subject: &str,
    body: &str,
    attachments: Vec<String>,
    in_reply_to: Option<String>,
    references: Option<String>,
) -> Result<(), String> {
    use lettre::message::{header::ContentType, Mailbox, MultiPart, SinglePart};
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
    // Threading headers: wrap bare message-ids in angle brackets as RFC
    // 5322 requires (the stored ids are bracket-less).
    if let Some(irt) = in_reply_to.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        let id = irt.trim_start_matches('<').trim_end_matches('>');
        builder = builder.in_reply_to(format!("<{id}>"));
    }
    if let Some(refs) = references.as_deref().filter(|s| !s.trim().is_empty()) {
        let joined = refs
            .split_whitespace()
            .map(|id| format!("<{}>", id.trim_start_matches('<').trim_end_matches('>')))
            .collect::<Vec<_>>()
            .join(" ");
        builder = builder.references(joined);
    }

    // Read attachment files (blocking IO) and build the multipart body.
    let mut parts: Vec<SinglePart> = Vec::with_capacity(attachments.len() + 1);
    parts.push(
        SinglePart::builder()
            .header(ContentType::TEXT_PLAIN)
            .body(body.to_string()),
    );
    for path in &attachments {
        let bytes = std::fs::read(path).map_err(|e| format!("read attachment {path}: {e}"))?;
        let filename = std::path::Path::new(path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("attachment")
            .to_string();
        let ct = content_type_for(&filename);
        parts.push(lettre::message::Attachment::new(filename).body(bytes, ct));
    }

    let email = if attachments.is_empty() {
        builder
            .subject(subject.to_string())
            .body(body.to_string())
            .map_err(|e| format!("build message: {e}"))?
    } else {
        let first = parts.pop().expect("text part always present");
        let mut mp = MultiPart::mixed().singlepart(first);
        for part in parts {
            mp = mp.singlepart(part);
        }
        builder
            .subject(subject.to_string())
            .multipart(mp)
            .map_err(|e| format!("build message: {e}"))?
    };

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

/// Fetch attachment parts (by their position in the attachments iterator)
/// from the server with a single round-trip: one full-message FETCH, one
/// parse, then extract every requested part. Shared by save and thumbnail
/// previews — previews used to re-download the whole message once per image.
pub(crate) fn fetch_attachment_parts(
    acc: &AccountConfig,
    folder: &str,
    uid: u32,
    part_indexes: &[usize],
) -> Result<Vec<Vec<u8>>, String> {
    if part_indexes.is_empty() {
        return Ok(Vec::new());
    }
    let mut session = imap_session(acc)?;
    session
        .select(folder)
        .map_err(|e| format!("SELECT {folder} failed: {e}"))?;
    let fetches = session
        .uid_fetch(format!("{uid}"), "(BODY.PEEK[])")
        .map_err(|e| format!("FETCH failed: {e}"))?;
    let raw = fetches
        .iter()
        .next()
        .and_then(|f| f.body())
        .ok_or_else(|| format!("message uid {uid} not found"))?
        .to_vec();
    let msg = MessageParser::default().parse(&raw).ok_or("unparseable message")?;
    let parts: Vec<_> = msg.attachments().collect();
    let mut out = Vec::with_capacity(part_indexes.len());
    for &idx in part_indexes {
        let part = parts
            .get(idx)
            .ok_or_else(|| format!("attachment {idx} not found"))?;
        let data = match &part.body {
            mail_parser::PartType::Binary(b) | mail_parser::PartType::InlineBinary(b) => b.to_vec(),
            mail_parser::PartType::Text(t) | mail_parser::PartType::Html(t) => t.as_bytes().to_vec(),
            mail_parser::PartType::Message(m) => m.raw_message().to_vec(),
            mail_parser::PartType::Multipart(_) => Vec::new(),
        };
        out.push(data);
    }
    Ok(out)
}

/// Fetch one attachment part (by its position in the attachments iterator)
/// from the server.
pub(crate) fn fetch_attachment_part(
    acc: &AccountConfig,
    folder: &str,
    uid: u32,
    part_index: usize,
) -> Result<Vec<u8>, String> {
    fetch_attachment_parts(acc, folder, uid, &[part_index]).map(|mut v| v.pop().unwrap_or_default())
}

/// Attachment metadata for a message, extracted from the raw server message.
#[cfg(test)]
pub(crate) fn attachment_meta(
    acc: &AccountConfig,
    folder: &str,
    uid: u32,
) -> Result<Vec<store::AttachmentMeta>, String> {
    let mut session = imap_session(acc)?;
    session
        .select(folder)
        .map_err(|e| format!("SELECT {folder} failed: {e}"))?;
    let fetches = session
        .uid_fetch(format!("{uid}"), "(BODY.PEEK[])")
        .map_err(|e| format!("FETCH failed: {e}"))?;
    let raw = fetches
        .iter()
        .next()
        .and_then(|f| f.body())
        .ok_or_else(|| format!("message uid {uid} not found"))?;
    Ok(store::extract_attachments(raw))
}

#[cfg(test)]
mod thread_tests {
    use super::*;

    #[test]
    fn thread_id_prefers_references_root() {
        let tid = compute_thread_id(
            "mid-child@x",
            "<mid-parent@x>",
            "<mid-root@x> <mid-parent@x>",
            "Re: hello",
        );
        assert_eq!(tid, "mid-root@x");
    }

    #[test]
    fn thread_id_falls_back_to_in_reply_to() {
        let tid = compute_thread_id("mid-child@x", "<mid-parent@x>", "", "Re: hello");
        assert_eq!(tid, "mid-parent@x");
    }

    #[test]
    fn thread_id_falls_back_to_own_message_id() {
        let tid = compute_thread_id("mid-own@x", "", "", "hello");
        assert_eq!(tid, "mid-own@x");
    }

    #[test]
    fn thread_id_falls_back_to_normalized_subject() {
        let tid = compute_thread_id("", "", "", "  Re: FWD: Hello   World ");
        assert_eq!(tid, "subj:hello world");
    }

    #[test]
    fn extract_message_ids_strips_angle_brackets() {
        assert_eq!(
            extract_message_ids("<a@x> <b@y>"),
            vec!["a@x".to_string(), "b@y".to_string()]
        );
        assert!(extract_message_ids("").is_empty());
    }

    #[test]
    fn normalize_subject_strips_prefixes_recursively() {
        assert_eq!(normalize_subject("Re: re: Fwd: hello"), "hello");
        assert_eq!(normalize_subject("Hello"), "hello");
        assert_eq!(normalize_subject("  Multiple   spaces "), "multiple spaces");
    }
}
