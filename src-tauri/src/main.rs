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
///
/// Runs on a plain detached thread on purpose: notify-rust's zbus blocking
/// path must not run on a tokio worker thread. With the tokio feature
/// (pulled in transitively by the dialog plugin's xdg-portal backend),
/// zbus::block_on drives its own static runtime, and Runtime::block_on
/// panics with "Cannot start a runtime from within a runtime" on a thread
/// already inside tauri's async runtime — which is exactly where the
/// notification plugin's internal spawn used to run `show()`, silently
/// killing every new-mail toast.
fn fire_new_mail_notification(from: &str, subject: &str, extra: usize) {
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
    std::thread::spawn(move || {
        if let Err(e) = notify_rust::Notification::new()
            .appname("sufi-email")
            .summary(&title)
            .body(&body)
            .show()
        {
            log::warn!("new-mail notification failed: {e}");
        }
    });
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
        if let Err(e) = on_refresh.send(match result {
            Ok(fresh) => (fresh, true),
            Err(_) => (Vec::new(), false),
        }) {
            log::warn!("folder refresh channel send failed: {e}");
        }
    });

    Ok(cached)
}

#[tauri::command]
async fn list_messages(
    account: String,
    folder: String,
    on_batch: tauri::ipc::Channel<Vec<mail::MessageSummary>>,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let cfg = {
        let c = state.config.lock().unwrap();
        c.accounts.iter().find(|a| a.name == account).cloned()
    };
    let acc = cfg.ok_or_else(|| format!("unknown account '{account}'"))?;

    // 1. Instantly serve whatever we have cached locally — bounded to the
    //    newest 200 (the same window the server stream uses), so large
    //    folders (e.g. Trash with thousands of rows) show a navigable list
    //    instead of every cached message.
    {
        let store = store::Store::open(&acc)?;
        // load_summaries already hides rows under an in-flight optimistic
        // delete; the pending-delete filter is belt and braces here.
        let mut cached = store.load_summaries(&folder)?;
        if cached.len() > 200 {
            cached.sort_by(|a, b| b.date.cmp(&a.date));
            cached.truncate(200);
        }
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
    let result = mail::list_messages_streamed(
        &acc,
        &folder,
        move |mut batch, mut bodies| {
            if let Ok(mut s) = store::Store::open(&acc2) {
                // Messages with an in-flight optimistic delete are still on
                // the server mid-move; drop them from the outgoing batch so
                // the UI doesn't flash them back for a moment.
                if let Ok(pending) = s.pending_delete_uids(&folder2) {
                    if !pending.is_empty() {
                        let mut kept = Vec::with_capacity(batch.len());
                        let mut kept_bodies = Vec::with_capacity(bodies.len());
                        for (m, b) in batch.drain(..).zip(bodies.drain(..)) {
                            if !pending.contains(&m.uid) {
                                kept.push(m);
                                kept_bodies.push(b);
                            }
                        }
                        batch = kept;
                        bodies = kept_bodies;
                    }
                }
                if let Err(e) = s.upsert_summaries(&folder2, &batch) {
                    log::warn!("cache upsert failed: {e}");
                }
                // Warm the body cache: the full bodies were downloaded to
                // build the snippets, so store them and make opening any
                // listed message a cache hit.
                if let Err(e) = s.store_bodies(&folder2, &bodies) {
                    log::warn!("body cache write failed: {e}");
                }
                // Apply locally-pending seen states to the outgoing batch so
                // the UI doesn't flash a just-marked message back to unread
                // while the server STORE is still in flight.
                if let Ok(pending) = s.pending_seen_map(&folder2) {
                    for m in batch.iter_mut() {
                        if let Some(seen) = pending.get(&m.uid) {
                            m.seen = *seen;
                        }
                    }
                }
            }
            let _ = store_channel.send(batch);
        },
        move |server_uids| {
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
                // The server delete for an optimistically-deleted message
                // finished: its UID is gone, so drop the shield.
                if let Err(e) = s.clear_pending_deletes_not_in(&folder3, &server_uids) {
                    log::warn!("pending-delete reconcile failed: {e}");
                }
            }
        },
    )
    .await;

    // Report connectivity so the ~offline tag tracks the real state.
    match &result {
        // No new-mail notification here: the background IDLE watcher is the
        // notifier (it watches INBOX, where new mail arrives). Detecting
        // "new" UIDs in an arbitrary fetched folder would fire toasts for
        // messages merely moved into it (e.g. an unread message deleted to
        // Trash), which is not new mail.
        Ok(()) => Ok(()),
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
    // Record the change as pending BEFORE the server call: a list refresh
    // during the SMTP/IMAP round trip would otherwise overwrite it with
    // the server's still-stale flag and flash the message back to unread.
    store.upsert_pending_flag(&folder, uid, seen)?;

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

/// Optimistic move, part 1: remove the message from the UI + shield the
/// cached row right away (fast local call), mirroring delete. The server
/// move runs in the background via [`move_message_server`] and relocates
/// the row to the destination.
#[tauri::command]
async fn move_message_local(
    account: String,
    folder: String,
    uid: u32,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let cfg = {
        let c = state.config.lock().unwrap();
        c.accounts.iter().find(|a| a.name == account).cloned()
    };
    let acc = cfg.ok_or_else(|| format!("unknown account '{account}'"))?;
    let mut store = store::Store::open(&acc)?;
    store.mark_pending_delete(&folder, uid)
}

/// Optimistic move, part 2: the server-side move, run in the background.
/// On success the cached row is relocated to the destination (so it shows
/// up there immediately) and recorded as already notified — a server move
/// looks like a brand-new UID to the IDLE watcher, which would otherwise
/// toast "new email" for a message the user just moved.
#[tauri::command]
async fn move_message_server(
    account: String,
    folder: String,
    uid: u32,
    dest_folder: String,
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let cfg = {
        let c = state.config.lock().unwrap();
        c.accounts.iter().find(|a| a.name == account).cloned()
    };
    let acc = cfg.ok_or_else(|| format!("unknown account '{account}'"))?;

    let result: Result<Option<u32>, String> =
        mail::move_message(&acc, &folder, uid, &dest_folder).await;

    if let Ok(new_uid) = &result {
        if let Ok(mut store) = store::Store::open(&acc) {
            if let Some(new_uid) = new_uid {
                let _ = store.relocate_message(&folder, uid, &dest_folder, *new_uid);
                let _ = store.mark_notified(&dest_folder, &[*new_uid]);
            }
            let _ = store.delete_message(&folder, uid);
            let _ = store.clear_pending_delete(&folder, uid);
        }
    } else {
        // Failure: unshield the row so the next folder sync re-syncs it
        // (the message never left the source folder on the server).
        if let Ok(mut store) = store::Store::open(&acc) {
            let _ = store.clear_pending_delete(&folder, uid);
        }
    }

    // Refresh the UI: a user sitting in the destination folder sees the
    // moved message appear there right away.
    if result.is_ok() {
        let _ = app.emit("mail-refresh", ());
    }
    result.map(|_| ())
}

/// Optimistic delete, part 1: remove the message from the local cache
/// immediately (UI + store). Returns fast; the server call follows in the
/// background via [`delete_message_server`].
#[tauri::command]
async fn delete_message_local(
    account: String,
    folder: String,
    uid: u32,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let cfg = {
        let c = state.config.lock().unwrap();
        c.accounts.iter().find(|a| a.name == account).cloned()
    };
    let acc = cfg.ok_or_else(|| format!("unknown account '{account}'"))?;
    let mut store = store::Store::open(&acc)?;
    // Keep the row (hidden from lists by the pending-delete marker) so the
    // background delete_message_server can relocate it into Trash's cache
    // with the copy's new UID — deleting it here would leave Trash without
    // the message until the next server sync.
    store.mark_pending_delete(&folder, uid)
}

/// Optimistic delete, part 2: the actual server-side delete (move to Trash,
/// or \Deleted + expunge when there is no Trash). Runs in the background;
/// a failure surfaces in the UI and the message reappears on the next
/// folder sync (the reconcile re-adds UIDs still on the server). On success
/// the cached row is relocated into Trash so the deleted message is already
/// there when the user opens it.
#[tauri::command]
async fn delete_message_server(
    account: String,
    folder: String,
    uid: u32,
    trash_folder: Option<String>,
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let cfg = {
        let c = state.config.lock().unwrap();
        c.accounts.iter().find(|a| a.name == account).cloned()
    };
    let acc = cfg.ok_or_else(|| format!("unknown account '{account}'"))?;

    let result: Result<Option<u32>, String> = (|| async {
        match trash_folder.clone() {
            // Move to Trash when the server has one (or the UI found one).
            // move_message returns the copy's new UID in Trash.
            Some(trash) if trash != folder => {
                mail::move_message(&acc, &folder, uid, &trash).await
            }
            // No Trash folder: fall back to plain IMAP delete (\Deleted + expunge).
            _ => {
                mail::delete_message(&acc, &folder, uid).await?;
                Ok(None)
            }
        }
    })()
    .await;

    if let Ok(mut store) = store::Store::open(&acc) {
        match &result {
            // Moved to Trash: re-point the cached row to the copy's new
            // UID so the deleted message shows up in Trash immediately.
            Ok(Some(new_uid)) => {
                if let Some(trash) = trash_folder.as_deref() {
                    let _ = store.relocate_message(&folder, uid, trash, *new_uid);
                }
                let _ = store.delete_message(&folder, uid);
            }
            // Permanent delete (no Trash): drop the cached row.
            Ok(None) => {
                let _ = store.delete_message(&folder, uid);
            }
            // Failure: keep the row (still shielded by pending_delete until
            // the next folder sync re-syncs it — the existing rollback).
            Err(_) => {}
        }
        // The delete is settled either way: success means the message is gone
        // from the server; failure means the next sync re-adds it (rollback).
        // Either way it must no longer be shielded from refreshes.
        let _ = store.clear_pending_delete(&folder, uid);
    }

    // Refresh the UI (a user sitting in Trash sees the deleted message
    // appear there right away instead of on the next manual refresh).
    if result.is_ok() {
        let _ = app.emit("mail-refresh", ());
    }
    result.map(|_| ())
}

#[derive(serde::Serialize)]
struct AttachmentInfo {
    filename: String,
    #[serde(rename = "contentType")]
    content_type: String,
    size: i64,
    part_id: String,
    #[serde(rename = "contentId")]
    content_id: Option<String>,
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
            content_id: a.content_id,
        })
        .collect())
}

#[derive(serde::Deserialize)]
struct SendArgs {
    account: String,
    to: Vec<String>,
    subject: String,
    body: String,
    #[serde(default)]
    attachments: Vec<String>,
    /// Message-ID being replied to (sets In-Reply-To, keeps threads linked).
    #[serde(default)]
    in_reply_to: Option<String>,
    /// The thread's References chain (space-joined message-ids).
    #[serde(default)]
    references: Option<String>,
}

#[tauri::command]
async fn send_email(args: SendArgs, state: State<'_, AppState>) -> Result<(), String> {
    let cfg = {
        let c = state.config.lock().unwrap();
        c.accounts.iter().find(|a| a.name == args.account).cloned()
    };
    let acc = cfg.ok_or_else(|| format!("unknown account '{}'", args.account))?;
    mail::send_email(
        &acc,
        args.to,
        &args.subject,
        &args.body,
        args.attachments,
        args.in_reply_to,
        args.references,
    )
    .await
}

/// All cached messages of one conversation, newest first.
#[tauri::command]
async fn get_thread(
    account: String,
    folder: String,
    thread_id: String,
    state: State<'_, AppState>,
) -> Result<Vec<mail::MessageSummary>, String> {
    let cfg = {
        let c = state.config.lock().unwrap();
        c.accounts.iter().find(|a| a.name == account).cloned()
    };
    let acc = cfg.ok_or_else(|| format!("unknown account '{account}'"))?;
    let store = store::Store::open(&acc)?;
    store.load_thread(&folder, &thread_id)
}

/// Fetch attachment parts and return them base64-encoded, for inline image
/// thumbnails and cid: rewriting in the message view. All requested parts of
/// one message are fetched with a single round-trip, and each part is cached
/// on first use so later views (and offline views) don't hit the server.
#[tauri::command]
async fn get_attachments_data(
    account: String,
    folder: String,
    uid: u32,
    part_ids: Vec<String>,
    state: State<'_, AppState>,
) -> Result<Vec<String>, String> {
    let cfg = {
        let c = state.config.lock().unwrap();
        c.accounts.iter().find(|a| a.name == account).cloned()
    };
    let acc = cfg.ok_or_else(|| format!("unknown account '{account}'"))?;
    let part_indexes: Vec<usize> = part_ids
        .iter()
        .map(|p| p.parse().map_err(|_| format!("invalid part id '{p}'")))
        .collect::<Result<_, _>>()?;

    // Serve everything we already have from the local cache.
    let mut store = store::Store::open(&acc)?;
    let mut cached = store.load_attachments_data(&folder, uid, &part_ids)?;

    // One round-trip for every part we're still missing.
    let miss_indexes: Vec<usize> = cached
        .iter()
        .enumerate()
        .filter_map(|(i, c)| if c.is_none() { Some(i) } else { None })
        .collect();
    if !miss_indexes.is_empty() {
        let miss_positions: Vec<usize> = miss_indexes.iter().map(|&i| part_indexes[i]).collect();
        let acc2 = acc.clone();
        let folder2 = folder.clone();
        let fetched = tauri::async_runtime::spawn_blocking(move || {
            mail::fetch_attachment_parts(&acc2, &folder2, uid, &miss_positions)
        })
        .await
        .map_err(|e| format!("join error: {e}"))??;
        let mut to_cache = Vec::with_capacity(fetched.len());
        for (k, data) in fetched.into_iter().enumerate() {
            cached[miss_indexes[k]] = Some(data.clone());
            to_cache.push((part_ids[miss_indexes[k]].clone(), data));
        }
        store.store_attachments_data(&folder, uid, &to_cache)?;
    }

    Ok(cached
        .into_iter()
        .map(|c| c.map(|d| crypto::base64_encode(&d)).unwrap_or_default())
        .collect())
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

    // Prefer the cached copy (already fetched for a thumbnail preview) so
    // saving doesn't re-download the whole message.
    let mut store = store::Store::open(&acc)?;
    if let Some(data) = store.load_attachment_data(&folder, uid, &part_id)? {
        let mut file = open_private_file(&dest_path)
            .map_err(|e| format!("create {}: {e}", dest_path))?;
        file.write_all(&data).map_err(|e| format!("write: {e}"))?;
        return Ok(());
    }

    // Not cached: fetch + extract the requested part off the UI thread.
    let acc2 = acc.clone();
    let folder2 = folder.clone();
    let data = tauri::async_runtime::spawn_blocking(move || {
        mail::fetch_attachment_part(&acc2, &folder2, uid, part_index)
    })
    .await
    .map_err(|e| format!("join error: {e}"))??;
    store.store_attachment_data(&folder, uid, &part_id, &data)?;

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

/// Split "new" UIDs into genuinely new mail vs. moves: a message moved
/// into Inbox from another folder keeps its Message-ID, so a UID whose
/// Message-ID is already known in the cache (any folder) is a move, not
/// new mail. Purely a function of the fetched ids and the `known` test, so
/// it is unit-testable without IMAP.
fn partition_genuine_vs_moved(
    new_uids: &[u32],
    message_ids: &[Option<String>],
    known: &dyn Fn(&str) -> bool,
) -> (Vec<u32>, Vec<u32>) {
    let mut genuine = Vec::new();
    let mut moved = Vec::new();
    for (i, &uid) in new_uids.iter().enumerate() {
        match message_ids.get(i).and_then(|m| m.as_deref()) {
            Some(mid) if known(mid) => moved.push(uid),
            _ => genuine.push(uid),
        }
    }
    (genuine, moved)
}

/// Of the UIDs that look new, drop the ones that are actually moves into
/// this folder (their Message-ID already exists in the cache). Returns the
/// genuinely new UIDs; the moved ones are marked notified so they don't
/// re-toast later. Uses the live session for one batched header fetch.
fn filter_moved_new_uids(
    acc: &AccountConfig,
    inbox: &str,
    session: &mut imap::Session<Box<dyn imap::ImapConnection>>,
    new_uids: Vec<u32>,
) -> Vec<u32> {
    if new_uids.is_empty() {
        return new_uids;
    }
    // Keep the fetch list sorted so the batched header fetch is compact;
    // fetch_message_ids aligns results by UID, so order never matters.
    let mut sorted = new_uids.clone();
    sorted.sort_unstable();
    let ids = match mail::fetch_message_ids(session, &sorted) {
        Ok(ids) => ids,
        Err(e) => {
            log::warn!("new-mail header fetch failed: {e}; notifying as new");
            return new_uids;
        }
    };
    let Ok(mut store) = store::Store::open(acc) else {
        return new_uids;
    };
    let (genuine, moved) = partition_genuine_vs_moved(&sorted, &ids, &|mid| {
        store.message_id_known(mid).unwrap_or(false)
    });
    if !moved.is_empty() {
        log::info!(
            "{} message(s) in {inbox} already known in the cache (moved or previously synced), no toast",
            moved.len()
        );
        let _ = store.mark_notified(inbox, &moved);
    }
    genuine
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
                    fire_new_mail_notification(&from, &subject, extra);
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

    // Catch up on anything that arrived while the connection was down:
    // IDLE only pushes FUTURE changes, so without this diff a message that
    // landed during a disconnect would stay invisible until a manual
    // refresh. Best-effort: a failed catch-up must not abort the watch.
    if let Err(e) = process_inbox_change(app, acc, &inbox, &mut session) {
        log::warn!("inbox catch-up after reconnect failed: {e}");
    }

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
                if let Err(e) = process_inbox_change(app, acc, &inbox, &mut session) {
                    log::warn!("inbox change handling failed: {e}");
                }
                // Re-enter IDLE.
            }
            Ok(WaitOutcome::TimedOut) => {
                // keepalive re-issued the IDLE; keep waiting.
            }
            Err(e) => return Err(e.to_string()),
        }
    }
}

/// One mailbox-change pass: diff the current UID set, drop moved messages,
/// notify about genuinely new mail, warm the cache, and poke the frontend.
/// Shared by the IDLE push handler and the post-reconnect catch-up.
fn process_inbox_change(
    app: &tauri::AppHandle,
    acc: &AccountConfig,
    inbox: &str,
    session: &mut imap::Session<Box<dyn imap::ImapConnection>>,
) -> Result<(), String> {
    let uids = session
        .uid_search("ALL")
        .map_err(|e| format!("UID SEARCH failed: {e}"))?;
    let mut uids: Vec<u32> = uids.into_iter().collect();
    uids.sort_unstable();
    let new = diff_new_uids(acc, inbox, &uids);
    // A message moved into Inbox from another folder (by any client) shows
    // up as a brand-new UID here; drop those whose Message-ID is already
    // cached so they don't toast.
    let new = filter_moved_new_uids(acc, inbox, session, new);
    if !new.is_empty() {
        log::info!(
            "new mail: {} message(s) for {} ({inbox})",
            new.len(),
            acc.email
        );
        // Sender + subject from the ENVELOPE on this very session —
        // instant, no full sync needed.
        let newest = *new.iter().max().unwrap_or(&0);
        let (from, subject) =
            mail::fetch_envelope_preview(session, newest).unwrap_or_default();
        fire_new_mail_notification(&from, &subject, new.len() - 1);
        // Warm the INBOX cache so the message is already in the list when
        // the user opens/refreshes it — the toast should not beat the
        // message.
        warm_inbox_cache(acc, inbox);
    }
    flush_pending_flags(app);
    let _ = app.emit("mail-refresh", ());
    Ok(())
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
            move_message_local,
            move_message_server,
            delete_message_local,
            delete_message_server,
            get_thread,
            get_attachments_data,
            send_email
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod main_tests {
    #[test]
    fn partition_genuine_vs_moved() {
        let known = |mid: &str| mid == "<known@x>" || mid == "<known2@x>";
        let uids = vec![1, 2, 3, 4];
        let ids = vec![
            Some("<known@x>".into()),   // moved
            None,                        // no Message-ID -> treat as new
            Some("<fresh@x>".into()),   // genuinely new
            Some("<known2@x>".into()),  // moved
        ];
        let (genuine, moved) = super::partition_genuine_vs_moved(&uids, &ids, &known);
        assert_eq!(genuine, vec![2, 3]);
        assert_eq!(moved, vec![1, 4]);
    }

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


#[cfg(test)]
mod manual_send_tests {
    use tauri::async_runtime::block_on;

    /// Manual check: sends a real email through the configured SMTP account
    /// (to itself). Run with: cargo test -- --ignored manual_send
    #[test]
    #[ignore]
    fn manual_send_test_email() {
        let cfg = crate::account::Config::load_or_default();
        let acc = cfg.accounts.first().expect("no configured account");
        let to = acc.email.clone();
        block_on(crate::mail::send_email(
            acc,
            vec![to.clone()],
            "sufi-email send test",
            "hello from the manual send test",
            Vec::new(),
            None,
            None,
        ))
        .expect("send failed");
        println!("sent to {to}");
    }
}

#[cfg(test)]
mod manual_inbox_tests {
    use tauri::async_runtime::block_on;

    /// List the newest INBOX messages on the server with subject + flags.
    #[test]
    #[ignore]
    fn manual_inbox() {
        let cfg = crate::account::Config::load_or_default();
        let acc = cfg.accounts.first().expect("no account");
        block_on(async {
            let acc = acc.clone();
            tauri::async_runtime::spawn_blocking(move || {
                let mut s = crate::mail::imap_session_pub(&acc).map_err(|e| e.to_string())?;
                let m = s.select("INBOX").map_err(|e| e.to_string())?;
                let top = m.exists.max(1);
                let from = top.saturating_sub(10);
                let f = s
                    .fetch(&format!("{from}:{top}"), "(UID ENVELOPE)")
                    .map_err(|e| e.to_string())?;
                for x in f.iter() {
                    let uid = x.uid.unwrap_or(0);
                    let subj = x
                        .envelope()
                        .and_then(|e| e.subject.as_ref())
                        .map(|s| String::from_utf8_lossy(s).into_owned())
                        .unwrap_or_default();
                    println!("uid {uid}: {subj}");
                }
                Ok::<(), String>(())
            })
            .await
        })
        .expect("failed");
    }
}
