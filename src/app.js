import {
  loadLastSelection,
  loadFolderSelection,
  saveSelection,
  clearFolderSelection,
  loadSidebarVisible,
  saveSidebarVisible,
} from "./lib/selection.js";
import { fmtDate, fmtSize, escapeHtml, withEmailCsp } from "./lib/format.js";
import {
  renderViewSubject,
  renderViewMeta,
  renderViewBody,
  rewriteCidImages,
  planAttachmentDisplay,
  cidDataMap,
  hasRemoteImages,
  placeholderRemoteImages,
  frameDisplayHeight,
  frameAvailableHeight,
  frameContentHeight,
  EMAIL_FRAME_SANDBOX,
} from "./lib/mailview.js";
import { omarchyThemeToCssVars, applyThemeVars, THEME_VAR_NAMES } from "./lib/theme.js";
import { shouldHandleFrameLoad, emailLinkUrlFromMessage, hrefFromEmailClick } from "./lib/links.js";
import {
  remoteImagesAllowed,
  buildRemoteImageBar,
} from "./lib/remote-images.js";

const invoke = window.__TAURI__ ? window.__TAURI__.core.invoke : null;

const state = {
  accounts: [],
  account: null,
  folders: [],
  folder: null,
  messages: [],
  selectedUid: null,
  sidebarVisible: loadSidebarVisible(localStorage),
  online: true,
  // Full-text search over the local cache; null = not searching, an array
  // = search hits shown in place of the folder's message list.
  searchResults: null,
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

// Applies a fresh folder list from a completed server refresh. Channels in
// Tauri v2 are tied to a per-invoke Rust-side counter, so a JS Channel must
// be created per invoke (see loadFolders/probeConnectivity) — a shared
// module-level channel collides on the message index and drops every
// refresh after the first.
function applyFolderRefresh(folders, online) {
  setOnlineStatus(online);
  if (folders.length > 0) {
    state.folders = folders;
    renderFolders();
  }
}

async function loadFolders() {
  if (!state.account) return;
  // Fresh channel per invoke: a shared channel drops refreshes after the
  // first (per-invoke Rust-side message counters collide on the JS side).
  const channel = new window.__TAURI__.core.Channel();
  channel.onmessage = ([folders, online]) => applyFolderRefresh(folders, online);
  try {
    // Returns the cached list immediately; the server refresh arrives later
    // on the channel (or never, if offline).
    const cached = await invoke("list_folders", {
      account: state.account.name,
      onRefresh: channel,
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

function setOnlineStatus(online) {
  const wasOffline = state.online === false;
  state.online = online;
  updateAccountsButton();

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
  channel.onmessage = ([folders, online]) => applyFolderRefresh(folders, online);
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
  // Opening a folder leaves search mode (results were spanning all folders).
  if (state.searchResults !== null) exitSearch();
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
  // Search results replace the folder's message list; don't clobber them
  // on a background refresh while the user is looking at search hits.
  if (state.searchResults !== null) return;
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

function stripSubjectPrefixes(subject) {
  let s = String(subject || "");
  for (let i = 0; i < 5; i++) {
    const m = /^\s*(re|fwd|fw|aw|sv)\s*:\s*/i.exec(s);
    if (!m) break;
    s = s.slice(m[0].length);
  }
  return s.trim();
}

// Group messages into conversations by thread_id, each thread sorted newest
// first; threads sorted by their newest message.
function groupThreads(messages) {
  const map = new Map();
  for (const m of messages) {
    const tid = m.thread_id || "unt:" + m.uid;
    if (!map.has(tid)) map.set(tid, []);
    map.get(tid).push(m);
  }
  const threads = [...map.values()];
  for (const t of threads) t.sort((a, b) => (b.date || 0) - (a.date || 0));
  threads.sort((a, b) => (b[0].date || 0) - (a[0].date || 0));
  return threads;
}

/* ---------- search ---------- */

let searchTimer = null;

// Toggle the toolbar search box: clicking the icon opens/focuses it, a
// second click (or Esc / ✕) closes it and restores the folder list.
function toggleSearch() {
  const box = $("search-box");
  if (box.classList.contains("active")) {
    exitSearch();
  } else {
    box.classList.add("active");
    $("search-input").value = "";
    $("search-input").focus();
  }
}

// Close the search box and drop the results (idempotent).
function exitSearch() {
  const hadResults = state.searchResults !== null;
  clearTimeout(searchTimer);
  state.searchResults = null;
  $("search-input").value = "";
  $("search-box").classList.remove("active");
  if (hadResults) renderMessages();
}

function onSearchInput() {
  clearTimeout(searchTimer);
  const q = $("search-input").value;
  searchTimer = setTimeout(() => runSearch(q), 250);
}

// Run a search over every cached message in every folder (backend searches
// all accounts' caches). Empty query leaves search mode.
async function runSearch(query) {
  const q = query.trim();
  if (!q) {
    state.searchResults = null;
    renderMessages();
    return;
  }
  state.searchResults = []; // show "Searching…"
  renderSearchResults();
  try {
    const results = await invoke("search_messages", { query: q });
    if ($("search-input").value.trim() !== q) return; // stale keystroke
    state.searchResults = results;
    renderSearchResults();
  } catch (e) {
    state.searchResults = null;
    showError(String(e));
  }
}

// Render search hits in the message list pane: folder badge + subject,
// sender, date and snippet. Clicking a hit opens that message.
function renderSearchResults() {
  const ul = $("message-list");
  const results = state.searchResults;
  ul.innerHTML = "";
  if (!results || results.length === 0) {
    const li = document.createElement("li");
    li.className = "search-empty";
    li.textContent = results ? "No messages found" : "Searching…";
    ul.appendChild(li);
    return;
  }
  for (const r of results) {
    const li = document.createElement("li");
    li.className = (r.seen ? "read" : "unread") + " search-result";

    const top = document.createElement("div");
    top.className = "msg-top";
    const from = document.createElement("span");
    from.className = "msg-from";
    from.textContent = r.from || "(unknown)";
    const folder = document.createElement("span");
    folder.className = "msg-folder";
    folder.textContent = r.folder;
    const date = document.createElement("span");
    date.className = "msg-date";
    date.textContent = fmtDate(r.date);
    top.append(from, folder, date);

    const subj = document.createElement("div");
    subj.className = "msg-subject";
    subj.textContent = stripSubjectPrefixes(r.subject) || "(no subject)";
    li.append(top, subj);
    if (r.snippet) {
      const snippet = document.createElement("div");
      snippet.className = "msg-snippet";
      snippet.textContent = r.snippet;
      li.appendChild(snippet);
    }

    li.addEventListener("click", () => openSearchResult(r));
    ul.appendChild(li);
  }
}

// Open a search hit: switch to its account + folder and show the message
// (fetched directly, so hits older than the folder's visible window open
// too), then mark it read.
async function openSearchResult(r) {
  exitSearch();
  if (state.account && state.account.name !== r.account) {
    await switchAccount(r.account);
  }
  // Remember the hit as this folder's selection BEFORE loading it, so the
  // folder restore after selectFolder picks it up (when it is inside the
  // visible window).
  saveSelection(localStorage, r.account, r.folder, r.uid);
  if (state.folder !== r.folder) {
    await selectFolder(r.folder);
  }
  state.selectedUid = r.uid;
  saveSelection(localStorage, state.account.name, r.folder, r.uid);
  $("preview-empty").classList.add("hidden");
  $("preview-content").classList.remove("hidden");
  const synthetic = {
    uid: r.uid,
    subject: r.subject,
    from: r.from,
    date: r.date,
    seen: r.seen,
    has_attachment: false,
    snippet: r.snippet,
    message_id: "",
    references: "",
    in_reply_to: "",
    thread_id: "search:" + r.uid,
  };
  await renderThread(
    $("preview-subject"),
    $("preview-meta"),
    $("preview-thread"),
    [synthetic]
  );
  if (!r.seen) setSeen(r.uid, true);
}

function renderMessages() {
  // While a search is active the list shows hits from every folder instead
  // of the current folder's messages.
  if (state.searchResults !== null) {
    renderSearchResults();
    return;
  }
  const ul = $("message-list");
  ul.innerHTML = "";
  for (const thread of groupThreads(state.messages)) {
    const newest = thread[0];
    const unread = thread.filter((m) => !m.seen).length;
    const li = document.createElement("li");
    if (unread > 0) li.classList.add("unread");
    else li.classList.add("read");
    if (newest.uid === state.selectedUid) li.classList.add("selected");
    li.dataset.threadId = newest.thread_id || "";

    const top = document.createElement("div");
    top.className = "msg-top";
    const from = document.createElement("span");
    from.className = "msg-from";
    from.textContent = newest.from || "(unknown)";
    const date = document.createElement("span");
    date.className = "msg-date";
    date.textContent = fmtDate(newest.date);
    top.append(from, date);

    const subj = document.createElement("div");
    subj.className = "msg-subject";
    const subject = stripSubjectPrefixes(newest.subject);
    subj.textContent =
      (newest.has_attachment ? "📎 " : "") +
      (subject || "(no subject)") +
      (thread.length > 1 ? ` (${thread.length})` : "");
    if (unread > 0) {
      const badge = document.createElement("span");
      badge.className = "unread-badge";
      badge.textContent = unread > 99 ? "99+" : String(unread);
      subj.appendChild(badge);
    }

    li.append(top, subj);
    if (newest.snippet) {
      const snippet = document.createElement("div");
      snippet.className = "msg-snippet";
      snippet.textContent = newest.snippet;
      li.appendChild(snippet);
    }

    li.addEventListener("click", () => selectThread(thread, li));
    li.addEventListener("dblclick", () => openThreadModal(thread));
    li.addEventListener("contextmenu", (e) => {
      e.preventDefault();
      showContextMenu(e.clientX, e.clientY, newest.uid);
    });
    ul.appendChild(li);
  }
}

// Reopen the last-read thread of the current folder after a (re)load: the
// remembered message UID identifies a thread, which is re-opened. No-ops
// once that thread is already selected, so auto-refresh cycles don't
// re-fetch.
function restoreSelection() {
  const savedUid = loadFolderSelection(localStorage, state.account.name, state.folder);
  if (savedUid == null || state.selectedUid === savedUid) return;
  const msg = state.messages.find((m) => m.uid === savedUid);
  if (!msg) return;
  const thread = groupThreads(state.messages).find((t) =>
    t.some((m) => m.uid === savedUid)
  );
  if (!thread) return;
  const tid = thread[0].thread_id || "";
  const li = document.querySelector(
    `#message-list li[data-thread-id="${CSS.escape(tid)}"]`
  );
  selectThread(thread, li);
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

// Open a conversation in the details column: renders the full discussion
// (newest first) and marks every unread message in it as read.
async function selectThread(thread, li) {
  if (!thread || !thread.length) return;
  const newest = thread[0];
  state.selectedUid = newest.uid;
  saveSelection(localStorage, state.account.name, state.folder, newest.uid);
  document
    .querySelectorAll("#message-list li")
    .forEach((el) => el.classList.remove("selected"));
  if (li) li.classList.add("selected");

  $("preview-empty").classList.add("hidden");
  $("preview-content").classList.remove("hidden");
  await renderThread(
    $("preview-subject"),
    $("preview-meta"),
    $("preview-thread"),
    thread
  );

  // Opening a thread is an explicit read for all of it.
  for (const m of thread) {
    if (!m.seen) setSeen(m.uid, true);
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
  const moved = state.messages.find((m) => m.uid === uid);
  // 1. Remove from the UI + shield the cached row right away (fast local
  // call), mirroring delete; the server move runs in the background.
  try {
    await invoke("move_message_local", {
      account: state.account.name,
      folder: state.folder,
      uid,
    });
  } catch (e) {
    showError(String(e));
    return;
  }
  state.messages = state.messages.filter((m) => m.uid !== uid);
  renderMessages();
  if (loadFolderSelection(localStorage, state.account.name, state.folder) === uid) {
    clearFolderSelection(localStorage, state.account.name, state.folder);
  }
  // Optimistic badge: an unread message leaves the source folder and
  // arrives unread in the destination. The loadFolders() server refresh
  // below reconciles both counts, but this keeps the sidebar correct the
  // moment the move finishes instead of seconds later.
  if (moved && !moved.seen) {
    const src = state.folders.find((x) => x.name === state.folder);
    if (src) src.unread = Math.max(0, (src.unread || 0) - 1);
    const dst = state.folders.find((x) => x.name === destFolder);
    if (dst) dst.unread = (dst.unread || 0) + 1;
    renderFolders();
  }
  // Badges come from the server, not a local recompute over a possibly-
  // incomplete message list.
  loadFolders();
  if (state.selectedUid === uid) {
    state.selectedUid = null;
    renderPreviewEmpty();
  }
  if (modalUid === uid) closeMessageModal();
  // 2. Server move in the background; on success the row lands in the
  // destination's cache and the UI refreshes. On failure the message
  // reappears on the next folder sync.
  invoke("move_message_server", {
    account: state.account.name,
    folder: state.folder,
    uid,
    destFolder,
  }).catch((e) => showError("Move failed: " + e));
}

/* ---------- preview ---------- */

/* ---------- preview (thread / discussion view) ---------- */

// Renders a conversation (newest message first) into the given elements:
// subject + meta as the thread header, then one block per message (sender,
// date, sandboxed body, attachments). Used by both the three-column preview
// and the full-width modal, so a future change to the message view updates
// both places. Each message body goes through the shared renderViewBody
// (same CSP + link handling as before).

// Attachments for one message inside a thread block. Image thumbnails
// (already filtered by planAttachmentDisplay — embedded parts are rendered
// in the body) at the top, then the full list with save buttons.
// `dataByPart` maps part_id -> base64 from the single batched
// get_attachments_data call made by renderThread.
function renderBlockAttachments(box, uid, atts, thumbParts, dataByPart) {
  if (!atts.length) return;

  if (thumbParts.length) {
    const row = document.createElement("div");
    row.className = "att-thumbs";
    for (const a of thumbParts) {
      const b64 = dataByPart.get(a.part_id);
      if (!b64) continue;
      const img = document.createElement("img");
      img.className = "att-thumb";
      img.src = `data:${a.contentType};base64,${b64}`;
      img.title = a.filename;
      img.alt = a.filename;
      img.addEventListener("click", () => saveAttachment(uid, a));
      row.appendChild(img);
    }
    if (row.childElementCount) box.appendChild(row);
  }

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
}

// The opt-in banner shown above a message body whose remote images are
// blocked: explains WHY images are hidden, with per-message and global
// ways to load them. Either click re-renders the frame with the relaxed
// CSP and the real URLs restored (no placeholders). The consent logic and
// bar DOM live in ./lib/remote-images.js (unit-tested); this is the app
// glue: the re-render itself.
function renderRemoteImageBar(block, frame, html, uid) {
  buildRemoteImageBar(block, frame, uid, {
    storage: localStorage,
    accountName: state.account.name,
    folder: state.folder,
    render: () => {
      // The srcdoc swap creates a new message document: let the load
      // handler attach a fresh ResizeObserver to it, so remote images
      // that decode late re-measure the frame.
      frame.__sufiObserved = false;
      renderViewBody({ frame }, { html }, { remoteImages: true }).catch(() => {});
    },
  });
}

async function renderThread(subjectEl, metaEl, container, thread) {
  container.innerHTML = "";
  const newest = thread[0];
  subjectEl.textContent = stripSubjectPrefixes(newest.subject) || "(no subject)";
  const people = new Set(
    thread.map((m) => (m.from || "").split(" <")[0].trim()).filter(Boolean)
  );
  metaEl.textContent =
    `${thread.length} message${thread.length > 1 ? "s" : ""}` +
    (people.size > 1 ? ` · ${people.size} people` : "");

  for (const m of thread) {
    const block = document.createElement("div");
    block.className = "thread-msg";
    block.dataset.uid = String(m.uid);

    const head = document.createElement("div");
    head.className = "thread-msg-head";
    const from = document.createElement("span");
    from.className = "thread-msg-from";
    from.textContent = m.from || "(unknown)";
    const date = document.createElement("span");
    date.className = "thread-msg-date";
    date.textContent = m.date ? new Date(m.date).toLocaleString() : "";
    head.append(from, date);

    const frame = document.createElement("iframe");
    frame.className = "thread-msg-frame";
    // Sandboxed body frame: same-origin (so the parent can measure the
    // message and act as a click backstop) + allow-scripts — it lets the
    // injected nonce'd link handler run inside the email, the only way
    // WebKitGTK delivers real clicks to a handler. The email's own scripts
    // stay blocked (they lack the nonce); forms, popups and navigation are
    // blocked by the sandbox flags we omit.
    frame.sandbox = EMAIL_FRAME_SANDBOX;
    initMessageFrame(frame);
    const attsBox = document.createElement("div");
    attsBox.className = "thread-msg-atts";

    // Attachments render ABOVE the body (send/date head, then files, then
    // message), so a message's files are visible before scrolling.
    block.append(head, attsBox, frame);
    container.appendChild(block);

    // Fill the reading area immediately: the frame starts empty and only
    // gets its real height when the body document finishes loading, so
    // without this it shows as a ~120px white sliver (its CSS min-height)
    // while the body is fetched. sizeFrameFromParent re-measures on load
    // and keeps or grows the height from there.
    frame.style.height = frameDisplayHeight(0, frameAvailableHeight(frame)) + "px";

    // Body + attachment metadata in parallel (attachment listing is
    // best-effort and never blocks the body).
    let body = null;
    let atts = [];
    try {
      [body, atts] = await Promise.all([
        invoke("fetch_message", {
          account: state.account.name,
          folder: state.folder,
          uid: m.uid,
        }),
        invoke("list_attachments", {
          account: state.account.name,
          folder: state.folder,
          uid: m.uid,
        }).catch(() => []),
      ]);
    } catch (_) {
      // Offline or not cached: leave the block header, empty body.
    }
    if (!Array.isArray(atts)) atts = [];

    // Plan how each part is displayed: embedded (cid:) images render in
    // the body via data: URLs; unreferenced image parts get thumbnails.
    // Everything is fetched in ONE round-trip and cached server-side.
    const MAX_CID_BYTES = 8 * 1024 * 1024;
    const MAX_PREVIEW_BYTES = 3 * 1024 * 1024;
    const plan = planAttachmentDisplay(body && body.html, atts, {
      maxCidBytes: MAX_CID_BYTES,
      maxPreviewBytes: MAX_PREVIEW_BYTES,
    });
    const dataByPart = new Map();
    if (plan.wantParts.length) {
      try {
        const data = await invoke("get_attachments_data", {
          account: state.account.name,
          folder: state.folder,
          uid: m.uid,
          partIds: plan.wantParts.map((p) => p.part_id),
        });
        plan.wantParts.forEach((p, i) => {
          if (data && data[i]) dataByPart.set(p.part_id, data[i]);
        });
      } catch (_) {
        // Preview failure is fine; the attachment list entry remains.
      }
    }

    // Rewrite cid: references to data: URLs so embedded images render in
    // the sandboxed iframe (whose CSP only allows data: images).
    let html = body && body.html;
    if (body && body.html && plan.cidParts.length) {
      html = rewriteCidImages(body.html, cidDataMap(plan.cidParts, dataByPart));
    }

    if (body) {
      // Remote images are blocked by default (tracking pixels cannot phone
      // home). If the message references any and the user hasn't opted in
      // (per-message or globally), render blank placeholders instead of
      // broken-image icons and offer the opt-in banner. If they HAVE
      // opted in, the relaxed CSP lets the real URLs load.
      const remote = html ? hasRemoteImages(html) : false;
      const allowRemote =
        remote &&
        remoteImagesAllowed(localStorage, state.account.name, state.folder, m.uid);
      let renderHtml = html;
      if (remote && !allowRemote) {
        renderHtml = placeholderRemoteImages(html);
      }
      try {
        await renderViewBody(
          { frame },
          { ...body, html: renderHtml },
          { remoteImages: allowRemote }
        );
      } catch (_) {
        // Unrenderable body: leave the block header, empty body.
      }
      if (remote && !allowRemote) {
        renderRemoteImageBar(block, frame, html, m.uid);
      }
    }
    renderBlockAttachments(attsBox, m.uid, atts, plan.thumbParts, dataByPart);
  }
}

function renderPreviewEmpty() {
  $("preview-empty").classList.remove("hidden");
  $("preview-content").classList.add("hidden");
}

// Full-width message modal: the whole conversation (opened by
// double-clicking a thread row).
let modalUid = null;
let modalThreadId = null;

async function openThreadModal(thread) {
  if (!thread || !thread.length) return;
  modalThreadId = thread[0].thread_id || "";
  modalUid = thread[0].uid;
  // Open the dialog BEFORE rendering the thread: each message iframe
  // measures its available height from #modal-thread's client height, which
  // is 0 while the dialog is still closed — frames would then collapse to
  // their content height instead of filling the modal.
  $("message-modal").showModal();
  try {
    await renderThread(
      $("modal-subject"),
      $("modal-meta"),
      $("modal-thread"),
      thread
    );
    for (const m of thread) {
      if (!m.seen) setSeen(m.uid, true);
    }
  } catch (e) {
    showError(String(e));
    closeMessageModal();
  }
}

function closeMessageModal() {
  const dlg = $("message-modal");
  if (dlg.open) dlg.close();
}

async function saveAttachment(uid, att) {
  let path = null;
  try {
    path = await window.__TAURI__.dialog.save({
      defaultPath: att.filename,
    });
  } catch (e) {
    console.error("[save] dialog error", e);
    showError("Save dialog error: " + e);
    return;
  }
  if (!path) return;
  try {
    await invoke("save_attachment", {
      account: state.account.name,
      folder: state.folder,
      uid,
      partId: att.part_id,
      destPath: path,
    });
    showStatus(`Saved ${att.filename}`);
  } catch (e) {
    console.error("[save] invoke failed", e);
    showError("Save failed: " + e);
  }
}

/* ---------- compose ---------- */

let composeAttachments = [];
let composeReplyContext = null; // { inReplyTo, references } when replying

// Attach files from disk to the current compose. Uses the native file
// picker; the paths are sent to the backend which reads them on send.
async function attachFiles() {
  if (!window.__TAURI__ || !window.__TAURI__.dialog) return;
  try {
    const picked = await window.__TAURI__.dialog.open({ multiple: true });
    if (!picked) return;
    const paths = Array.isArray(picked) ? picked : [picked];
    for (const p of paths) {
      if (!composeAttachments.includes(p)) composeAttachments.push(p);
    }
    renderComposeAttachments();
  } catch (e) {
    showError(String(e));
  }
}

function renderComposeAttachments() {
  const box = $("compose-attachments");
  box.innerHTML = "";
  for (const path of composeAttachments) {
    const item = document.createElement("div");
    item.className = "att-item";
    const name = document.createElement("span");
    name.className = "att-name";
    name.textContent = path.split("/").pop();
    name.title = path;
    const remove = document.createElement("button");
    remove.className = "att-remove";
    remove.textContent = "✕";
    remove.title = "Remove attachment";
    remove.addEventListener("click", () => {
      composeAttachments = composeAttachments.filter((p) => p !== path);
      renderComposeAttachments();
    });
    item.append(name, remove);
    box.appendChild(item);
  }
}

// Open the compose dialog. mode: "reply" (default), "reply-all" or
// "forward"; `extra` is the fetched message body (to/cc for Reply All,
// plain text for the forward quote) when available.
function openCompose(replyTo, mode = "reply", extra = null) {
  const dlg = $("compose-dialog");
  $("compose-error").classList.add("hidden");
  composeAttachments = [];
  renderComposeAttachments();
  if (!replyTo) {
    composeReplyContext = null;
    $("compose-to").value = "";
    $("compose-cc").value = "";
    $("compose-subject").value = "";
    $("compose-body").value = "";
    dlg.showModal();
    return;
  }
  const subject = replyTo.subject || "";
  if (mode === "forward") {
    composeReplyContext = null;
    $("compose-to").value = "";
    $("compose-cc").value = "";
    $("compose-subject").value = /^fwd:/i.test(subject)
      ? subject
      : "Fwd: " + subject;
    $("compose-body").value = quoteOriginal(replyTo, extra);
  } else {
    composeReplyContext = {
      inReplyTo: replyTo.message_id || null,
      references: replyTo.references || null,
    };
    if (mode === "reply-all") {
      // Reply All: sender + original To in the To field, and the original
      // Cc recipients stay in the Cc field.
      $("compose-to").value = replyToAddresses(replyTo, extra);
      $("compose-cc").value = replyCcAddresses(extra);
    } else {
      $("compose-to").value = replyTo.from || "";
      $("compose-cc").value = "";
    }
    $("compose-subject").value = /^re:/i.test(subject) ? subject : "Re: " + subject;
    $("compose-body").value = quoteOriginal(replyTo, extra);
  }
  dlg.showModal();
}

// "----- Original message -----" header block (plus the quoted text when
// the body was fetched) for reply / reply-all / forward bodies.
function quoteOriginal(replyTo, extra) {
  const lines = [
    "\n\n----- Original message -----",
    `From: ${replyTo.from || "(unknown)"}`,
    `Subject: ${replyTo.subject || "(no subject)"}`,
  ];
  if (replyTo.date) lines.push(`Date: ${new Date(replyTo.date).toLocaleString()}`);
  if (extra && extra.text) {
    lines.push("");
    lines.push(extra.text.split("\n").map((l) => "> " + l).join("\n"));
  }
  return lines.join("\n");
}

// Addresses (minus the current account, deduplicated) for Reply All.
function filterAddresses(list) {
  const self = (state.account ? state.account.email : "").toLowerCase();
  const seen = new Set();
  const out = [];
  for (const a of list || []) {
    const s = String(a || "").trim();
    if (!s || seen.has(s.toLowerCase())) continue;
    if (self && s.toLowerCase().includes(self)) continue;
    seen.add(s.toLowerCase());
    out.push(s);
  }
  return out;
}

// Reply All To field: sender + the original To list, minus self.
function replyToAddresses(replyTo, extra) {
  return filterAddresses([replyTo.from, ...((extra && extra.to) || [])]).join(", ");
}

// Reply All Cc field: the original Cc list, minus self.
function replyCcAddresses(extra) {
  return filterAddresses((extra && extra.cc) || []).join(", ");
}

// Fetch the message body (recipients for Reply All, text for the forward
// quote) and open compose in the requested mode.
async function openComposeMode(msg, mode) {
  let extra = null;
  if (mode !== "reply") {
    try {
      extra = await invoke("fetch_message", {
        account: state.account.name,
        folder: state.folder,
        uid: msg.uid,
      });
    } catch (_) {
      // Offline / not cached: reply-all degrades to the sender only.
    }
  }
  openCompose(msg, mode, extra);
}

// The dropdown under the Reply caret: Reply / Reply All / Forward.
function showReplyMenu(anchor, msg) {
  let menu = document.getElementById("reply-menu");
  if (!menu) {
    menu = document.createElement("div");
    menu.id = "reply-menu";
    menu.className = "reply-menu";
    document.body.appendChild(menu);
  }
  menu.innerHTML = "";
  const items = [
    { label: "Reply", mode: "reply" },
    { label: "Reply all", mode: "reply-all" },
    { label: "Forward", mode: "forward" },
  ];
  for (const it of items) {
    const el = document.createElement("div");
    el.className = "menu-item";
    el.textContent = it.label;
    el.addEventListener("click", () => {
      closeReplyMenu();
      openComposeMode(msg, it.mode);
    });
    menu.appendChild(el);
  }
  const rect = anchor.getBoundingClientRect();
  menu.style.left = rect.left + "px";
  menu.style.top = rect.bottom + 2 + "px";

  const close = (e) => {
    if (!menu.contains(e.target)) {
      closeReplyMenu();
      document.removeEventListener("click", close);
      document.removeEventListener("contextmenu", close);
    }
  };
  setTimeout(() => {
    document.addEventListener("click", close);
    document.addEventListener("contextmenu", close);
  }, 0);
}

function closeReplyMenu() {
  const menu = document.getElementById("reply-menu");
  if (menu) menu.remove();
}

// Wire a split Reply control: the main part replies, the caret opens the
// Reply / Reply All / Forward dropdown.
function attachReplySplit(mainBtn, caretBtn, getMsg) {
  mainBtn.addEventListener("click", () => {
    const m = getMsg();
    if (m) openCompose(m, "reply");
  });
  caretBtn.addEventListener("click", (e) => {
    e.stopPropagation();
    const m = getMsg();
    if (m) showReplyMenu(caretBtn, m);
  });
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
  const cc = $("compose-cc").value
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
        cc,
        subject,
        body,
        attachments: composeAttachments,
        inReplyTo: composeReplyContext ? composeReplyContext.inReplyTo : null,
        references: composeReplyContext ? composeReplyContext.references : null,
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
      const proceed = await confirmDialog(
        `Delete account '${acc.name}'?`,
        `The account '${acc.name}' (${acc.email}) will be removed from this ` +
        `app and its locally cached mail will be deleted. Messages on the ` +
        `server are not affected.`,
        "Delete"
      );
      if (!proceed) return;
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
  updateAccountsButton();
  renderEmptyStates();
  await resetMailState();
}

// The Accounts button doubles as the current-account badge: it shows the
// selected account's email (with a ·~offline marker while disconnected), and
// reverts to plain "Accounts" when no account is configured.
function updateAccountsButton() {
  const btn = $("accounts-btn");
  const acc = state.account;
  if (!acc) {
    btn.textContent = "Accounts ▾";
    btn.classList.remove("offline");
    btn.title = "No account configured — use this menu to add one";
    return;
  }
  const offline = !state.online;
  btn.textContent = offline ? `${acc.email} · ~offline ▾` : `${acc.email} ▾`;
  btn.classList.toggle("offline", offline);
  btn.title = `Account: ${acc.name} (${acc.email})`;
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
  updateAccountsButton();
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
  // Remember the choice so the app reopens the same way next launch.
  saveSidebarVisible(localStorage, state.sidebarVisible);
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
    if (!state.account) return;
    if (state.online) {
      loadFolders();
      if (state.folder) loadMessages();
    } else {
      // The backend only emits mail-refresh after a successful server
      // round trip (new-mail detection, reconnect catch-up, slow poll,
      // optimistic move/delete), so the event itself proves connectivity
      // is back — flip online, which also triggers the recovery refresh
      // (loadFolders + loadMessages) instead of dropping the event.
      setOnlineStatus(true);
    }
  });
  // The backend reports a lost IMAP connection (also on startup when the
  // app begins offline): mark offline so the recovery poller starts. This
  // is the reliable offline signal — the webview's window "offline" event
  // is not dependable on Linux/WebKitGTK.
  window.__TAURI__.event.listen("connection-lost", () => {
    if (state.account) setOnlineStatus(false);
  });
}

// Wire a freshly created email-body iframe: measure its content from the
// parent (the frame is sandboxed same-origin so its document is readable)
// when the message loads, keep the measurement current via a ResizeObserver,
// and attach a parent-side link-click listener as a backstop for the
// nonce'd script injected inside the email.
//
// The frame is created empty and navigated to its srcdoc afterwards, and
// WebKitGTK fires a `load` event for BOTH documents — the initial
// about:blank one and the real about:srcdoc content. The handler must skip
// the blank document (measuring/attaching there would operate on a
// document that is immediately discarded), so it is deliberately NOT
// once-only.
function initMessageFrame(frame) {
  frame.addEventListener("load", () => {
    if (!shouldHandleFrameLoad(frame.contentDocument)) return;
    sizeFrameFromParent(frame);
    attachFrameLinkHandler(frame);
  });
}

// Size the iframe from its own document: the content height it reports via
// scrollHeight (never shorter than the pane, so no nested scrollbar — the
// outer container scrolls long content), re-measured whenever the email's
// content changes (e.g. data: images decoding late).
function sizeFrameFromParent(frame) {
  const doc = frame.contentDocument;
  if (!doc) return;
  const content = frameContentHeight(doc);
  if (content <= 0) return;
  frame.dataset.contentHeight = String(content);
  frame.style.height =
    frameDisplayHeight(content, frameAvailableHeight(frame)) + "px";
  if (!frame.__sufiObserved && typeof ResizeObserver !== "undefined") {
    frame.__sufiObserved = true;
    try {
      new ResizeObserver(() => sizeFrameFromParent(frame)).observe(doc.body);
    } catch (_) {}
  }
}

// Parent-side click backstop for http(s) links inside the email. The
// PRIMARY handler is the nonce'd script injected into the message (see
// withEmailCsp): WebKitGTK does not deliver real clicks to listeners the
// parent attaches to a JS-created srcdoc iframe's document, but DOES run
// scripts inside the frame — so the injected script intercepts the click
// and forwards the URL via postMessage (handled by initExternalLinks).
// This listener still covers the case where that script could not run; it
// checks e.defaultPrevented so the two handlers never open the same link
// twice.
function attachFrameLinkHandler(frame) {
  const doc = frame.contentDocument;
  if (!doc || doc.__sufiLinks) return;
  doc.__sufiLinks = true;
  doc.addEventListener(
    "click",
    (e) => {
      const href = hrefFromEmailClick(e);
      if (!href) return;
      e.preventDefault();
      openUrlInBrowser(href);
    },
    true
  );
}

// Open an http(s) URL in the system's default browser via the opener
// plugin. All link clicks from inside emails funnel through here.
function openUrlInBrowser(href) {
  if (!window.__TAURI__ || !window.__TAURI__.opener) return;
  window.__TAURI__.opener
    .openUrl(href)
    .catch((err) => showError(String(err)));
}

// Links inside the sandboxed email iframes are intercepted by the nonce'd
// script injected into the message body (see withEmailCsp) and forwarded
// here, so they open in the system's default browser instead of being
// dead. Only http(s) URLs are ever opened.
function initExternalLinks() {
  if (!window.__TAURI__ || !window.__TAURI__.opener) return;
  window.addEventListener("message", (e) => {
    const url = emailLinkUrlFromMessage(e.data);
    if (!url) return;
    openUrlInBrowser(url);
  });
}

// Auto-size every email-body iframe from the parent: content shorter than
// the pane fills the entire available height; content taller than it keeps
// its full height so the OUTER container (preview pane / modal thread)
// scrolls — never a nested scrollbar inside the message body. The
// available height is re-clamped on window resize so short messages track
// the pane.
function initFrameSizing() {
  window.addEventListener("resize", clampFrameHeights);
}

// Re-clamp every rendered message frame to the current available height
// (short messages track the pane size, long ones keep their full content
// height). Uses the content height each iframe last reported.
function clampFrameHeights() {
  for (const f of document.querySelectorAll(".thread-msg-frame")) {
    const content = Number(f.dataset.contentHeight) || 0;
    if (content > 0) {
      f.style.height =
        frameDisplayHeight(content, frameAvailableHeight(f)) + "px";
    }
  }
}

// ------------------------------------------------------------ system theme
//
// Follow the active Omarchy theme (see theme.rs): the backend reads the
// current theme's colors.toml and emits `system-theme-changed` when the
// theme switches; the palette is mapped onto the app's CSS variables. The
// last applied variables are cached so the UI opens already themed (no
// flash of the default dark palette) and also themed offline.
const THEME_CACHE_KEY = "sufi-theme-vars";

function cacheThemeVars(vars) {
  try {
    if (vars) localStorage.setItem(THEME_CACHE_KEY, JSON.stringify(vars));
    else localStorage.removeItem(THEME_CACHE_KEY);
  } catch (_) {}
}

function loadCachedThemeVars() {
  try {
    const raw = localStorage.getItem(THEME_CACHE_KEY);
    return raw ? JSON.parse(raw) : null;
  } catch (_) {
    return null;
  }
}

// Map an Omarchy palette (from the backend) onto the CSS variables and
// apply + cache it. A null palette resets to the built-in defaults.
function applySystemTheme(colors) {
  const vars = omarchyThemeToCssVars(colors);
  applyThemeVars(document.documentElement, vars);
  cacheThemeVars(vars);
}

// Apply the cached theme immediately, then load the real one and follow
// theme switches for the rest of the session.
async function initSystemTheme() {
  applyThemeVars(document.documentElement, loadCachedThemeVars());
  try {
    const colors = await invoke("get_system_theme");
    applySystemTheme(colors);
  } catch (_) {
    // No theme command (dev in plain browser): keep the cached/default look.
  }
  if (window.__TAURI__ && window.__TAURI__.event) {
    window.__TAURI__.event.listen("system-theme-changed", (e) => {
      applySystemTheme(e.payload);
    });
  }
}

function init() {
  if (!guardTauri()) return;

  initSystemTheme();
  initFontSize();
  initResizers();
  initConnectivityEvents();
  syncResponsiveColumns();
  window.addEventListener("resize", syncResponsiveColumns);

  $("toggle-sidebar").addEventListener("click", toggleSidebar);
  // Full manual refresh: folders (badges) + the visible message list.
  $("refresh-btn").addEventListener("click", refreshMail);

  // Search: toolbar icon toggles an expanding input; typing searches every
  // cached folder; Esc / ✕ closes. Ctrl+K focuses search from anywhere.
  $("search-btn").addEventListener("click", toggleSearch);
  $("search-clear").addEventListener("click", exitSearch);
  $("search-input").addEventListener("input", onSearchInput);
  $("search-input").addEventListener("keydown", (e) => {
    if (e.key === "Escape") {
      e.preventDefault();
      exitSearch();
    } else if (e.key === "Enter") {
      e.preventDefault();
      clearTimeout(searchTimer);
      runSearch($("search-input").value);
    }
  });

  initMailRefreshListener();
  initFrameSizing();
  initExternalLinks();
  $("compose-btn").addEventListener("click", () => openCompose(null));
  $("compose-attach").addEventListener("click", attachFiles);
  document.getElementById("compose-form").addEventListener("submit", sendCompose);
  $("compose-cancel").addEventListener("click", () => $("compose-dialog").close());
  // Reply split control (preview + modal): main part replies, the caret
  // opens the Reply / Reply All / Forward dropdown.
  attachReplySplit($("reply-btn"), $("reply-more"), () =>
    state.messages.find((x) => x.uid === state.selectedUid)
  );
  attachReplySplit($("modal-reply"), $("modal-reply-more"), () =>
    state.messages.find((x) => x.uid === modalUid)
  );
  $("delete-btn").addEventListener("click", () => {
    if (state.selectedUid != null) deleteMessage(state.selectedUid);
  });
  $("move-btn").addEventListener("click", () => {
    if (state.selectedUid != null) openMoveDialog(state.selectedUid);
  });
  $("move-cancel").addEventListener("click", () => $("move-dialog").close());

  // Full-width message modal.
  $("modal-close").addEventListener("click", closeMessageModal);
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
    if (e.ctrlKey && e.key === "k") {
      e.preventDefault();
      if (!$("search-box").classList.contains("active")) toggleSearch();
      else $("search-input").focus();
    }
  });

  refreshAccounts(null)
    .then(() => {
      // First run with no accounts: open the add-account dialog right away.
      if (state.accounts.length === 0) openAccountDialog();
    })
    .catch((err) => showError(String(err)));
}

init();
