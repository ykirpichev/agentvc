# Set up this checkout to develop agentvc with agentvc, on Windows.
# The PowerShell twin of scripts/dogfood-setup.sh; safe to run again.
# See docs/dogfooding.md. Forks use the copy backend here (no overlayfs or
# reflinks), so a fork costs a copy of the checkout.
$ErrorActionPreference = 'Stop'
Set-Location (Join-Path $PSScriptRoot '..')

# The UI goes first: release builds of agentvcd embed ui/dist at compile time.
if (Test-Path ui/package.json) {
    Write-Host '==> building the review UI'
    Push-Location ui
    npm ci --silent; if ($LASTEXITCODE) { throw 'npm ci failed' }
    npm run build --silent; if ($LASTEXITCODE) { throw 'npm run build failed' }
    Pop-Location
}

Write-Host '==> building agentvc and agentvcd (release)'
cargo build --release -p agentvc-cli -p agentvcd
if ($LASTEXITCODE) { throw 'cargo build failed' }

$bin = Join-Path $PWD 'target/release/agentvc.exe'
if (Test-Path .agentvc/meta.db) {
    Write-Host '==> space already initialized'
} else {
    & $bin init --from-remote --this-branch 2>$null
    if ($LASTEXITCODE -eq 0) {
        Write-Host "==> restored this branch's space from origin"
    } else {
        Write-Host '==> agentvc init'
        & $bin init
        if ($LASTEXITCODE) { throw 'agentvc init failed' }
    }
}

Write-Host '==> installing Claude Code hooks'
& $bin hooks
if ($LASTEXITCODE) { throw 'agentvc hooks failed' }

$release = Join-Path $PWD 'target/release'
Write-Host @"

Done. Add agentvc to PATH for this session (or in your profile):
  `$env:PATH = "$release;`$env:PATH"
Then follow the loop in docs/dogfooding.md:
  agentvc task new "..."; agentvc run --task t1 -- claude -p "..."
"@
