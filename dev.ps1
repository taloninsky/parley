# dev.ps1 — Start the token proxy + Dioxus dev server together.
# Usage: .\dev.ps1

$ErrorActionPreference = "Stop"

Write-Host "Starting token proxy..." -ForegroundColor Cyan
$proxy = Start-Process -FilePath "cargo" `
    -ArgumentList "run", "--manifest-path", "proxy/Cargo.toml" `
    -PassThru -NoNewWindow

# Give the proxy a moment to bind
Start-Sleep -Seconds 2

Write-Host "Starting Dioxus dev server..." -ForegroundColor Cyan

try {
    dx serve --platform web
}
finally {
    Write-Host "`nShutting down proxy (PID $($proxy.Id))..." -ForegroundColor Yellow
    if (!$proxy.HasExited) {
        Stop-Process -Id $proxy.Id -Force -ErrorAction SilentlyContinue
    }
    Write-Host "Done." -ForegroundColor Green
}
