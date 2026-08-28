// Per-folder last-selected-message storage.
//
// Pure functions over a storage object (localStorage in the app, a mock in
// tests): remember the last message opened in each account+folder, plus the
// most recently used one for launch restore.

export const SELECTION_KEY = "sufi-selections";

export function selectionKey(account, folder) {
  return account + "::" + folder;
}

export function loadSelectionRaw(storage) {
  try {
    const raw = JSON.parse(storage.getItem(SELECTION_KEY));
    if (raw && typeof raw === "object") {
      // Migrate the old single-selection format { account, folder, uid }.
      if (raw.account && raw.folder && Number.isFinite(raw.uid)) {
        const key = selectionKey(raw.account, raw.folder);
        return { lastKey: key, folders: { [key]: raw.uid } };
      }
      if (raw.folders && typeof raw.folders === "object") return raw;
    }
  } catch (_) {}
  return { lastKey: null, folders: {} };
}

export function saveSelection(storage, account, folder, uid) {
  const sel = loadSelectionRaw(storage);
  sel.folders[selectionKey(account, folder)] = uid;
  sel.lastKey = selectionKey(account, folder);
  try {
    storage.setItem(SELECTION_KEY, JSON.stringify(sel));
  } catch (_) {}
}

// Most recent selection overall — used to reopen the app where you left off.
export function loadLastSelection(storage) {
  const sel = loadSelectionRaw(storage);
  const key = sel.lastKey;
  if (!key || !(key in sel.folders)) return null;
  const sep = key.indexOf("::");
  return {
    account: key.slice(0, sep),
    folder: key.slice(sep + 2),
    uid: sel.folders[key],
  };
}

// The last message opened in a specific account+folder, or null.
export function loadFolderSelection(storage, account, folder) {
  const sel = loadSelectionRaw(storage);
  return sel.folders[selectionKey(account, folder)] ?? null;
}

// Forget a folder's remembered message (when it was deleted/moved away).
export function clearFolderSelection(storage, account, folder) {
  const sel = loadSelectionRaw(storage);
  const key = selectionKey(account, folder);
  if (key in sel.folders) {
    delete sel.folders[key];
    if (sel.lastKey === key) sel.lastKey = null;
    try {
      storage.setItem(SELECTION_KEY, JSON.stringify(sel));
    } catch (_) {}
  }
}
