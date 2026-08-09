# Instala el DLL de ScreenCam compilado el 2026-08-02 desde el arbol actual
# (rama feature/screencam-srt-preview) sobre la instalacion de este equipo.
#
# Motivo: la instalacion vigente se rehizo hoy 17:44 con el instalador completo,
# de modo que el binario en ejecucion no corresponde a ningun artefacto conocido
# y el respaldo que dejaba el procedimiento anterior ya no existe. Este script
# crea su propio respaldo, con nombre nuevo, del DLL que este instalado en el
# momento de correrlo -- sea cual sea.
#
# Requiere consola de PowerShell ELEVADA (detener el servicio y escribir en
# Program Files necesitan administrador).
#
# Es idempotente: si el DLL candidato ya esta instalado, no hace nada.

$ErrorActionPreference = 'Stop'

$serviceName       = 'Sehcontrol'
$installDirectory  = 'C:\Program Files\Sehcontrol'
$installedDll      = Join-Path $installDirectory 'sehcontrol.dll'
$backupDll         = Join-Path $installDirectory 'sehcontrol.dll.bak-before-srtlatency-20260802'
$candidateDll      = 'C:\tmp\sehcontrol-entrega4-68b7d4ed6\artifacts\production-deploy\sehcontrol-screencam-srtlatency-diag-release.dll'
$expectedCandidate = '3394B65FFF35E750D437300240B611C88492EBEA16973FA7A0DA792C9153D937'
$logDirectory      = 'C:\ProgramData\Sehcontrol'
$logPath           = Join-Path $logDirectory 'screencam-deploy-20260802.log'

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

    # El respaldo se toma del DLL que este instalado AHORA, y solo si todavia
    # no existe: una segunda corrida no debe pisar el respaldo bueno con una
    # copia del candidato a medio instalar.
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
