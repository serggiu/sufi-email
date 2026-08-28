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
}

/// Fire the silent "new mail" toast through the OS notification daemon.
/// Visual only: no sound hint is set (the freedesktop spec is silent by
/// default; `.silent()` makes that explicit on platforms that support it).
fn notify_new_mail(app: &tauri::AppHandle) {
    use tauri_plugin_notification::NotificationExt;
    if let Err(e) = app
        .notification()
        .builder()
        .title("New email")
        .body("You have a new email.")
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
    result
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
                        notify_new_mail(&app);
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
/// the same cycle as any notification.
fn poll_inboxes(app: &tauri::AppHandle) {
    let st = app.state::<AppState>();
    let accounts = st.config.lock().unwrap().accounts.clone();
    let mut connected = false;
    if !accounts.is_empty() {
        for acc in accounts {
            let Some(inbox) = inbox_folder_name(&acc) else {
                continue;
            };
            match mail::list_folder_uids(&acc, &inbox) {
                Ok(uids) => {
                    connected = true;
                    if let Ok(mut s) = store::Store::open(&acc) {
                        match s.new_uids_since_last_sync(&inbox, &uids) {
                            Ok(new) if !new.is_empty() => {
                                log::info!(
                                    "inbox poll: {} new message(s) for {} ({inbox})",
                                    new.len(),
                                    acc.email
                                );
                                notify_new_mail(app);
                            }
                            Ok(_) => {}
                            Err(e) => log::warn!("inbox poll: new-mail check failed: {e}"),
                        }
                    }
                }
                Err(e) => log::warn!("inbox poll failed for {}: {e}", acc.email),
            }
        }
    }
    // We just proved connectivity: push any offline flag changes to the
    // server (read/unread made while disconnected) BEFORE telling the UI
    // to refresh, so the refreshed badges reflect the synced flags.
    if connected {
        flush_pending_flags(app);
    }

    // Tell the UI to refresh regardless of whether anything was new — this
    // is the app's auto-refresh pulse (unread badges, message list, offline
    // state all ride on it). The frontend only acts when it is online.
    let _ = app.emit("mail-refresh", ());
}

/// Background INBOX poller — the single refresh driver for the whole app.
/// Every `refresh_interval_minutes` (config, default 1, 0 = off) it checks
/// every account's inbox for new mail and fires a silent notification when
/// new mail arrives; after each cycle it emits a `mail-refresh` event so the
/// frontend reloads folders + the visible message list in the same tick —
/// the notification and the list refresh are therefore always in sync, and
/// the UI auto-refresh and the background poll share one interval.
///
/// A short tick with elapsed-time bookkeeping (rather than sleeping the
/// whole interval) means interval changes in config.toml — including 0 →
/// on — take effect within seconds instead of at the next long sleep.
fn spawn_inbox_poller(app: tauri::AppHandle) {
    tauri::async_runtime::spawn_blocking(move || {
        use std::time::Instant;
        // First poll ~5 s after startup, then on the configured cadence.
        let mut last_poll = Instant::now() - Duration::from_secs(55);
        let mut last_interval: Option<u64> = None;
        loop {
            let st = app.state::<AppState>();
            let minutes = st.config.lock().unwrap().refresh_interval_minutes;
            drop(st);
            let interval = minutes.min(24 * 60);
            if interval != last_interval.unwrap_or(interval) {
                log::info!("refresh interval set to {interval} minute(s)");
                last_interval = Some(interval);
            }
            if interval > 0 && last_poll.elapsed() >= Duration::from_secs(interval * 60) {
                last_poll = Instant::now();
                poll_inboxes(&app);
            }
            std::thread::sleep(Duration::from_secs(5));
        }
    });
}

/// The configured auto-refresh interval in minutes (0 = off).
#[tauri::command]
fn get_refresh_interval(state: State<AppState>) -> u64 {
    state.config.lock().unwrap().refresh_interval_minutes
}

/// Persist the auto-refresh interval (minutes, 0 = off). Clamped so a
/// nonsense value can't disable polling forever by accident.
#[tauri::command]
fn set_refresh_interval(minutes: u64, state: State<AppState>) -> Result<(), String> {
    let minutes = minutes.min(24 * 60);
    let mut cfg = state.config.lock().unwrap();
    cfg.refresh_interval_minutes = minutes;
    let result = cfg.save();
    drop(cfg);
    log::info!("refresh interval set to {minutes} minute(s)");
    result
}

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
        })
        .setup(|app| {
            spawn_inbox_poller(app.handle().clone());
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
            send_email,
            get_refresh_interval,
            set_refresh_interval
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
