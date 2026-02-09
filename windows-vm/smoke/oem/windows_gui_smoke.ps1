$ErrorActionPreference = "Stop"

$sharePath = "\\host.lan\Data"
$resultFile = Join-Path $sharePath "windows_gui_smoke_result.txt"
$markerFile = Join-Path $sharePath "windows_gui_smoke_marker.txt"
$exePath = Join-Path $sharePath "dist\windows\ec2_manager_gui.exe"
$workingDir = Split-Path -Parent $exePath
$appDataDir = Join-Path $sharePath "appdata"
$configDir = Join-Path $appDataDir "ec2-manager"
$configPath = Join-Path $configDir "config.ini"
$msys2Candidates = @(
  "C:\msys64\usr\bin\bash.exe"
  "C:\msys64\usr\bin\bash.exe"
  "C:\tools\msys64\usr\bin\bash.exe"
) | Select-Object -Unique
$msys2Bash = $msys2Candidates | Where-Object { Test-Path $_ } | Select-Object -First 1

for ($i = 0; $i -lt 120; $i++) {
  if (Test-Path $sharePath) { break }
  Start-Sleep -Seconds 2
}

if (-not (Test-Path $sharePath)) {
  exit 1
}

if (-not (Test-Path $exePath)) {
  Set-Content -Path $resultFile -Value "FAIL:missing-exe" -Encoding Ascii -Force
  exit 1
}

if (-not $msys2Bash) {
  Set-Content -Path $resultFile -Value "FAIL:msys2-not-found" -Encoding Ascii -Force
  exit 1
}

Remove-Item -Path $markerFile -ErrorAction SilentlyContinue
Remove-Item -Path $resultFile -ErrorAction SilentlyContinue

$msys2Root = Split-Path -Parent (Split-Path -Parent (Split-Path -Parent $msys2Bash))
New-Item -ItemType Directory -Path $configDir -Force | Out-Null
Set-Content -Path $configPath -Value "default_mode=sim`ndefault_terminal=msys2-bash`n" -Encoding Ascii -Force
$env:APPDATA = $appDataDir
$env:MSYS2_ROOT = $msys2Root

$env:EC2_MANAGER_GUI_SMOKE_MARKER = $markerFile
$env:EC2_MANAGER_GUI_SMOKE_EXPECTED_TEXT = "[SIM MODE] session open for"
$env:EC2_MANAGER_GUI_SMOKE_EXIT_ON_MARKER = "1"
$env:EC2_MANAGER_GUI_SMOKE_AUTO_CONNECT = "1"

$proc = Start-Process -FilePath $exePath -ArgumentList "--mode", "sim", "--no-dry-run" -WorkingDirectory $workingDir -PassThru
$deadline = (Get-Date).AddSeconds(240)

while ((Get-Date) -lt $deadline) {
  if (Test-Path $markerFile) {
    Set-Content -Path $resultFile -Value "PASS`nterminal=msys2-bash`nmsys2_bash=$msys2Bash" -Encoding Ascii -Force
    if (-not $proc.HasExited) {
      Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue
    }
    exit 0
  }
  Start-Sleep -Seconds 2
  $proc.Refresh()
  if ($proc.HasExited) {
    Set-Content -Path $resultFile -Value "FAIL:gui-exited:$($proc.ExitCode)" -Encoding Ascii -Force
    exit 1
  }
}

Set-Content -Path $resultFile -Value "FAIL:timeout" -Encoding Ascii -Force
if (-not $proc.HasExited) {
  Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue
}
exit 1
