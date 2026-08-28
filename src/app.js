import {
  loadLastSelection,
  loadFolderSelection,
  saveSelection,
  clearFolderSelection,
} from "./lib/selection.js";
import { fmtDate, fmtSize, escapeHtml, withEmailCsp } from "./lib/format.js";
import {
  renderViewSubject,
  renderViewMeta,
  renderViewBody,
} from "./lib/mailview.js";

const invoke = window.__TAURI__ ? window.__TAURI__.core.invoke : null;

const state = {
  accounts: [],
  account: null,
  folders: [],
  folder: null,
  messages: [],
  selectedUid: null,
  sidebarVisible: true,
  online: true,
};

const $ = (id) => document.getElementById(id);

function showError(msg) {
  // Errors surface in the status line at the bottom of the folders column,
  // not as a banner pushing content around.
  showStatus(msg);
}

function guardTauri() {
  if (invoke) return true;
  showError(
    "Tauri API unavailable — the UI was loaded outside the app runtime. " +
      "Launch with: npm run tauri dev"
  );
  return false;
}

/* ---------- folders ---------- */

async function loadFolders() {
  if (!state.account) return;
  try {
    // Returns the cached list immediately; the server refresh arrives later
    // on the channel (or never, if offline).
    const cached = await invoke("list_folders", {
      account: state.account.name,
      onRefresh: folderRefreshChannel,
    });
    if (cached.length > 0) {
      state.folders = cached;
      renderFolders();
      // On first load open the folder the last message was read in; the
      // message itself is restored once its list has rendered.
      if (!state.folder) {
        const saved = loadLastSelection(localStorage);
        const target =
          (saved && state.folders.find((f) => f.name === saved.folder)) ||
          state.folders.find(
            (f) => f.specialUse === null && /inbox/i.test(f.name)
          ) ||
          state.folders[0];
        if (target) selectFolder(target.name);
      }
    }
  } catch (e) {
    setOnlineStatus(false);
    showStatus(String(e));
  }
}

// Receives (folders, online) when the background server refresh completes.
const folderRefreshChannel = new window.__TAURI__.core.Channel();
folderRefreshChannel.onmessage = ([folders, online]) => {
  if (folders.length > 0) {
    state.folders = folders;
    renderFolders();
  }
  setOnlineStatus(online);
};

function setOnlineStatus(online) {
  const wasOffline = state.online === false;
  state.online = online;
  const label = $("account-label");
  const base = state.account
    ? `${state.account.name} — ${state.account.email}`
    : "No accounts configured — use Accounts → Add account";
  label.textContent = online ? base : `${base}  ·  ~offline`;
  label.classList.toggle("offline", !online);

  if (online) {
    clearStatus();
    stopOfflinePolling();
    // Coming back online: refresh the visible folder (badges + list) —
    // with no interval poller, this is the recovery refresh.
    if (wasOffline && state.account) {
      loadFolders();
      if (state.folder) loadMessages();
    }
  } else if (!wasOffline) {
    // Just went offline: poll periodically so the indicator clears itself
    // as soon as connectivity returns, without user action.
    startOfflinePolling();
  }
}

let offlineTimer = null;

// Re-check connectivity against the IMAP server and apply the outcome.
// Uses the same channel flow as loadFolders: the (folders, online) verdict
// arrives on the channel — the invoke return value is only the cached list.
async function probeConnectivity() {
  if (!state.account) return;
  const channel = new window.__TAURI__.core.Channel();
  channel.onmessage = ([folders, online]) => {
    if (folders.length > 0) {
      state.folders = folders;
      renderFolders();
    }
    setOnlineStatus(online);
  };
  try {
    const cached = await invoke("list_folders", {
      account: state.account.name,
      onRefresh: channel,
    });
    if (cached.length > 0) {
      state.folders = cached;
      renderFolders();
    }
  } catch (_) {
    // Server unreachable; keep the offline tag.
    setOnlineStatus(false);
  }
}

function startOfflinePolling() {
  if (offlineTimer) return;
  offlineTimer = setInterval(async () => {
    if (!state.account || state.online) {
      stopOfflinePolling();
      return;
    }
    await probeConnectivity();
  }, 15000);
}

function stopOfflinePolling() {
  if (offlineTimer) {
    clearInterval(offlineTimer);
    offlineTimer = null;
  }
}

// Native connectivity events from the OS: instant, zero-cost. The webview
// fires these when the network interface state changes.
function initConnectivityEvents() {
  window.addEventListener("online", () => {
    // OS says the interface is up; verify against the IMAP server right away
    // (the interface can be up while the network is still unusable).
    probeConnectivity();
  });
  window.addEventListener("offline", () => setOnlineStatus(false));
}

/* ---------- status line (bottom of folders column) ---------- */

function showStatus(msg) {
  const el = $("status-line");
  el.textContent = msg;
  el.classList.remove("hidden");
}

function clearStatus() {
  const el = $("status-line");
  el.textContent = "";
  el.classList.add("hidden");
}

function renderFolders() {
  const ul = $("folder-list");
  ul.innerHTML = "";
  for (const f of state.folders) {
    const li = document.createElement("li");
    if (state.folder === f.name) li.classList.add("selected");

    const label = document.createElement("span");
    label.className = "folder-name";
    label.textContent = f.name;
    li.title = f.name;
    li.appendChild(label);

    if (f.unread > 0) {
      const badge = document.createElement("span");
      badge.className = "unread-badge";
      badge.textContent = f.unread > 99 ? "99+" : String(f.unread);
      li.appendChild(badge);
    }

    li.addEventListener("click", () => selectFolder(f.name));
    ul.appendChild(li);
  }
}

async function selectFolder(name) {
  state.folder = name;
  state.selectedUid = null;
  renderFolders();
  $("list-header").textContent = name;
  renderPreviewEmpty();
  await loadMessages();
}

/* ---------- message list ---------- */

function showListLoading(text) {
  const ul = $("message-list");
  ul.innerHTML = "";
  const li = document.createElement("li");
  li.className = "loading-row";
  const spinner = document.createElement("span");
  spinner.className = "spinner";
  const label = document.createElement("span");
  label.textContent = text;
  li.append(spinner, label);
  ul.appendChild(li);
}

function hideListLoading() {
  document
    .querySelectorAll("#message-list .loading-row")
    .forEach((el) => el.remove());
}

async function loadMessages() {
  if (!state.account || !state.folder) return;
  const folderAtStart = state.folder;
  showListLoading(`Loading ${state.folder}…`);
  state.messages = [];
  let first = true;

  const finish = () => {
    // A folder switch mid-load invalidates this load; the newer load's
    // finish() takes over (same guard as onBatch).
    if (state.folder !== folderAtStart) return;
    hideListLoading();
    renderMessages();
    restoreSelection();
  };

  try {
    // Channel must be created inside the try: a failure here previously
    // swallowed the rejection and left the spinner up forever.
    // Note: in the withGlobalTauri bundle, Channel is under core, not ipc.
    const onBatch = new window.__TAURI__.core.Channel();
    onBatch.onmessage = (batch) => {
      // Ignore batches from a stale folder load.
      if (state.folder !== folderAtStart) return;
      if (first) {
        first = false;
        hideListLoading();
      }
      // Merge, dedup by uid (cached batch + server batch overlap), and keep
      // the whole list sorted newest-first.
      const byUid = new Map(state.messages.map((m) => [m.uid, m]));
      for (const m of batch) byUid.set(m.uid, m);
      state.messages = [...byUid.values()].sort(
        (a, b) => (b.date || 0) - (a.date || 0)
      );
      renderMessages();
      // Restore as soon as the first (cached) batch renders — the backend
      // serves the cache before it streams from the server, so the last
      // selection appears immediately instead of after the full sync.
      // Idempotent: the guard in restoreSelection skips once selected.
      restoreSelection();
    };

    await invoke("list_messages", {
      account: state.account.name,
      folder: state.folder,
      onBatch,
    });
  } catch (e) {
    hideListLoading();
    setOnlineStatus(false);
    showError(String(e));
  } finally {
    // Always finish (render + restore the folder's last selection), even
    // when the server sync failed: the cached batch already made it to the
    // list via the channel, so the restore works offline too.
    finish();
  }
}

function renderMessages() {
  const ul = $("message-list");
  ul.innerHTML = "";
  for (const m of state.messages) {
    const li = document.createElement("li");
    if (m.seen) li.classList.add("read");
    else li.classList.add("unread");
    if (m.uid === state.selectedUid) li.classList.add("selected");
    li.dataset.uid = String(m.uid);

    const top = document.createElement("div");
    top.className = "msg-top";
    const from = document.createElement("span");
    from.className = "msg-from";
    from.textContent = m.from || "(unknown)";
    const date = document.createElement("span");
    date.className = "msg-date";
    date.textContent = fmtDate(m.date);
    top.append(from, date);

    const subj = document.createElement("div");
    subj.className = "msg-subject";
    subj.textContent = (m.has_attachment ? "📎 " : "") + m.subject;

    li.append(top, subj);
    if (m.snippet) {
      const snippet = document.createElement("div");
      snippet.className = "msg-snippet";
      snippet.textContent = m.snippet;
      li.appendChild(snippet);
    }

    li.addEventListener("click", () => selectMessage(m.uid, li));
    // Double-click: full-width message details modal.
    li.addEventListener("dblclick", () => openMessageModal(m.uid));
    li.addEventListener("contextmenu", (e) => {
      e.preventDefault();
      showContextMenu(e.clientX, e.clientY, m.uid);
    });
    ul.appendChild(li);
  }
}

// Reopen the last-read message of the current folder after a (re)load: the
// remembered UID for this account+folder, if it is still in the list. Runs
// on every completed load but no-ops once the message is already selected,
// so auto-refresh cycles don't re-fetch its body.
function restoreSelection() {
  const savedUid = loadFolderSelection(localStorage, state.account.name, state.folder);
  if (savedUid == null || state.selectedUid === savedUid) return;
  const msg = state.messages.find((m) => m.uid === savedUid);
  if (!msg) return;
  const li = document.querySelector(`#message-list li[data-uid="${savedUid}"]`);
  if (!li) return;
  selectMessage(savedUid, li);
}

/* ---------- mark as read / unread ---------- */

let markReadTimer = null;

async function setSeen(uid, seen) {
  const msg = state.messages.find((m) => m.uid === uid);
  const wasUnread = !!msg && !msg.seen;

  // Optimistic UI: flip the row and adjust the badge right away (the
  // debounce has already elapsed), without waiting for the IMAP round
  // trip. The server call below reconciles with the authoritative count.
  if (msg) msg.seen = seen;
  const f = state.folders.find((x) => x.name === state.folder);
  if (f && msg && wasUnread === seen) {
    // The flip actually changed the unread status.
    f.unread = seen ? Math.max(0, (f.unread || 0) - 1) : (f.unread || 0) + 1;
  }
  renderMessages();
  renderFolders();

  try {
    // Returns the folder's authoritative unread count from the server
    // (offline: applies locally + queues, returns a local estimate).
    const unread = await invoke("mark_message", {
      account: state.account.name,
      folder: state.folder,
      uid,
      seen,
    });
    const f2 = state.folders.find((x) => x.name === state.folder);
    if (f2) f2.unread = Number(unread) || 0;
    renderFolders();
  } catch (e) {
    // Genuine failure (account gone, store error…): roll the row back so
    // the UI matches reality until the next refresh.
    showError(String(e));
    const m2 = state.messages.find((m) => m.uid === uid);
    if (m2) m2.seen = !seen;
    renderMessages();
  }
}

async function selectMessage(uid, li) {
  state.selectedUid = uid;
  saveSelection(localStorage, state.account.name, state.folder, uid);
  document
    .querySelectorAll("#message-list li")
    .forEach((el) => el.classList.remove("selected"));
  li.classList.add("selected");

  // Keep selected for >=0.7s to mark as read. While offline the backend
  // applies the change locally and queues it for the server.
  clearTimeout(markReadTimer);
  const msg = state.messages.find((m) => m.uid === uid);
  if (msg && !msg.seen) {
    markReadTimer = setTimeout(() => setSeen(uid, true), 700);
  }

  try {
    const body = await invoke("fetch_message", {
      account: state.account.name,
      folder: state.folder,
      uid,
    });
    // The user may have clicked another message while this body was
    // in flight (e.g. a background restore racing a manual click).
    if (state.selectedUid !== uid) return;
    renderPreview(body);
  } catch (e) {
    showError(String(e));
  }
}

function showContextMenu(x, y, uid) {
  let menu = document.getElementById("msg-context-menu");
  if (!menu) {
    menu = document.createElement("div");
    menu.id = "msg-context-menu";
    document.body.appendChild(menu);
  }
  const msg = state.messages.find((m) => m.uid === uid);
  menu.innerHTML = "";

  const items = [
    {
      label: msg && msg.seen ? "Mark as unread" : "Mark as read",
      action: () => {
        clearTimeout(markReadTimer);
        setSeen(uid, !(msg && msg.seen));
      },
    },
    { label: "Move to…", action: () => openMoveDialog(uid) },
    { label: "Delete", danger: true, action: () => deleteMessage(uid) },
  ];
  for (const it of items) {
    const el = document.createElement("div");
    el.className = "menu-item" + (it.danger ? " danger" : "");
    el.textContent = it.label;
    el.addEventListener("click", () => {
      menu.remove();
      clearTimeout(markReadTimer);
      it.action();
    });
    menu.appendChild(el);
  }

  menu.style.left = x + "px";
  menu.style.top = y + "px";
  menu.classList.add("open");

  const close = (e) => {
    if (!menu.contains(e.target)) {
      menu.classList.remove("open");
      document.removeEventListener("click", close);
      document.removeEventListener("contextmenu", close);
    }
  };
  setTimeout(() => {
    document.addEventListener("click", close);
    document.addEventListener("contextmenu", close);
  }, 0);
}

/* ---------- move / delete ---------- */

let moveTargetUid = null;

function findTrashFolder() {
  // Prefer the server-declared special-use, then a name match.
  return (
    state.folders.find((f) => f.specialUse === "trash") ||
    state.folders.find((f) => /trash|deleted/i.test(f.name))
  );
}

/* ---------- confirm dialog ---------- */

// Show a modal confirmation; resolves true when the user clicks OK.
function confirmDialog(title, message, okLabel) {
  return new Promise((resolve) => {
    const dlg = $("confirm-dialog");
    $("confirm-title").textContent = title;
    $("confirm-message").textContent = message;
    $("confirm-ok").textContent = okLabel || "OK";
    const ok = () => {
      cleanup();
      resolve(true);
    };
    const cancel = () => {
      cleanup();
      resolve(false);
    };
    const cleanup = () => {
      dlg.removeEventListener("click", onClick);
      $("confirm-ok").removeEventListener("click", ok);
      $("confirm-cancel").removeEventListener("click", cancel);
      dlg.close();
    };
    // Clicking the backdrop cancels too.
    const onClick = (e) => {
      if (e.target === dlg) cancel();
    };
    dlg.addEventListener("click", onClick);
    $("confirm-ok").addEventListener("click", ok);
    $("confirm-cancel").addEventListener("click", cancel);
    dlg.showModal();
  });
}

async function deleteMessage(uid) {
  const trash = findTrashFolder();
  // Without a Trash folder the server delete is permanent and cannot be
  // undone — ask first.
  if (!trash) {
    const proceed = await confirmDialog(
      "Delete permanently?",
      "No Trash folder exists for this account — the message will be " +
        "permanently deleted from the server and cannot be recovered.",
      "Delete"
    );
    if (!proceed) return;
  }
  // 1. Remove from the UI + local cache right away (fast local call).
  try {
    await invoke("delete_message_local", {
      account: state.account.name,
      folder: state.folder,
      uid,
    });
  } catch (e) {
    showError(String(e));
    return;
  }

  // Adjust the unread badge locally (corrected by loadFolders below).
  const f = state.folders.find((x) => x.name === state.folder);
  const msg = state.messages.find((m) => m.uid === uid);
  if (f && msg && !msg.seen) f.unread = Math.max(0, (f.unread || 0) - 1);

  state.messages = state.messages.filter((m) => m.uid !== uid);
  renderMessages();
  if (loadFolderSelection(localStorage, state.account.name, state.folder) === uid) {
    clearFolderSelection(localStorage, state.account.name, state.folder);
  }
  if (state.selectedUid === uid) {
    state.selectedUid = null;
    renderPreviewEmpty();
  }
  // If the full-width modal was showing this message, close it.
  if (modalUid === uid) closeMessageModal();
  // Badges come from the server (the local estimate above is corrected here).
  loadFolders();

  // 2. Server delete in the background — the UI is already updated. If it
  // fails, the message reappears on the next folder sync.
  invoke("delete_message_server", {
    account: state.account.name,
    folder: state.folder,
    uid,
    trashFolder: trash ? trash.name : null,
  }).catch((e) => showError("Server delete failed: " + e));
}

function openMoveDialog(uid) {
  moveTargetUid = uid;
  const ul = $("move-folder-list");
  ul.innerHTML = "";
  for (const f of state.folders) {
    if (f.name === state.folder) continue;
    const li = document.createElement("li");
    li.textContent = f.name;
    li.addEventListener("click", async () => {
      $("move-dialog").close();
      await moveMessage(moveTargetUid, f.name);
    });
    ul.appendChild(li);
  }
  $("move-dialog").showModal();
}

async function moveMessage(uid, destFolder) {
  try {
    await invoke("move_message", {
      account: state.account.name,
      folder: state.folder,
      uid,
      destFolder,
    });
    state.messages = state.messages.filter((m) => m.uid !== uid);
    renderMessages();
    if (loadFolderSelection(localStorage, state.account.name, state.folder) === uid) {
      clearFolderSelection(localStorage, state.account.name, state.folder);
    }
    // Badges come from the server, not a local recompute over a possibly-
    // incomplete message list.
    loadFolders();
    if (state.selectedUid === uid) {
      state.selectedUid = null;
      renderPreviewEmpty();
    }
    if (modalUid === uid) closeMessageModal();
  } catch (e) {
    showError(String(e));
  }
}

/* ---------- preview ---------- */

// The message-details view is shared between the three-column preview and
// the full-width message modal: both render subject / meta / body / attachments
// through the same functions, so a future change to the message view updates
// both places. A "view" is just the set of elements to render into.
const columnView = {
  empty: $("preview-empty"),
  content: $("preview-content"),
  subject: $("preview-subject"),
  meta: $("preview-meta"),
  frame: $("preview-frame"),
  attachments: $("attachment-list"),
};
const modalView = {
  subject: $("modal-subject"),
  meta: $("modal-meta"),
  frame: $("modal-frame"),
  attachments: $("modal-attachment-list"),
};

async function renderViewAttachments(view, uid) {
  const box = view.attachments;
  box.innerHTML = "";
  box.classList.add("hidden");
  let atts = [];
  try {
    atts = await invoke("list_attachments", {
      account: state.account.name,
      folder: state.folder,
      uid,
    });
  } catch (_) {
    return; // attachment listing is best-effort
  }
  if (!atts.length) return;

  const header = document.createElement("div");
  header.className = "att-header";
  header.textContent = `${atts.length} attachment${atts.length > 1 ? "s" : ""}`;
  box.appendChild(header);

  for (const a of atts) {
    const item = document.createElement("div");
    item.className = "att-item";
    const name = document.createElement("span");
    name.className = "att-name";
    name.textContent = a.filename;
    const size = document.createElement("span");
    size.className = "att-size";
    size.textContent = fmtSize(a.size);
    const btn = document.createElement("button");
    btn.className = "att-save";
    btn.textContent = "Save as…";
    btn.addEventListener("click", () => saveAttachment(uid, a));
    item.append(name, size, btn);
    box.appendChild(item);
  }
  box.classList.remove("hidden");
}

function renderPreviewEmpty() {
  $("preview-empty").classList.remove("hidden");
  $("preview-content").classList.add("hidden");
}

function renderPreview(body) {
  const summary = state.messages.find((m) => m.uid === body.uid) || {};
  $("preview-empty").classList.add("hidden");
  $("preview-content").classList.remove("hidden");
  renderViewSubject(columnView, summary);
  renderViewMeta(columnView, summary);
  renderViewBody(columnView, body);
  renderViewAttachments(columnView, body.uid);
}

// Full-width message details modal (opened by double-clicking a row). Uses
// the same view functions as the three-column preview.
let modalUid = null;

async function openMessageModal(uid) {
  modalUid = uid;
  const summary = state.messages.find((m) => m.uid === uid) || {};
  try {
    const body = await invoke("fetch_message", {
      account: state.account.name,
      folder: state.folder,
      uid,
    });
    renderViewSubject(modalView, summary);
    renderViewMeta(modalView, summary);
    renderViewBody(modalView, body);
    renderViewAttachments(modalView, uid);
    $("message-modal").showModal();
    // Double-clicking is an explicit read: mark immediately.
    if (summary.seen === false) setSeen(uid, true);
  } catch (e) {
    showError(String(e));
  }
}

function closeMessageModal() {
  const dlg = $("message-modal");
  if (dlg.open) dlg.close();
}

async function saveAttachment(uid, att) {
  const path = await window.__TAURI__.dialog.save({
    defaultPath: att.filename,
  });
  if (!path) return;
  try {
    await invoke("save_attachment", {
      account: state.account.name,
      folder: state.folder,
      uid,
      partId: att.part_id,
      destPath: path,
    });
  } catch (e) {
    showError(String(e));
  }
}

/* ---------- compose ---------- */

function openCompose(replyTo) {
  const dlg = $("compose-dialog");
  $("compose-error").classList.add("hidden");
  if (replyTo) {
    $("compose-to").value = replyTo.from || "";
    $("compose-subject").value = replyTo.subject.startsWith("Re:")
      ? replyTo.subject
      : "Re: " + replyTo.subject;
    $("compose-body").value = `\n\n----- Original message -----\nFrom: ${replyTo.from}\nSubject: ${replyTo.subject}\n`;
  } else {
    $("compose-to").value = "";
    $("compose-subject").value = "";
    $("compose-body").value = "";
  }
  dlg.showModal();
}

let sendStatusTimer = null;

// Transient send progress in the toolbar: "Sending…", then "Sent ✓" or
// an error. Auto-clears after a few seconds.
function showSendStatus(msg, isError) {
  const el = $("send-status");
  if (!el) return;
  el.textContent = msg;
  el.classList.toggle("error", !!isError);
  el.classList.remove("hidden");
  clearTimeout(sendStatusTimer);
  sendStatusTimer = setTimeout(() => {
    el.classList.add("hidden");
    el.textContent = "";
  }, isError ? 8000 : 4000);
}

async function sendCompose(e) {
  e.preventDefault();
  const to = $("compose-to").value
    .split(",")
    .map((s) => s.trim())
    .filter(Boolean);
  const subject = $("compose-subject").value;
  const body = $("compose-body").value;

  // Close the modal right away — the SMTP round trip takes seconds, and
  // the user should not stare at a frozen dialog.
  $("compose-dialog").close();
  showSendStatus("Sending…");
  try {
    await invoke("send_email", {
      args: {
        account: state.account.name,
        to,
        subject,
        body,
      },
    });
    showSendStatus("Sent ✓");
  } catch (err) {
    showSendStatus("Send failed: " + err, true);
    // Reopen with the content intact so nothing typed is lost on a
    // transient failure.
    $("compose-to").value = to.join(", ");
    $("compose-subject").value = subject;
    $("compose-body").value = body;
    $("compose-dialog").showModal();
  }
}

/* ---------- accounts ---------- */

function renderAccountsMenu() {
  const menu = $("accounts-menu");
  menu.innerHTML = "";

  for (const acc of state.accounts) {
    const item = document.createElement("div");
    item.className = "menu-item" + (state.account && state.account.name === acc.name ? " active" : "");
    const label = document.createElement("span");
    label.textContent = `${acc.name} (${acc.email})`;
    item.appendChild(label);

    const del = document.createElement("button");
    del.className = "del";
    del.textContent = "Delete";
    del.addEventListener("click", async (e) => {
      e.stopPropagation();
      if (!confirm(`Delete account '${acc.name}'?`)) return;
      try {
        await invoke("delete_account", { account: acc.name });
        await refreshAccounts(null);
      } catch (err) {
        showError(String(err));
      }
    });
    item.appendChild(del);

    item.addEventListener("click", async () => {
      menu.classList.add("hidden");
      await switchAccount(acc.name);
    });
    menu.appendChild(item);
  }

  menu.appendChild(Object.assign(document.createElement("div"), { className: "menu-sep" }));

  const add = document.createElement("div");
  add.className = "menu-item";
  add.textContent = "＋ Add account…";
  add.addEventListener("click", () => {
    menu.classList.add("hidden");
    openAccountDialog();
  });
  menu.appendChild(add);
}

async function refreshAccounts(preferredName) {
  state.accounts = await invoke("get_accounts");
  // Reopen the account the last message was read in, falling back to the
  // existing preference chain.
  const saved = loadLastSelection(localStorage);
  const target =
    (saved && state.accounts.find((a) => a.name === saved.account)) ||
    (preferredName && state.accounts.find((a) => a.name === preferredName)) ||
    (state.account && state.accounts.find((a) => a.name === state.account.name)) ||
    state.accounts[0] ||
    null;
  state.account = target;
  renderAccountsMenu();
  updateAccountLabel();
  renderEmptyStates();
  await resetMailState();
}

function updateAccountLabel() {
  $("account-label").textContent = state.account
    ? `${state.account.name} — ${state.account.email}`
    : "No accounts configured — use Accounts → Add account";
}

function renderEmptyStates() {
  const hasAccounts = state.accounts.length > 0;
  $("no-accounts").classList.toggle("hidden", hasAccounts);
  if (!hasAccounts) {
    $("preview-empty").classList.add("hidden");
  }
}

async function switchAccount(name) {
  const acc = state.accounts.find((a) => a.name === name);
  if (!acc) return;
  state.account = acc;
  renderAccountsMenu();
  updateAccountLabel();
  renderEmptyStates();
  await resetMailState();
}

async function resetMailState() {
  state.folder = null;
  state.folders = [];
  state.messages = [];
  state.selectedUid = null;
  $("folder-list").innerHTML = "";
  $("message-list").innerHTML = "";
  $("list-header").textContent = "Messages";
  renderPreviewEmpty();
  // Only connect when there is a real, fully-configured account.
  if (state.account && state.account.hasPassword) await loadFolders();
}

/* ---------- add-account dialog ---------- */

function openAccountDialog() {
  $("acc-error").classList.add("hidden");
  $("account-form").reset();
  $("acc-imap-port").value = 993;
  $("acc-smtp-port").value = 465;
  $("acc-starttls").checked = false;
  $("account-dialog").showModal();
}

function accountFormError(msg) {
  const box = $("acc-error");
  box.textContent = msg;
  box.classList.remove("hidden");
}

async function saveAccount(e) {
  e.preventDefault();
  const btn = $("acc-save");
  btn.disabled = true;
  btn.textContent = "Testing connection…";
  const name = $("acc-name").value.trim();
  try {
    await invoke("add_account", {
      account: {
        name,
        email: $("acc-email").value.trim(),
        imapHost: $("acc-imap-host").value.trim(),
        imapPort: Number($("acc-imap-port").value),
        username: $("acc-username").value.trim(),
        password: $("acc-password").value,
        smtpHost: $("acc-smtp-host").value.trim(),
        smtpPort: Number($("acc-smtp-port").value),
        smtpUsername: $("acc-smtp-username").value.trim() || null,
        smtpPassword: $("acc-smtp-password").value || null,
        smtpStarttls: $("acc-starttls").checked,
      },
    });
    $("account-dialog").close();
    await refreshAccounts(name);
  } catch (err) {
    accountFormError(String(err));
  } finally {
    btn.disabled = false;
    btn.textContent = "Test & Save";
  }
}

function toggleSidebar() {
  const folders = $("folders");
  const narrow = window.matchMedia("(max-width: 899px)").matches;

  if (narrow) {
    // Narrow mode: folders hidden by default. Toggling shows folders and
    // hides the preview (folders + messages only).
    state.sidebarVisible = !state.sidebarVisible;
    folders.classList.toggle("hidden-col", !state.sidebarVisible);
    document.body.classList.toggle("narrow-folders", state.sidebarVisible);
  } else {
    state.sidebarVisible = !state.sidebarVisible;
    folders.classList.toggle("hidden-col", !state.sidebarVisible);
    document.body.classList.remove("narrow-folders");
  }
}

// Keep column visibility consistent when the window is resized.
function syncResponsiveColumns() {
  const narrow = window.matchMedia("(max-width: 899px)").matches;
  if (!narrow) {
    document.body.classList.remove("narrow-folders");
    $("folders").classList.toggle("hidden-col", !state.sidebarVisible);
  } else {
    // Entering narrow mode: hide folders unless the user explicitly
    // toggled them on; preview is hidden by CSS while folders are shown.
    $("folders").classList.toggle("hidden-col", !state.sidebarVisible);
    document.body.classList.toggle("narrow-folders", state.sidebarVisible);
  }
}

/* ---------- column widths ---------- */

const WIDTH_KEY = "sufi-col-widths";
const DEFAULT_WIDTHS = { folders: 20, messages: 30 }; // preview gets the rest

function loadWidths() {
  try {
    const raw = JSON.parse(localStorage.getItem(WIDTH_KEY));
    if (raw && typeof raw.folders === "number" && typeof raw.messages === "number") {
      return raw;
    }
  } catch (_) {}
  return { ...DEFAULT_WIDTHS };
}

function saveWidths(w) {
  localStorage.setItem(WIDTH_KEY, JSON.stringify(w));
}

function applyWidths() {
  const w = loadWidths();
  $("folders").style.width = w.folders + "%";
  $("message-list-pane").style.width = w.messages + "%";
  // Preview takes the remaining space via flex:1.
}

function initResizers() {
  const main = $("main");

  for (const handle of document.querySelectorAll(".col-resizer")) {
    handle.addEventListener("mousedown", (e) => {
      e.preventDefault();
      const target = handle.dataset.resize; // "folders" | "messages"
      const el = target === "folders" ? $("folders") : $("message-list-pane");
      if (el.classList.contains("hidden-col")) return;

      handle.classList.add("dragging");
      document.body.style.cursor = "col-resize";
      document.body.style.userSelect = "none";

      const startX = e.clientX;
      const startW = el.getBoundingClientRect().width;
      const totalW = main.getBoundingClientRect().width;

      const onMove = (ev) => {
        let pct = ((startW + ev.clientX - startX) / totalW) * 100;
        // Clamp: keep every column usable.
        const minPct = target === "folders" ? 4 : 10;
        const other = target === "folders"
          ? loadWidths().messages
          : loadWidths().folders;
        pct = Math.max(minPct, Math.min(pct, 90 - other));
        el.style.width = pct.toFixed(2) + "%";
      };

      const onUp = () => {
        document.removeEventListener("mousemove", onMove);
        document.removeEventListener("mouseup", onUp);
        handle.classList.remove("dragging");
        document.body.style.cursor = "";
        document.body.style.userSelect = "";

        const w = loadWidths();
        w[target] = (el.getBoundingClientRect().width / main.getBoundingClientRect().width) * 100;
        saveWidths(w);
      };

      document.addEventListener("mousemove", onMove);
      document.addEventListener("mouseup", onUp);
    });
  }

  applyWidths();
}

/* ---------- font size ---------- */

const FONT_KEY = "sufi-base-font";

function applyFontSize(px) {
  document.documentElement.style.setProperty("--base-font", px + "px");
  localStorage.setItem(FONT_KEY, String(px));
}

function initFontSize() {
  const saved = Number(localStorage.getItem(FONT_KEY)) || 14;
  applyFontSize(saved);
  $("font-inc").addEventListener("click", () => {
    const cur = Number(localStorage.getItem(FONT_KEY)) || 14;
    applyFontSize(Math.min(cur + 1, 22));
  });
  $("font-dec").addEventListener("click", () => {
    const cur = Number(localStorage.getItem(FONT_KEY)) || 14;
    applyFontSize(Math.max(cur - 1, 10));
  });
}

// Refresh folders + the visible message list. Triggered by the manual
// Refresh button and by `mail-refresh` events from the background IDLE
// watchers (instant new-mail detection). The offline flag guards against
// spamming errors while disconnected (the offline poller handles recovery).
function refreshMail() {
  if (!state.online || !state.account) return;
  loadFolders();
  if (state.folder) loadMessages();
}

function initMailRefreshListener() {
  if (!window.__TAURI__ || !window.__TAURI__.event) return;
  window.__TAURI__.event.listen("mail-refresh", () => {
    if (!state.online || !state.account) return;
    loadFolders();
    if (state.folder) loadMessages();
  });
}

function init() {
  if (!guardTauri()) return;

  initFontSize();
  initResizers();
  initConnectivityEvents();
  syncResponsiveColumns();
  window.addEventListener("resize", syncResponsiveColumns);

  $("toggle-sidebar").addEventListener("click", toggleSidebar);
  // Full manual refresh: folders (badges) + the visible message list.
  $("refresh-btn").addEventListener("click", refreshMail);

  initMailRefreshListener();
  $("compose-btn").addEventListener("click", () => openCompose(null));
  document.getElementById("compose-form").addEventListener("submit", sendCompose);
  $("compose-cancel").addEventListener("click", () => $("compose-dialog").close());
  $("reply-btn").addEventListener("click", () => {
    const m = state.messages.find((x) => x.uid === state.selectedUid);
    if (m) openCompose(m);
  });
  $("delete-btn").addEventListener("click", () => {
    if (state.selectedUid != null) deleteMessage(state.selectedUid);
  });
  $("move-btn").addEventListener("click", () => {
    if (state.selectedUid != null) openMoveDialog(state.selectedUid);
  });
  $("move-cancel").addEventListener("click", () => $("move-dialog").close());

  // Full-width message modal.
  $("modal-close").addEventListener("click", closeMessageModal);
  $("modal-reply").addEventListener("click", () => {
    const m = state.messages.find((x) => x.uid === modalUid);
    if (m) openCompose(m);
  });
  $("modal-move").addEventListener("click", () => {
    if (modalUid != null) openMoveDialog(modalUid);
  });
  $("modal-delete").addEventListener("click", () => {
    if (modalUid != null) deleteMessage(modalUid);
  });

  // Accounts menu
  $("accounts-btn").addEventListener("click", (e) => {
    e.stopPropagation();
    renderAccountsMenu();
    $("accounts-menu").classList.toggle("hidden");
  });
  document.addEventListener("click", (e) => {
    if (!e.target.closest("#accounts-menu-wrap")) {
      $("accounts-menu").classList.add("hidden");
    }
  });
  $("account-form").addEventListener("submit", saveAccount);
  $("acc-cancel").addEventListener("click", () => $("account-dialog").close());
  $("empty-add-btn").addEventListener("click", openAccountDialog);
  // Keep SMTP port sensible when toggling STARTTLS.
  $("acc-starttls").addEventListener("change", (e) => {
    $("acc-smtp-port").value = e.target.checked ? 587 : 465;
  });

  document.addEventListener("keydown", (e) => {
    if (e.ctrlKey && e.key === "b") toggleSidebar();
    if (e.ctrlKey && e.key === "n") openCompose(null);
  });

  refreshAccounts(null)
    .then(() => {
      // First run with no accounts: open the add-account dialog right away.
      if (state.accounts.length === 0) openAccountDialog();
    })
    .catch((err) => showError(String(err)));
}

init();
