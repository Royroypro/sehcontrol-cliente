# Cambia la version de Sehcontrol en los dos archivos que hay que tocar a mano.
#
# Por que dos y no uno: src/version.rs -de donde sale crate::VERSION, que es lo
# que el cliente compara contra el panel- lo genera hbb_common::gen_version()
# leyendo Cargo.toml en cada build, asi que no se edita. flutter/pubspec.yaml
# en cambio es independiente y define la version del recurso de Windows (el
# "1.4.9+67" que muestra Propiedades del .exe).
#
# Mantenerlos sincronizados a mano es justo lo que se desincroniza. Si la
# version publicada en el panel no coincide con la que el binario reporta, los
# equipos entran en un bucle: ofrecen la actualizacion, la instalan, siguen
# viendo la misma version y la vuelven a ofrecer.
#
# El numero despues del "+" en pubspec es el build number: se incrementa solo.

[CmdletBinding()]
param(
    # Sin valor, la pide de forma interactiva.
    [string]$Version,
    # Solo informa la version actual y sale.
    [switch]$Mostrar
)

$ErrorActionPreference = 'Stop'

$root = Split-Path -Parent $PSScriptRoot
$cargoPath = Join-Path $root 'Cargo.toml'
$pubspecPath = Join-Path $root 'flutter\pubspec.yaml'

foreach ($p in @($cargoPath, $pubspecPath)) {
    if (-not (Test-Path -LiteralPath $p)) { throw "No se encontro $p" }
}

# Primera coincidencia: es la de [package]. Las de las dependencias vienen
# despues y no deben tocarse.
$cargo = Get-Content -LiteralPath $cargoPath
$cargoIdx = ($cargo | Select-String -Pattern '^version\s*=' | Select-Object -First 1).LineNumber - 1
if ($cargoIdx -lt 0) { throw 'No se encontro la version en Cargo.toml' }
$actual = [regex]::Match($cargo[$cargoIdx], '"([^"]+)"').Groups[1].Value

$pubspec = Get-Content -LiteralPath $pubspecPath
$pubIdx = ($pubspec | Select-String -Pattern '^version:' | Select-Object -First 1).LineNumber - 1
if ($pubIdx -lt 0) { throw 'No se encontro la version en pubspec.yaml' }
$pubMatch = [regex]::Match($pubspec[$pubIdx], '^version:\s*([0-9.]+)(?:\+(\d+))?')
$pubVersion = $pubMatch.Groups[1].Value
$build = if ($pubMatch.Groups[2].Success) { [int]$pubMatch.Groups[2].Value } else { 0 }

Write-Output "Version actual : $actual   (pubspec: $pubVersion+$build)"
if ($actual -ne $pubVersion) {
    Write-Warning "Cargo.toml y pubspec.yaml NO coinciden. Al cambiar la version quedan sincronizados."
}
if ($Mostrar) { return }

if (-not $Version) {
    # Sugerencia: sube el ultimo componente, que es el caso habitual.
    $partes = $actual.Split('.')
    $partes[-1] = [string]([int]$partes[-1] + 1)
    $sugerida = $partes -join '.'
    $Version = Read-Host "Nueva version (Enter para $sugerida)"
    if ([string]::IsNullOrWhiteSpace($Version)) { $Version = $sugerida }
}
$Version = $Version.Trim()

# Mismo formato que acepta el panel al declararla: numerica pura. Un valor con
# sufijos no se puede ordenar de forma confiable contra la version instalada.
if ($Version -notmatch '^\d+(\.\d+){1,3}$') {
    throw "La version debe ser numerica, por ejemplo 1.5.0. Recibido: '$Version'"
}
if ($Version -eq $actual) {
    Write-Output 'La version no cambio. No se toco ningun archivo.'
    return
}

# Rechaza retroceder: una version menor no la ofreceria ningun cliente, y
# descubrirlo despues de compilar y publicar cuesta una vuelta entera.
$comparar = {
    param($a, $b)
    $pa = $a.Split('.'); $pb = $b.Split('.')
    for ($i = 0; $i -lt [Math]::Max($pa.Length, $pb.Length); $i++) {
        $va = if ($i -lt $pa.Length) { [int]$pa[$i] } else { 0 }
        $vb = if ($i -lt $pb.Length) { [int]$pb[$i] } else { 0 }
        if ($va -ne $vb) { return $va - $vb }
    }
    return 0
}
if ((& $comparar $Version $actual) -lt 0) {
    throw "La version $Version es menor que la actual $actual. Ningun equipo instalado la ofreceria."
}

$cargo[$cargoIdx] = $cargo[$cargoIdx] -replace '"[^"]+"', "`"$Version`""
$pubspec[$pubIdx] = "version: $Version+$($build + 1)"

Set-Content -LiteralPath $cargoPath -Value $cargo -Encoding UTF8
Set-Content -LiteralPath $pubspecPath -Value $pubspec -Encoding UTF8

# Cargo.lock tambien registra la version del propio paquete, y build.py compila
# con --locked: sin esto el build aborta con "the lock file needs to be updated
# but --locked was passed" y hay que descubrir por que a mano.
#
# Se deja que cargo lo reescriba en vez de editarlo con texto: el formato del
# lock es suyo. --offline para que no salga a la red solo por cambiar un
# numero. Su codigo de salida se ignora a proposito -- en offline puede quejarse
# de otras cosas mientras igual sincroniza el lock, asi que lo que se comprueba
# es el resultado.
Push-Location $root
try {
    & cargo metadata --offline --format-version 1 *> $null
} finally {
    Pop-Location
    # Sin esto el codigo de salida de cargo se convierte en el del script, y
    # quien lo llame -el .bat, por ejemplo- lo lee como un fallo aunque el
    # lock haya quedado bien. El resultado real se comprueba abajo.
    $global:LASTEXITCODE = 0
}

$lockPath = Join-Path $root 'Cargo.lock'
$lockOk = $false
if (Test-Path -LiteralPath $lockPath) {
    $lock = Get-Content -LiteralPath $lockPath -Raw
    # La entrada del paquete propio, no la de una dependencia que se llame igual.
    $lockOk = $lock -match "(?m)^name = ""sehcontrol""\r?\nversion = ""$([regex]::Escape($Version))"""
}
if (-not $lockOk) {
    Write-Warning "Cargo.lock no quedo sincronizado. Antes de compilar, corre: cargo metadata --offline"
}

Write-Output ''
Write-Output "Version cambiada: $actual  ->  $Version"
Write-Output "  Cargo.toml            version = `"$Version`""
Write-Output "  flutter/pubspec.yaml  version: $Version+$($build + 1)"
Write-Output "  Cargo.lock            sincronizado"
Write-Output "  src/version.rs        se regenera solo al compilar"
Write-Output ''
Write-Output 'Recorda: al publicar en el panel hay que declarar EXACTAMENTE esta misma version.'
