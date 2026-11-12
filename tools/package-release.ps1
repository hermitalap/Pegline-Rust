# Build and package the portable release.
#
#   pwsh -File tools\package-release.ps1
#
# Produces dist\Pegline-<version>-win-x64.zip with the executable, README,
# LICENCE and changelog, plus a .sha256 file. The version is read from
# Cargo.toml so the package, the embedded version resource and the tag agree.

param(
    [string]$Output = 'dist',
    [switch]$SkipTests
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$outputRoot = [System.IO.Path]::GetFullPath((Join-Path $root $Output))
if (-not $outputRoot.StartsWith([System.IO.Path]::GetFullPath($root), [System.StringComparison]::OrdinalIgnoreCase)) {
    throw "output directory must live inside the repository: $outputRoot"
}

Push-Location $root
try {
    $manifest = Get-Content Cargo.toml -Raw
    $match = [regex]::Match($manifest, '(?m)^version\s*=\s*"([^"]+)"')
    if (-not $match.Success) { throw 'cannot read version from Cargo.toml' }
    $version = $match.Groups[1].Value

    if (-not $SkipTests) {
        cargo test
        if ($LASTEXITCODE -ne 0) { throw 'cargo test failed' }
        cargo clippy --all-targets -- -D warnings
        if ($LASTEXITCODE -ne 0) { throw 'cargo clippy failed' }
    }
    cargo build --release
    if ($LASTEXITCODE -ne 0) { throw 'cargo build --release failed' }

    $exe = Join-Path $root 'target\release\pegline.exe'
    if (-not (Test-Path -LiteralPath $exe)) { throw "missing $exe" }
    $info = (Get-Item -LiteralPath $exe).VersionInfo
    if ($info.ProductVersion -ne "${version}.0") {
        throw "embedded version $($info.ProductVersion) does not match Cargo.toml $version"
    }

    $name = "Pegline-$version-win-x64"
    $staging = Join-Path $outputRoot $name
    $zip = Join-Path $outputRoot "$name.zip"
    $checksum = "$zip.sha256"
    # The paths were built from the repository root and this script owns them.
    if (Test-Path -LiteralPath $staging) { Remove-Item -LiteralPath $staging -Recurse -Force }
    if (Test-Path -LiteralPath $zip) { Remove-Item -LiteralPath $zip -Force }
    if (Test-Path -LiteralPath $checksum) { Remove-Item -LiteralPath $checksum -Force }
    New-Item -ItemType Directory -Path $staging -Force | Out-Null

    Copy-Item -LiteralPath $exe -Destination $staging
    foreach ($file in 'README.md', 'LICENSE', 'CHANGELOG.md') {
        Copy-Item -LiteralPath (Join-Path $root $file) -Destination $staging
    }
    Compress-Archive -Path (Join-Path $staging '*') -DestinationPath $zip -CompressionLevel Optimal

    $hash = (Get-FileHash -LiteralPath $zip -Algorithm SHA256).Hash.ToLowerInvariant()
    "$hash  $name.zip" | Set-Content -LiteralPath $checksum -Encoding ascii
    $size = (Get-Item -LiteralPath $zip).Length
    Write-Host ("packaged {0} ({1:N0} bytes)" -f $zip, $size)
    Write-Host ("sha256   {0}" -f $hash)
}
finally {
    Pop-Location
}
