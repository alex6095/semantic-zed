# Semantic Zed release packaging

Semantic Zed is a native Rust/GPUI fork of Zed with the `semantic_overleaf`
Rust crate compiled into the desktop application. The Overleaf transport,
browser sign-in bridge, replica store, collaboration presence, cloud compile,
and PDF preview are not an extension host or a Node runtime sidecar.

## Supported release targets

The public `semantic-zed-release.yml` workflow builds these first-class alpha
artifacts from the `semantic-zed` branch:

| Platform | Artifact | Overleaf integration included |
| --- | --- | --- |
| macOS Apple Silicon | `Semantic-Zed-macos-aarch64.dmg` | Rust core, app-owned Chromium sign-in profile, Keychain credentials, PDFium preview |
| Windows x86_64 | `Semantic-Zed-windows-x86_64.exe` | Rust core, app-owned Chromium sign-in profile, private credential file, PDFium preview |
| Linux x86_64 | `semantic-zed-linux-x86_64.tar.gz` | Rust core, app-owned Chromium sign-in profile, private credential file, PDFium preview |

The two PDFium helpers also map macOS x86_64, Linux aarch64, and Windows
aarch64 to pinned platform libraries. Add a runner and artifact entry only
after that architecture's package and GUI smoke test pass; do not publish an
unverified architecture merely because the Rust source cross-compiles.

## Published alpha

[`semantic-zed-v0.1.0-alpha.1`](https://github.com/alex6095/semantic-zed/releases/tag/semantic-zed-v0.1.0-alpha.1)
is the first unsigned, cross-platform alpha. Its macOS Apple Silicon, Windows
x86_64, and Linux x86_64 package jobs all passed, including their bundled
PDFium layout checks. The release assets are:

| Artifact | Size | SHA-256 |
| --- | ---: | --- |
| `Semantic-Zed-macos-aarch64.dmg` | 152,873,110 bytes | `7d826eabf8e962a098ecc4d174dac6e9b9710735ebccd62695705c9a7f8b8920` |
| `Semantic-Zed-windows-x86_64.exe` | 96,639,242 bytes | `78779a75e773f6e9328a8fee326bbf5c02b18c5305cc3fb938181c6ea524a48a` |
| `semantic-zed-linux-x86_64.tar.gz` | 165,422,030 bytes | `482ee1e737000f4ded064e73fd6147af375ddf2274666336d228350260beb7b9` |

The first workflow promotion job lacked a repository context for `gh`; the
verified artifacts were promoted once manually without rebuilding, and future
tags set `GH_REPO` explicitly. A clean-machine functional smoke test remains
required before treating this alpha as a stable release.

## Source archives are not application packages

GitHub automatically shows a small ZIP/TAR source snapshot on a tag. That is
useful for building from source, but it cannot launch: it contains neither a
platform executable nor the native PDF renderer. The executable artifacts
above are deliberately separate and architecture-specific.

Each executable bundle excludes the source tree, Cargo target directory, test
fixtures, compiler caches, and development-only diagnostics. It includes only
the compiled application, its runtime resources, the matching PDFium shared
library, and required license notices. A full native editor will therefore be
larger than a source ZIP; the workflow publishes the measured compressed size
with each release artifact rather than pretending that a runnable desktop app
is a 20 MB download.

## Packaging guarantees

- The package scripts download PDFium 7881 at build time, pin its SHA-256,
  check archive contents, and copy its full license notices beside the shared
  library.
- PDFium is loaded from the package resource path: `Contents/Resources/pdfium`
  on macOS, `resources/pdfium` beside the installed Windows executable, and
  `libexec/resources/pdfium` in the Linux archive.
- The app never downloads a PDF renderer on first launch and does not depend
  on a platform PDF viewer for its primary preview.
- Semantic Zed uses its own product identity and user-data directories, so an
  installed upstream Zed is not overwritten or made to share its settings.
- The browser sign-in flow never imports cookies from a user's regular browser
  profile. It reuses only the persistent, app-owned profile created by
  Semantic Zed.

## Release process

1. Push the `semantic-zed` branch and run **Semantic Zed cross-platform
   release** from GitHub Actions. It uploads three packages only after each
   package contains its native PDFium library.
2. Install each artifact in a clean VM or physical machine. Check first launch,
   Overleaf login, project list, text/media file sync, cloud compile, and PDF
   preview before making a tag.
3. Create a tag named `semantic-zed-v<version>`. The workflow publishes a
   GitHub prerelease only if all three platform jobs pass.
4. Keep alpha builds explicitly unsigned. A stable macOS release additionally
   needs a product-specific Developer ID certificate, notarization, and a
   final PDFium redistribution review; Windows signing needs an independent
   certificate as well.

The product follows the repository's GPL-3.0-or-later and third-party notice
obligations. The PDFium notices placed in each bundle are part of that release
material and must remain intact.
