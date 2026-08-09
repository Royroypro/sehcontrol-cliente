# Instala el DLL de ScreenCam con el limite de pacing SRT (2026-08-03).
#
# Corrige la causa medida de la pantalla negra: cada access unit se envia como
# un unico mensaje SRT, y sin techo de ancho de banda el control de congestion
# estimaba el enlace en 3,2 Gbps y espaciaba los paquetes ~11 us. Un keyframe
# (80-110 KB) salia como una rafaga de ~85 paquetes en ~1 ms, que el enlace de
# subida no absorbe: se perdia entera, siempre. Este build limita el pacing a
# 16 Mbps, muy por encima del bitrate real (~2 Mbps a 1080p).
#
# El respaldo apunta a proposito al MISMO archivo que dejo la instalacion del
# 2026-08-02: ahi esta el DLL original previo a cualquier cambio, que es el
# destino de rollback que interesa conservar.
#
# Requiere consola de PowerShell ELEVADA. Es idempotente.

$ErrorActionPreference = 'Stop'

$serviceName       = 'Sehcontrol'
$installDirectory  = 'C:\Program Files\Sehcontrol'
$installedDll      = Join-Path $installDirectory 'sehcontrol.dll'
$backupDll         = Join-Path $installDirectory 'sehcontrol.dll.bak-before-srtlatency-20260802'
$candidateDll      = 'C:\tmp\sehcontrol-entrega4-68b7d4ed6\artifacts\production-deploy\sehcontrol-screencam-srtpacing-release.dll'
$expectedCandidate = '0A3C03D0A8C49D0D57A925785B3E261AA5043CB79589921F7DAFD9A6A7CB5E28'
$logDirectory      = 'C:\ProgramData\Sehcontrol'
$logPath           = Join-Path $logDirectory 'screencam-deploy-20260803.log'

function Assert-Administrator {
    $identity  = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = [Security.Principal.WindowsPrincipal]$identity
    if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        throw 'Se requiere una consola de PowerShell elevada (Ejecutar como administrador).'
    }
}

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
                # La siguiente iteracion vuelve a enumerar cualquier proceso vivo.
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

Assert-Administrator

New-Item -ItemType Directory -Force -Path $logDirectory | Out-Null
Start-Transcript -Path $logPath -Force

try {
    if (-not (Test-Path -LiteralPath $candidateDll -PathType Leaf)) {
        throw "No existe el DLL candidato: $candidateDll"
    }
    if (-not (Test-Path -LiteralPath $installedDll -PathType Leaf)) {
        throw "No existe el DLL instalado: $installedDll"
    }

    $candidateHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $candidateDll).Hash
    if ($candidateHash -ne $expectedCandidate) {
        throw "El DLL candidato no coincide con el SHA-256 esperado. Actual: $candidateHash"
    }

    $originalHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $installedDll).Hash
    Write-Output "instalado_antes=$originalHash"
    Write-Output "candidato=$candidateHash"

    if ($originalHash -eq $candidateHash) {
        Write-Output 'sin_cambios=el DLL candidato ya esta instalado'
        Stop-Transcript
        return
    }

    if (-not (Test-Path -LiteralPath $backupDll)) {
        Copy-Item -LiteralPath $installedDll -Destination $backupDll
    }
    $backupHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $backupDll).Hash
    Write-Output "respaldo=$backupHash ($backupDll)"

    Stop-SehcontrolProcesses
    Copy-Item -LiteralPath $candidateDll -Destination $installedDll -Force

    $installedHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $installedDll).Hash
    if ($installedHash -ne $candidateHash) {
        throw 'El DLL instalado no coincide con el candidato'
    }

    Start-SehcontrolProcesses
    Write-Output "instalado_ahora=$installedHash"
    Write-Output 'resultado=OK'
    Stop-Transcript
} catch {
    $installError = $_
    try {
        if (Test-Path -LiteralPath $backupDll) {
            Stop-SehcontrolProcesses
            $currentHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $installedDll).Hash
            $backupHash  = (Get-FileHash -Algorithm SHA256 -LiteralPath $backupDll).Hash
            if ($currentHash -ne $backupHash) {
                Copy-Item -LiteralPath $backupDll -Destination $installedDll -Force
            }
            Start-SehcontrolProcesses
            Write-Output 'rollback=OK'
        }
    } catch {
        Write-Error "El rollback automatico fallo: $($_.Exception.Message)"
    }
    Write-Error "La instalacion fallo: $($installError.Exception.Message)"
    Stop-Transcript
    throw $installError
}
