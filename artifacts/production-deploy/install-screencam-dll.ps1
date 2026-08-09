$ErrorActionPreference = 'Stop'

$serviceName = 'Sehcontrol'
$installDirectory = 'C:\Program Files\Sehcontrol'
$installedDll = Join-Path $installDirectory 'sehcontrol.dll'
$backupDll = Join-Path $installDirectory 'sehcontrol.dll.bak-before-screencam-20260801'
$candidateDll = 'C:\tmp\sehcontrol-entrega4-68b7d4ed6\artifacts\production-deploy\sehcontrol-screencam-43928c649-release.dll'
$launcher = Join-Path $installDirectory 'Sehcontrol.exe'
$logDirectory = 'C:\ProgramData\Sehcontrol'
$logPath = Join-Path $logDirectory 'screencam-deploy-20260801.log'

function Stop-SehcontrolProcesses {
    $service = Get-Service -Name $serviceName
    if ($service.Status -ne [System.ServiceProcess.ServiceControllerStatus]::Stopped) {
        Stop-Service -Name $serviceName -Force
        $service.WaitForStatus(
            [System.ServiceProcess.ServiceControllerStatus]::Stopped,
            [TimeSpan]::FromSeconds(30)
        )
    }
    $deadline = [DateTime]::UtcNow.AddSeconds(30)
    do {
        $processes = @(Get-Process -Name 'sehcontrol' -ErrorAction SilentlyContinue)
        foreach ($process in $processes) {
            Stop-Process -Id $process.Id -Force -ErrorAction SilentlyContinue
            try {
                $process.WaitForExit(1000)
            } catch {
                # La siguiente iteración vuelve a enumerar cualquier proceso vivo.
            }
        }
        if ($processes.Count -gt 0) {
            Start-Sleep -Milliseconds 250
        }
    } while ($processes.Count -gt 0 -and [DateTime]::UtcNow -lt $deadline)

    $remaining = @(Get-Process -Name 'sehcontrol' -ErrorAction SilentlyContinue)
    if ($remaining.Count -gt 0) {
        throw "Persisten procesos Sehcontrol: $($remaining.Id -join ',')"
    }
    Start-Sleep -Seconds 2
    & tasklist.exe /m sehcontrol.dll
}

function Start-SehcontrolProcesses {
    $service = Get-Service -Name $serviceName
    if ($service.Status -ne [System.ServiceProcess.ServiceControllerStatus]::Running) {
        Start-Service -Name $serviceName
        $service.WaitForStatus(
            [System.ServiceProcess.ServiceControllerStatus]::Running,
            [TimeSpan]::FromSeconds(30)
        )
    }
}

New-Item -ItemType Directory -Force -Path $logDirectory | Out-Null
Start-Transcript -Path $logPath -Force

if (-not (Test-Path -LiteralPath $candidateDll -PathType Leaf)) {
    throw "No existe el DLL candidato: $candidateDll"
}

$originalHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $installedDll).Hash
$candidateHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $candidateDll).Hash

if (-not (Test-Path -LiteralPath $backupDll)) {
    Copy-Item -LiteralPath $installedDll -Destination $backupDll
}
$backupHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $backupDll).Hash
if ($originalHash -ne $candidateHash -and $backupHash -ne $originalHash) {
    throw 'El respaldo no coincide con el DLL original instalado'
}

try {
    Stop-SehcontrolProcesses
    Copy-Item -LiteralPath $candidateDll -Destination $installedDll -Force
    $installedHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $installedDll).Hash
    if ($installedHash -ne $candidateHash) {
        throw 'El DLL instalado no coincide con el candidato'
    }
    Start-SehcontrolProcesses
    Write-Output "installed=$installedHash"
    Write-Output "backup=$backupHash"
    Stop-Transcript
} catch {
    $installError = $_
    try {
        Stop-SehcontrolProcesses
        $currentHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $installedDll).Hash
        if ($currentHash -ne $backupHash) {
            Copy-Item -LiteralPath $backupDll -Destination $installedDll -Force
        }
        Start-SehcontrolProcesses
    } catch {
        Write-Error "El rollback automático falló: $($_.Exception.Message)"
    }
    Write-Error "La instalación falló: $($installError.Exception.Message)"
    Stop-Transcript
    throw $installError
}
