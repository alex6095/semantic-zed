# Semantic Zed changelog

This file tracks the native Semantic Zed product. It is intentionally separate
from the legacy `semantic-researcher-overleaf` VSIX changelog and from Zed's
upstream application version.

## [Unreleased]

### Changed

- Replaced the verbose Overleaf account card with compact native header actions
  and an account popover showing the saved account email, reauthentication, and
  local disconnect.
- Reduced the current-project card to the remote project name, live state, local
  folder, collaborators, and direct Sync/Compile/PDF actions. The full local
  path is available by tooltip and copy button.
- Moved project access and update metadata into the project row's secondary
  line so narrow sidebars no longer clip `Owner` as `wner`.
- Made the development build runner stop only this checkout's exact app bundle
  and resolve Zed's final child process before recording a successful PID.

### Verified

- `cargo test -p semantic_zed --lib`: 13 passed, 1 packaging-only PDFium test
  ignored.
- Signed macOS arm64 development bundle built and remained running.
- Semantic Light sidebar and account popover inspected in the native GPUI app at
  a 296 px sidebar width. A fresh compact-layout Dark/minimum-width screenshot
  remains pending.

## [0.1.0-alpha.1] - 2026-08-12

Source milestone; no notarized public binary has been released.

### Added

- Native Rust `semantic_overleaf` authentication, HTTPS/WebSocket transport,
  Socket.IO 0.9, ShareJS/history-OT, local replica, compile, and PDF download.
- Native Rust/GPUI Overleaf project list, blank project creation, recoverable
  trash/restore, collaborator avatars, click-to-follow, and bidirectional cursor
  presence.
- Cross-platform GPUI PDF preview backed by bundled PDFium adapters.
- Semantic Light/Dark themes and restrained Codex-style Agent surfaces.

### Fixed

- UTF-16 cursor conversion for Korean and BMP emoji.
- User undo history isolation from Remote, Agent, and External edits.
- Open-buffer save/echo races and stale-disk reloads.
- Project refresh retention and narrow New project UI.
- Lossless event-only file/folder/media synchronization through bootstrap; no
  periodic authoritative entity-tree poll or event-lag full refresh.

### Security

- Stores Overleaf credentials in macOS Keychain (private-file fallback on other
  platforms), uses an app-owned browser profile, validates paths, and excludes
  the legacy Node Overleaf prototype from the app bundle.

### Release boundary

- Verified locally on macOS arm64 with a development signing identity.
- Windows/Linux packaging, PDFium distribution review, Apple notarization, and
  public release artifacts remain pending.
