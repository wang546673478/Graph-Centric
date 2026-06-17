# Graph-Centric self-improvement loop launcher.
# Keeps restarting the server after each round completes.
# Press Ctrl+C to stop.

Write-Host "🫀 Graph-Centric Self-Improvement Loop" -ForegroundColor Magenta
Write-Host "Press Ctrl+C to stop" -ForegroundColor DarkGray

$round = 0
while ($true) {
    # Kill any lingering server process before building.
    Get-Process -Name "serve","graph_harness" -ErrorAction SilentlyContinue | Stop-Process -Force
    Start-Sleep 1

    $round++
    Write-Host "`n=== Round $round starting ===" -ForegroundColor Cyan
    cargo run --bin serve
    Write-Host "Server exited. Restarting in 3 seconds..." -ForegroundColor Yellow
    Start-Sleep 3
}
