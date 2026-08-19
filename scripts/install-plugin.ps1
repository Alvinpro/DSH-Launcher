# install-plugin.ps1 - one-shot installer for the dsh-launcher plugin
# The bundle ships its exe inside the package (dist/) and has NO lifecycle
# scripts, so `dsh plugin add` succeeds on the FIRST try (pnpm >=10 does not
# block script-less packages). This script then runs the package's install
# step immediately so the desktop shortcut appears right away - no need to
# wait for the next dsh start.
#
# Usage:
#   powershell -ExecutionPolicy Bypass -File scripts\install-plugin.ps1            # profile: web
#   powershell -ExecutionPolicy Bypass -File scripts\install-plugin.ps1 -Profile demo

param(
    [string]$Profile = "web",
    [string]$Repo = "github:alvinpro/dsh-launcher"
)

$ErrorActionPreference = "Continue"
$pkgDir = Join-Path $env:USERPROFILE ".dsh\profiles\$Profile\node_modules\dsh-launcher"

Write-Host "=== Installing dsh-launcher plugin (profile: $Profile) ==="

$prevEAP = $ErrorActionPreference
$ErrorActionPreference = "Continue"
$out = $null
try {
    $out = & dsh plugin --profile $Profile add $Repo 2>&1
} catch {
    $out = "dsh: $($_.Exception.Message)"
}
$ErrorActionPreference = $prevEAP
$code = if ($null -ne $LASTEXITCODE) { $LASTEXITCODE } else { 1 }

$out | Select-Object -Last 8
if ($code -ne 0) {
    Write-Warning "Install failed (exit=$code). See output above."
    exit $code
}

$installJs = Join-Path $pkgDir "lib\install.js"
if (Test-Path $installJs) {
    Write-Host ""
    Write-Host "Initializing (copy exe + desktop shortcut)..."
    node $installJs
} else {
    Write-Warning "Package installed but lib/install.js not found at $pkgDir; shortcut will be created on the next dsh start."
}

$pkgExe = Join-Path $pkgDir "dist\dsh-launcher.exe"
$lnk = Join-Path ([Environment]::GetFolderPath('Desktop')) 'DSH Launcher.lnk'
Write-Host ""
Write-Host "OK - dsh-launcher installed:"
Write-Host "  bundled exe:      $pkgExe -> $(Test-Path $pkgExe)"
Write-Host "  desktop shortcut: $lnk -> $(Test-Path $lnk) (points at the bundled exe, nothing copied)"
Write-Host ""
Write-Host "Note: if you install via plain 'dsh plugin add' instead of this script,"
Write-Host "the desktop shortcut is created automatically on the NEXT dsh start"
Write-Host "(pnpm >=10 blocks install-time scripts, so init runs on dsh start, not at add)."