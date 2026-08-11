# Semantic Zed upstream and release policy

## Source identity

- Product source: this native Zed fork (`semantic-zed` branch)
- Product version: `SEMANTIC_ZED_VERSION`
- Upstream: <https://github.com/zed-industries/zed>
- Upstream base: `08827f9208b4848d62f3faf86ffa15155966d63c`
- Current upstream application crate version at this base: `1.16.0`

The `semantic_zed` and `semantic_overleaf` crate versions are internal Rust
package versions. They are not the public product release number.

## Repository split

`semantic-researcher-overleaf` remains the legacy cross-platform VS Code/VSIX
product. Its `0.16.x` releases must not be reused for this native application.
The native fork should be published as a separate `alex6095/semantic-zed`
repository so cloning it includes the real Rust/GPUI source and commit history.

Recommended remotes after the repository is created:

```text
origin  https://github.com/zed-industries/zed.git
fork    https://github.com/alex6095/semantic-zed.git
```

Keep `origin` read-only for upstream synchronization and push the
`semantic-zed` branch and Semantic Zed tags to `fork`.

## Versioning

- `0.1.0-alpha.N`: source checkpoints and locally signed development bundles
- `0.1.0-beta.N`: signed/notarized macOS preview with a documented update path
- `0.1.0`: supported release after the declared platform/packaging checklist

Do not tag `0.1.0-alpha.1` publicly until the new repository exists and its
license/attribution files, commit range, and release notes have been reviewed.

## Licensing and distribution

Preserve Zed's GPL-3.0-or-later and marked Apache-2.0 files and all third-party
attributions. Do not replace them with the legacy VSIX repository's AGPL label.
Review PDFium redistribution, Apple signing/notarization, and bundled native
libraries before attaching binaries to a public release.
