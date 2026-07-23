param(
    [ValidateSet("x64", "arm64")]
    [string]$Architecture = $(if ($env:PROCESSOR_ARCHITECTURE -eq "ARM64") { "arm64" } else { "x64" })
)

$ErrorActionPreference = "Stop"

$repoRoot = Split-Path -Parent $PSScriptRoot
$releaseDir = Join-Path $repoRoot "flutter\build\windows\$Architecture\runner\Release"
$driverDir = Join-Path $releaseDir "drivers\RustDeskPrinterDriver"
$infFile = Join-Path $driverDir "RustDeskPrinterDriver.inf"

if (Test-Path -LiteralPath $infFile -PathType Leaf) {
    Write-Host "Printer driver already exists at $infFile"
    exit 0
}

Write-Host "Downloading RustDesk printer driver..."

$tempDir = Join-Path ([System.IO.Path]::GetTempPath()) ("sehcontrol-printer-driver-" + [guid]::NewGuid())
$zipFile = Join-Path $tempDir "rustdesk_printer_driver_v4-1.4.zip"
$expandedDir = Join-Path $tempDir "expanded"
$downloadUrl = "https://github.com/rustdesk/hbb_common/releases/download/driver/rustdesk_printer_driver_v4-1.4.zip"

try {
    New-Item -ItemType Directory -Path $tempDir | Out-Null
    Invoke-WebRequest -Uri $downloadUrl -OutFile $zipFile
    Expand-Archive -LiteralPath $zipFile -DestinationPath $expandedDir

    $downloadedDriverDir = Join-Path $expandedDir "rustdesk_printer_driver_v4-1.4"
    $downloadedInf = Join-Path $downloadedDriverDir "RustDeskPrinterDriver.inf"
    if (-not (Test-Path -LiteralPath $downloadedInf -PathType Leaf)) {
        throw "The downloaded package does not contain RustDeskPrinterDriver.inf"
    }

    New-Item -ItemType Directory -Force -Path (Split-Path -Parent $driverDir) | Out-Null
    if (Test-Path -LiteralPath $driverDir) {
        Remove-Item -LiteralPath $driverDir -Recurse -Force
    }
    Copy-Item -LiteralPath $downloadedDriverDir -Destination $driverDir -Recurse -Force

    if (-not (Test-Path -LiteralPath $infFile -PathType Leaf)) {
        throw "Printer driver copy failed: $infFile was not created"
    }

    Write-Host "Printer driver copied successfully to $driverDir"
}
finally {
    if (Test-Path -LiteralPath $tempDir) {
        Remove-Item -LiteralPath $tempDir -Recurse -Force
    }
}
