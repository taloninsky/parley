# dev.ps1 — Start the token proxy + Dioxus dev server together.
# Usage: .\dev.ps1
#
# The proxy is started as a background job so its stdout/stderr stream
# into this console alongside `dx serve` output. If the proxy exits
# early (e.g. fails to bind 127.0.0.1:3033 because another instance is
# already running) the script aborts before launching `dx serve` so
# the failure is visible instead of silently producing a "Failed to
# fetch" error in the browser later.

$ErrorActionPreference = "Stop"

$proxyPort = 3033
$proxyHost = "127.0.0.1"

# Bail early if something is already bound to the proxy port — otherwise
# `cargo run` would fail with a panic deep in the output and we'd have
# no idea why STT stops working.
$existing = Get-NetTCPConnection -LocalAddress $proxyHost -LocalPort $proxyPort `
    -State Listen -ErrorAction SilentlyContinue
if ($existing) {
    Write-Host "ERROR: ${proxyHost}:${proxyPort} is already in use (PID $($existing[0].OwningProcess))." -ForegroundColor Red
    Write-Host "Stop the other process or kill the PID above, then re-run .\dev.ps1." -ForegroundColor Red
    exit 1
}

Write-Host "Starting token proxy..." -ForegroundColor Cyan
$proxyJob = Start-Job -Name "parley-proxy" -ScriptBlock {
    param($repoRoot)
    Set-Location $repoRoot
    # Merge stderr into stdout so cargo's progress / warnings show up in
    # Receive-Job alongside normal log lines.
    cargo run --manifest-path proxy/Cargo.toml 2>&1
} -ArgumentList $PSScriptRoot

# Helper: drain any pending proxy output to the console, tagged so it's
# distinguishable from `dx serve` lines.
function Drain-ProxyOutput {
    param($job)
    if ($null -eq $job) { return }
    Receive-Job -Job $job -ErrorAction SilentlyContinue | ForEach-Object {
        Write-Host "[proxy] $_" -ForegroundColor DarkGray
    }
}

try {
    # Wait until the proxy binds the port, or fails. Cargo may take a
    # while to compile on first run, so give it a generous timeout.
    $deadline = (Get-Date).AddSeconds(180)
    $ready = $false
    while ((Get-Date) -lt $deadline) {
        Drain-ProxyOutput $proxyJob
        if ($proxyJob.State -ne "Running") {
            throw "Proxy job exited early with state '$($proxyJob.State)'. See [proxy] output above."
        }
        $bound = Get-NetTCPConnection -LocalAddress $proxyHost -LocalPort $proxyPort `
            -State Listen -ErrorAction SilentlyContinue
        if ($bound) {
            $ready = $true
            break
        }
        Start-Sleep -Milliseconds 500
    }
    Drain-ProxyOutput $proxyJob
    if (-not $ready) {
        throw "Proxy did not bind ${proxyHost}:${proxyPort} within 180s."
    }

    Write-Host "Proxy ready on http://${proxyHost}:${proxyPort}" -ForegroundColor Green
    Write-Host "Starting Dioxus dev server..." -ForegroundColor Cyan
    dx serve --platform web
}
finally {
    Write-Host "`nShutting down proxy..." -ForegroundColor Yellow
    Drain-ProxyOutput $proxyJob
    if ($proxyJob) {
        Stop-Job -Job $proxyJob -ErrorAction SilentlyContinue
        Remove-Job -Job $proxyJob -Force -ErrorAction SilentlyContinue
    }
    # Stop-Job kills the PowerShell job but cargo's child process may
    # outlive it on Windows. Sweep any lingering parley-proxy.exe.
    Get-Process -Name "parley-proxy" -ErrorAction SilentlyContinue | ForEach-Object {
        Stop-Process -Id $_.Id -Force -ErrorAction SilentlyContinue
    }
    Write-Host "Done." -ForegroundColor Green
}
