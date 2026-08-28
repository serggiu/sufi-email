//! A minimal in-process IMAP server for integration tests.
//!
//! Implements just enough of RFC 3501 for the client operations the app
//! performs: greeting, LOGIN, LIST, SELECT, STATUS, SEARCH (ALL/UNSEEN),
//! FETCH (FLAGS/ENVELOPE/BODY[]), UID FETCH, UID STORE, UID COPY, UID
//! EXPUNGE, EXPUNGE, LOGOUT, CAPABILITY, NOOP.
//!
//! State is held in a shared `FakeMailboxState` so tests can assert on
//! server-side effects (e.g. that a move really copied + deleted).

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

/// Log line only when FAKE_IMAP_DEBUG is set (keeps test output quiet).
macro_rules! debug_log {
    ($($arg:tt)*) => {
        if std::env::var("FAKE_IMAP_DEBUG").is_ok() {
            eprintln!("[fake-imap] {}", format!($($arg)*));
        }
    };
}

#[derive(Debug, Clone)]
pub struct FakeMessage {
    pub uid: u32,
    pub flags: Vec<String>,
    pub subject: String,
    pub from_name: String,
    pub from_email: String,
    pub date: String,
    pub body: String,
    /// Attachment filename when the message carries one (MIME multipart).
    pub attachment: Option<String>,
}

impl FakeMessage {
    pub fn new(uid: u32, subject: &str, from_email: &str, days_ago: i64, seen: bool) -> Self {
        let ts = 1_700_000_000 - days_ago * 86_400;
        let date = chrono::DateTime::<Utc>::from_timestamp(ts, 0)
            .unwrap()
            .format("%a, %d %b %Y %H:%M:%S +0000")
            .to_string();
        FakeMessage {
            uid,
            flags: if seen { vec!["\\Seen".into()] } else { vec![] },
            subject: subject.into(),
            from_name: String::new(),
            from_email: from_email.into(),
            date,
            body: format!("Body of '{subject}'\nLine two.\n"),
            attachment: None,
        }
    }

    /// Turn this message into a proper MIME multipart/mixed message with one
    /// text part plus one `application/pdf` attachment. The raw() output is
    /// then parseable by mail-parser, and BODYSTRUCTURE reports the parts.
    pub fn with_attachment(mut self, filename: &str) -> Self {
        self.body = format!(
            "--BOUND\r\nContent-Type: text/plain; charset=\"UTF-8\"\r\nContent-Transfer-Encoding: 7bit\r\n\r\n{}\r\n--BOUND\r\nContent-Type: application/pdf; name=\"{f}\"\r\nContent-Disposition: attachment; filename=\"{f}\"\r\nContent-Transfer-Encoding: base64\r\n\r\nJVBERi0=\r\n--BOUND--\r\n",
            self.body.replace('\n', "\r\n"),
            f = filename
        );
        self.attachment = Some(filename.into());
        self
    }

    fn envelope(&self) -> String {
        // All 10 envelope fields; empty name must be NIL, not "".
        format!(
            "(\"{}\" \"{}\" ((NIL NIL \"{}\" NIL)) ((NIL NIL \"{}\" NIL)) ((NIL NIL \"{}\" NIL)) NIL NIL NIL NIL \"<fake-{}@test>\")",
            self.date,
            self.subject.replace('"', "'").replace('\r', "").replace('\n', " "),
            self.from_email,
            self.from_email,
            self.from_email,
            self.uid
        )
    }

    fn raw(&self) -> Vec<u8> {
        let mut out = format!(
            "From: {} <{}>\r\nSubject: {}\r\nDate: {}\r\nMessage-ID: <fake-{}@test>\r\n",
            self.from_name, self.from_email, self.subject, self.date, self.uid
        );
        if self.attachment.is_some() {
            out.push_str("MIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=\"BOUND\"\r\n");
        }
        out.push_str("\r\n");
        out.push_str(&self.body);
        out.into_bytes()
    }

    /// BODYSTRUCTURE response for this message: a multipart/mixed structure
    /// when it has an attachment, otherwise a plain TEXT part.
    /// Note: multipart bodies are `(part1)(part2) "SUBTYPE" ...` with no
    /// space between parts (imap-proto's parser requires that).
    fn bodystructure(&self) -> String {
        if let Some(fname) = &self.attachment {
            format!(
                "((\"TEXT\" \"PLAIN\" (\"CHARSET\" \"UTF-8\") NIL NIL \"7BIT\" 20 2)\
                 (\"APPLICATION\" \"PDF\" (\"NAME\" \"{fname}\") NIL NIL \"BASE64\" 8 NIL \
                 (\"ATTACHMENT\" (\"FILENAME\" \"{fname}\")) NIL NIL) \"MIXED\" (\"BOUNDARY\" \"BOUND\") NIL NIL NIL)"
            )
        } else {
            "(\"TEXT\" \"PLAIN\" (\"CHARSET\" \"US-ASCII\") NIL NIL \"7BIT\" 100 3)".to_string()
        }
    }
}

#[derive(Default)]
pub struct Mailbox {
    pub messages: Vec<FakeMessage>,
    pub uid_validity: u32,
}

impl Mailbox {
    fn next_uid(&self) -> u32 {
        self.messages.iter().map(|m| m.uid).max().unwrap_or(0) + 1
    }
}

/// Shared, mutable server state visible to tests.
#[derive(Clone, Default)]
pub struct FakeMailboxState {
    pub mailboxes: Arc<Mutex<HashMap<String, Mailbox>>>,
    /// Accepted LOGIN credentials; `None` accepts anything (convenience).
    credentials: Arc<Mutex<Option<(String, String)>>>,
}

impl FakeMailboxState {
    pub fn new() -> Self {
        let mut mailboxes = HashMap::new();
        mailboxes.insert("INBOX".into(), Mailbox { uid_validity: 1, messages: Vec::new() });
        FakeMailboxState {
            mailboxes: Arc::new(Mutex::new(mailboxes)),
            credentials: Arc::new(Mutex::new(None)),
        }
    }

    /// Require a specific username/password for LOGIN from now on.
    pub fn set_credentials(&self, username: &str, password: &str) {
        *self.credentials.lock().unwrap() = Some((username.into(), password.into()));
    }

    pub fn add_folder(&self, name: &str) {
        self.mailboxes
            .lock()
            .unwrap()
            .insert(name.into(), Mailbox { uid_validity: 1, messages: Vec::new() });
    }

    pub fn add_message(&self, folder: &str, msg: FakeMessage) {
        let mut boxes = self.mailboxes.lock().unwrap();
        let mb = boxes.entry(folder.into()).or_insert(Mailbox { uid_validity: 1, messages: Vec::new() });
        let mut m = msg;
        m.uid = mb.next_uid();
        mb.messages.push(m);
    }

    pub fn count(&self, folder: &str) -> usize {
        self.mailboxes.lock().unwrap().get(folder).map(|m| m.messages.len()).unwrap_or(0)
    }

    pub fn uid_exists(&self, folder: &str, uid: u32) -> bool {
        self.mailboxes
            .lock()
            .unwrap()
            .get(folder)
            .map(|m| m.messages.iter().any(|x| x.uid == uid))
            .unwrap_or(false)
    }

    pub fn is_seen(&self, folder: &str, uid: u32) -> bool {
        self.mailboxes
            .lock()
            .unwrap()
            .get(folder)
            .and_then(|m| m.messages.iter().find(|x| x.uid == uid))
            .map(|x| x.flags.iter().any(|f| f == "\\Seen"))
            .unwrap_or(false)
    }
}

/// Spawn the fake server on an ephemeral localhost port. Returns the port
/// and the shared state handle.
pub fn spawn(state: FakeMailboxState) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { break };
            let st = state.clone();
            std::thread::spawn(move || handle_connection(stream, st));
        }
    });
    port
}

fn handle_connection(stream: TcpStream, state: FakeMailboxState) {
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(30)));
    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    });
    let mut writer = stream;

    let _ = writer.write_all(b"* OK Fake IMAP server ready\r\n");

    let mut selected: Option<String> = None;
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let line = line.trim_end();
        if std::env::var("FAKE_IMAP_DEBUG").is_ok() {
            eprintln!("[fake-imap] C: {line}");
        }
        let Some((tag, rest)) = line.split_once(' ') else { continue };
        let mut parts = rest.splitn(2, ' ');
        let cmd = parts.next().unwrap_or("").to_uppercase();
        let mut args = parts.next().unwrap_or("").to_string();

        // UID-prefixed commands: "UID STORE ..." -> cmd=STORE, is_uid=true.
        let mut is_uid = false;
        let cmd = if cmd == "UID" {
            let mut sub = args.splitn(2, ' ');
            let sub_cmd = sub.next().unwrap_or("").to_uppercase();
            args = sub.next().unwrap_or("").to_string();
            is_uid = true;
            sub_cmd
        } else {
            cmd
        };

        match cmd.as_str() {
            "CAPABILITY" => {
                let _ = writer.write_all(
                    b"* CAPABILITY IMAP4rev1 UIDPLUS\r\n",
                );
                let _ = writer.write_all(format!("{tag} OK CAPABILITY done\r\n").as_bytes());
            }
            "NOOP" => {
                let _ = writer.write_all(format!("{tag} OK NOOP\r\n").as_bytes());
            }
            "LOGIN" => {
                // Client sends: LOGIN "username" "password".
                let parts: Vec<&str> = args.split('"').collect();
                let user = parts.get(1).copied().unwrap_or("");
                let pass = parts.get(3).copied().unwrap_or("");
                let creds = state.credentials.lock().unwrap();
                let ok = match creds.as_ref() {
                    None => true,
                    Some((u, p)) => u == user && p == pass,
                };
                drop(creds);
                if ok {
                    let _ = writer.write_all(format!("{tag} OK LOGIN done\r\n").as_bytes());
                } else {
                    let _ = writer.write_all(format!("{tag} NO invalid credentials\r\n").as_bytes());
                }
            }
            "LOGOUT" => {
                let _ = writer.write_all(b"* BYE bye\r\n");
                let _ = writer.write_all(format!("{tag} OK LOGOUT done\r\n").as_bytes());
                break;
            }
            "LIST" => {
                let boxes = state.mailboxes.lock().unwrap();
                for name in boxes.keys() {
                    let _ = writer.write_all(
                        format!("* LIST (\\HasNoChildren) \"/\" \"{name}\"\r\n").as_bytes(),
                    );
                }
                let _ = writer.write_all(format!("{tag} OK LIST done\r\n").as_bytes());
            }
            "SELECT" | "EXAMINE" => {
                let name = args.trim().trim_matches('"').to_string();
                let (exists, uid_validity) = {
                    let boxes = state.mailboxes.lock().unwrap();
                    match boxes.get(&name) {
                        Some(m) => (m.messages.len(), m.uid_validity),
                        None => (0, 0),
                    }
                };
                if exists > 0 || uid_validity > 0 {
                    selected = Some(name);
                    let _ = writer.write_all(
                        format!("* {exists} EXISTS\r\n* 0 RECENT\r\n* OK [UIDVALIDITY {uid_validity}] UIDs\r\n* OK [UIDNEXT {exists}] next\r\n* FLAGS (\\Seen \\Deleted \\Answered)\r\n* OK [PERMANENTFLAGS (\\Seen \\Deleted)] limited\r\n").as_bytes(),
                    );
                    let _ = writer.write_all(format!("{tag} OK [READ-WRITE] SELECT done\r\n").as_bytes());
                } else {
                    let _ = writer.write_all(format!("{tag} NO no such mailbox\r\n").as_bytes());
                }
            }
            "STATUS" => {
                let name = args.split('(').next().unwrap_or("").trim().trim_matches('"').to_string();
                let n = state.mailboxes.lock().unwrap().get(&name).map(|m| m.messages.len()).unwrap_or(0);
                let _ = writer.write_all(format!("* STATUS \"{name}\" (MESSAGES {n})\r\n").as_bytes());
                let _ = writer.write_all(format!("{tag} OK STATUS done\r\n").as_bytes());
            }
            "SEARCH" => {
                let Some(sel) = &selected else {
                    let _ = writer.write_all(format!("{tag} NO nothing selected\r\n").as_bytes());
                    continue;
                };
                let query = args.clone();
                let boxes = state.mailboxes.lock().unwrap();
                let mb = boxes.get(sel).unwrap();
                let ids: Vec<String> = mb
                    .messages
                    .iter()
                    .filter(|m| {
                        if query.contains("UNSEEN") {
                            !m.flags.iter().any(|f| f == "\\Seen")
                        } else {
                            true
                        }
                    })
                    .map(|m| m.uid.to_string())
                    .collect();
                let _ = writer.write_all(format!("* SEARCH {}\r\n", ids.join(" ")).as_bytes());
                let _ = writer.write_all(format!("{tag} OK SEARCH done\r\n").as_bytes());
            }
            "FETCH" => {
                let Some(sel) = &selected else {
                    let _ = writer.write_all(format!("{tag} NO nothing selected\r\n").as_bytes());
                    continue;
                };
                let rest = args.clone();
                let mut it = rest.splitn(2, ' ');
                let set = it.next().unwrap_or("");
                let items = it.next().unwrap_or("");
                let items_upper = items.to_uppercase();

                let boxes = state.mailboxes.lock().unwrap();
                let mb = match boxes.get(sel) {
                    Some(m) => m,
                    None => {
                        let _ = writer.write_all(format!("{tag} NO no mailbox\r\n").as_bytes());
                        continue;
                    }
                };

                let matches: Vec<&FakeMessage> = mb
                    .messages
                    .iter()
                    .filter(|m| {
                        if is_uid {
                            uid_in_set(m.uid, set)
                        } else {
                            seq_in_set(mb, m, set)
                        }
                    })
                    .collect();

                for m in &matches {
                    let seq = mb.messages.iter().position(|x| x.uid == m.uid).unwrap() + 1;
                    let prefix = if is_uid { format!("* {seq} FETCH (UID {} ", m.uid) } else { format!("* {seq} FETCH (") };

                    let mut resp = String::new();
                    // RFC 3501: a FETCH that requests the UID item must echo it
                    // back even when the command was not UID-prefixed.
                    if !is_uid && items_upper.contains("UID") {
                        resp.push_str(&format!("UID {} ", m.uid));
                    }
                    if items_upper.contains("FLAGS") {
                        let flags = m.flags.join(" ");
                        resp.push_str(&format!("FLAGS ({flags}) "));
                    }
                    if items_upper.contains("ENVELOPE") {
                        resp.push_str(&format!("ENVELOPE {} ", m.envelope()));
                    }
                    if items_upper.contains("BODYSTRUCTURE") {
                        resp.push_str(&format!("BODYSTRUCTURE {} ", m.bodystructure()));
                    }
                    if items_upper.contains("BODY.PEEK[HEADER]") || items_upper.contains("BODY[HEADER]") {
                        let raw = m.raw();
                        // Header = everything up to the blank line.
                        let header_end = raw
                            .windows(4)
                            .position(|w| w == b"\r\n\r\n")
                            .map(|p| p + 4)
                            .unwrap_or(raw.len());
                        let header = &raw[..header_end];
                        let _ = writer.write_all(prefix.as_bytes());
                        let _ = writer.write_all(resp.as_bytes());
                        let _ = writer.write_all(b"BODY[HEADER] {");
                        let _ = writer.write_all(header.len().to_string().as_bytes());
                        let _ = writer.write_all(b"}\r\n");
                        let _ = writer.write_all(header);
                        let _ = writer.write_all(b")\r\n");
                    } else if items_upper.contains("BODY.PEEK[]") || items_upper.contains("BODY[]") {
                        let raw = m.raw();
                        let _ = writer.write_all(prefix.as_bytes());
                        let _ = writer.write_all(resp.as_bytes());
                        let _ = writer.write_all(b"BODY[] {");
                        let _ = writer.write_all(raw.len().to_string().as_bytes());
                        let _ = writer.write_all(b"}\r\n");
                        let _ = writer.write_all(&raw);
                        let _ = writer.write_all(b")\r\n");
                    } else {
                        let _ = writer.write_all(prefix.as_bytes());
                        let _ = writer.write_all(resp.as_bytes());
                        let _ = writer.write_all(b")\r\n");
                    }
                }
                let _ = writer.write_all(format!("{tag} OK FETCH done\r\n").as_bytes());
            }
            "STORE" => {
                let Some(sel) = &selected else {
                    let _ = writer.write_all(format!("{tag} NO nothing selected\r\n").as_bytes());
                    continue;
                };
                let rest = args.clone();
                let mut it = rest.splitn(2, ' ');
                let set = it.next().unwrap_or("");
                let item = it.next().unwrap_or("");
                let item_upper = item.to_uppercase();

                let mut boxes = state.mailboxes.lock().unwrap();
                let mb = boxes.get_mut(sel).unwrap();
                let uids: Vec<u32> = mb
                    .messages
                    .iter()
                    .enumerate()
                    .map(|(i, m)| {
                        let seq = (i + 1) as u32;
                        let hit = if is_uid { uid_in_set(m.uid, set) } else { uid_in_set(seq, set) };
                        (m.uid, hit)
                    })
                    .filter(|(_, hit)| *hit)
                    .map(|(uid, _)| uid)
                    .collect();
                debug_log!("STORE: uids={:?} item={:?} is_uid={}", uids, item, is_uid);
                for m in mb.messages.iter_mut() {
                    if !uids.contains(&m.uid) {
                        continue;
                    }
                    if item_upper.contains("+FLAGS") {
                        for f in ["\\Seen", "\\Deleted"] {
                            if item_upper.contains(&f.to_uppercase()) && !m.flags.iter().any(|x| x == f) {
                                m.flags.push(f.into());
                            }
                        }
                    } else if item_upper.contains("-FLAGS") {
                        m.flags.retain(|f| {
                            !(f == "\\Seen" && item_upper.contains("\\SEEN"))
                                && !(f == "\\Deleted" && item_upper.contains("\\DELETED"))
                        });
                    }
                    debug_log!("STORE: uid {:?} flags now {:?}", m.uid, m.flags);
                }
                let _ = writer.write_all(format!("{tag} OK STORE done\r\n").as_bytes());
            }
            "COPY" => {
                let Some(sel) = &selected else {
                    let _ = writer.write_all(format!("{tag} NO nothing selected\r\n").as_bytes());
                    continue;
                };
                let rest = args.clone();
                let mut it = rest.splitn(2, ' ');
                let set = it.next().unwrap_or("");
                let dest = it.next().unwrap_or("").trim_matches('"').to_string();

                let mut boxes = state.mailboxes.lock().unwrap();
                let src_msgs: Vec<FakeMessage> = {
                    let mb = boxes.get(sel).unwrap();
                    mb.messages
                        .iter()
                        .filter(|m| {
                            let seq = mb.messages.iter().position(|x| x.uid == m.uid).unwrap() as u32 + 1;
                            if is_uid { uid_in_set(m.uid, set) } else { uid_in_set(seq, set) }
                        })
                        .cloned()
                        .collect()
                };
                let dest_mb = boxes.entry(dest).or_insert(Mailbox { uid_validity: 1, messages: Vec::new() });
                for mut m in src_msgs {
                    m.uid = dest_mb.next_uid();
                    dest_mb.messages.push(m);
                }
                let _ = writer.write_all(format!("{tag} OK COPY done\r\n").as_bytes());
            }
            "EXPUNGE" => {
                debug_log!("EXPUNGE arm reached, is_uid={}, args={:?}", is_uid, args);
                let Some(sel) = &selected else {
                    let _ = writer.write_all(format!("{tag} NO nothing selected\r\n").as_bytes());
                    continue;
                };
                let uid_set = if is_uid { args.clone() } else { String::new() };

                let mut boxes = state.mailboxes.lock().unwrap();
                let mb = boxes.get_mut(sel).unwrap();
                debug_log!("EXPUNGE before: {} messages, uid_set={:?}", mb.messages.len(), uid_set);
                for m in &mb.messages {
                    debug_log!("  uid {} flags {:?}", m.uid, m.flags);
                }
                mb.messages.retain(|m| {
                    let deleted = m.flags.iter().any(|f| f == "\\Deleted");
                    if !deleted {
                        return true;
                    }
                    if is_uid {
                        !uid_in_set(m.uid, &uid_set) // UID EXPUNGE purges only the given set
                    } else {
                        false // plain EXPUNGE purges all deleted
                    }
                });
                debug_log!("EXPUNGE after: {} messages", mb.messages.len());
                let _ = writer.write_all(format!("{tag} OK EXPUNGE done\r\n").as_bytes());
            }
            _ => {
                let _ = writer.write_all(format!("{tag} BAD unknown command {cmd}\r\n").as_bytes());
            }
        }
    }
}

fn uid_in_set(uid: u32, set: &str) -> bool {
    set.split(',').any(|part| match part.split_once(':') {
        Some((a, b)) => {
            let a: u32 = a.parse().unwrap_or(0);
            let b: u32 = b.parse().unwrap_or(u32::MAX);
            uid >= a.min(b) && uid <= a.max(b)
        }
        None => part.parse::<u32>().map(|v| v == uid).unwrap_or(false),
    })
}

fn seq_in_set(mb: &Mailbox, m: &FakeMessage, set: &str) -> bool {
    let seq = mb.messages.iter().position(|x| x.uid == m.uid).unwrap_or(0) + 1;
    uid_in_set(seq as u32, set)
}

// chrono is already a dependency of the crate; bring Utc into scope for the
// helper above.
use chrono::Utc;
