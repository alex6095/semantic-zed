#!/usr/bin/env bash

# Package the exact PDFium shared library used by Semantic Zed's native
# Rust/GPUI preview. This runs while building a distributable bundle; the app
# never downloads a renderer at first launch.

set -euo pipefail

pdfium_version="7881"
target=""
destination=""
resolve_only=false

usage() {
    cat <<'EOF'
Usage: script/semantic-zed-pdfium.sh --target <rust-target> --destination <directory>

Downloads the pinned PDFium binary for the target, verifies its SHA-256, and
places the shared library in the supplied bundle resource directory.

Options:
  --target <triple>       Rust target triple to package.
  --destination <path>    Destination resource directory.
  --resolve               Print the pinned asset metadata without downloading.
  -h, --help              Display this help.
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --target)
            target="${2:-}"
            shift 2
            ;;
        --destination)
            destination="${2:-}"
            shift 2
            ;;
        --resolve)
            resolve_only=true
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "Unknown option: $1" >&2
            usage >&2
            exit 2
            ;;
    esac
done

if [[ -z "$target" ]]; then
    echo "--target is required" >&2
    exit 2
fi

case "$target" in
    aarch64-apple-darwin)
        asset="pdfium-mac-arm64.tgz"
        member="lib/libpdfium.dylib"
        sha256="52e94ca5aa8847934330daf3f8150c190682c5ca93831468794f8b90d4392e40"
        ;;
    x86_64-apple-darwin)
        asset="pdfium-mac-x64.tgz"
        member="lib/libpdfium.dylib"
        sha256="6dedf83990e0e3d6b7c93c9e7589c5a126b0ae14b7464d76120cff7a26afb18b"
        ;;
    aarch64-unknown-linux-gnu|aarch64-unknown-linux-musl)
        asset="pdfium-linux-arm64.tgz"
        member="lib/libpdfium.so"
        sha256="ee7f7b7d5468958336a818c1cd580bdd20972846b7377b13f9a923d92d1d4674"
        ;;
    x86_64-unknown-linux-gnu|x86_64-unknown-linux-musl)
        asset="pdfium-linux-x64.tgz"
        member="lib/libpdfium.so"
        sha256="1470e21b8b4a3b4ad7f85684e2da11d94f3b69a86d81dee11b9b6709d927ac1d"
        ;;
    *)
        echo "Unsupported Semantic Zed PDFium target: $target" >&2
        exit 2
        ;;
esac

if [[ "$resolve_only" = true ]]; then
    printf '%s\t%s\t%s\n' "$asset" "$member" "$sha256"
    exit 0
fi

if [[ -z "$destination" ]]; then
    echo "--destination is required unless --resolve is used" >&2
    exit 2
fi

temporary_directory="$(mktemp -d)"
trap 'rm -rf -- "$temporary_directory"' EXIT
archive="$temporary_directory/$asset"
url="https://github.com/bblanchon/pdfium-binaries/releases/download/chromium/${pdfium_version}/${asset}"

echo "Fetching PDFium ${pdfium_version} for ${target}"
curl --fail --location --retry 3 --retry-delay 1 --output "$archive" "$url"
printf '%s  %s\n' "$sha256" "$archive" | shasum -a 256 -c -

if ! tar -tzf "$archive" "$member" LICENSE licenses/pdfium.txt >/dev/null 2>&1; then
    echo "Pinned PDFium archive did not contain its library and license notices" >&2
    exit 1
fi

tar -xzf "$archive" -C "$temporary_directory" "$member" LICENSE licenses
mkdir -p "$destination"
install -m 755 "$temporary_directory/$member" "$destination/$(basename "$member")"
install -m 644 "$temporary_directory/LICENSE" "$destination/LICENSE.txt"
mkdir -p "$destination/licenses"
cp -R "$temporary_directory/licenses/." "$destination/licenses/"
echo "Bundled $destination/$(basename "$member")"
