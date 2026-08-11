[CmdletBinding()]
Param(
    [Parameter(Mandatory = $true)][ValidateSet('x86_64', 'aarch64')][string]$Architecture,
    [Parameter()][string]$Destination,
    [Parameter()][switch]$Resolve
)

# Package the exact PDFium DLL used by Semantic Zed's native Rust/GPUI preview.
# This is a build-time bundle resource, never a first-run app download.

$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $true
$PdfiumVersion = '7881'

$asset = switch ($Architecture) {
    'x86_64' {
        [pscustomobject]@{
            Name = 'pdfium-win-x64.tgz'
            Member = 'bin/pdfium.dll'
            Sha256 = '73cc0de638ac2095e7445bf56a38200a5b7c7ca0e9f4ba144598f2457377ac08'
        }
    }
    'aarch64' {
        [pscustomobject]@{
            Name = 'pdfium-win-arm64.tgz'
            Member = 'bin/pdfium.dll'
            Sha256 = 'd3035d4d2cacac6ecd1a2ece197a3d702a1b2a58466276b9f870b8cb278a9d84'
        }
    }
}

if ($Resolve) {
    Write-Output "$($asset.Name)`t$($asset.Member)`t$($asset.Sha256)"
    exit 0
}

if ([string]::IsNullOrWhiteSpace($Destination)) {
    throw '-Destination is required unless -Resolve is used.'
}

$temporaryDirectory = Join-Path ([System.IO.Path]::GetTempPath()) ("semantic-zed-pdfium-" + [guid]::NewGuid().ToString('N'))
try {
    New-Item -ItemType Directory -Path $temporaryDirectory -Force | Out-Null
    $archive = Join-Path $temporaryDirectory $asset.Name
    $url = "https://github.com/bblanchon/pdfium-binaries/releases/download/chromium/$PdfiumVersion/$($asset.Name)"

    Write-Output "Fetching PDFium $PdfiumVersion for $Architecture"
    Invoke-WebRequest -Uri $url -OutFile $archive -UseBasicParsing
    $actualSha256 = (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actualSha256 -ne $asset.Sha256) {
        throw "Pinned PDFium checksum mismatch. Expected $($asset.Sha256), got $actualSha256."
    }

    & tar.exe -tzf $archive $asset.Member 'LICENSE' 'licenses/pdfium.txt' | Out-Null
    if ($LASTEXITCODE -ne 0) {
        throw 'Pinned PDFium archive did not contain its library and license notices.'
    }
    & tar.exe -xzf $archive -C $temporaryDirectory $asset.Member 'LICENSE' 'licenses'
    if ($LASTEXITCODE -ne 0) {
        throw 'Could not extract the pinned PDFium archive.'
    }

    New-Item -ItemType Directory -Path $Destination -Force | Out-Null
    $source = Join-Path $temporaryDirectory ($asset.Member -replace '/', '\\')
    Copy-Item -LiteralPath $source -Destination (Join-Path $Destination 'pdfium.dll') -Force
    Copy-Item -LiteralPath (Join-Path $temporaryDirectory 'LICENSE') -Destination (Join-Path $Destination 'LICENSE.txt') -Force
    Copy-Item -LiteralPath (Join-Path $temporaryDirectory 'licenses') -Destination (Join-Path $Destination 'licenses') -Recurse -Force
    Write-Output "Bundled $(Join-Path $Destination 'pdfium.dll')"
}
finally {
    if (Test-Path -LiteralPath $temporaryDirectory) {
        Remove-Item -LiteralPath $temporaryDirectory -Recurse -Force
    }
}
