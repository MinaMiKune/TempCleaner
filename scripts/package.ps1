param(
    [string]$Version = "1.0.0"
)

$ErrorActionPreference = "Stop"
$project = Split-Path -Parent $PSScriptRoot
$name = "TempCleaner-$Version-win-x64"
$stage = Join-Path $project "artifacts\$name"
$zip = Join-Path $project "artifacts\$name.zip"

Push-Location $project
try {
    cargo build --release --locked
    if ($LASTEXITCODE -ne 0) { throw "release build failed" }

    if (Test-Path $stage) {
        $resolvedStage = (Resolve-Path $stage).Path
        $expectedRoot = (Resolve-Path (Join-Path $project "artifacts")).Path
        if (-not $resolvedStage.StartsWith($expectedRoot, [System.StringComparison]::OrdinalIgnoreCase)) {
            throw "refusing to remove unexpected path: $resolvedStage"
        }
        Remove-Item -Recurse -Force $resolvedStage
    }
    New-Item -ItemType Directory -Force $stage | Out-Null

    Copy-Item "target\release\temp-cleaner.exe" $stage
    Copy-Item "README.md" $stage
    Copy-Item "LICENSE" $stage
    Copy-Item "docs" $stage -Recurse

    $binary = Join-Path $stage "temp-cleaner.exe"
    $hash = (Get-FileHash $binary -Algorithm SHA256).Hash.ToLowerInvariant()
    "$hash *temp-cleaner.exe" | Set-Content (Join-Path $stage "SHA256SUMS.txt") -Encoding ascii

    if (Test-Path $zip) { Remove-Item -Force $zip }
    Compress-Archive -Path "$stage\*" -DestinationPath $zip

    Write-Host "Package directory: $stage"
    Write-Host "ZIP: $zip"
    Write-Host "SHA256: $hash"
}
finally {
    Pop-Location
}
