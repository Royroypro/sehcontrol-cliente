# Copia los logs del servicio Sehcontrol (corre como LocalSystem, cuyo perfil
# no es legible sin elevacion) a una carpeta accesible, para poder analizar el
# rastro [screencam-preview-diag] de la sesion de previsualizacion.
#
# Solo lee y copia: no toca la instalacion, el servicio ni los logs originales.
#
# Requiere consola de PowerShell ELEVADA.

$ErrorActionPreference = 'Stop'

# hbb_common::config::patch() reescribe en Windows
#   system32\config\systemprofile  ->  ServiceProfiles\LocalService
# asi que los procesos que corren como SYSTEM (--server, que es el que hace la
# captura, y --service) NO escriben bajo systemprofile sino aca. Cada rol usa
# su propio subdirectorio segun init_log(): server\, service\, cm\.
$candidateRoots = @(
    'C:\Windows\ServiceProfiles\LocalService\AppData\Roaming\Sehcontrol\log',
    'C:\Windows\System32\config\systemprofile\AppData\Roaming\Sehcontrol\log',
    'C:\Windows\SysWOW64\config\systemprofile\AppData\Roaming\Sehcontrol\log'
)
$sourceLog   = $candidateRoots | Where-Object { Test-Path -LiteralPath $_ } | Select-Object -First 1
$destination = 'C:\tmp\seh-service-log'

$identity  = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = [Security.Principal.WindowsPrincipal]$identity
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'Se requiere una consola de PowerShell elevada (Ejecutar como administrador).'
}

if (-not $sourceLog) {
    Write-Output 'No se encontro el directorio de logs en ninguna ruta candidata:'
    $candidateRoots | ForEach-Object { Write-Output "  $_" }
    throw 'Directorio de logs del servicio no encontrado.'
}
Write-Output "origen=$sourceLog"

New-Item -ItemType Directory -Force -Path $destination | Out-Null

# Solo lo escrito en las ultimas 6 horas: alcanza para la sesion en curso y
# evita arrastrar meses de historial.
$cutoff = (Get-Date).AddHours(-6)
$files  = Get-ChildItem -LiteralPath $sourceLog -Recurse -File |
          Where-Object { $_.LastWriteTime -ge $cutoff }

if ($files.Count -eq 0) {
    Write-Output 'sin_archivos_recientes=1'
    Write-Output 'Archivos mas nuevos disponibles:'
    Get-ChildItem -LiteralPath $sourceLog -Recurse -File |
        Sort-Object LastWriteTime -Descending |
        Select-Object -First 5 FullName, Length, LastWriteTime |
        Format-Table -AutoSize
} else {
    # Cada rol escribe en su propio subdirectorio (server\, service\, cm\) y los
    # nombres se repiten entre ellos, asi que el destino conserva el subdirectorio
    # en el nombre en vez de aplanarlo y pisar archivos.
    foreach ($file in $files) {
        $relative = $file.FullName.Substring($sourceLog.Length).TrimStart('\')
        $flatName = $relative -replace '\\', '__'
        Copy-Item -LiteralPath $file.FullName -Destination (Join-Path $destination $flatName) -Force
        Write-Output ("copiado=" + $flatName + " bytes=" + $file.Length + " modificado=" + $file.LastWriteTime)
    }
}

# Dar lectura al usuario interactivo, que es quien analizara los archivos.
$user = "$env:USERDOMAIN\$env:USERNAME"
& icacls.exe $destination /grant "${user}:(OI)(CI)R" /T | Out-Null

Write-Output "destino=$destination"
Write-Output '--- coincidencias de diagnostico ---'
foreach ($pattern in @('screencam-preview-diag', 'scrap-dxgi-diag', '\[screencam\]')) {
    $hits = @(Select-String -Path (Join-Path $destination '*') `
                            -Pattern $pattern -ErrorAction SilentlyContinue)
    Write-Output ("patron='" + $pattern + "' lineas=" + $hits.Count)
}
