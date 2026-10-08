# Sufi Email

A lightweight, configurable email client built with Rust + Tauri. Connects to
standard IMAP/SMTP accounts, fetches mail, and sends messages.

Built on Omarchy (Arch) and ported to Fedora, where it is packaged and run as a
Flatpak. It also builds as a `.deb`/`.rpm` or a bare binary.

## Icon & artwork credit

The app icon is adapted from **"Semakar"** — whirling-dervish line art by
**Mahak (محک)** on Wikimedia Commons, licensed under
[CC BY-SA 3.0](https://creativecommons.org/licenses/by-sa/3.0):

- Original: https://commons.wikimedia.org/wiki/File:Semakar.svg
- Adapted to a square sand-coloured tile (`src-tauri/icons/semakar-square.svg`);
  the adapted version is distributed under the same CC BY-SA 3.0 license.

## Features (current)

- Three-column UI: folders | message list | message preview (collapsible
  sidebar, resizable columns)
- **Remembers the window size**: resizes are saved and the app reopens at the
  size you last left it at (maximized/fullscreen sizes are ignored, so a
  window that fits returns to its normal size)
- **Title bar matches the desktop**: the window buttons follow the system's
  decoration layout, so e.g. Fedora/GNOME shows just minimize + close instead
  of an extra Maximize button; on Hyprland/Omarchy the title bar is hidden
  altogether, as before
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
- **System tray**: an envelope icon shows unread mail (grey with no
  unread, blue when mail is waiting, tooltip with the count); closing the
  window hides it to the tray and Quit from the tray menu is the real
  exit. Where no StatusNotifier host is present (e.g. GNOME without the
  AppIndicator extension) the icon has nowhere to appear, so closing the
  window quits the app instead of hiding it
- **Omarchy theme integration**: on Omarchy systems the app follows the
  active desktop theme — its palette (background, text, accent, borders,
  error/warning colors) is mapped onto the UI from the current theme's
  `colors.toml`, and switching themes repaints the app live. Elsewhere
  (including Fedora) it keeps the built-in dark palette
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

Requirements: Rust (stable) plus Node/npm, and the WebKitGTK 4.1 / GTK 3
development stack for your distribution.

Debian/Ubuntu:

```sh
sudo apt install libwebkit2gtk-4.1-dev libgtk-3-dev \
  libayatana-appindicator3-dev librsvg2-dev libxdo-dev
```

Fedora/RHEL:

```sh
sudo dnf install webkit2gtk4.1-devel gtk3-devel libappindicator-gtk3-devel \
  librsvg2-devel dbus-devel openssl-devel
sudo dnf group install "C Development Tools and Libraries"
```

`dbus-devel` is required because Tauri's windowing layer links libdbus
(`libdbus-sys` fails the build without `dbus-1.pc`). `libayatana-appindicator3-devel`
is a drop-in alternative to `libappindicator-gtk3-devel`; install either one.

Arch/Omarchy:

```sh
sudo pacman -S webkit2gtk-4.1 gtk3 librsvg libayatana-appindicator
```

(`libappindicator-gtk3` also works where it is available; the tray library is
detected at build time and the matching runtime dependency is declared.)

Then run the app with the dev server (vite + the debug binary):

```sh
npm install          # frontend deps (vite)
npm run tauri dev    # starts the vite dev server + the desktop app
```

## Building & Distribution

`bin/build` produces the standalone application (frontend + release binary +
bundles). Bundle targets come from `src-tauri/tauri.linux.conf.json` (a `.deb`
by default); pass `--bundles` to choose others:

```sh
bin/build                 # release binary + the configured bundle (deb)
bin/build --no-bundle     # just the release binary
bin/build --bundles deb   # pick specific bundles
bin/build --bundles rpm   # e.g. an rpm
```

Artifacts:

- `src-tauri/target/release/sufi-email` — the release binary
- `src-tauri/target/release/bundle/deb/Sufi Email_0.1.0_amd64.deb` — Debian
  package (default)
- `src-tauri/target/release/bundle/rpm/Sufi Email-0.1.0-1.x86_64.rpm` — RPM
  package (with `--bundles rpm`)

The RPM bundler is pure Rust and needs no extra tooling; it declares its
runtime dependencies (WebKitGTK 4.1, GTK 3, and the appindicator library Tauri
detects at build time), so `dnf` pulls them in.

### Flatpak

`bin/flatpak` packages the release binary as a Flatpak (per-user install). This
is the supported way to run Sufi Email on Fedora, and the packaging has been
built and validated end to end there (Fedora 44 with GNOME Shell 50 on Wayland,
Flatpak 1.18). It rebuilds the binary on every run so the packaged app always
matches the sources (`--no-build` skips that):

```sh
bin/flatpak                # rebuild the app + build/install for the current user
bin/flatpak --run          # ... and launch it
bin/flatpak --build-only   # build + export to flatpak/repo, don't install
bin/flatpak --no-build     # package the existing binary as-is
```

One-time setup:

```sh
sudo dnf install flatpak-builder          # or apt/pacman equivalent
flatpak install flathub org.gnome.Platform//50 org.gnome.Sdk//50
```

The Flatpak uses the GNOME runtime, which provides WebKitGTK 4.1 and GTK 3. No
Flatpak runtime ships an appindicator library, so the **tray icon is not
available inside the sandbox**: the app detects this at startup and runs
without one (closing the window then quits). Everything else — accounts, the
offline cache, new-mail notifications, file dialogs and opening links — works,
and per-user data lives in `~/.var/app/com.sufi.email/config/sufi-email/`. The
manifest's finish-args also expose the host Omarchy theme read-only, so theme
integration still works in a Flatpak on Omarchy.

`bin/flatpak` packages the binary built on this host, so the host's glibc and
WebKitGTK must not be newer than the runtime's (they aren't on current
Fedora/Arch). For a fully reproducible build that compiles inside the SDK
against the runtime's own libraries, see Tauri's Flatpak guide
(`sdk-extensions` for Rust/Node plus `flatpak-cargo-generator` and
`flatpak-node-generator`).

`bin/test` runs the full test suite (unit + fake-IMAP integration tests).

### Copying to another machine

The `.deb` is a single file that installs and runs on compatible machines:

```sh
sudo apt install ./Sufi\ Email_0.1.0_amd64.deb
```

On Fedora/RHEL the equivalent is the `.rpm`:

```sh
sudo dnf install ./Sufi\ Email-0.1.0-1.x86_64.rpm
```

GNOME note: the tray icon needs a StatusNotifier host. Fedora Workstation does
not ship one by default — install and enable the AppIndicator extension or
the icon is invisible and closing the window quits the app:

```sh
sudo dnf install gnome-shell-extension-appindicator
```

Compatibility constraints:

- **OS/arch**: Linux **x86_64** only — no Windows, macOS, or ARM builds.
- **glibc**: the binary is linked against the build machine's glibc (2.39+ on
  current Arch-family distros) and needs an equal-or-newer glibc on the
  target. Ubuntu 24.04+, Debian 13+, Fedora 40+, and current Arch work;
  Debian 12 / Ubuntu 22.04 are too old.
- **Packaging**: the `.deb` installs on Debian/Ubuntu-family systems and the
  `.rpm` on Fedora/RHEL-family systems; each pulls the WebKitGTK/GTK runtime
  dependencies automatically. Other distros must run the bare binary and
  install the runtime deps themselves (`libwebkit2gtk-4.1`, `libgtk-3` and an
  appindicator library).
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
- [x] Flatpak packaging for Fedora (GNOME runtime)
- [ ] Google OAuth account support (Gmail API / XOAUTH2)
- [ ] Full-text message search
- [ ] Message filters/rules (auto-move, auto-archive)
- [ ] Rich-text (HTML) composing with inline images
