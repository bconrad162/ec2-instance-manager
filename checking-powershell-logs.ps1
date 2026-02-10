$ErrorActionPreference = "Stop"

$exe = ".\ec2_manager_gui.exe"
if (-not (Test-Path $exe)) {
  Write-Host "error: $exe not found in current directory"
  Get-ChildItem | Select-Object Name
  exit 1
}

Write-Host "starting $exe --mode sim"
$proc = Start-Process $exe -ArgumentList "--mode","sim" -PassThru
Start-Sleep -Seconds 2
Write-Host "process running:" (-not $proc.HasExited)
if ($proc.HasExited) {
  Write-Host "exit code:" $proc.ExitCode
} else {
  Write-Host "process id:" $proc.Id
}

Write-Host ""
Write-Host "recent application log entries (ec2_manager_gui/ec2_manager):"
Get-WinEvent -LogName Application -MaxEvents 50 |
  Where-Object { $_.Message -match "ec2_manager_gui|ec2_manager" } |
  Select-Object -First 10 | Format-List

Write-Host ""
Write-Host "panic log (if present):"
$panic = Join-Path $env:APPDATA "ec2-manager\ec2_manager_gui_panic.log"
if (Test-Path $panic) {
  Get-Content $panic -Tail 50
} else {
  Write-Host "no panic log found at $panic"
}

Write-Host ""
Write-Host "Mark-of-the-Web streams (if any):"
Get-Item $exe -Stream * | Select-Object Stream,Length
