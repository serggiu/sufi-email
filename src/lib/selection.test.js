import { describe, it, expect, beforeEach } from "vitest";
import {
  SELECTION_KEY,
  selectionKey,
  loadSelectionRaw,
  saveSelection,
  loadLastSelection,
  loadFolderSelection,
  clearFolderSelection,
  loadSidebarVisible,
  saveSidebarVisible,
} from "./selection.js";

function mockStorage() {
  const map = new Map();
  return {
    getItem: (k) => (map.has(k) ? map.get(k) : null),
    setItem: (k, v) => map.set(k, String(v)),
    removeItem: (k) => map.delete(k),
    _map: map,
  };
}

describe("selection storage", () => {
  let storage;
  beforeEach(() => {
    storage = mockStorage();
  });

  it("returns empty defaults with no data", () => {
    expect(loadSelectionRaw(storage)).toEqual({ lastKey: null, folders: {} });
    expect(loadLastSelection(storage)).toBeNull();
    expect(loadFolderSelection(storage, "A", "INBOX")).toBeNull();
  });

  it("saves and restores per-folder selections", () => {
    saveSelection(storage, "Sergiu", "INBOX", 10);
    saveSelection(storage, "Sergiu", "excursii", 198);

    expect(loadFolderSelection(storage, "Sergiu", "INBOX")).toBe(10);
    expect(loadFolderSelection(storage, "Sergiu", "excursii")).toBe(198);
    expect(loadFolderSelection(storage, "Sergiu", "Archive")).toBeNull();
    expect(loadFolderSelection(storage, "Other", "INBOX")).toBeNull();
  });

  it("remembers the most recent selection overall", () => {
    saveSelection(storage, "Sergiu", "INBOX", 10);
    saveSelection(storage, "Sergiu", "excursii", 198);

    const last = loadLastSelection(storage);
    expect(last).toEqual({ account: "Sergiu", folder: "excursii", uid: 198 });
  });

  it("overwrites a folder's selection", () => {
    saveSelection(storage, "A", "INBOX", 1);
    saveSelection(storage, "A", "INBOX", 2);
    expect(loadFolderSelection(storage, "A", "INBOX")).toBe(2);
    expect(loadLastSelection(storage).uid).toBe(2);
  });

  it("clears a folder's selection and the last pointer", () => {
    saveSelection(storage, "A", "INBOX", 1);
    clearFolderSelection(storage, "A", "INBOX");
    expect(loadFolderSelection(storage, "A", "INBOX")).toBeNull();
    expect(loadLastSelection(storage)).toBeNull();
  });

  it("keeps other folders when clearing one", () => {
    saveSelection(storage, "A", "INBOX", 1);
    saveSelection(storage, "A", "Archive", 2);
    clearFolderSelection(storage, "A", "INBOX");
    expect(loadFolderSelection(storage, "A", "Archive")).toBe(2);
  });

  it("migrates the old single-selection format", () => {
    storage.setItem(
      SELECTION_KEY,
      JSON.stringify({ account: "Old", folder: "INBOX", uid: 42 })
    );
    expect(loadFolderSelection(storage, "Old", "INBOX")).toBe(42);
    expect(loadLastSelection(storage)).toEqual({
      account: "Old",
      folder: "INBOX",
      uid: 42,
    });
  });

  it("tolerates corrupt JSON", () => {
    storage.setItem(SELECTION_KEY, "{not json");
    expect(loadSelectionRaw(storage)).toEqual({ lastKey: null, folders: {} });
    expect(loadFolderSelection(storage, "A", "INBOX")).toBeNull();
  });

  it("builds stable keys", () => {
    expect(selectionKey("Sergiu T", "INBOX")).toBe("Sergiu T::INBOX");
    expect(selectionKey("a", "b::c")).toBe("a::b::c");
  });
});

describe("sidebar visibility persistence", () => {
  let storage;
  beforeEach(() => {
    storage = mockStorage();
  });

  it("defaults to visible when nothing was stored", () => {
    expect(loadSidebarVisible(storage)).toBe(true);
  });

  it("reopens hidden after the user hid the folders column", () => {
    saveSidebarVisible(storage, false);
    expect(loadSidebarVisible(storage)).toBe(false);
  });

  it("round-trips back to visible", () => {
    saveSidebarVisible(storage, false);
    saveSidebarVisible(storage, true);
    expect(loadSidebarVisible(storage)).toBe(true);
  });
});
