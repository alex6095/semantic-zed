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
