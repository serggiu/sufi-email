#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod account;
mod crypto;
#[cfg(test)]
mod fake_imap;
mod mail;
mod store;
#[cfg(test)]
mod account_tests;
#[cfg(test)]
mod imap_integration_tests;
#[cfg(test)]
mod crypto_tests;
#[cfg(test)]
mod store_tests;

use account::{AccountConfig, AccountInfo, Config};
use std::sync::Mutex;
use std::time::Duration;
use tauri::Emitter;
use tauri::Manager;
use tauri::State;

pub struct AppState {
    pub config: Mutex<Config>,
    /// Accounts that currently have a live IDLE watcher thread. Used to
    /// avoid spawning duplicate watchers when new accounts are added at
    /// runtime.
    pub idle_watched: Mutex<std::collections::HashSet<String>>,
}

/// Fire the silent new-mail toast through the OS notification daemon.
/// Visual only: no sound hint is set (the freedesktop spec is silent by
/// default; `.silent()` makes that explicit on platforms that support it).
/// Shows the sender and subject when available; `extra` is the number of
/// additional new messages beyond the one described.
fn fire_new_mail_notification(app: &tauri::AppHandle, from: &str, subject: &str, extra: usize) {
    use tauri_plugin_notification::NotificationExt;
    let title = if from.is_empty() {
        "New email".to_string()
    } else {
        format!("New email from {from}")
    };
    let mut body = if subject.is_empty() {
        "You have a new email.".to_string()
    } else {
        subject.to_string()
    };
    if extra > 0 {
        body.push_str(&format!("  (+{extra} more)"));
    }
    if let Err(e) = app
        .notification()
        .builder()
        .title(title)
        .body(body)
        .silent()
        .show()
    {
        log::warn!("new-mail notification failed: {e}");
    }
}

/// Accounts for the UI — a sanitized view without the sealed passwords.
#[tauri::command]
fn get_accounts(state: State<AppState>) -> Vec<AccountInfo> {
    state
        .config
        .lock()
        .unwrap()
        .accounts
        .iter()
        .map(AccountInfo::from)
        .collect()
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewAccount {
    pub name: String,
    pub email: String,
    pub imap_host: String,
    pub imap_port: u16,
    pub username: String,
    pub password: String,
    pub smtp_host: String,
    pub smtp_port: u16,
    #[serde(default)]
    pub smtp_username: Option<String>,
    #[serde(default)]
    pub smtp_password: Option<String>,
    #[serde(default)]
    pub smtp_starttls: bool,
}

impl From<NewAccount> for AccountConfig {
    fn from(n: NewAccount) -> Self {
        // Passwords are sealed (AES-256-GCM) before ever touching the disk.
        let password = crypto::seal_password(&n.password).unwrap_or_default();
        let smtp_password = n
            .smtp_password
            .as_deref()
            .filter(|p| !p.is_empty())
            .map(|p| crypto::seal_password(p).unwrap_or_default());
        Self {
            name: n.name,
            email: n.email,
            imap_host: n.imap_host,
            imap_port: n.imap_port,
            smtp_host: n.smtp_host,
            smtp_port: n.smtp_port,
            username: n.username,
            password,
            smtp_username: n.smtp_username,
            smtp_password,
            smtp_starttls: n.smtp_starttls,
        }
    }
}

/// Validate + save a new account. Tests IMAP login and SMTP auth first so
/// broken credentials are never persisted.
#[tauri::command]
async fn add_account(
    account: NewAccount,
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let acc: AccountConfig = account.into();

    mail::test_imap(&acc).await?;
    mail::test_smtp(&acc).await?;

    let mut cfg = state.config.lock().unwrap();
    if cfg.accounts.iter().any(|a| a.name == acc.name) {
        return Err(format!("an account named '{}' already exists", acc.name));
    }
    cfg.accounts.push(acc);
    let result = cfg.save();
    drop(cfg);
    result?;

    // Give the new account its own background watcher right away.
    spawn_missing_idle_watchers(&app);
    Ok(())
}

#[tauri::command]
async fn delete_account(
    account: String,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let mut cfg = state.config.lock().unwrap();
    let removed = cfg.accounts.iter().find(|a| a.name == account).cloned();
    let before = cfg.accounts.len();
    cfg.accounts.retain(|a| a.name != account);
    if cfg.accounts.len() == before {
        return Err(format!("no account named '{account}'"));
    }
    let result = cfg.save();
    drop(cfg);
    result?;

    // Remove the account's local mail cache (best effort).
    if let Some(acc) = removed {
        store::Store::remove_account_db(&acc);
    }
    Ok(())
}

/// Serve the cached folder list instantly; refresh from the server in the
/// background and push the fresh list over the channel when it arrives.
#[tauri::command]
async fn list_folders(
    account: String,
    on_refresh: tauri::ipc::Channel<(Vec<mail::Folder>, bool)>,
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<Vec<mail::Folder>, String> {
    let cfg = {
        let c = state.config.lock().unwrap();
        c.accounts.iter().find(|a| a.name == account).cloned()
    };
    let acc = cfg.ok_or_else(|| format!("unknown account '{account}'"))?;

    // 1. Cached list, returned immediately (works offline).
    let cached = {
        let store = store::Store::open(&acc)?;
        store.load_folders()?
    };

    // 2. Server refresh in the background; UI gets the result via channel.
    let acc2 = acc.clone();
    let app2 = app.clone();
    tauri::async_runtime::spawn(async move {
        let result = match mail::list_folders(&acc2).await {
            Ok(fresh) => {
                if let Ok(mut store) = store::Store::open(&acc2) {
                    let _ = store.store_folders(&fresh);
                }
                // A successful round trip proves we are online — push any
                // offline read/unread changes to the server.
                let app3 = app2.clone();
                tauri::async_runtime::spawn_blocking(move || flush_pending_flags(&app3));
                Ok(fresh)
            }
            Err(e) => {
                log::warn!("folder refresh failed (offline?): {e}");
                Err(e)
            }
        };
        let _ = on_refresh.send(match result {
            Ok(fresh) => (fresh, true),
            Err(_) => (Vec::new(), false),
        });
    });

    Ok(cached)
}

#[tauri::command]
async fn list_messages(
    account: String,
    folder: String,
    on_batch: tauri::ipc::Channel<Vec<mail::MessageSummary>>,
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let cfg = {
        let c = state.config.lock().unwrap();
        c.accounts.iter().find(|a| a.name == account).cloned()
    };
    let acc = cfg.ok_or_else(|| format!("unknown account '{account}'"))?;

    // 1. Instantly serve whatever we have cached locally.
    {
        let store = store::Store::open(&acc)?;
        let cached = store.load_summaries(&folder)?;
        if !cached.is_empty() {
            on_batch.send(cached).map_err(|e| e.to_string())?;
        }
    }

    // 2. Refresh from the server, streaming batches; each batch is also
    //    persisted so the cache stays warm for the next launch. The final
    //    on_reconcile callback receives the server's full UID set — capture
    //    it so we can diff it against the notified set afterwards.
    let acc2 = acc.clone();
    let folder2 = folder.clone();
    let store_channel = on_batch.clone();
    let acc3 = acc.clone();
    let folder3 = folder.clone();
    let reconciled: std::sync::Arc<std::sync::Mutex<Vec<u32>>> =
        std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let reconciled2 = reconciled.clone();
    let result = mail::list_messages_streamed(
        &acc,
        &folder,
        move |batch, bodies| {
            if let Ok(mut s) = store::Store::open(&acc2) {
                if let Err(e) = s.upsert_summaries(&folder2, &batch) {
                    log::warn!("cache upsert failed: {e}");
                }
                // Warm the body cache: the full bodies were downloaded to
                // build the snippets, so store them and make opening any
                // listed message a cache hit.
                if let Err(e) = s.store_bodies(&folder2, &bodies) {
                    log::warn!("body cache write failed: {e}");
                }
            }
            let _ = store_channel.send(batch);
        },
        move |server_uids| {
            *reconciled2.lock().unwrap() = server_uids.iter().copied().collect();
            // Remove cache rows for UIDs that vanished from the server.
            if let Ok(mut s) = store::Store::open(&acc3) {
                match s.remove_uids_not_in(&folder3, &server_uids) {
                    Ok(n) if n > 0 => log::info!(
                        "reconciled {folder3}: removed {n} stale cache row(s)"
                    ),
                    Ok(_) => {}
                    Err(e) => log::warn!("reconcile failed: {e}"),
                }
                // Pending offline flag changes for vanished messages have
                // nothing left to sync.
                if let Err(e) = s.remove_pending_uids_not_in(&folder3, &server_uids) {
                    log::warn!("pending-flag reconcile failed: {e}");
                }
            }
        },
    )
    .await;

    // Report connectivity so the ~offline tag tracks the real state.
    match &result {
        Ok(()) => {
            // New-mail detection, shared with the background inbox poller:
            // UIDs present on the server but not yet recorded fire exactly
            // one generic notification (the first check of a folder only
            // records the baseline).
            let server_uids = reconciled.lock().unwrap().clone();
            if let Ok(mut s) = store::Store::open(&acc) {
                match s.new_uids_since_last_sync(&folder, &server_uids) {
                    Ok(new) if !new.is_empty() => {
                        log::info!("{}: {} new message(s), notifying", folder, new.len());
                        // The stream just upserted the summaries, so the
                        // sender/subject come from the cache.
                        let (from, subject, extra) = cached_preview(&acc, &folder, &new);
                        fire_new_mail_notification(&app, &from, &subject, extra);
                    }
                    Ok(_) => {}
                    Err(e) => log::warn!("new-mail check failed: {e}"),
                }
            }
            Ok(())
        }
        Err(e) => {
            // Serve-from-cache already happened; report offline instead of
            // failing so the UI can show the tag.
            log::warn!("message refresh failed (offline?): {e}");
            Err(e.clone())
        }
    }
}

#[tauri::command]
async fn mark_message(
    account: String,
    folder: String,
    uid: u32,
    seen: bool,
    state: State<'_, AppState>,
) -> Result<u32, String> {
    let cfg = {
        let c = state.config.lock().unwrap();
        c.accounts.iter().find(|a| a.name == account).cloned()
    };
    let acc = cfg.ok_or_else(|| format!("unknown account '{account}'"))?;
    // Apply to the local cache first: offline, reading a message still
    // marks it read locally, and the change is queued for the server.
    let mut store = store::Store::open(&acc)?;
    store.set_seen(&folder, uid, seen)?;

    match mail::set_seen(&acc, &folder, uid, seen).await {
        Ok(server_unread) => {
            // Synced: nothing pending for this message anymore, and the
            // badge reflects the server's authoritative count.
            store.remove_pending_flag(&folder, uid)?;
            store.set_folder_unread(&folder, server_unread)?;
            Ok(server_unread)
        }
        Err(e) => {
            // Offline: queue the change; the poller flushes the queue once
            // a connection is back. Badge gets a best-effort local count,
            // corrected by the next online folder refresh.
            log::warn!("mark {folder}/{uid} seen={seen} failed (queued): {e}");
            store.upsert_pending_flag(&folder, uid, seen)?;
            let local_unread = store
                .load_summaries(&folder)?
                .iter()
                .filter(|m| !m.seen)
                .count() as u32;
            store.set_folder_unread(&folder, local_unread)?;
            Ok(local_unread)
        }
    }
}

#[tauri::command]
async fn fetch_message(
    account: String,
    folder: String,
    uid: u32,
    state: State<'_, AppState>,
) -> Result<mail::MessageBody, String> {
    let cfg = {
        let c = state.config.lock().unwrap();
        c.accounts.iter().find(|a| a.name == account).cloned()
    };
    let acc = cfg.ok_or_else(|| format!("unknown account '{account}'"))?;

    // Serve from cache when we already have the body.
    {
        let store = store::Store::open(&acc)?;
        if let Some((text, html)) = store.load_body(&folder, uid)? {
            return Ok(mail::MessageBody { uid, text, html });
        }
    }

    // Not cached: fetch body + attachment metadata in a single connection,
    // then persist both so the next open is served from the cache.
    let fetched = mail::fetch_message_full(&acc, &folder, uid).await?;
    let mut store = store::Store::open(&acc)?;
    store.store_body(
        &folder,
        uid,
        fetched.body.text.as_deref(),
        fetched.body.html.as_deref(),
        &fetched.attachments,
    )?;
    Ok(fetched.body)
}

#[tauri::command]
async fn move_message(
    account: String,
    folder: String,
    uid: u32,
    dest_folder: String,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let cfg = {
        let c = state.config.lock().unwrap();
        c.accounts.iter().find(|a| a.name == account).cloned()
    };
    let acc = cfg.ok_or_else(|| format!("unknown account '{account}'"))?;
    mail::move_message(&acc, &folder, uid, &dest_folder).await?;

    // Remove from the source folder's cache; the copy lands in the dest
    // folder's cache next time it syncs.
    let mut store = store::Store::open(&acc)?;
    store.delete_message(&folder, uid)
}

#[tauri::command]
async fn delete_message(
    account: String,
    folder: String,
    uid: u32,
    trash_folder: Option<String>,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let cfg = {
        let c = state.config.lock().unwrap();
        c.accounts.iter().find(|a| a.name == account).cloned()
    };
    let acc = cfg.ok_or_else(|| format!("unknown account '{account}'"))?;

    match trash_folder {
        // Move to Trash when the server has one (or the UI found one).
        Some(trash) if trash != folder => {
            mail::move_message(&acc, &folder, uid, &trash).await?;
        }
        // No Trash folder: fall back to plain IMAP delete (\Deleted + expunge).
        _ => {
            mail::delete_message(&acc, &folder, uid).await?;
        }
    }

    let mut store = store::Store::open(&acc)?;
    store.delete_message(&folder, uid)
}

#[derive(serde::Serialize)]
struct AttachmentInfo {
    filename: String,
    #[serde(rename = "contentType")]
    content_type: String,
    size: i64,
    part_id: String,
}

#[tauri::command]
async fn list_attachments(
    account: String,
    folder: String,
    uid: u32,
    state: State<'_, AppState>,
) -> Result<Vec<AttachmentInfo>, String> {
    let cfg = {
        let c = state.config.lock().unwrap();
        c.accounts.iter().find(|a| a.name == account).cloned()
    };
    let acc = cfg.ok_or_else(|| format!("unknown account '{account}'"))?;
    let store = store::Store::open(&acc)?;
    Ok(store
        .load_attachments(&folder, uid)?
        .into_iter()
        .map(|a| AttachmentInfo {
            filename: a.filename,
            content_type: a.content_type,
            size: a.size,
            part_id: a.part_id,
        })
        .collect())
}

#[derive(serde::Deserialize)]
struct SendArgs {
    account: String,
    to: Vec<String>,
    subject: String,
    body: String,
}

#[tauri::command]
async fn send_email(args: SendArgs, state: State<'_, AppState>) -> Result<(), String> {
    let cfg = {
        let c = state.config.lock().unwrap();
        c.accounts.iter().find(|a| a.name == args.account).cloned()
    };
    let acc = cfg.ok_or_else(|| format!("unknown account '{}'", args.account))?;
    mail::send_email(&acc, args.to, &args.subject, &args.body).await
}

/// Fetch one attachment part by its position in the attachments iterator
/// and write it to the user-chosen destination.
#[tauri::command]
async fn save_attachment(
    account: String,
    folder: String,
    uid: u32,
    part_id: String,
    dest_path: String,
    state: State<'_, AppState>,
) -> Result<(), String> {
    use std::io::Write;

    let cfg = {
        let c = state.config.lock().unwrap();
        c.accounts.iter().find(|a| a.name == account).cloned()
    };
    let acc = cfg.ok_or_else(|| format!("unknown account '{account}'"))?;
    let part_index: usize = part_id.parse().map_err(|_| "invalid part id")?;

    // Fetch + extract the requested part off the UI thread.
    let data = tauri::async_runtime::spawn_blocking(move || {
        let mut session = mail::imap_session_pub(&acc)?;
        session
            .select(&folder)
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
        let msg = mail_parser::MessageParser::default()
            .parse(&raw)
            .ok_or("unparseable message")?;
        let parts: Vec<_> = msg.attachments().collect();
        let part = parts
            .get(part_index)
            .ok_or_else(|| format!("attachment {part_index} not found"))?;
        Ok::<Vec<u8>, String>(match &part.body {
            mail_parser::PartType::Binary(b) | mail_parser::PartType::InlineBinary(b) => b.to_vec(),
            mail_parser::PartType::Text(t) | mail_parser::PartType::Html(t) => t.as_bytes().to_vec(),
            mail_parser::PartType::Message(m) => m.raw_message().to_vec(),
            mail_parser::PartType::Multipart(_) => Vec::new(),
        })
    })
    .await
    .map_err(|e| format!("join error: {e}"))??;

    let mut file = open_private_file(&dest_path)
        .map_err(|e| format!("create {}: {e}", dest_path))?;
    file.write_all(&data).map_err(|e| format!("write: {e}"))?;
    Ok(())
}

/// Open a file for writing with owner-only permissions (0o600), so saved
/// message attachments aren't world-readable by default.
fn open_private_file(path: &str) -> std::io::Result<std::fs::File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
    }
    #[cfg(not(unix))]
    {
        std::fs::File::create(path)
    }
}

/// Push queued offline flag changes to the server. Called whenever the app
/// proves it is online (a successful inbox poll, a successful folder
/// refresh). Runs on a worker thread; each pending change gets its own
/// connection, and only successful syncs are dropped from the queue.
fn flush_pending_flags(app: &tauri::AppHandle) {
    let st = app.state::<AppState>();
    let accounts = st.config.lock().unwrap().accounts.clone();
    drop(st);
    for acc in &accounts {
        let Ok(mut store) = store::Store::open(acc) else { continue };
        let pending = match store.pending_flags() {
            Ok(p) => p,
            Err(e) => {
                log::warn!("pending-flag read failed for {}: {e}", acc.email);
                continue;
            }
        };
        for (folder, uid, seen) in pending {
            match mail::set_seen_blocking(acc, &folder, uid, seen) {
                Ok(_) => {
                    log::info!("synced offline flag {folder}/{uid} seen={seen}");
                    if let Err(e) = store.remove_pending_flag(&folder, uid) {
                        log::warn!("pending-flag removal failed: {e}");
                    }
                }
                Err(e) => log::debug!("pending flag {folder}/{uid} still unsynced: {e}"),
            }
        }
    }
}

/// Find the folder that plays the role of the inbox: prefer the server's
/// declared \Inbox special-use, then a name match on "INBOX", then any
/// folder containing "inbox". Returns None when the folder list is
/// unreachable (offline) or no inbox-like folder exists.
fn inbox_folder_name(acc: &AccountConfig) -> Option<String> {
    let folders = match mail::list_folders_blocking(acc) {
        Ok(f) => f,
        Err(e) => {
            log::warn!("inbox poll: folder list failed for {}: {e}", acc.email);
            return None;
        }
    };
    folders
        .iter()
        .find(|f| f.special_use.as_deref() == Some("inbox"))
        .map(|f| f.name.clone())
        .or_else(|| {
            folders
                .iter()
                .find(|f| f.name.eq_ignore_ascii_case("INBOX"))
                .map(|f| f.name.clone())
        })
        .or_else(|| {
            folders
                .iter()
                .find(|f| f.name.to_lowercase().contains("inbox"))
                .map(|f| f.name.clone())
        })
}

/// One poll cycle: check every account's inbox for new mail and notify.
/// Runs on the poller thread; never touches the UI directly. Emits
/// `mail-refresh` afterwards so the frontend reloads folders + messages in
/// Diff the server UID set against what we've already notified about and
/// return the genuinely-new UIDs (recording them so they won't re-fire).
fn diff_new_uids(acc: &AccountConfig, inbox: &str, uids: &[u32]) -> Vec<u32> {
    match store::Store::open(acc).and_then(|mut s| s.new_uids_since_last_sync(inbox, uids)) {
        Ok(new) => new,
        Err(e) => {
            log::warn!("new-mail check failed for {}: {e}", acc.email);
            Vec::new()
        }
    }
}

/// Sender display + subject of the newest of `new_uids`, read from the
/// local cache (used by the slow-poll fallback, which has no live session).
fn cached_preview(acc: &AccountConfig, inbox: &str, new_uids: &[u32]) -> (String, String, usize) {
    let Ok(store) = store::Store::open(acc) else {
        return (String::new(), String::new(), 0);
    };
    let Ok(all) = store.load_summaries(inbox) else {
        return (String::new(), String::new(), 0);
    };
    let set: std::collections::HashSet<u32> = new_uids.iter().copied().collect();
    let mut hits: Vec<&crate::mail::MessageSummary> =
        all.iter().filter(|m| set.contains(&m.uid)).collect();
    hits.sort_by(|a, b| b.uid.cmp(&a.uid));
    match hits.first() {
        Some(m) => (m.from.clone(), m.subject.clone(), hits.len().saturating_sub(1)),
        None => (String::new(), String::new(), 0),
    }
}

/// Spawn one IDLE watcher thread per account that doesn't have one yet.
/// Called at startup and periodically (accounts can be added at runtime).
fn spawn_missing_idle_watchers(app: &tauri::AppHandle) {
    let st = app.state::<AppState>();
    let accounts = st.config.lock().unwrap().accounts.clone();
    let mut watched = st.idle_watched.lock().unwrap();
    for acc in &accounts {
        if watched.insert(acc.name.clone()) {
            let app2 = app.clone();
            let acc2 = acc.clone();
            log::info!("starting IDLE watcher for {}", acc.email);
            tauri::async_runtime::spawn_blocking(move || {
                idle_watch_account(&app2, &acc2);
            });
        }
    }
}

/// Result of one IDLE connection attempt.
enum IdleOutcome {
    /// The server does not advertise IDLE; fall back to slow polling.
    NoIdle,
}

/// Per-account background watcher: prefers the RFC 2177 IDLE push (one
/// dormant connection per account, instant new-mail notification with no
/// polling), falling back to a slow poll loop for servers without IDLE.
/// Reconnects with backoff on connection loss; exits if the account is
/// removed.
fn idle_watch_account(app: &tauri::AppHandle, acc: &AccountConfig) {
    let mut backoff = 5u64;
    loop {
        // Stop if the account was deleted while we were away.
        let st = app.state::<AppState>();
        let still_there = {
            let cfg = st.config.lock().unwrap();
            cfg.accounts.iter().any(|a| a.name == acc.name)
        };
        if !still_there {
            log::info!("background watcher for {} exiting (account removed)", acc.email);
            let st = app.state::<AppState>();
            let mut watched = st.idle_watched.lock().unwrap();
            watched.remove(&acc.name);
            return;
        }
        match idle_cycle(app, acc) {
            Ok(IdleOutcome::NoIdle) => {
                log::info!(
                    "{} has no IDLE support; falling back to slow polling",
                    acc.email
                );
                slow_poll_account(app, acc);
                return;
            }
            Err(e) => {
                log::warn!("background watcher for {} lost: {e}; retrying in {backoff}s", acc.email);
                std::thread::sleep(Duration::from_secs(backoff));
                backoff = (backoff * 2).min(60);
            }
        }
    }
}

/// Fallback for servers without IDLE: a modest interval check (new mail +
/// offline-flag sync) that keeps notifications working without hammering
/// the server. The frontend's manual Refresh stays the primary trigger.
fn slow_poll_account(app: &tauri::AppHandle, acc: &AccountConfig) {
    loop {
        std::thread::sleep(Duration::from_secs(15 * 60));
        let Some(inbox) = inbox_folder_name(acc) else { continue };
        match mail::list_folder_uids(acc, &inbox) {
            Ok(uids) => {
                let new = diff_new_uids(acc, &inbox, &uids);
                if !new.is_empty() {
                    log::info!(
                        "new mail: {} message(s) for {} ({inbox})",
                        new.len(),
                        acc.email
                    );
                    // No live session here: sync the cache first so the
                    // sender/subject can come from the stored summaries.
                    warm_inbox_cache(acc, &inbox);
                    let (from, subject, extra) = cached_preview(acc, &inbox, &new);
                    fire_new_mail_notification(app, &from, &subject, extra);
                }
                flush_pending_flags(app);
                let _ = app.emit("mail-refresh", ());
            }
            Err(e) => log::warn!("slow poll for {} failed: {e}", acc.email),
        }
    }
}

/// Full INBOX sync triggered by new-mail detection: fetch + cache the
/// folder's summaries and bodies so the message list is warm. Switching to
/// INBOX (or refreshing it) then shows the new mail instantly from cache
/// instead of streaming from the server first — closing the gap between
/// the toast and the message appearing.
fn warm_inbox_cache(acc: &AccountConfig, inbox: &str) {
    let acc2 = acc.clone();
    let inbox2 = inbox.to_string();
    if let Err(e) = mail::list_messages_streamed_blocking(
        acc,
        inbox,
        move |batch, bodies| {
            if let Ok(mut s) = store::Store::open(&acc2) {
                let _ = s.upsert_summaries(&inbox2, &batch);
                let _ = s.store_bodies(&inbox2, &bodies);
            }
        },
        |_server_uids| {},
    ) {
        log::warn!("inbox warm refresh failed for {}: {e}", acc.email);
    }
}

/// One IDLE connection lifetime: connect, select the inbox, verify the
/// server supports IDLE, then idle until the connection drops. Returns
/// `NoIdle` (fall back to slow polling) or an error to trigger a reconnect.
fn idle_cycle(app: &tauri::AppHandle, acc: &AccountConfig) -> Result<IdleOutcome, String> {
    let Some(inbox) = inbox_folder_name(acc) else {
        return Err("no inbox folder found".into());
    };
    let mut session = mail::imap_session_pub(acc)?;
    session
        .select(&inbox)
        .map_err(|e| format!("SELECT {inbox} failed: {e}"))?;
    let caps = session
        .capabilities()
        .map_err(|e| format!("CAPABILITY failed: {e}"))?;
    if !caps.has_str("IDLE") {
        return Ok(IdleOutcome::NoIdle);
    }
    // We are connected: flush any offline flag changes queued while we
    // were away (the reconnect itself is the "back online" signal).
    flush_pending_flags(app);
    log::info!("background watcher for {} watching {inbox} (IDLE)", acc.email);

    loop {
        use imap::extensions::idle::WaitOutcome;
        // Bind the outcome first so the temporary IDLE handle (which borrows
        // the session mutably) is dropped before we touch the session again.
        let outcome = session.idle().wait_while(imap::extensions::idle::stop_on_any);
        match outcome {
            Ok(WaitOutcome::MailboxChanged) => {
                // The server pushed a mailbox change — most likely new mail.
                // Reuse this session (the IDLE borrow has ended) to diff the
                // UIDs and react instantly.
                let uids = match session.uid_search("ALL") {
                    Ok(ids) => {
                        let mut v: Vec<u32> = ids.into_iter().collect();
                        v.sort_unstable();
                        v
                    }
                    Err(e) => {
                        log::warn!("UID SEARCH after IDLE failed: {e}");
                        continue;
                    }
                };
                let new = diff_new_uids(acc, &inbox, &uids);
                if !new.is_empty() {
                    log::info!(
                        "new mail: {} message(s) for {} ({inbox})",
                        new.len(),
                        acc.email
                    );
                    // Sender + subject from the ENVELOPE on this very
                    // session — instant, no full sync needed.
                    let newest = *new.iter().max().unwrap_or(&0);
                    let (from, subject) =
                        mail::fetch_envelope_preview(&mut session, newest).unwrap_or_default();
                    fire_new_mail_notification(app, &from, &subject, new.len() - 1);
                    // Warm the INBOX cache so the message is already in the
                    // list when the user opens/refreshes it — the toast
                    // should not beat the message.
                    warm_inbox_cache(acc, &inbox);
                }
                flush_pending_flags(app);
                let _ = app.emit("mail-refresh", ());
                // Re-enter IDLE.
            }
            Ok(WaitOutcome::TimedOut) => {
                // keepalive re-issued the IDLE; keep waiting.
            }
            Err(e) => return Err(e.to_string()),
        }
    }
}

/// The configured auto-refresh interval in minutes (0 = off).
#[tauri::command]
fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("debug")).init();    let mut config = Config::load_or_default();
    config.migrate_plaintext_passwords();

    // Backfill content hashes + remove duplicates in existing caches.
    for acc in &config.accounts {
        match store::Store::open(acc) {
            Ok(mut s) => {
                if let Err(e) = s.backfill_hashes() {
                    log::warn!("hash backfill failed for {}: {e}", acc.email);
                }
            }
            Err(e) => log::warn!("store open failed for {}: {e}", acc.email),
        }
        match store::dedupe_existing(acc) {
            Ok(n) if n > 0 => log::info!("removed {n} duplicate message(s) from {}", acc.email),
            Ok(_) => {}
            Err(e) => log::warn!("dedupe failed for {}: {e}", acc.email),
        }
    }
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .manage(AppState {
            config: Mutex::new(config),
            idle_watched: Mutex::new(std::collections::HashSet::new()),
        })
        .setup(|app| {
            spawn_missing_idle_watchers(app.handle());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_accounts,
            add_account,
            delete_account,
            list_folders,
            list_messages,
            mark_message,
            fetch_message,
            list_attachments,
            save_attachment,
            move_message,
            delete_message,
            send_email
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod main_tests {
    #[test]
    fn open_private_file_creates_owner_only_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("saved-attachment.bin");
        let file = super::open_private_file(path.to_str().unwrap()).expect("create file");
        drop(file);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "saved attachments must be owner-only");
        }
    }
}
