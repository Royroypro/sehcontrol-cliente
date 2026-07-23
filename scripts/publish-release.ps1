[CmdletBinding()]
param(
    [Parameter(Mandatory = $true, Position = 0)]
    [ValidatePattern('^v?\d+\.\d+\.\d+$')]
    [string]$Version,

    [string[]]$AssetPath = @(),

    [switch]$PackageWindows,

    [string]$Remote = '',

    [string]$Branch = 'master'
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

function Invoke-Checked {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Command,

        [Parameter(ValueFromRemainingArguments = $true)]
        [string[]]$Arguments
    )

    & $Command @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "El comando '$Command $($Arguments -join ' ')' terminó con código $LASTEXITCODE."
    }
}

function Resolve-ReleaseAsset {
    param([Parameter(Mandatory = $true)][string]$Path)

    $resolved = Resolve-Path -LiteralPath $Path -ErrorAction Stop
    if (-not (Test-Path -LiteralPath $resolved.Path -PathType Leaf)) {
        throw "El recurso para el Release no es un archivo: $Path"
    }
    return $resolved.Path
}

$tag = if ($Version.StartsWith('v')) { $Version } else { "v$Version" }
$plainVersion = $tag.Substring(1)
$repositoryRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
Set-Location $repositoryRoot

foreach ($requiredCommand in @('git', 'gh')) {
    if (-not (Get-Command $requiredCommand -ErrorAction SilentlyContinue)) {
        throw "Falta '$requiredCommand'. Instálelo y vuelva a ejecutar el script."
    }
}

Invoke-Checked gh auth status

$availableRemotes = @(& git remote)
if (-not $Remote) {
    $Remote = if ($availableRemotes -contains 'sehcontrol') {
        'sehcontrol'
    } else {
        'origin'
    }
}
if ($availableRemotes -notcontains $Remote) {
    throw "No existe el remoto '$Remote'. Remotos disponibles: $($availableRemotes -join ', ')."
}

$remoteUrl = (& git remote get-url $Remote).Trim()
if ($LASTEXITCODE -ne 0 -or $remoteUrl -notmatch 'github\.com[/:]Royroypro/sehcontrol-cliente(?:\.git)?$') {
    throw "El remoto '$Remote' no apunta a Royroypro/sehcontrol-cliente: $remoteUrl"
}

$currentBranch = (& git branch --show-current).Trim()
if ($LASTEXITCODE -ne 0 -or $currentBranch -ne $Branch) {
    throw "Debe ejecutar la publicación desde la rama '$Branch'. Rama actual: '$currentBranch'."
}

$pendingChanges = & git status --porcelain --untracked-files=no
if ($LASTEXITCODE -ne 0) {
    throw 'No se pudo comprobar el estado del repositorio.'
}
if ($pendingChanges) {
    throw 'Hay cambios versionados sin confirmar. Cree un commit antes de publicar.'
}

& git show-ref --verify --quiet "refs/tags/$tag"
if ($LASTEXITCODE -eq 0) {
    throw "La etiqueta local '$tag' ya existe. Use una versión nueva."
}

& git ls-remote --exit-code --tags $Remote "refs/tags/$tag" *> $null
if ($LASTEXITCODE -eq 0) {
    throw "La etiqueta remota '$tag' ya existe. Use una versión nueva."
}

Invoke-Checked git fetch $Remote $Branch

$localCommit = (& git rev-parse HEAD).Trim()
$remoteCommit = (& git rev-parse "$Remote/$Branch").Trim()
if ($LASTEXITCODE -ne 0 -or $localCommit -ne $remoteCommit) {
    throw "HEAD ($localCommit) no coincide con $Remote/$Branch ($remoteCommit). Publique primero la rama."
}

$releaseAssets = [System.Collections.Generic.List[string]]::new()

if ($PackageWindows) {
    $windowsReleaseDir = Join-Path $repositoryRoot 'flutter\build\windows\x64\runner\Release'
    $windowsExecutable = Join-Path $windowsReleaseDir 'sehcontrol.exe'
    if (-not (Test-Path -LiteralPath $windowsExecutable -PathType Leaf)) {
        throw "No existe la compilación Windows esperada: $windowsExecutable"
    }

    $distDir = Join-Path $repositoryRoot 'dist'
    New-Item -ItemType Directory -Path $distDir -Force | Out-Null
    $windowsArchive = Join-Path $distDir "Sehcontrol-Windows-x64-$tag.zip"
    Compress-Archive -Path (Join-Path $windowsReleaseDir '*') `
        -DestinationPath $windowsArchive -CompressionLevel Optimal -Force
    $releaseAssets.Add($windowsArchive)
}

foreach ($asset in $AssetPath) {
    $releaseAssets.Add((Resolve-ReleaseAsset -Path $asset))
}

Write-Host "Publicando $tag desde $localCommit..." -ForegroundColor Cyan
Invoke-Checked git tag -a $tag -m "Sehcontrol Cliente $tag"
Invoke-Checked git push $Remote $tag

Write-Host 'Esperando a que GitHub Actions cree el Release...' -ForegroundColor Cyan
$releaseReady = $false
for ($attempt = 1; $attempt -le 30; $attempt++) {
    & gh release view $tag --repo Royroypro/sehcontrol-cliente *> $null
    if ($LASTEXITCODE -eq 0) {
        $releaseReady = $true
        break
    }
    Start-Sleep -Seconds 5
}

if (-not $releaseReady) {
    throw "La etiqueta se publicó, pero el Release no apareció después de 150 segundos. Revise GitHub Actions."
}

if ($releaseAssets.Count -gt 0) {
    Write-Host "Adjuntando $($releaseAssets.Count) archivo(s)..." -ForegroundColor Cyan
    $uploadArguments = @(
        'release', 'upload', $tag,
        '--repo', 'Royroypro/sehcontrol-cliente',
        '--clobber'
    ) + $releaseAssets.ToArray()
    Invoke-Checked gh @uploadArguments
}

$releaseUrl = (& gh release view $tag `
    --repo Royroypro/sehcontrol-cliente `
    --json url `
    --jq '.url').Trim()
if ($LASTEXITCODE -ne 0) {
    throw 'El Release se creó, pero no se pudo obtener su dirección.'
}

Write-Host "Release publicado correctamente: $releaseUrl" -ForegroundColor Green
