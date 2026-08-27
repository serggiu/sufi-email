# Sufi Email

A lightweight, configurable email client built with Rust + Tauri. Connects to
standard IMAP/SMTP accounts, fetches mail, and sends messages.

## Features (current)

- Three-column UI: folders | message list | message preview
- Collapsible folder sidebar (`Ctrl+B` or the ☰ button)
- Messages sorted newest-first; unread messages highlighted
- HTML and plain-text message rendering (HTML is rendered in a fully sandboxed
  iframe — no scripts, no same-origin access)
- Compose & send via SMTP (`Ctrl+N`), reply support
- Config-driven accounts

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

## Roadmap

- [x] IMAP folder listing, message list, message body fetching
- [x] SMTP sending
- [x] Column-based UI with collapsible folders
- [ ] Google OAuth account support (Gmail API / XOAUTH2)
- [ ] Attachment download/upload
- [ ] Mark as read/unread, move/delete messages
- [ ] In-app account management UI
