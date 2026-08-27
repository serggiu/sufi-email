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
        },
        AttachmentMeta {
            filename: "b.png".into(),
            content_type: "image/png".into(),
            size: 200,
            part_id: "1".into(),
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
    }];
    store.store_body("INBOX", 1, Some("b"), None, &v1).unwrap();

    let v2 = vec![AttachmentMeta {
        filename: "new.txt".into(),
        content_type: "text/plain".into(),
        size: 2,
        part_id: "0".into(),
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
