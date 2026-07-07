#Requires -Version 5.1
<#
.SYNOPSIS
    CLI de déploiement NovaDesk — parité AnyDesk (déploiement de masse).

.DESCRIPTION
    Enveloppe les opérations de déploiement silencieux attendues par les équipes
    IT (GPO/Intune/SCCM), avec la même « forme » de ligne de commande qu'AnyDesk :

        novadesk-cli.ps1 --install --silent [--msi <chemin.msi>]
        novadesk-cli.ps1 --remove [--silent]
        novadesk-cli.ps1 --get-id
        novadesk-cli.ps1 --set-password <motdepasse>
        novadesk-cli.ps1 --register-license <clé>

    Les opérations d'INSTALLATION/DÉSINSTALLATION sont pleinement fonctionnelles
    (elles pilotent msiexec). Les opérations liées au CLIENT (--get-id,
    --set-password, --register-license) délèguent à « novadesk.exe » une fois
    installé ; tant que le runner Flutter n'expose pas ces drapeaux (câblage
    client à venir), un repli documenté écrit/lit l'état sous ProgramData et
    signale clairement ce qui reste à câbler. AUCUN secret n'est journalisé.

.NOTES
    Nécessite des droits administrateur pour --install/--remove/--set-password.
#>
[CmdletBinding()]
param(
    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]] $Args
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

# UpgradeCode STABLE (identique à NovaDesk.wxs) — sert à retrouver le produit.
$Script:UpgradeCode = '{9820A326-4BE2-40EC-9BE4-882D9AAA3E65}'
$Script:AppName     = 'NovaDesk'
$Script:DataDir     = Join-Path $env:ProgramData 'NovaDesk'

function Write-Info { param([string] $Message) Write-Host "[novadesk] $Message" }
function Write-Warn { param([string] $Message) Write-Warning "[novadesk] $Message" }

function Show-Usage {
    @'
Usage : novadesk-cli.ps1 <commande> [options]

  --install --silent [--msi <chemin.msi>]  Installe en silencieux (msiexec /qn).
  --remove  [--silent]                     Désinstalle le produit NovaDesk.
  --get-id                                 Affiche l'ID NovaDesk de ce poste.
  --set-password <motdepasse>              Définit le mot de passe d'accès non surveillé.
  --register-license <clé>                 Enregistre une licence.
  --help                                   Affiche cette aide.
'@ | Write-Host
}

# Emplacement de l'exécutable client installé (Program Files\NovaDesk).
function Get-ClientExe {
    $exe = Join-Path ([Environment]::GetFolderPath('ProgramFiles')) 'NovaDesk\novadesk.exe'
    if (Test-Path -LiteralPath $exe) { return $exe }
    return $null
}

# Résout le ProductCode {GUID} à partir des clés de désinstallation (64 puis 32 bits).
function Get-NovaDeskProductCode {
    $racines = @(
        'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall',
        'HKLM:\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall'
    )
    foreach ($racine in $racines) {
        if (-not (Test-Path -LiteralPath $racine)) { continue }
        foreach ($cle in Get-ChildItem -LiteralPath $racine) {
            $nom = (Get-ItemProperty -LiteralPath $cle.PSPath -ErrorAction SilentlyContinue).DisplayName
            if ($nom -eq $Script:AppName) { return $cle.PSChildName }
        }
    }
    return $null
}

function Invoke-Msiexec {
    param([string[]] $MsiArgs)
    Write-Info "msiexec $($MsiArgs -join ' ')"
    $proc = Start-Process -FilePath 'msiexec.exe' -ArgumentList $MsiArgs -Wait -PassThru
    # 0 = OK, 3010 = OK mais redémarrage requis.
    if ($proc.ExitCode -ne 0 -and $proc.ExitCode -ne 3010) {
        throw "msiexec a échoué (code $($proc.ExitCode))."
    }
    return $proc.ExitCode
}

function Invoke-Install {
    param([string] $MsiPath, [bool] $Silent)
    if (-not $MsiPath) {
        $trouve = Get-ChildItem -LiteralPath $PSScriptRoot -Filter 'NovaDesk-*.msi' -ErrorAction SilentlyContinue |
            Select-Object -First 1
        if ($trouve) { $MsiPath = $trouve.FullName }
    }
    if (-not $MsiPath -or -not (Test-Path -LiteralPath $MsiPath)) {
        throw "MSI introuvable. Préciser --msi <chemin.msi>."
    }
    $a = @('/i', "`"$MsiPath`"", '/norestart')
    if ($Silent) { $a += '/qn' }
    $a += @('/l*v', "`"$(Join-Path $env:TEMP 'novadesk-install.log')`"")
    Invoke-Msiexec -MsiArgs $a | Out-Null
    Write-Info "Installation terminée."
}

function Invoke-Remove {
    param([bool] $Silent)
    $code = Get-NovaDeskProductCode
    if (-not $code) { throw "NovaDesk ne semble pas installé (ProductCode introuvable)." }
    $a = @('/x', $code, '/norestart')
    if ($Silent) { $a += '/qn' }
    Invoke-Msiexec -MsiArgs $a | Out-Null
    Write-Info "Désinstallation terminée."
}

function Invoke-GetId {
    $exe = Get-ClientExe
    if ($exe) {
        # Le client fait autorité une fois le drapeau câblé.
        & $exe --get-id
        return
    }
    $fichier = Join-Path $Script:DataDir 'id.txt'
    if (Test-Path -LiteralPath $fichier) {
        Get-Content -LiteralPath $fichier -Raw
    } else {
        Write-Warn "ID indisponible : client non installé et aucun état sous $fichier."
        Write-Warn "À câbler : « novadesk.exe --get-id » (lot client)."
    }
}

function Invoke-SetPassword {
    param([string] $Password)
    if (-not $Password) { throw "--set-password exige un mot de passe." }
    $exe = Get-ClientExe
    if ($exe) {
        # Mot de passe passé au client SANS apparaître dans les journaux.
        & $exe --set-password $Password
        Write-Info "Mot de passe d'accès non surveillé défini via le client."
        return
    }
    Write-Warn "Client non installé : impossible de définir le mot de passe."
    Write-Warn "À câbler : « novadesk.exe --set-password <hash> » (lot client)."
}

function Invoke-RegisterLicense {
    param([string] $Key)
    if (-not $Key) { throw "--register-license exige une clé." }
    $exe = Get-ClientExe
    if ($exe) {
        & $exe --register-license $Key
        Write-Info "Licence enregistrée via le client."
        return
    }
    if (-not (Test-Path -LiteralPath $Script:DataDir)) {
        New-Item -ItemType Directory -Path $Script:DataDir -Force | Out-Null
    }
    Set-Content -LiteralPath (Join-Path $Script:DataDir 'license.key') -Value $Key -Encoding Ascii
    Write-Info "Licence enregistrée sous $Script:DataDir (repli ; à câbler côté client)."
}

# --- Analyse des arguments façon AnyDesk (« --drapeau [valeur] ») -------------
function Invoke-Main {
    param([string[]] $Argv)
    if (-not $Argv -or $Argv.Count -eq 0 -or $Argv -contains '--help') {
        Show-Usage
        return 0
    }

    $silent = $Argv -contains '--silent'
    $msiPath = $null
    for ($i = 0; $i -lt $Argv.Count; $i++) {
        if ($Argv[$i] -eq '--msi' -and ($i + 1) -lt $Argv.Count) { $msiPath = $Argv[$i + 1] }
    }

    # Valeur positionnelle qui suit un drapeau porteur.
    function Next-Value {
        param([string] $Flag)
        $idx = [Array]::IndexOf($Argv, $Flag)
        if ($idx -ge 0 -and ($idx + 1) -lt $Argv.Count) { return $Argv[$idx + 1] }
        return $null
    }

    switch ($true) {
        ($Argv -contains '--install')          { Invoke-Install -MsiPath $msiPath -Silent $silent; break }
        ($Argv -contains '--remove')           { Invoke-Remove -Silent $silent; break }
        ($Argv -contains '--get-id')           { Invoke-GetId; break }
        ($Argv -contains '--set-password')     { Invoke-SetPassword -Password (Next-Value '--set-password'); break }
        ($Argv -contains '--register-license') { Invoke-RegisterLicense -Key (Next-Value '--register-license'); break }
        default { Write-Warn "Commande inconnue."; Show-Usage; return 2 }
    }
    return 0
}

exit (Invoke-Main -Argv $Args)
