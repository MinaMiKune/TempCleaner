$ErrorActionPreference = "Stop"
$project = Split-Path -Parent $PSScriptRoot
Push-Location $project
try {
    Write-Host "== cargo fmt =="
    cargo fmt --all -- --check
    if ($LASTEXITCODE -ne 0) { throw "cargo fmt failed" }

    Write-Host "== cargo check =="
    cargo check --all-targets --locked
    if ($LASTEXITCODE -ne 0) { throw "cargo check failed" }

    Write-Host "== cargo test =="
    cargo test --all-targets --locked
    if ($LASTEXITCODE -ne 0) { throw "cargo test failed" }

    Write-Host "== cargo clippy =="
    cargo clippy --all-targets --all-features --locked -- -D warnings
    if ($LASTEXITCODE -ne 0) { throw "cargo clippy failed" }

    Write-Host "== cargo build --release =="
    cargo build --release --locked
    if ($LASTEXITCODE -ne 0) { throw "release build failed" }

    Write-Host "All checks passed."
    Write-Host "Executable: $project\target\release\temp-cleaner.exe"
}
finally {
    Pop-Location
}
