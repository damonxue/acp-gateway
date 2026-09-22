$ErrorActionPreference = "Stop"

Set-Location (Join-Path $PSScriptRoot "..")
$version = (Select-String -Path "Cargo.toml" -Pattern '^version = "([^"]+)"$').Matches.Groups[1].Value
$architecture = if ([Environment]::Is64BitOperatingSystem) { "x86_64" } else { "x86" }
$staging = Join-Path "target" "windows-package"
$archive = Join-Path "target" "Agent-Gateway-$version-windows-$architecture.zip"

if (Test-Path $staging) { Remove-Item -Recurse -Force $staging }
if (Test-Path $archive) { Remove-Item -Force $archive }
New-Item -ItemType Directory -Force $staging | Out-Null

Copy-Item "target/release/agent-gateway.exe" "$staging/agent-gateway-daemon.exe"
Copy-Item "target/release/agent-gateway-app.exe" "$staging/agent-gateway-app.exe"
Copy-Item "README.md" "$staging/README.md"
Copy-Item "README.zh-CN.md" "$staging/README.zh-CN.md"

Compress-Archive -Path "$staging/*" -DestinationPath $archive
Write-Host "built $archive"
