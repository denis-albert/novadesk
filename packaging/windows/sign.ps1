#Requires -Version 5.1
<#
.SYNOPSIS
    Signature Authenticode des artefacts NovaDesk (Windows) — SHA-256 + horodatage.

.DESCRIPTION
    Enveloppe « signtool.exe » (Windows SDK) pour signer MSI/EXE/DLL avec :
      - condensat SHA-256 (/fd SHA256),
      - horodatage RFC 3161 (/tr … /td SHA256) — la signature survit à
        l'expiration du certificat.

    Matériel de signature (par ordre de priorité), fourni par des SECRETS CI :
      1. Empreinte de certificat déjà présent dans le magasin / jeton HSM :
         $env:NOVADESK_SIGN_THUMBPRINT   (recommandé pour un certificat EV sur HSM) ;
      2. PFX encodé base64 (certificat + clé) : $env:NOVADESK_SIGN_PFX_BASE64
         (déchiffré vers un fichier temporaire, supprimé après signature) ;
      3. PFX sur disque : $env:NOVADESK_SIGN_PFX_PATH.
    Mot de passe du PFX : $env:NOVADESK_SIGN_PFX_PASSWORD (jamais journalisé).
    URL d'horodatage : $env:NOVADESK_TIMESTAMP_URL (défaut DigiCert).

    En l'absence de tout matériel : erreur (sauf -DryRun, qui n'imprime que la
    commande — utile pour valider la logique sans certificat, comme ici).

.EXAMPLE
    ./sign.ps1 -Path dist\NovaDesk-0.1.0-x86_64.msi
.EXAMPLE
    ./sign.ps1 -Path (Get-ChildItem dist\*.msi) -DryRun
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string[]] $Path,

    [string] $TimestampUrl,

    # N'imprime que la commande signtool (aucune signature ni magasin requis).
    [switch] $DryRun
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

# URL d'horodatage par défaut si non fournie (évite un défaut de paramètre complexe).
if (-not $TimestampUrl) {
    if ($env:NOVADESK_TIMESTAMP_URL) { $TimestampUrl = $env:NOVADESK_TIMESTAMP_URL }
    else { $TimestampUrl = 'http://timestamp.digicert.com' }
}

function Write-Info { param([string] $m) Write-Host "[sign] $m" }

# Localise signtool.exe : PATH, puis dossiers classiques du Windows SDK.
function Find-SignTool {
    $cmd = Get-Command signtool.exe -ErrorAction SilentlyContinue
    if ($cmd) { return $cmd.Source }
    $bases = @()
    $pf86 = [Environment]::GetEnvironmentVariable('ProgramFiles(x86)')
    if ($pf86) { $bases += (Join-Path $pf86 'Windows Kits\10\bin') }
    if ($env:ProgramFiles) { $bases += (Join-Path $env:ProgramFiles 'Windows Kits\10\bin') }
    foreach ($base in $bases) {
        if (-not (Test-Path -LiteralPath $base)) { continue }
        $trouve = Get-ChildItem -LiteralPath $base -Recurse -Filter 'signtool.exe' -ErrorAction SilentlyContinue |
            Where-Object { $_.FullName -match 'x64' } | Select-Object -First 1
        if ($trouve) { return $trouve.FullName }
    }
    return $null
}

# Construit les arguments signtool selon le matériel de signature disponible.
# Renvoie une table @{ Args = [string[]]; TempPfx = <chemin ou $null> }.
function Resolve-SigningArgs {
    $commun = @('sign', '/fd', 'SHA256', '/tr', $TimestampUrl, '/td', 'SHA256')

    if ($env:NOVADESK_SIGN_THUMBPRINT) {
        Write-Info "Certificat par empreinte (magasin/HSM)."
        return @{ Args = $commun + @('/sha1', $env:NOVADESK_SIGN_THUMBPRINT); TempPfx = $null }
    }

    $pfxPath = $null
    $tempPfx = $null
    if ($env:NOVADESK_SIGN_PFX_BASE64) {
        Write-Info "Certificat PFX depuis un secret base64."
        $tempPfx = Join-Path $env:TEMP ("novadesk-" + [guid]::NewGuid().ToString('N') + '.pfx')
        [IO.File]::WriteAllBytes($tempPfx, [Convert]::FromBase64String($env:NOVADESK_SIGN_PFX_BASE64))
        $pfxPath = $tempPfx
    } elseif ($env:NOVADESK_SIGN_PFX_PATH) {
        $pfxPath = $env:NOVADESK_SIGN_PFX_PATH
    }

    if ($pfxPath) {
        $a = $commun + @('/f', $pfxPath)
        if ($env:NOVADESK_SIGN_PFX_PASSWORD) { $a += @('/p', $env:NOVADESK_SIGN_PFX_PASSWORD) }
        return @{ Args = $a; TempPfx = $tempPfx }
    }

    if ($DryRun) {
        Write-Info "Aucun matériel de signature — mode -DryRun : placeholder d'empreinte."
        return @{ Args = $commun + @('/sha1', '<THUMBPRINT>'); TempPfx = $null }
    }
    throw "Aucun matériel de signature configuré (NOVADESK_SIGN_THUMBPRINT / _PFX_BASE64 / _PFX_PATH)."
}

$signtool = Find-SignTool
if (-not $signtool -and -not $DryRun) {
    throw "signtool.exe introuvable (installer le Windows SDK)."
}

$resolue = Resolve-SigningArgs
try {
    foreach ($cible in $Path) {
        if (-not (Test-Path -LiteralPath $cible) -and -not $DryRun) {
            throw "Fichier à signer introuvable : $cible"
        }
        $ligne = $resolue.Args + @($cible)
        if ($DryRun) {
            # Mot de passe masqué dans l'affichage.
            $affiche = $ligne | ForEach-Object { if ($_ -eq $env:NOVADESK_SIGN_PFX_PASSWORD -and $_) { '***' } else { $_ } }
            Write-Info "DRYRUN: `"$($signtool)`" $($affiche -join ' ')"
            continue
        }
        Write-Info "Signature de $cible"
        & $signtool @ligne
        if ($LASTEXITCODE -ne 0) { throw "signtool a échoué (code $LASTEXITCODE) sur $cible." }
        & $signtool 'verify' '/pa' '/v' $cible
        if ($LASTEXITCODE -ne 0) { throw "Vérification de signature échouée sur $cible." }
    }
}
finally {
    if ($resolue.TempPfx -and (Test-Path -LiteralPath $resolue.TempPfx)) {
        Remove-Item -LiteralPath $resolue.TempPfx -Force
    }
}
Write-Info "Terminé."
