# Sufi Email

A lightweight, configurable email client built with Rust + Tauri. Connects to
standard IMAP/SMTP accounts, fetches mail, and sends messages.

## Features (current)

- Three-column UI: folders | message list | message preview (collapsible
  sidebar, resizable columns)
- **Conversation threading**: one row per thread (newest subject/from/snippet,
  message count, unread badge); the preview pane and a double-click modal
  show the full discussion newest-first; replies automatically join the thread
- HTML and plain-text message rendering in a **fully sandboxed iframe** (no
  scripts, no same-origin access); remote images are blocked (no tracking
  pixels); links inside emails open in the system browser
- **Attachments**: download (Save as…), inline image thumbnails, embedded
  (cid:) images rendered inside the message body, and attaching files when
  composing (sent as multipart/mixed)
- **New-mail notifications**: instant IMAP IDLE push detection with desktop
  toasts (sender + subject); moves into Inbox don't produce false "new email"
  toasts
- **Offline support**: folders, message lists, bodies and attachment previews
  all read from a local cache first; read/unread changes made while offline
  are queued and flushed when the connection returns
- **Optimistic UI**: delete and move apply instantly with no flash-back, the
  message lands in Trash/the destination right away, and failures roll back
  on the next sync
- **Multiple accounts**, added and switched from the UI; connections are
  tested before an account is saved
- Config-driven accounts (`config.toml`), passwords sealed with a
  machine-local key

## Accounts

Manage accounts from the UI: **Accounts ▾ → Add account…** in the toolbar.
Enter a display name, email address, IMAP server/port, username/password, and
SMTP server/port. Before saving, the app **tests both connections** — wrong
credentials or unreachable servers are rejected with an error instead of being
stored.

Security:

- IMAP always uses TLS (implicit TLS on the chosen port, typically 993).
- SMTP uses implicit TLS (typically port 465) or STARTTLS (typically 587,
  tick the checkbox). Plain-text SMTP is not supported.
- Certificates are verified against your system's root certificate store.

SMTP credentials default to the IMAP ones; use *Different SMTP credentials*
in the dialog if your provider needs separate login details.

## Configuration

Accounts live in `~/.config/sufi-email/config.toml`. A template is created on
first run:

```toml
[[accounts]]
name = "My Account"
email = "you@example.com"
imap_host = "imap.example.com"
imap_port = 993
smtp_host = "smtp.example.com"
smtp_port = 465
username = "you@example.com"
password = "app-password-here"
```

Add multiple `[[accounts]]` blocks for additional accounts. Use an app-specific
password where the provider requires one (e.g. Gmail, Fastmail).

## Development

Requirements: Rust (stable), Node/npm, and WebKitGTK 4.1 (`webkit2gtk-4.1`,
usually via `libwebkit2gtk-4.1-dev` or your distro's equivalent).

```sh
npm install          # frontend deps (vite)
cd src-tauri
cargo run            # starts vite dev server + the desktop app
```

## Building & Distribution

`bin/build` produces the standalone application (frontend + release binary +
bundles):

```sh
bin/build                 # release binary + .deb bundle
bin/build --no-bundle     # just the release binary
bin/build --bundles deb   # pick specific bundles
```

Artifacts:

- `src-tauri/target/release/sufi-email` — the release binary
- `src-tauri/target/release/bundle/deb/Sufi Email_0.1.0_amd64.deb` — Debian
  package (the copyable artifact)

`bin/test` runs the full test suite (unit + fake-IMAP integration tests).

### Copying to another machine

The `.deb` is a single file that installs and runs on compatible machines:

```sh
sudo apt install ./Sufi\ Email_0.1.0_amd64.deb
```

Compatibility constraints:

- **OS/arch**: Linux **x86_64** only — no Windows, macOS, or ARM builds.
- **glibc**: the binary is linked against the build machine's glibc (2.39+ on
  current Arch-family distros) and needs an equal-or-newer glibc on the
  target. Ubuntu 24.04+, Debian 13+, Fedora 40+, and current Arch work;
  Debian 12 / Ubuntu 22.04 are too old.
- **Packaging**: the `.deb` installs on Debian/Ubuntu-family systems and pulls
  the WebKitGTK/GTK dependencies automatically. Other distros must run the
  bare binary and install the runtime deps themselves (`libwebkit2gtk-4.1`,
  `libgtk-3`).
- **The bare binary is not standalone**: it links dynamically against the
  WebKitGTK/GTK stack (~140 shared libraries). Copy the `.deb`, not just the
  binary.
- **Data stays local**: accounts, the mail cache, and the `.secret`
  encryption key live in `~/.config/sufi-email/`. Passwords are sealed with a
  machine-specific key, so accounts must be re-added on the new machine —
  copying `config.toml` does not transfer working credentials.

AppImage bundles are not currently produced on all systems: the `linuxdeploy`
tool used by Tauri has known incompatibilities with some newer distros
(broken `strip`/`gdk-pixbuf` paths). Try `bin/build --bundles appimage` on
distros where it is supported.

## Roadmap

- [x] IMAP folder listing, message list, message body fetching
- [x] SMTP sending
- [x] Column-based UI with collapsible folders
- [x] Mark as read/unread, move/delete messages (optimistic, with rollback)
- [x] In-app account management UI
- [x] Attachment download (Save as…)
- [x] Attachment upload in compose
- [x] Conversation threading
- [x] Inline image previews (thumbnails + embedded images)
- [x] New-mail notifications (IDLE push + desktop toasts)
- [x] Offline cache + queued read/unread sync
- [ ] Google OAuth account support (Gmail API / XOAUTH2)
- [ ] Full-text message search
- [ ] Message filters/rules (auto-move, auto-archive)
- [ ] Dark theme / theme switcher
- [ ] Rich-text (HTML) composing with inline images
