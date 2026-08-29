//! End-to-end integration tests: the app's real mail layer (mail.rs) running
//! against an in-process fake IMAP server (fake_imap.rs).
//!
//! The fake server speaks plaintext TCP on 127.0.0.1. mail.rs is wired so
//! that under #[cfg(test)] a localhost host uses ConnectionMode::Plaintext
//! instead of TLS, letting every production function run against the fake
//! server unchanged.
//!
//! Passwords are sealed through the real crypto path (temp config dir, see
//! crypto_tests::with_config_dir) because mail::imap_session unseals before
//! logging in.

use crate::account::AccountConfig;
use crate::crypto_tests::with_config_dir;
use crate::fake_imap::{spawn, FakeMailboxState, FakeMessage};
use std::sync::mpsc;
use tauri::async_runtime::block_on;

/// Spawn a fake server and build an account pointing at it. The password is
/// sealed with the real crypto path, so the whole login pipeline runs.
fn test_account(state: &FakeMailboxState, name: &str) -> AccountConfig {
    state.set_credentials("tester", "pw");
    let port = spawn(state.clone());
    AccountConfig {
        name: name.into(),
        email: format!("{name}@test.local"),
        imap_host: "127.0.0.1".into(),
        imap_port: port,
        smtp_host: "127.0.0.1".into(),
        smtp_port: port,
        username: "tester".into(),
        password: crate::crypto::seal_password("pw").expect("seal test password"),
        smtp_username: None,
        smtp_password: None,
        smtp_starttls: false,
    }
}

/// Collect everything the streaming callback pushed into the channel.
fn drain_batches(
    rx: &mpsc::Receiver<Vec<crate::mail::MessageSummary>>,
) -> Vec<crate::mail::MessageSummary> {
    let mut out = Vec::new();
    loop {
        match rx.recv_timeout(std::time::Duration::from_millis(200)) {
            Ok(batch) => out.extend(batch),
            Err(_) => break,
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Session plumbing
// ---------------------------------------------------------------------------

#[test]
fn login_logout_roundtrip() {
    with_config_dir(|_| {
        let state = FakeMailboxState::new();
        state.set_credentials("tester", "pw");
        let port = spawn(state);
        let mut session = crate::mail::imap_session_insecure("127.0.0.1", port, "tester", "pw")
            .expect("login against fake server");
        session.logout().expect("logout");
    });
}

#[test]
fn login_with_wrong_credentials_fails() {
    with_config_dir(|_| {
        let state = FakeMailboxState::new();
        state.set_credentials("tester", "pw");
        let port = spawn(state);
        let err = crate::mail::imap_session_insecure("127.0.0.1", port, "nobody", "bad")
            .expect_err("bad credentials must be rejected");
        assert!(!err.is_empty());
    });
}

#[test]
fn test_imap_verifies_credentials() {
    with_config_dir(|_| {
        let state = FakeMailboxState::new();
        let acc = test_account(&state, "Creds");
        block_on(crate::mail::test_imap(&acc)).expect("test_imap should succeed");
    });
}

// ---------------------------------------------------------------------------
// Folder listing
// ---------------------------------------------------------------------------

#[test]
fn list_folders_returns_sorted_folders_with_unread_counts() {
    with_config_dir(|_| {
        let state = FakeMailboxState::new();
        state.add_folder("Sent");
        state.add_folder("Trash");
        state.add_message("INBOX", FakeMessage::new(0, "unseen", "a@b.com", 1, false));
        state.add_message("INBOX", FakeMessage::new(0, "seen", "a@b.com", 2, true));
        state.add_message("Sent", FakeMessage::new(0, "sent mail", "me@me.com", 3, true));

        let acc = test_account(&state, "Folders");
        let folders = block_on(crate::mail::list_folders(&acc)).expect("list_folders");

        let names: Vec<&str> = folders.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, vec!["INBOX", "Sent", "Trash"], "folders sorted by name");
        let inbox = folders.iter().find(|f| f.name == "INBOX").unwrap();
        assert_eq!(inbox.unread, 1, "one unseen message in INBOX");
        let sent = folders.iter().find(|f| f.name == "Sent").unwrap();
        assert_eq!(sent.unread, 0, "only seen messages in Sent");
    });
}

#[test]
fn list_folders_empty_server_returns_inbox() {
    with_config_dir(|_| {
        let state = FakeMailboxState::new();
        let acc = test_account(&state, "OnlyInbox");
        let folders = block_on(crate::mail::list_folders(&acc)).expect("list_folders");
        assert_eq!(folders.len(), 1);
        assert_eq!(folders[0].name, "INBOX");
        assert_eq!(folders[0].unread, 0);
    });
}

// ---------------------------------------------------------------------------
// Message listing (streaming)
// ---------------------------------------------------------------------------

#[test]
fn list_messages_streams_summaries_newest_first() {
    with_config_dir(|_| {
        let state = FakeMailboxState::new();
        state.add_message("INBOX", FakeMessage::new(0, "Oldest", "old@x.org", 3, false));
        state.add_message("INBOX", FakeMessage::new(0, "Middle", "mid@x.org", 2, true));
        state.add_message("INBOX", FakeMessage::new(0, "Newest", "new@x.org", 1, false));

        let acc = test_account(&state, "Stream");
        let (tx, rx) = mpsc::channel();
        let reconciled = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u32>::new()));
        let reconciled2 = reconciled.clone();
        block_on(crate::mail::list_messages_streamed(
            &acc,
            "INBOX",
            move |batch, _bodies| {
                let _ = tx.send(batch);
            },
            move |uids| {
                let mut v: Vec<u32> = uids.into_iter().collect();
                v.sort();
                *reconciled2.lock().unwrap() = v;
            },
        ))
        .expect("stream");

        let all = drain_batches(&rx);
        assert_eq!(all.len(), 3, "every summary streamed");

        let subjects: Vec<&str> = all.iter().map(|m| m.subject.as_str()).collect();
        assert_eq!(subjects, vec!["Newest", "Middle", "Oldest"], "newest first");
        assert_eq!(all[0].uid, 3);
        assert!(!all[0].seen, "Newest is unseen");
        assert!(all[1].seen, "Middle is seen");
        assert_eq!(all[2].from, "old@x.org");
        assert!(all[2].snippet.contains("Oldest"), "snippet from body");

        // Reconcile callback must report every uid still on the server.
        assert_eq!(*reconciled.lock().unwrap(), vec![1, 2, 3]);
    });
}

#[test]
fn list_messages_reconciles_deleted_uids() {
    with_config_dir(|_| {
        let state = FakeMailboxState::new();
        state.add_message("INBOX", FakeMessage::new(0, "A", "a@b.com", 2, false));
        state.add_message("INBOX", FakeMessage::new(0, "B", "a@b.com", 1, false));
        let acc = test_account(&state, "Reconcile");

        // Delete B on the server (as another client would).
        {
            let mut boxes = state.mailboxes.lock().unwrap();
            let mb = boxes.get_mut("INBOX").unwrap();
            mb.messages.retain(|m| m.uid != 2);
        }

        let (tx, _rx) = mpsc::channel();
        let reconciled = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u32>::new()));
        let reconciled2 = reconciled.clone();
        block_on(crate::mail::list_messages_streamed(
            &acc,
            "INBOX",
            move |batch, _bodies| {
                let _ = tx.send(batch);
            },
            move |uids| {
                *reconciled2.lock().unwrap() = uids.into_iter().collect();
            },
        ))
        .expect("stream");

        assert_eq!(*reconciled.lock().unwrap(), vec![1], "uid 2 no longer exists");
    });
}

#[test]
fn list_messages_empty_mailbox_streams_nothing() {
    with_config_dir(|_| {
        let state = FakeMailboxState::new();
        let acc = test_account(&state, "EmptyInbox");
        let (tx, rx) = mpsc::channel();
        block_on(crate::mail::list_messages_streamed(
            &acc,
            "INBOX",
            move |batch, _bodies| {
                let _ = tx.send(batch);
            },
            |_| {},
        ))
        .expect("stream empty mailbox");
        assert!(drain_batches(&rx).is_empty());
    });
}

#[test]
fn list_messages_empty_mailbox_still_reconciles() {
    with_config_dir(|_| {
        let state = FakeMailboxState::new();
        state.add_message("INBOX", FakeMessage::new(0, "A", "a@b.com", 1, false));
        let acc = test_account(&state, "ReconcileEmpty");

        // First sync reports the one uid on the server.
        let (tx, _rx) = mpsc::channel();
        let reconciled = std::sync::Arc::new(std::sync::Mutex::new(None::<Vec<u32>>));
        let r1 = reconciled.clone();
        block_on(crate::mail::list_messages_streamed(
            &acc,
            "INBOX",
            move |batch, _bodies| {
                let _ = tx.send(batch);
            },
            move |uids| {
                *r1.lock().unwrap() = Some(uids.into_iter().collect());
            },
        ))
        .expect("first stream");
        assert_eq!(*reconciled.lock().unwrap(), Some(vec![1]));

        // Empty the mailbox on the server, as another client deleting
        // everything would.
        {
            let mut boxes = state.mailboxes.lock().unwrap();
            boxes.get_mut("INBOX").unwrap().messages.clear();
        }

        // The second sync must still fire the reconcile callback with an
        // empty server uid set, so stale cache rows get pruned.
        let (tx2, _rx2) = mpsc::channel();
        let reconciled2 = std::sync::Arc::new(std::sync::Mutex::new(None::<Vec<u32>>));
        let r2 = reconciled2.clone();
        block_on(crate::mail::list_messages_streamed(
            &acc,
            "INBOX",
            move |batch, _bodies| {
                let _ = tx2.send(batch);
            },
            move |uids| {
                *r2.lock().unwrap() = Some(uids.into_iter().collect());
            },
        ))
        .expect("second stream");
        assert_eq!(
            *reconciled2.lock().unwrap(),
            Some(Vec::<u32>::new()),
            "reconcile must fire with an empty server uid set"
        );
    });
}

// ---------------------------------------------------------------------------
// Message bodies + attachments
// ---------------------------------------------------------------------------

#[test]
fn fetch_message_returns_parsed_plain_text_body() {
    with_config_dir(|_| {
        let state = FakeMailboxState::new();
        state.add_message("INBOX", FakeMessage::new(0, "Hello", "a@b.com", 1, false));
        let acc = test_account(&state, "Fetch");

        let fetched = block_on(crate::mail::fetch_message_full(&acc, "INBOX", 1)).expect("fetch");
        let body = fetched.body;
        assert_eq!(body.uid, 1);
        let text = body.text.expect("plain text part");
        assert!(text.contains("Body of 'Hello'"));
        assert!(text.contains("Line two"));
        // mail-parser synthesizes an HTML view for a plain-text body.
        let html = body.html.expect("html view synthesized from plain text");
        assert!(html.contains("<html><body>"), "html is a generated document");
        assert!(html.contains("Body of 'Hello'"), "html contains the body text");
    });
}

#[test]
fn fetch_message_for_missing_uid_errors() {
    with_config_dir(|_| {
        let state = FakeMailboxState::new();
        let acc = test_account(&state, "Missing");
        let err = block_on(crate::mail::fetch_message_full(&acc, "INBOX", 99))
            .expect_err("missing uid must error");
        assert!(err.contains("not found"), "unexpected error: {err}");
    });
}

#[test]
fn fetch_message_unknown_folder_errors() {
    with_config_dir(|_| {
        let state = FakeMailboxState::new();
        let acc = test_account(&state, "NoFolder");
        let err = block_on(crate::mail::fetch_message_full(&acc, "DoesNotExist", 1))
            .expect_err("unknown folder must error");
        assert!(!err.is_empty());
    });
}

#[test]
fn fetch_message_full_parses_mime_attachment() {
    with_config_dir(|_| {
        let state = FakeMailboxState::new();
        state.add_message(
            "INBOX",
            FakeMessage::new(0, "With file", "a@b.com", 1, false).with_attachment("report.pdf"),
        );
        let acc = test_account(&state, "Att");

        let fetched = block_on(crate::mail::fetch_message_full(&acc, "INBOX", 1))
            .expect("fetch");
        let atts = fetched.attachments;
        assert_eq!(atts.len(), 1);
        assert_eq!(atts[0].filename, "report.pdf");
        assert_eq!(atts[0].content_type, "application/pdf");
        assert!(atts[0].size > 0);
    });
}

#[test]
fn summary_reports_has_attachment_from_bodystructure() {
    with_config_dir(|_| {
        let state = FakeMailboxState::new();
        state.add_message(
            "INBOX",
            FakeMessage::new(0, "No file", "a@b.com", 2, false),
        );
        state.add_message(
            "INBOX",
            FakeMessage::new(0, "Has file", "a@b.com", 1, false).with_attachment("x.pdf"),
        );
        let acc = test_account(&state, "AttFlag");

        let (tx, rx) = mpsc::channel();
        block_on(crate::mail::list_messages_streamed(
            &acc,
            "INBOX",
            move |batch, _bodies| {
                let _ = tx.send(batch);
            },
            |_| {},
        ))
        .expect("stream");

        let all = drain_batches(&rx);
        let has = all.iter().find(|m| m.subject == "Has file").unwrap();
        let none = all.iter().find(|m| m.subject == "No file").unwrap();
        assert!(has.has_attachment, "attachment detected via BODYSTRUCTURE");
        assert!(!none.has_attachment);
    });
}

// ---------------------------------------------------------------------------
// Flag changes, move, delete
// ---------------------------------------------------------------------------

#[test]
fn set_seen_toggles_server_flag() {
    with_config_dir(|_| {
        let state = FakeMailboxState::new();
        state.add_message("INBOX", FakeMessage::new(0, "Read me", "a@b.com", 1, false));
        let acc = test_account(&state, "Seen");
        assert!(!state.is_seen("INBOX", 1), "starts unseen");

        let unseen = block_on(crate::mail::set_seen(&acc, "INBOX", 1, true)).expect("mark seen");
        assert_eq!(unseen, 0, "the only message is now seen");
        assert!(state.is_seen("INBOX", 1), "\\Seen set on the server");

        let unseen = block_on(crate::mail::set_seen(&acc, "INBOX", 1, false)).expect("mark unseen");
        assert_eq!(unseen, 1, "marked unseen again -> 1 unseen");
        assert!(!state.is_seen("INBOX", 1), "\\Seen cleared on the server");
    });
}

#[test]
fn set_seen_unknown_folder_errors() {
    with_config_dir(|_| {
        let state = FakeMailboxState::new();
        let acc = test_account(&state, "SeenNoFolder");
        let err = block_on(crate::mail::set_seen(&acc, "Nope", 1, true))
            .expect_err("unknown folder must error");
        assert!(!err.is_empty());
    });
}

#[test]
fn move_message_copies_to_destination_and_removes_source() {
    with_config_dir(|_| {
        let state = FakeMailboxState::new();
        state.add_folder("Trash");
        state.add_message("INBOX", FakeMessage::new(0, "Move me", "a@b.com", 1, false));
        let acc = test_account(&state, "Move");

        block_on(crate::mail::move_message(&acc, "INBOX", 1, "Trash")).expect("move");

        assert_eq!(state.count("INBOX"), 0, "source expunged after move");
        assert_eq!(state.count("Trash"), 1, "copy landed in destination");
        assert!(!state.is_seen("Trash", 1), "copy is not marked seen");
        assert!(state.uid_exists("Trash", 1));
    });
}

#[test]
fn move_message_leaves_other_messages_alone() {
    with_config_dir(|_| {
        let state = FakeMailboxState::new();
        state.add_folder("Archive");
        state.add_message("INBOX", FakeMessage::new(0, "Move me", "a@b.com", 2, false));
        state.add_message("INBOX", FakeMessage::new(0, "Keep me", "a@b.com", 1, false));
        let acc = test_account(&state, "MoveOne");

        block_on(crate::mail::move_message(&acc, "INBOX", 1, "Archive")).expect("move");

        assert_eq!(state.count("INBOX"), 1, "the other message stays");
        assert!(state.uid_exists("INBOX", 2));
        assert_eq!(state.count("Archive"), 1);
        assert!(state.uid_exists("Archive", 1));
    });
}

#[test]
fn delete_message_expunges_from_server() {
    with_config_dir(|_| {
        let state = FakeMailboxState::new();
        state.add_message("INBOX", FakeMessage::new(0, "Delete me", "a@b.com", 1, false));
        let acc = test_account(&state, "Delete");

        block_on(crate::mail::delete_message(&acc, "INBOX", 1)).expect("delete");
        assert_eq!(state.count("INBOX"), 0, "message expunged");
    });
}

// ---------------------------------------------------------------------------
// Cross-check with a raw session: the fake server must agree with what the
// app's streaming path reports, catching fake-server inconsistencies.
// ---------------------------------------------------------------------------

#[test]
fn raw_session_sees_same_messages_as_streaming() {
    with_config_dir(|_| {
        let state = FakeMailboxState::new();
        state.add_message("INBOX", FakeMessage::new(0, "S1", "a@b.com", 1, false));
        state.add_message("INBOX", FakeMessage::new(0, "S2", "b@b.com", 2, true));
        let acc = test_account(&state, "CrossCheck");
        let port = acc.imap_port;

        // App path.
        let (tx, rx) = mpsc::channel();
        block_on(crate::mail::list_messages_streamed(
            &acc,
            "INBOX",
            move |batch, _bodies| {
                let _ = tx.send(batch);
            },
            |_| {},
        ))
        .expect("stream");
        let app_uids: Vec<u32> = drain_batches(&rx).iter().map(|m| m.uid).collect();

        // Raw path (independent connection + UID SEARCH).
        let mut session =
            crate::mail::imap_session_insecure("127.0.0.1", port, "tester", "pw").expect("login");
        session.select("INBOX").expect("select");
        let raw_uids = session.uid_search("ALL").expect("uid search");

        let app_set: std::collections::HashSet<u32> = app_uids.into_iter().collect();
        assert_eq!(app_set, raw_uids, "app and raw session agree on uids");
    });
}


// ---------------------------------------------------------------------------
// New-mail notification preview
// ---------------------------------------------------------------------------

#[test]
fn fetch_envelope_preview_returns_plain_from_and_subject() {
    with_config_dir(|_| {
        let state = FakeMailboxState::new();
        state.add_message(
            "INBOX",
            FakeMessage::new(1, "Plain subject", "sender@example.com", 1, false),
        );
        let acc = test_account(&state, "Preview");
        let mut session = crate::mail::imap_session_pub(&acc).expect("session");
        session.select("INBOX").expect("select");

        let (from, subject) =
            crate::mail::fetch_envelope_preview(&mut session, 1).expect("preview");
        assert!(from.contains("sender@example.com"));
        assert_eq!(subject, "Plain subject");
    });
}

#[test]
fn fetch_envelope_preview_decodes_rfc2047_and_strips_markup() {
    with_config_dir(|_| {
        let state = FakeMailboxState::new();
        // The fake server writes headers verbatim, so encoded words and
        // markup survive into the raw message exactly like a real server.
        let mut msg = FakeMessage::new(1, "=?UTF-8?Q?Na=C3=AFve?=", "sender@example.com", 1, false);
        msg.from_name = "=?UTF-8?B?SsO2aG4=?=".into(); // "Jöhn"
        state.add_message("INBOX", msg);
        let acc = test_account(&state, "PreviewEncoded");
        let mut session = crate::mail::imap_session_pub(&acc).expect("session");
        session.select("INBOX").expect("select");

        let (from, subject) =
            crate::mail::fetch_envelope_preview(&mut session, 1).expect("preview");
        assert!(from.contains("Jöhn"), "decoded display name, got: {from}");
        assert!(from.contains("sender@example.com"));
        assert_eq!(subject, "Naïve", "decoded subject");
    });
}

#[test]
fn fetch_envelope_preview_strips_html_from_subject() {
    with_config_dir(|_| {
        let state = FakeMailboxState::new();
        state.add_message(
            "INBOX",
            FakeMessage::new(1, "<b>Bold &amp; clear</b>", "sender@example.com", 1, false),
        );
        let acc = test_account(&state, "PreviewHtml");
        let mut session = crate::mail::imap_session_pub(&acc).expect("session");
        session.select("INBOX").expect("select");

        let (_from, subject) =
            crate::mail::fetch_envelope_preview(&mut session, 1).expect("preview");
        assert_eq!(subject, "Bold & clear", "markup stripped and entities decoded");
    });
}

#[test]
fn fetch_envelope_preview_missing_uid_errors() {
    with_config_dir(|_| {
        let state = FakeMailboxState::new();
        let acc = test_account(&state, "PreviewMissing");
        let mut session = crate::mail::imap_session_pub(&acc).expect("session");
        session.select("INBOX").expect("select");
        let err = crate::mail::fetch_envelope_preview(&mut session, 99)
            .expect_err("missing uid must error");
        assert!(err.contains("not found"), "unexpected error: {err}");
    });
}

#[test]
fn fetch_envelope_preview_decodes_real_world_github_subject() {
    with_config_dir(|_| {
        let state = FakeMailboxState::new();
        // Mirrors a real GitHub-style notification: the raw header carries
        // the special char as an RFC 2047 Q-encoded word (versiòne =
        // versi=C3=B2ne). Uses a generic repo/recipient so no real user or
        // repository is referenced in the tests.
        let mut msg = FakeMessage::new(
            1,
            "[testuser/testmessage] Release v1.9.0 - =?utf-8?Q?versi=C3=B2ne?= v1.9.0",
            "notifications@example.com",
            1,
            false,
        );
        msg.from_name = "Test Sender".into();
        state.add_message("INBOX", msg);
        let acc = test_account(&state, "PreviewGithub");
        let mut session = crate::mail::imap_session_pub(&acc).expect("session");
        session.select("INBOX").expect("select");

        let (from, subject) =
            crate::mail::fetch_envelope_preview(&mut session, 1).expect("preview");
        assert_eq!(subject, "[testuser/testmessage] Release v1.9.0 - versiòne v1.9.0");
        assert!(from.contains("Test Sender"));
        assert!(from.contains("notifications@example.com"));
    });
}

// ---------------------------------------------------------------------------
// Attachment MIME guessing
// ---------------------------------------------------------------------------

#[test]
fn content_type_for_guesses_common_extensions() {
    use lettre::message::header::ContentType;
    let ct = |name: &str| crate::mail::content_type_for(name);
    assert_eq!(ct("report.pdf"), ContentType::parse("application/pdf").unwrap());
    assert_eq!(ct("photo.PNG"), ContentType::parse("image/png").unwrap());
    assert_eq!(ct("notes.txt"), ContentType::parse("text/plain").unwrap());
    assert_eq!(ct("doc.docx"), ContentType::parse("application/vnd.openxmlformats-officedocument.wordprocessingml.document").unwrap());
    // Unknown / missing extensions fall back to octet-stream.
    assert_eq!(ct("archive.xyz"), ContentType::parse("application/octet-stream").unwrap());
    assert_eq!(ct("README"), ContentType::parse("application/octet-stream").unwrap());
}

// ---------------------------------------------------------------------------
// Image attachment previews
// ---------------------------------------------------------------------------

#[test]
fn list_attachments_reports_image_content_types() {
    with_config_dir(|_| {
        let state = FakeMailboxState::new();
        let msg = FakeMessage::new(1, "With pics", "a@b.com", 1, false)
            .with_image_attachment("photo.png", "image/png", "iVBORw0KGgo=");
        state.add_message("INBOX", msg);
        let acc = test_account(&state, "ImgAtt");

        let atts = crate::mail::attachment_meta(&acc, "INBOX", 1)
            .expect("attachments");
        assert_eq!(atts.len(), 1);
        assert_eq!(atts[0].filename, "photo.png");
        assert_eq!(atts[0].content_type, "image/png");
        assert!(atts[0].size > 0);
    });
}

#[test]
fn get_attachment_data_returns_decodable_base64() {
    with_config_dir(|_| {
        let state = FakeMailboxState::new();
        let msg = FakeMessage::new(1, "With pics", "a@b.com", 1, false)
            .with_image_attachment("photo.png", "image/png", "iVBORw0KGgo=");
        state.add_message("INBOX", msg);
        let acc = test_account(&state, "ImgData");

        let atts = crate::mail::attachment_meta(&acc, "INBOX", 1).expect("atts");
        let part_id = &atts[0].part_id;
        let part_index: usize = part_id.parse().unwrap();
        let bytes = block_on(async {
            let acc = acc.clone();
            tauri::async_runtime::spawn_blocking(move || {
                crate::mail::fetch_attachment_part(&acc, "INBOX", 1, part_index)
            })
            .await
        })
        .expect("join")
        .expect("data");
        // "iVBORw0KGgo=" base64-decodes to the PNG magic bytes \x89PNG\r\n\x1a\n.
        assert_eq!(bytes, b"\x89PNG\r\n\x1a\n");
    });
}

#[test]
fn fetch_attachment_parts_batch_fetches_all_in_one_roundtrip() {
    with_config_dir(|_| {
        let state = FakeMailboxState::new();
        let msg = FakeMessage::new(1, "Two pics", "a@b.com", 1, false)
            .with_image_attachment("one.png", "image/png", "iVBORw0KGgo=")
            .add_image_attachment("two.jpg", "image/jpeg", "/9j/4AAQSkZJRg==");
        state.add_message("INBOX", msg);
        let acc = test_account(&state, "BatchData");

        let atts = crate::mail::attachment_meta(&acc, "INBOX", 1).expect("atts");
        assert_eq!(atts.len(), 2);
        let indexes: Vec<usize> = atts.iter().map(|a| a.part_id.parse().unwrap()).collect();
        let datas = block_on(async {
            let acc = acc.clone();
            tauri::async_runtime::spawn_blocking(move || {
                crate::mail::fetch_attachment_parts(&acc, "INBOX", 1, &indexes)
            })
            .await
        })
        .expect("join")
        .expect("data");
        assert_eq!(datas.len(), 2);
        assert_eq!(datas[0], b"\x89PNG\r\n\x1a\n");
        assert_eq!(datas[1], b"\xff\xd8\xff\xe0\x00\x10JFIF");
    });
}

#[test]
fn embedded_image_reports_content_id() {
    with_config_dir(|_| {
        let state = FakeMailboxState::new();
        let msg = FakeMessage::new(1, "Embedded pic", "a@b.com", 1, false)
            .with_embedded_image("logo.png", "image/png", "iVBORw0KGgo=", "logo123@example.com");
        state.add_message("INBOX", msg);
        let acc = test_account(&state, "CidAtt");

        let atts = crate::mail::attachment_meta(&acc, "INBOX", 1).expect("atts");
        assert_eq!(atts.len(), 1);
        assert_eq!(atts[0].filename, "logo.png");
        assert_eq!(atts[0].content_type, "image/png");
        // Angle brackets are stripped from the header value.
        assert_eq!(atts[0].content_id.as_deref(), Some("logo123@example.com"));
    });
}
