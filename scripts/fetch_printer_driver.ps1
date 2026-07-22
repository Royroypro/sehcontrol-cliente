$rel = "C:\sehcontrol\flutter\build\windows\x64\runner\Release"
$dst = "$rel\drivers\RustDeskPrinterDriver"

if (Test-Path "$dst\RustDeskPrinterDriver.inf") {
    Write-Host "Printer driver ya existe."
    exit 0
}

Write-Host "Descargando driver de impresora RustDesk..."

$tempZip = "$env:TEMP\rustdesk_printer_driver.zip"
$tempDir = "$env:TEMP\rustdesk_printer_driver"

Invoke-WebRequest `
  -Uri "https://github.com/rustdesk/hbb_common/releases/download/driver/rustdesk_printer_driver_v4-1.4.zip" `
  -OutFile $tempZip

if (Test-Path $tempDir) { Remove-Item $tempDir -Recurse -Force }
Expand-Archive $tempZip $tempDir -Force

New-Item -ItemType Directory -Force -Path "$rel\drivers" | Out-Null

Move-Item `
  "$tempDir\rustdesk_printer_driver_v4-1.4" `
  $dst `
  -Force

Write-Host "OK: Driver copiado correctamente a:"

Write-Host $dst
