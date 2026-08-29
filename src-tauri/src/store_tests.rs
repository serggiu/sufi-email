//! Tests for the local SQLite message cache (store.rs).
//!
//! Every test opens an isolated store in a temp directory, so these never
//! touch the user's real cache.

use crate::store::{AttachmentMeta, Store};
use crate::account::AccountConfig;
use chrono::{TimeZone, Utc};

fn test_store(tag: &str) -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open_in(dir.path(), &format!("test-{tag}@example.com")).expect("open store");
    (dir, store)
}

fn summary(uid: u32, subject: &str, from: &str, days_ago: i64, seen: bool) -> crate::mail::MessageSummary {
    crate::mail::MessageSummary {
        uid,
        subject: subject.to_string(),
        from: from.to_string(),
        date: Some(Utc.timestamp_opt(1_700_000_000 - days_ago * 86_400, 0).unwrap()),
        seen,
        has_attachment: false,
        snippet: format!("snippet of {subject}"),
        message_id: format!("mid-{uid}@test"),
        references: String::new(),
        in_reply_to: String::new(),
        thread_id: format!("thread-{uid}"),
    }
}

#[test]
fn upsert_inserts_new_rows() {
    let (_dir, mut store) = test_store("insert");
    let batch = vec![
        summary(1, "Hello", "a@b.com", 2, false),
        summary(2, "World", "c@d.com", 1, true),
    ];
    let n = store.upsert_summaries("INBOX", &batch).unwrap();
    assert_eq!(n, 2);

    let loaded = store.load_summaries("INBOX").unwrap();
    assert_eq!(loaded.len(), 2);
    // Newest first.
    assert_eq!(loaded[0].subject, "World");
    assert_eq!(loaded[1].subject, "Hello");
}

#[test]
fn upsert_same_uid_updates_flags_in_place() {
    let (_dir, mut store) = test_store("flag-update");
    store
        .upsert_summaries("INBOX", &[summary(1, "Hello", "a@b.com", 1, false)])
        .unwrap();

    // Same uid arrives again, now seen on the server.
    store
        .upsert_summaries("INBOX", &[summary(1, "Hello", "a@b.com", 1, true)])
        .unwrap();

    let loaded = store.load_summaries("INBOX").unwrap();
    assert_eq!(loaded.len(), 1, "no duplicate rows");
    assert!(loaded[0].seen, "seen flag refreshed from server");
}

#[test]
fn upsert_same_uid_updates_subject_and_metadata() {
    let (_dir, mut store) = test_store("meta-update");
    store
        .upsert_summaries("INBOX", &[summary(1, "Old subject", "a@b.com", 1, false)])
        .unwrap();

    store
        .upsert_summaries("INBOX", &[summary(1, "New subject", "a@b.com", 1, true)])
        .unwrap();

    let loaded = store.load_summaries("INBOX").unwrap();
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].subject, "New subject");
}

#[test]
fn moved_message_gets_new_uid_without_duplicating() {
    // Trash -> Inbox round-trip: the server assigns a new UID but the
    // content (from+date+subject) is identical. The cache must re-point
    // the existing row instead of inserting a duplicate.
    let (_dir, mut store) = test_store("moved");
    store
        .upsert_summaries("INBOX", &[summary(100, "Fwd: lunch", "mihaela@x.org", 1, true)])
        .unwrap();

    // Same content comes back under uid 200.
    store
        .upsert_summaries("INBOX", &[summary(200, "Fwd: lunch", "mihaela@x.org", 1, true)])
        .unwrap();

    let loaded = store.load_summaries("INBOX").unwrap();
    assert_eq!(loaded.len(), 1, "moved message must not duplicate");
    assert_eq!(loaded[0].uid, 200, "row re-pointed to the new uid");
}

#[test]
fn same_subject_different_content_is_not_merged() {
    // Two genuinely different messages that happen to share a subject must
    // both be kept (different sender or different date => different hash).
    let (_dir, mut store) = test_store("same-subject");
    store
        .upsert_summaries("INBOX", &[
            summary(1, "Oferta", "dragos@bforce.ro", 2, true),
            summary(2, "Oferta", "dragos@bforce.ro", 1, true), // different date
        ])
        .unwrap();
    let loaded = store.load_summaries("INBOX").unwrap();
    assert_eq!(loaded.len(), 2, "different content = different messages");
}

#[test]
fn folders_are_isolated_from_each_other() {
    let (_dir, mut store) = test_store("folders-isolated");
    store
        .upsert_summaries("INBOX", &[summary(1, "In inbox", "a@b.com", 1, false)])
        .unwrap();
    store
        .upsert_summaries("Trash", &[summary(1, "In trash", "a@b.com", 1, false)])
        .unwrap();

    assert_eq!(store.load_summaries("INBOX").unwrap().len(), 1);
    assert_eq!(store.load_summaries("Trash").unwrap().len(), 1);
    assert_eq!(store.load_summaries("Nope").unwrap().len(), 0);
}

#[test]
fn remove_uids_not_in_deletes_stale_rows() {
    let (_dir, mut store) = test_store("reconcile");
    store
        .upsert_summaries("INBOX", &[
            summary(1, "kept", "a@b.com", 3, false),
            summary(2, "also kept", "a@b.com", 2, false),
            summary(3, "deleted on server", "a@b.com", 1, false),
        ])
        .unwrap();

    let server_uids: std::collections::HashSet<u32> = [1u32, 2u32].into_iter().collect();
    let removed = store.remove_uids_not_in("INBOX", &server_uids).unwrap();
    assert_eq!(removed, 1);

    let loaded = store.load_summaries("INBOX").unwrap();
    assert_eq!(loaded.len(), 2);
    assert!(loaded.iter().all(|m| m.uid != 3));
}

#[test]
fn remove_uids_not_in_only_touches_given_folder() {
    let (_dir, mut store) = test_store("reconcile-scope");
    store
        .upsert_summaries("INBOX", &[summary(1, "inbox msg", "a@b.com", 1, false)])
        .unwrap();
    store
        .upsert_summaries("Trash", &[summary(1, "trash msg", "a@b.com", 1, false)])
        .unwrap();

    // Server says INBOX only has uid 1; Trash is untouched.
    let server_uids: std::collections::HashSet<u32> = [1u32].into_iter().collect();
    store.remove_uids_not_in("INBOX", &server_uids).unwrap();

    assert_eq!(store.load_summaries("INBOX").unwrap().len(), 1);
    assert_eq!(store.load_summaries("Trash").unwrap().len(), 1);
}

#[test]
fn set_seen_updates_only_target_row() {
    let (_dir, mut store) = test_store("set-seen");
    store
        .upsert_summaries("INBOX", &[
            summary(1, "one", "a@b.com", 2, false),
            summary(2, "two", "a@b.com", 1, false),
        ])
        .unwrap();

    store.set_seen("INBOX", 1, true).unwrap();
    let loaded = store.load_summaries("INBOX").unwrap();
    let one = loaded.iter().find(|m| m.uid == 1).unwrap();
    let two = loaded.iter().find(|m| m.uid == 2).unwrap();
    assert!(one.seen);
    assert!(!two.seen);
}

#[test]
fn delete_message_removes_row_and_attachments() {
    let (_dir, mut store) = test_store("delete");
    store
        .upsert_summaries("INBOX", &[summary(1, "with attachment", "a@b.com", 1, false)])
        .unwrap();
    store
        .store_body(
            "INBOX",
            1,
            Some("body text"),
            None,
            &[AttachmentMeta {
                filename: "report.pdf".into(),
                content_type: "application/pdf".into(),
                size: 1234,
                part_id: "0".into(),
                content_id: None,
            }],
        )
        .unwrap();

    assert_eq!(store.load_attachments("INBOX", 1).unwrap().len(), 1);
    store.delete_message("INBOX", 1).unwrap();

    assert!(store.load_summaries("INBOX").unwrap().is_empty());
    assert!(store.load_attachments("INBOX", 1).unwrap().is_empty());
}

#[test]
fn store_and_load_body_roundtrip() {
    let (_dir, mut store) = test_store("body");
    store
        .upsert_summaries("INBOX", &[summary(1, "html mail", "a@b.com", 1, false)])
        .unwrap();

    // Initially no body cached.
    assert!(store.load_body("INBOX", 1).unwrap().is_none());

    store
        .store_body("INBOX", 1, Some("plain text"), Some("<p>html</p>"), &[])
        .unwrap();

    let (text, html) = store.load_body("INBOX", 1).unwrap().unwrap();
    assert_eq!(text.as_deref(), Some("plain text"));
    assert_eq!(html.as_deref(), Some("<p>html</p>"));
}

#[test]
fn load_body_returns_none_for_missing_message() {
    let (_dir, store) = test_store("body-missing");
    assert!(store.load_body("INBOX", 999).unwrap().is_none());
}

#[test]
fn attachments_roundtrip() {
    let (_dir, mut store) = test_store("attachments");
    store
        .upsert_summaries("INBOX", &[summary(1, "with att", "a@b.com", 1, false)])
        .unwrap();
    let atts = vec![
        AttachmentMeta {
            filename: "a.pdf".into(),
            content_type: "application/pdf".into(),
            size: 100,
            part_id: "0".into(),
            content_id: None,
        },
        AttachmentMeta {
            filename: "b.png".into(),
            content_type: "image/png".into(),
            size: 200,
            part_id: "1".into(),
            content_id: None,
        },
    ];
    store.store_body("INBOX", 1, Some("body"), None, &atts).unwrap();

    let loaded = store.load_attachments("INBOX", 1).unwrap();
    assert_eq!(loaded.len(), 2);
    assert_eq!(loaded[0].filename, "a.pdf");
    assert_eq!(loaded[1].content_type, "image/png");
    assert_eq!(loaded[1].size, 200);
}

#[test]
fn store_body_replaces_old_attachments() {
    let (_dir, mut store) = test_store("att-replace");
    store
        .upsert_summaries("INBOX", &[summary(1, "mail", "a@b.com", 1, false)])
        .unwrap();

    let v1 = vec![AttachmentMeta {
        filename: "old.txt".into(),
        content_type: "text/plain".into(),
        size: 1,
        part_id: "0".into(),
        content_id: None,
    }];
    store.store_body("INBOX", 1, Some("b"), None, &v1).unwrap();

    let v2 = vec![AttachmentMeta {
        filename: "new.txt".into(),
        content_type: "text/plain".into(),
        size: 2,
        part_id: "0".into(),
        content_id: None,
    }];
    store.store_body("INBOX", 1, Some("b"), None, &v2).unwrap();

    let loaded = store.load_attachments("INBOX", 1).unwrap();
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].filename, "new.txt");
}

#[test]
fn folders_cache_roundtrip_preserves_order_and_special_use() {
    let (_dir, mut store) = test_store("folders-cache");
    let folders = vec![
        crate::mail::Folder { name: "INBOX".into(), delimiter: "/".into(), special_use: None, unread: 2 },
        crate::mail::Folder { name: "Trash".into(), delimiter: "/".into(), special_use: Some("trash".into()), unread: 0 },
        crate::mail::Folder { name: "Archive".into(), delimiter: "/".into(), special_use: Some("archive".into()), unread: 5 },
    ];
    store.store_folders(&folders).unwrap();

    let loaded = store.load_folders().unwrap();
    assert_eq!(loaded.len(), 3);
    assert_eq!(loaded[0].name, "INBOX");
    assert_eq!(loaded[0].unread, 2);
    assert_eq!(loaded[1].special_use.as_deref(), Some("trash"));
    assert_eq!(loaded[2].unread, 5);
}

#[test]
fn store_folders_replaces_previous_list() {
    let (_dir, mut store) = test_store("folders-replace");
    store
        .store_folders(&[crate::mail::Folder {
            name: "Old".into(),
            delimiter: "/".into(),
            special_use: None,
            unread: 0,
        }])
        .unwrap();
    store
        .store_folders(&[
            crate::mail::Folder { name: "INBOX".into(), delimiter: "/".into(), special_use: None, unread: 1 },
            crate::mail::Folder { name: "Sent".into(), delimiter: "/".into(), special_use: Some("sent".into()), unread: 0 },
        ])
        .unwrap();

    let loaded = store.load_folders().unwrap();
    assert_eq!(loaded.len(), 2);
    assert!(loaded.iter().all(|f| f.name != "Old"));
}

#[test]
fn set_folder_unread_updates_badge() {
    let (_dir, mut store) = test_store("folder-unread");
    store
        .store_folders(&[crate::mail::Folder {
            name: "INBOX".into(),
            delimiter: "/".into(),
            special_use: None,
            unread: 0,
        }])
        .unwrap();
    store.set_folder_unread("INBOX", 7).unwrap();
    let loaded = store.load_folders().unwrap();
    assert_eq!(loaded[0].unread, 7);
}

#[test]
fn utf8_subjects_survive_roundtrip() {
    let (_dir, mut store) = test_store("utf8");
    let subject = "Dacă ai păstrat programul pentru „când vei avea timp” — 日本語 🎉";
    store
        .upsert_summaries("INBOX", &[summary(1, subject, "señor@españa.es", 1, false)])
        .unwrap();
    let loaded = store.load_summaries("INBOX").unwrap();
    assert_eq!(loaded[0].subject, subject);
    // from_name/from_email are split out of the combined "Name <email>"
    // string on upsert; with no display name the combined form is rebuilt.
    assert_eq!(loaded[0].from, "señor@españa.es <señor@españa.es>");
}

#[test]
fn db_files_are_keyed_by_email_not_name() {
    let dir = tempfile::tempdir().unwrap();
    let acc_a = AccountConfig {
        name: "Name One".into(),
        email: "fixed@example.com".into(),
        imap_host: "x".into(),
        imap_port: 993,
        smtp_host: "x".into(),
        smtp_port: 465,
        username: "fixed@example.com".into(),
        password: String::new(),
        smtp_username: None,
        smtp_password: None,
        smtp_starttls: false,
    };
    let _ = Store::open_in(dir.path(), &acc_a.email).unwrap();
    let db_name = format!("{}.db", crate::store::sanitize_for_test(&acc_a.email));
    assert!(dir.path().join(&db_name).exists(), "db named by email");
}

#[test]
fn empty_folder_load_is_empty_not_error() {
    let (_dir, store) = test_store("empty");
    assert!(store.load_summaries("INBOX").unwrap().is_empty());
    assert!(store.load_folders().unwrap().is_empty());
}

// ---------------------------------------------------- new-mail tracking

#[test]
fn first_check_records_baseline_without_notifying() {
    let (_dir, mut store) = test_store("baseline");
    let new = store
        .new_uids_since_last_sync("INBOX", &[1, 2, 3, 4, 5])
        .unwrap();
    assert!(new.is_empty(), "first check must not report new mail");
    assert_eq!(
        store.notified_uids("INBOX").unwrap().len(),
        5,
        "baseline UIDs recorded"
    );
}

#[test]
fn later_check_reports_only_genuinely_new_uids() {
    let (_dir, mut store) = test_store("diff");
    store
        .new_uids_since_last_sync("INBOX", &[1, 2, 3])
        .unwrap();
    let new = store
        .new_uids_since_last_sync("INBOX", &[1, 2, 3, 7, 8])
        .unwrap();
    assert_eq!(new, vec![7, 8], "only new UIDs reported");
    // Recorded now, so a third identical check reports nothing.
    let new = store
        .new_uids_since_last_sync("INBOX", &[1, 2, 3, 7, 8])
        .unwrap();
    assert!(new.is_empty());
}

#[test]
fn empty_baseline_then_first_message_notifies() {
    let (_dir, mut store) = test_store("empty-first");
    // First check with an empty inbox establishes an empty baseline.
    assert!(store.new_uids_since_last_sync("INBOX", &[]).unwrap().is_empty());
    // A message arriving later must be reported as new — it must not be
    // absorbed into a second baseline.
    let new = store.new_uids_since_last_sync("INBOX", &[42]).unwrap();
    assert_eq!(new, vec![42]);
}

#[test]
fn folders_tracked_independently() {
    let (_dir, mut store) = test_store("per-folder");
    store.new_uids_since_last_sync("INBOX", &[1, 2]).unwrap();
    store.new_uids_since_last_sync("Archive", &[1]).unwrap();
    assert!(store.new_uids_since_last_sync("INBOX", &[1, 2, 3]).unwrap() == vec![3]);
    assert!(store.new_uids_since_last_sync("Archive", &[1, 2]).unwrap() == vec![2]);
}

#[test]
fn notified_state_survives_store_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let email = "reopen@example.com";
    {
        let mut store = Store::open_in(dir.path(), email).unwrap();
        store.new_uids_since_last_sync("INBOX", &[5, 6]).unwrap();
    }
    {
        let mut store = Store::open_in(dir.path(), email).unwrap();
        let new = store.new_uids_since_last_sync("INBOX", &[5, 6, 9]).unwrap();
        assert_eq!(new, vec![9], "persisted across reopens");
    }
}

// ---------------------------------------------------- offline flag queue

#[test]
fn pending_flags_queue_roundtrip() {
    let (_dir, mut store) = test_store("pending");
    assert!(store.pending_flags().unwrap().is_empty());

    store.upsert_pending_flag("INBOX", 7, true).unwrap();
    store.upsert_pending_flag("INBOX", 8, false).unwrap();
    let pending = store.pending_flags().unwrap();
    assert_eq!(pending.len(), 2);
    assert!(pending.contains(&("INBOX".into(), 7, true)));
    assert!(pending.contains(&("INBOX".into(), 8, false)));

    store.remove_pending_flag("INBOX", 7).unwrap();
    let pending = store.pending_flags().unwrap();
    assert_eq!(pending, vec![("INBOX".into(), 8, false)]);
}

#[test]
fn pending_flag_upsert_replaces_state() {
    let (_dir, mut store) = test_store("pending-replace");
    store.upsert_pending_flag("INBOX", 3, true).unwrap();
    store.upsert_pending_flag("INBOX", 3, false).unwrap();
    let pending = store.pending_flags().unwrap();
    assert_eq!(pending, vec![("INBOX".into(), 3, false)]);
}

#[test]
fn pending_flags_purged_for_vanished_uids() {
    let (_dir, mut store) = test_store("pending-purge");
    store.upsert_pending_flag("INBOX", 1, true).unwrap();
    store.upsert_pending_flag("INBOX", 2, true).unwrap();
    store.upsert_pending_flag("Archive", 2, true).unwrap();
    let server: std::collections::HashSet<u32> = [2].into_iter().collect();
    store.remove_pending_uids_not_in("INBOX", &server).unwrap();
    let pending = store.pending_flags().unwrap();
    assert_eq!(pending, vec![("INBOX".into(), 2, true), ("Archive".into(), 2, true)]);
}

#[test]
fn upsert_respects_pending_seen_override() {
    let (_dir, mut store) = test_store("pending-override");
    // In-flight optimistic mark: seen=true pending, server still says unread.
    store.upsert_pending_flag("INBOX", 1, true).unwrap();
    store
        .upsert_summaries("INBOX", &[summary(1, "Hi", "a@b.com", 1, false)])
        .unwrap();
    let loaded = store.load_summaries("INBOX").unwrap();
    assert!(loaded[0].seen, "pending read wins over stale server unread");

    // Once the server confirms, the pending entry is gone and the server
    // flag applies again.
    store.remove_pending_flag("INBOX", 1).unwrap();
    store
        .upsert_summaries("INBOX", &[summary(1, "Hi", "a@b.com", 1, false)])
        .unwrap();
    let loaded = store.load_summaries("INBOX").unwrap();
    assert!(!loaded[0].seen, "server value applies once pending is cleared");
}

#[test]
fn pending_seen_map_returns_only_folder_flags() {
    let (_dir, mut store) = test_store("pending-map");
    store.upsert_pending_flag("INBOX", 1, true).unwrap();
    store.upsert_pending_flag("INBOX", 2, false).unwrap();
    store.upsert_pending_flag("Archive", 3, true).unwrap();
    let map = store.pending_seen_map("INBOX").unwrap();
    assert_eq!(map.get(&1), Some(&true));
    assert_eq!(map.get(&2), Some(&false));
    assert!(!map.contains_key(&3));
}

// ---------------------------------------------------- body pre-caching

#[test]
fn store_bodies_caches_and_loads_bodies() {
    let (_dir, mut store) = test_store("bodies");
    store
        .upsert_summaries("INBOX", &[summary(1, "Hi", "a@b.com", 1, false)])
        .unwrap();

    // Nothing cached yet.
    assert!(store.load_body("INBOX", 1).unwrap().is_none());

    store
        .store_bodies(
            "INBOX",
            &[crate::mail::BatchBody {
                uid: 1,
                text: Some("plain".into()),
                html: Some("<p>html</p>".into()),
                attachments: vec![],
            }],
        )
        .unwrap();

    let (text, html) = store.load_body("INBOX", 1).unwrap().expect("cached body");
    assert_eq!(text.as_deref(), Some("plain"));
    assert_eq!(html.as_deref(), Some("<p>html</p>"));
}

#[test]
fn store_bodies_keeps_previous_body_when_updated() {
    let (_dir, mut store) = test_store("bodies-update");
    store
        .upsert_summaries("INBOX", &[summary(1, "Hi", "a@b.com", 1, false)])
        .unwrap();
    store
        .store_bodies("INBOX", &[crate::mail::BatchBody { uid: 1, text: Some("v1".into()), html: None, attachments: vec![] }])
        .unwrap();
    // A summary refresh must not wipe the cached body.
    store
        .upsert_summaries("INBOX", &[summary(1, "Hi", "a@b.com", 1, true)])
        .unwrap();
    let (text, _html) = store.load_body("INBOX", 1).unwrap().expect("body survives upsert");
    assert_eq!(text.as_deref(), Some("v1"));
}

// ---------------------------------------------------- pending deletes

#[test]
fn pending_delete_prevents_re_add_while_in_flight() {
    let (_dir, mut store) = test_store("pending-del");
    store
        .upsert_summaries("INBOX", &[summary(1, "Hi", "a@b.com", 1, false)])
        .unwrap();

    // Optimistic delete: remove + mark pending.
    store.delete_message("INBOX", 1).unwrap();
    store.mark_pending_delete("INBOX", 1).unwrap();
    assert!(store.load_summaries("INBOX").unwrap().is_empty());

    // A concurrent refresh still sees the message on the server — it must
    // NOT re-add it while the delete is in flight.
    store
        .upsert_summaries("INBOX", &[summary(1, "Hi", "a@b.com", 1, false)])
        .unwrap();
    assert!(store.load_summaries("INBOX").unwrap().is_empty());
}

#[test]
fn pending_delete_cleared_when_server_confirms() {
    let (_dir, mut store) = test_store("pending-del-confirm");
    store.mark_pending_delete("INBOX", 7).unwrap();
    assert!(store.pending_delete_uids("INBOX").unwrap().contains(&7));

    // Server reconcile: uid 7 is no longer on the server -> delete finished.
    let server: std::collections::HashSet<u32> = [].into_iter().collect();
    store.clear_pending_deletes_not_in("INBOX", &server).unwrap();
    assert!(!store.pending_delete_uids("INBOX").unwrap().contains(&7));

    // If still on the server (delete failed / still in flight), keep it.
    store.mark_pending_delete("INBOX", 8).unwrap();
    let server: std::collections::HashSet<u32> = [8].into_iter().collect();
    store.clear_pending_deletes_not_in("INBOX", &server).unwrap();
    assert!(store.pending_delete_uids("INBOX").unwrap().contains(&8));
}

#[test]
fn load_thread_groups_messages_by_thread_id() {
    let (_dir, mut store) = test_store("threads");
    // Three messages: two in one thread, one standalone.
    let mut m1 = summary(1, "Re: hello", "a@b.com", 1, false);
    m1.thread_id = "root@x".into();
    let mut m2 = summary(2, "hello", "c@d.com", 2, false);
    m2.thread_id = "root@x".into();
    let mut m3 = summary(3, "other", "e@f.com", 1, false);
    m3.thread_id = "other@y".into();
    store.upsert_summaries("INBOX", &[m1, m2, m3]).unwrap();

    // Reconciliation re-roots the thread to its oldest member id.
    let all = store.load_summaries("INBOX").unwrap();
    let t1 = all.iter().find(|m| m.uid == 1).unwrap().thread_id.clone();
    let t2 = all.iter().find(|m| m.uid == 2).unwrap().thread_id.clone();
    assert_eq!(t1, t2, "messages linked by a shared thread root resolve together");

    let thread = store.load_thread("INBOX", &t1).unwrap();
    assert_eq!(thread.len(), 2);

    let other = store.load_thread("INBOX", &t2).unwrap();
    assert_eq!(other.len(), 2);

    let solo = all.iter().find(|m| m.uid == 3).unwrap().thread_id.clone();
    assert_ne!(solo, t1, "standalone message stays in its own thread");
    assert_eq!(store.load_thread("INBOX", &solo).unwrap().len(), 1);
}

#[test]
fn reconciliation_links_messages_via_in_reply_to() {
    let (_dir, mut store) = test_store("resolve");
    // Original with its own id; a reply without References but with
    // In-Reply-To pointing at the original; a second reply referencing both.
    let mut orig = summary(1, "hello", "a@b.com", 5, false);
    orig.message_id = "root@x".into();
    orig.thread_id = "root@x".into();
    let mut r1 = summary(2, "Re: hello", "c@d.com", 2, false);
    r1.message_id = "r1@x".into();
    r1.in_reply_to = "<root@x>".into();
    r1.thread_id = "r1@x".into(); // pre-fix reply: no references, own id
    let mut r2 = summary(3, "Re: hello", "a@b.com", 1, false);
    r2.message_id = "r2@x".into();
    r2.references = "<root@x> <r1@x>".into();
    r2.thread_id = "root@x".into();
    store.upsert_summaries("INBOX", &[orig, r1, r2]).unwrap();

    let all = store.load_summaries("INBOX").unwrap();
    assert_eq!(all.len(), 3);
    // r1 (own-id thread) is pulled into the root thread via In-Reply-To.
    let ids: std::collections::HashSet<String> =
        all.iter().map(|m| m.thread_id.clone()).collect();
    assert_eq!(ids.len(), 1, "all three messages resolve to one thread");
    assert_eq!(all.iter().find(|m| m.uid == 1).unwrap().thread_id, "root@x");
    assert_eq!(store.load_thread("INBOX", "root@x").unwrap().len(), 3);
}

#[test]
fn store_bodies_caches_attachment_metadata() {
    let (_dir, mut store) = test_store("bodies-attachments");
    store
        .upsert_summaries("INBOX", &[summary(1, "With pics", "a@b.com", 1, false)])
        .unwrap();
    store
        .store_bodies(
            "INBOX",
            &[crate::mail::BatchBody {
                uid: 1,
                text: Some("hi".into()),
                html: None,
                attachments: vec![crate::store::AttachmentMeta {
                    filename: "photo.png".into(),
                    content_type: "image/png".into(),
                    size: 42,
                    part_id: "0".into(),
                    content_id: None,
                }],
            }],
        )
        .unwrap();
    let atts = store.load_attachments("INBOX", 1).unwrap();
    assert_eq!(atts.len(), 1);
    assert_eq!(atts[0].filename, "photo.png");
    assert_eq!(atts[0].content_type, "image/png");
}

#[test]
fn attachment_data_roundtrip_and_batch_load() {
    let (_dir, mut store) = test_store("att-data");
    store
        .upsert_summaries("INBOX", &[summary(1, "mail", "a@b.com", 1, false)])
        .unwrap();
    store
        .store_body(
            "INBOX",
            1,
            Some("b"),
            None,
            &[AttachmentMeta {
                filename: "a.png".into(),
                content_type: "image/png".into(),
                size: 100,
                part_id: "0".into(),
                content_id: Some("a@x".into()),
            }],
        )
        .unwrap();

    // Nothing cached yet.
    assert!(store.load_attachment_data("INBOX", 1, "0").unwrap().is_none());

    store
        .store_attachment_data("INBOX", 1, "0", b"png-bytes")
        .unwrap();
    assert_eq!(
        store.load_attachment_data("INBOX", 1, "0").unwrap(),
        Some(b"png-bytes".to_vec())
    );
    // Updating the same part replaces the bytes.
    store
        .store_attachment_data("INBOX", 1, "0", b"newer")
        .unwrap();
    assert_eq!(
        store.load_attachment_data("INBOX", 1, "0").unwrap(),
        Some(b"newer".to_vec())
    );

    // Batch load aligns with the requested part ids.
    let got = store
        .load_attachments_data("INBOX", 1, &["0".into(), "1".into(), "2".into()])
        .unwrap();
    assert_eq!(got, vec![Some(b"newer".to_vec()), None, None]);

    // Batch store.
    store
        .store_attachments_data(
            "INBOX",
            1,
            &[("1".into(), b"one".to_vec()), ("2".into(), b"two".to_vec())],
        )
        .unwrap();
    let got = store
        .load_attachments_data("INBOX", 1, &["0".into(), "1".into(), "2".into()])
        .unwrap();
    assert_eq!(
        got,
        vec![Some(b"newer".to_vec()), Some(b"one".to_vec()), Some(b"two".to_vec())]
    );

    // Content-id survives the meta round-trip through the store.
    let atts = store.load_attachments("INBOX", 1).unwrap();
    assert_eq!(atts[0].content_id.as_deref(), Some("a@x"));
}

#[test]
fn attachment_data_survives_body_refresh() {
    let (_dir, mut store) = test_store("att-data-refresh");
    store
        .upsert_summaries("INBOX", &[summary(1, "mail", "a@b.com", 1, false)])
        .unwrap();
    let atts = vec![AttachmentMeta {
        filename: "a.png".into(),
        content_type: "image/png".into(),
        size: 100,
        part_id: "0".into(),
        content_id: None,
    }];
    store.store_body("INBOX", 1, Some("b1"), None, &atts).unwrap();
    store.store_attachment_data("INBOX", 1, "0", b"bytes").unwrap();

    // Body warm-up rewrites the metadata rows but must not wipe cached data.
    store
        .store_bodies(
            "INBOX",
            &[crate::mail::BatchBody {
                uid: 1,
                text: Some("b2".into()),
                html: None,
                attachments: atts.clone(),
            }],
        )
        .unwrap();
    assert_eq!(
        store.load_attachment_data("INBOX", 1, "0").unwrap(),
        Some(b"bytes".to_vec())
    );
}

#[test]
fn attachment_data_pruned_when_part_disappears() {
    let (_dir, mut store) = test_store("att-data-prune");
    store
        .upsert_summaries("INBOX", &[summary(1, "mail", "a@b.com", 1, false)])
        .unwrap();
    let atts = vec![
        AttachmentMeta {
            filename: "a.png".into(),
            content_type: "image/png".into(),
            size: 1,
            part_id: "0".into(),
            content_id: None,
        },
        AttachmentMeta {
            filename: "b.pdf".into(),
            content_type: "application/pdf".into(),
            size: 2,
            part_id: "1".into(),
            content_id: None,
        },
    ];
    store.store_body("INBOX", 1, Some("b"), None, &atts).unwrap();
    store
        .store_attachments_data("INBOX", 1, &[("0".into(), b"a".to_vec()), ("1".into(), b"b".to_vec())])
        .unwrap();

    // Server message now only has one part: the other's cached bytes go away.
    store
        .store_body(
            "INBOX",
            1,
            Some("b"),
            None,
            &[AttachmentMeta {
                filename: "a.png".into(),
                content_type: "image/png".into(),
                size: 1,
                part_id: "0".into(),
                content_id: None,
            }],
        )
        .unwrap();
    assert_eq!(
        store.load_attachment_data("INBOX", 1, "0").unwrap(),
        Some(b"a".to_vec())
    );
    assert!(store.load_attachment_data("INBOX", 1, "1").unwrap().is_none());
}

#[test]
fn attachment_content_id_column_migrated_from_old_schema() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("migrated.db");
    // Simulate a database created by an older version: the attachments
    // table has no content_id column yet.
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(
        "CREATE TABLE messages (
            rowid_pk INTEGER PRIMARY KEY AUTOINCREMENT,
            folder TEXT NOT NULL, uid INTEGER NOT NULL,
            message_id TEXT NOT NULL DEFAULT '', refs TEXT NOT NULL DEFAULT '',
            in_reply_to TEXT NOT NULL DEFAULT '', thread_id TEXT NOT NULL DEFAULT '',
            content_hash TEXT NOT NULL DEFAULT '', subject TEXT, from_name TEXT,
            from_email TEXT, date INTEGER, seen INTEGER NOT NULL DEFAULT 0,
            has_attachment INTEGER NOT NULL DEFAULT 0, snippet TEXT,
            body_text TEXT, body_html TEXT);
         CREATE TABLE attachments (
            msg_row INTEGER NOT NULL REFERENCES messages(rowid_pk) ON DELETE CASCADE,
            part_id TEXT NOT NULL, filename TEXT, mime TEXT, size INTEGER);
         CREATE UNIQUE INDEX idx_messages_folder_uid ON messages(folder, uid);",
    )
    .unwrap();
    drop(conn);

    let mut store = Store::open_in(dir.path(), "migrated@test").unwrap();
    store
        .upsert_summaries("INBOX", &[summary(1, "m", "a@b.com", 1, false)])
        .unwrap();
    store
        .store_body(
            "INBOX",
            1,
            Some("b"),
            None,
            &[AttachmentMeta {
                filename: "x.png".into(),
                content_type: "image/png".into(),
                size: 1,
                part_id: "0".into(),
                content_id: Some("cid@x".into()),
            }],
        )
        .unwrap();
    let atts = store.load_attachments("INBOX", 1).unwrap();
    assert_eq!(atts[0].content_id.as_deref(), Some("cid@x"));
    // The attachment-data cache table was created by the migration too.
    store.store_attachment_data("INBOX", 1, "0", b"bytes").unwrap();
    assert_eq!(
        store.load_attachment_data("INBOX", 1, "0").unwrap(),
        Some(b"bytes".to_vec())
    );
}

#[test]
fn message_id_known_detects_cached_content_across_folders() {
    let (_dir, mut store) = test_store("mid-known");
    store
        .upsert_summaries("Trash", &[summary(1, "old mail", "a@b.com", 3, true)])
        .unwrap();
    assert!(
        store.message_id_known("mid-1@test").unwrap(),
        "moved message's Message-ID should be known from the Trash cache"
    );
    // A genuinely new message has a fresh Message-ID.
    assert!(!store.message_id_known("brand-new@test").unwrap());
    // Empty ids never match.
    assert!(!store.message_id_known("").unwrap());
}

#[test]
fn pending_delete_hides_row_from_lists_but_load_summary_by_uid_finds_it() {
    let (_dir, mut store) = test_store("pending-hide");
    store
        .upsert_summaries("INBOX", &[summary(1, "Hi", "a@b.com", 1, false)])
        .unwrap();
    // Optimistic delete keeps the row but shields it from lists, so the
    // background delete can relocate it to Trash afterwards.
    store.mark_pending_delete("INBOX", 1).unwrap();
    assert!(
        store.load_summaries("INBOX").unwrap().is_empty(),
        "pending-deleted row must not appear in lists"
    );
    assert!(
        store.load_summary_by_uid("INBOX", 1).unwrap().is_some(),
        "row must still be loadable by uid for relocation"
    );
}

#[test]
fn relocate_message_moves_row_to_destination_with_new_uid() {
    let (_dir, mut store) = test_store("relocate");
    store
        .upsert_summaries("INBOX", &[summary(1, "Hi", "a@b.com", 1, false)])
        .unwrap();
    store.mark_pending_delete("INBOX", 1).unwrap();

    store.relocate_message("INBOX", 1, "Trash", 99).unwrap();

    assert!(store.load_summaries("INBOX").unwrap().is_empty());
    let trash = store.load_summaries("Trash").unwrap();
    assert_eq!(trash.len(), 1, "deleted message must land in Trash's cache");
    assert_eq!(trash[0].uid, 99);
    assert_eq!(trash[0].subject, "Hi");
    assert_eq!(trash[0].message_id, "mid-1@test", "Message-ID survives the move");
}
