#Requires -Version 5.1
<#
.SYNOPSIS
    Signature Authenticode des artefacts NovaDesk (EXE / DLL / MSI) —
    SHA-256 + horodatage RFC 3161, vérification intégrée.

.DESCRIPTION
    Enveloppe robuste de « signtool.exe » (Windows SDK) :

      - condensat SHA-256 (/fd SHA256) ;
      - horodatage RFC 3161 (/tr <url> /td SHA256) : la signature reste
        valide APRÈS l'expiration du certificat, car le tampon prouve que la
        signature a été apposée pendant sa période de validité ;
      - double signature SHA-1 + SHA-256 optionnelle (-DualSign, via /as)
        pour la compatibilité héritée (Windows Vista / 7 sans correctif
        SHA-256). PE uniquement : un MSI ne porte qu'UNE seule signature ;
      - vérification post-signature : « signtool verify /pa /all » puis
        Get-AuthenticodeSignature (statut + certificat d'horodatage exigé) ;
      - idempotent : un fichier déjà signé par le même certificat est ignoré
        (re-signer avec -Force) ;
      - code retour non nul au moindre échec (utilisable tel quel en CI) ;
      - aucun secret journalisé (mot de passe masqué, PFX temporaire détruit).

    Matériel de signature, par ordre de priorité :
      1. -Thumbprint <empreinte SHA-1> : certificat présent dans le magasin
         Windows (Cert:\CurrentUser\My ou Cert:\LocalMachine\My). C'est ainsi
         qu'apparaissent AUSSI les certificats EV sur jeton USB / HSM : leur
         mini-pilote (ex. SafeNet) publie le certificat dans le magasin, la
         clé privée restant dans le matériel. Voie recommandée en production.
      2. -PfxPath <fichier.pfx> [-Password <mdp>] : fichier PFX sur disque
         (certificats de test ou d'autorité interne uniquement — depuis
         juin 2023 les autorités publiques ne livrent plus de PFX logiciel,
         voir CERTIFICAT.md).
      3. Variables d'environnement (secrets CI, jamais en dur) :
         NOVADESK_SIGN_THUMBPRINT, NOVADESK_SIGN_PFX_BASE64 (PFX encodé
         base64, écrit dans un fichier temporaire supprimé après usage),
         NOVADESK_SIGN_PFX_PATH, NOVADESK_SIGN_PFX_PASSWORD,
         NOVADESK_TIMESTAMP_URL.

.PARAMETER Files
    Fichiers à signer, ET/OU dossiers (parcourus récursivement pour trouver
    *.exe, *.dll, *.msi). Les jokers sont acceptés (ex. dist\*.msi).
    Alias : -Path (compatibilité avec l'ancienne interface).

.PARAMETER Thumbprint
    Empreinte SHA-1 (40 caractères hex.) d'un certificat de signature de code
    du magasin Windows. Espaces tolérés (copie depuis certmgr). Le magasin
    machine (LocalMachine\My) est détecté automatiquement (/sm).

.PARAMETER PfxPath
    Chemin d'un fichier PFX (certificat + clé privée).

.PARAMETER Password
    Mot de passe du PFX. Préférer $env:NOVADESK_SIGN_PFX_PASSWORD pour ne pas
    laisser le secret dans l'historique du shell. Jamais affiché.

.PARAMETER TimestampUrl
    Serveur d'horodatage RFC 3161. Défaut : http://timestamp.digicert.com
    (gratuit, sans compte). Autres : http://timestamp.sectigo.com,
    http://timestamp.globalsign.com/tsa/r6advanced1, http://ts.ssl.com.

.PARAMETER DualSign
    Appose DEUX signatures sur les fichiers PE : SHA-1 (primaire, horodatage
    Authenticode hérité /t) puis SHA-256 ajoutée (/as, horodatage RFC 3161).
    Nécessaire uniquement pour des clients très anciens (Vista / 7 non mis à
    jour). Ignoré, avec avertissement, pour les MSI (une seule signature).

.PARAMETER Force
    Re-signe même si le fichier porte déjà une signature du même certificat.

.PARAMETER AllowUntrusted
    Tolère l'échec de confiance de CHAÎNE à la vérification — cas du
    certificat auto-signé de test, dont la racine n'est approuvée nulle part.
    La présence de la signature ET de l'horodatage reste exigée (statuts
    HashMismatch / NotSigned toujours fatals). Ne PAS utiliser en production.

.PARAMETER Description
    Description embarquée dans la signature (signtool /d) — affichée par
    l'invite UAC.

.PARAMETER DescriptionUrl
    URL embarquée dans la signature (signtool /du).

.PARAMETER DryRun
    N'imprime que les commandes signtool (aucune signature, aucun certificat
    requis). Utile pour valider la logique sans matériel.

.EXAMPLE
    # Production — certificat (EV sur jeton, ou OV) visible dans le magasin :
    ./sign.ps1 -Files dist\NovaDesk-0.1.0-x86_64.msi -Thumbprint 'AB12…40 hex…'

.EXAMPLE
    # Tout un dossier de sortie + le MSI, en une passe :
    ./sign.ps1 -Files ui\build\windows\x64\runner\Release, dist\*.msi `
               -Thumbprint $env:NOVADESK_SIGN_THUMBPRINT

.EXAMPLE
    # PFX de test / CA interne :
    ./sign.ps1 -Files target\release\nd_ffi.dll -PfxPath .\test.pfx -Password $pwd

.EXAMPLE
    # Preuve locale avec le certificat auto-signé (chaîne non approuvée = attendu) :
    ./sign.ps1 -Files target\release\novadesk-svc.exe `
               -Thumbprint FC32C2EB4EDB5F19BCD1D97D87B03EC2890830F0 -AllowUntrusted

.NOTES
    Voir packaging\windows\CERTIFICAT.md pour l'obtention et l'usage d'un
    vrai certificat d'autorité (OV / EV, jeton / HSM, CI).
#>
[CmdletBinding(DefaultParameterSetName = 'Store')]
param(
    [Parameter(Mandatory = $true, Position = 0)]
    [Alias('Path')]
    [string[]] $Files,

    [Parameter(ParameterSetName = 'Store')]
    [string] $Thumbprint,

    [Parameter(ParameterSetName = 'Pfx', Mandatory = $true)]
    [string] $PfxPath,

    [Parameter(ParameterSetName = 'Pfx')]
    [string] $Password,

    [string] $TimestampUrl,

    [switch] $DualSign,
    [switch] $Force,
    [switch] $AllowUntrusted,

    [string] $Description,
    [string] $DescriptionUrl,

    [switch] $DryRun
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

# ---------------------------------------------------------------- affichage
function Write-Info { param([string] $m) Write-Host "[sign] $m" }
function Write-Warn { param([string] $m) Write-Host "[sign] AVERTISSEMENT : $m" -ForegroundColor Yellow }

# ------------------------------------------------------- localiser signtool
# PATH d'abord, puis les dossiers classiques du Windows SDK (préférence x64).
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
            Where-Object { $_.FullName -match '\\x64\\' } |
            Sort-Object FullName -Descending | Select-Object -First 1
        if ($trouve) { return $trouve.FullName }
    }
    return $null
}

# ------------------------------------------- résoudre les fichiers à signer
# Accepte fichiers, jokers et dossiers (récursif : .exe, .dll, .msi).
function Resolve-Targets {
    param([string[]] $Specs)
    $extensions = @('.exe', '.dll', '.msi')
    $cibles = New-Object System.Collections.Generic.List[string]
    foreach ($spec in $Specs) {
        $items = @(Get-Item -Path $spec -ErrorAction SilentlyContinue)
        if ($items.Count -eq 0) { throw "Aucun fichier ne correspond à : $spec" }
        foreach ($item in $items) {
            if ($item.PSIsContainer) {
                $enfants = @(Get-ChildItem -LiteralPath $item.FullName -Recurse -File |
                    Where-Object { $extensions -contains $_.Extension.ToLowerInvariant() })
                if ($enfants.Count -eq 0) {
                    Write-Warn "Aucun .exe/.dll/.msi dans le dossier $($item.FullName)."
                }
                foreach ($e in $enfants) { $cibles.Add($e.FullName) }
            }
            else {
                $cibles.Add($item.FullName)
            }
        }
    }
    return @($cibles | Select-Object -Unique)
}

# ------------------------------------------ résoudre le matériel de signature
# Renvoie @{ CertArgs = [string[]] ; Thumbprint = <hex ou $null> ;
#            TempPfx = <chemin ou $null> ; Secret = <mdp ou $null> }
# Le Thumbprint sert au contrôle d'idempotence ; le Secret, au masquage.
function Resolve-SigningMaterial {

    # --- 1. Empreinte (paramètre puis variable d'environnement) -------------
    $emp = $null
    if ($Thumbprint) { $emp = $Thumbprint }
    elseif ($env:NOVADESK_SIGN_THUMBPRINT) { $emp = $env:NOVADESK_SIGN_THUMBPRINT }
    if ($emp) {
        $emp = ($emp -replace '[\s\-]', '').ToUpperInvariant()
        if ($emp -notmatch '^[0-9A-F]{40}$') {
            throw "Empreinte invalide : « $emp » (attendu : 40 caractères hexadécimaux)."
        }
        $certArgs = @('/sha1', $emp)
        # Localiser le certificat pour un message clair (et /sm si magasin machine).
        $dansCU = Test-Path -LiteralPath "Cert:\CurrentUser\My\$emp"
        $dansLM = Test-Path -LiteralPath "Cert:\LocalMachine\My\$emp"
        if ($dansCU) {
            Write-Info "Certificat $emp trouvé dans Cert:\CurrentUser\My."
        }
        elseif ($dansLM) {
            Write-Info "Certificat $emp trouvé dans Cert:\LocalMachine\My (ajout de /sm)."
            $certArgs += '/sm'
        }
        elseif (-not $DryRun) {
            throw "Certificat $emp introuvable dans Cert:\CurrentUser\My ni Cert:\LocalMachine\My. Jeton EV branché ? Pilote installé ?"
        }
        return @{ CertArgs = $certArgs; Thumbprint = $emp; TempPfx = $null; Secret = $null }
    }

    # --- 2. PFX (paramètre, puis secrets CI base64 / chemin) ----------------
    $pfx = $null; $tempPfx = $null
    if ($PfxPath) { $pfx = $PfxPath }
    elseif ($env:NOVADESK_SIGN_PFX_BASE64) {
        Write-Info "PFX reconstitué depuis un secret base64 (fichier temporaire, détruit après usage)."
        $tempPfx = Join-Path $env:TEMP ("novadesk-" + [guid]::NewGuid().ToString('N') + '.pfx')
        [IO.File]::WriteAllBytes($tempPfx, [Convert]::FromBase64String($env:NOVADESK_SIGN_PFX_BASE64))
        $pfx = $tempPfx
    }
    elseif ($env:NOVADESK_SIGN_PFX_PATH) { $pfx = $env:NOVADESK_SIGN_PFX_PATH }

    if ($pfx) {
        if (-not (Test-Path -LiteralPath $pfx)) { throw "Fichier PFX introuvable : $pfx" }
        $mdp = $null
        if ($Password) { $mdp = $Password }
        elseif ($env:NOVADESK_SIGN_PFX_PASSWORD) { $mdp = $env:NOVADESK_SIGN_PFX_PASSWORD }
        $certArgs = @('/f', $pfx)
        if ($mdp) { $certArgs += @('/p', $mdp) }
        # Empreinte du PFX pour l'idempotence (échec non fatal : signtool tranchera).
        $empPfx = $null
        try {
            $x509 = New-Object System.Security.Cryptography.X509Certificates.X509Certificate2($pfx, $mdp)
            $empPfx = $x509.Thumbprint
        }
        catch {
            Write-Warn "Lecture du PFX impossible pour le contrôle d'idempotence : $($_.Exception.Message)"
        }
        return @{ CertArgs = $certArgs; Thumbprint = $empPfx; TempPfx = $tempPfx; Secret = $mdp }
    }

    # --- 3. Rien : acceptable seulement en -DryRun ---------------------------
    if ($DryRun) {
        Write-Info "Aucun matériel de signature — -DryRun : placeholder <THUMBPRINT>."
        return @{ CertArgs = @('/sha1', '<THUMBPRINT>'); Thumbprint = $null; TempPfx = $null; Secret = $null }
    }
    throw "Aucun matériel de signature : fournir -Thumbprint, -PfxPath, ou les variables NOVADESK_SIGN_*."
}

# --------------------------------------------------- idempotence (déjà signé ?)
function Test-AlreadySigned {
    param([string] $File, [string] $CertThumbprint)
    if (-not $CertThumbprint) { return $false }
    $sig = Get-AuthenticodeSignature -LiteralPath $File
    if ($sig.Status -eq 'NotSigned') { return $false }
    if ($sig.SignerCertificate -and $sig.SignerCertificate.Thumbprint -eq $CertThumbprint) { return $true }
    return $false
}

# ----------------------------------- signer (avec reprise sur l'horodatage)
# Les serveurs d'horodatage étant parfois indisponibles, 3 essais espacés de 5 s.
function Invoke-SignToolSign {
    param([string[]] $SignArgs, [string] $Target)
    $essaisMax = 3
    for ($essai = 1; $essai -le $essaisMax; $essai++) {
        & $script:SignTool @SignArgs
        if ($LASTEXITCODE -eq 0) { return }
        if ($essai -lt $essaisMax) {
            Write-Warn "signtool a échoué (code $LASTEXITCODE) — nouvel essai $($essai + 1)/$essaisMax dans 5 s (serveur d'horodatage indisponible ?)."
            Start-Sleep -Seconds 5
        }
    }
    throw "signtool sign a échoué (code $LASTEXITCODE) sur $Target après $essaisMax essais."
}

# ------------------------------------------------- vérification post-signature
function Test-SignedFile {
    param([string] $File)

    & $script:SignTool verify /pa /all $File
    $verifieOk = ($LASTEXITCODE -eq 0)

    $sig = Get-AuthenticodeSignature -LiteralPath $File
    $signataire = '(aucun)'
    if ($sig.SignerCertificate) { $signataire = $sig.SignerCertificate.Subject }
    $horodateur = '(aucun)'
    if ($sig.TimeStamperCertificate) { $horodateur = $sig.TimeStamperCertificate.Subject }
    Write-Info ("  Statut Authenticode : {0}" -f $sig.Status)
    Write-Info ("  Signataire          : {0}" -f $signataire)
    Write-Info ("  Horodateur          : {0}" -f $horodateur)

    if (-not $sig.SignerCertificate) { throw "Aucune signature détectée sur $File." }
    if (-not $sig.TimeStamperCertificate) { throw "Signature présente mais SANS horodatage sur $File — la signature expirerait avec le certificat." }

    if ($verifieOk) {
        Write-Info "  Vérification signtool : OK (chaîne de confiance valide)."
        return
    }
    if ($AllowUntrusted -and ($sig.Status -in @('UnknownError', 'NotTrusted'))) {
        Write-Warn "  Chaîne de confiance non approuvée (certificat auto-signé/test) — accepté via -AllowUntrusted. Signature + horodatage présents."
        return
    }
    throw "Vérification échouée sur $File (signtool verify code $LASTEXITCODE, statut $($sig.Status))."
}

# ============================================================== programme ==
$script:SignTool = Find-SignTool
if (-not $script:SignTool) {
    if ($DryRun) { $script:SignTool = 'signtool.exe' }
    else {
        Write-Host "[sign] ERREUR : signtool.exe introuvable (installer le Windows SDK)." -ForegroundColor Red
        exit 1
    }
}
Write-Info "signtool : $script:SignTool"

# URL d'horodatage : paramètre > variable d'environnement > défaut DigiCert.
if (-not $TimestampUrl) {
    if ($env:NOVADESK_TIMESTAMP_URL) { $TimestampUrl = $env:NOVADESK_TIMESTAMP_URL }
    else { $TimestampUrl = 'http://timestamp.digicert.com' }
}
Write-Info "Horodatage RFC 3161 : $TimestampUrl"

$materiel = $null
$echecs = @()
$signes = 0
$ignores = 0

try {
    $materiel = Resolve-SigningMaterial
    # @() : une cible unique serait sinon « déroulée » en chaîne scalaire.
    $cibles = @(Resolve-Targets -Specs $Files)
    Write-Info ("{0} fichier(s) à traiter." -f $cibles.Count)

    # Arguments optionnels de description (/d, /du).
    $descArgs = @()
    if ($Description) { $descArgs += @('/d', $Description) }
    if ($DescriptionUrl) { $descArgs += @('/du', $DescriptionUrl) }

    foreach ($cible in $cibles) {
        Write-Info "--- $cible"

        # Idempotence : déjà signé par CE certificat → ignoré sauf -Force.
        if (-not $DryRun -and -not $Force -and (Test-AlreadySigned -File $cible -CertThumbprint $materiel.Thumbprint)) {
            Write-Info "  Déjà signé par ce certificat — ignoré (re-signer avec -Force)."
            $ignores++
            continue
        }

        $estMsi = ([IO.Path]::GetExtension($cible).ToLowerInvariant() -eq '.msi')
        $passes = @()   # liste de tableaux d'arguments signtool

        if ($DualSign -and -not $estMsi) {
            # Passe 1 : SHA-1 primaire, horodatage Authenticode hérité (/t) —
            # les systèmes assez vieux pour exiger SHA-1 ignorent RFC 3161.
            $passes += , (@('sign') + $materiel.CertArgs + @('/fd', 'SHA1', '/t', $TimestampUrl) + $descArgs + @($cible))
            # Passe 2 : SHA-256 AJOUTÉE (/as), horodatage RFC 3161.
            $passes += , (@('sign') + $materiel.CertArgs + @('/fd', 'SHA256', '/tr', $TimestampUrl, '/td', 'SHA256', '/as') + $descArgs + @($cible))
        }
        else {
            if ($DualSign -and $estMsi) {
                Write-Warn "  Un MSI ne porte qu'une seule signature — SHA-256 uniquement (option -DualSign ignorée ici)."
            }
            $passes += , (@('sign') + $materiel.CertArgs + @('/fd', 'SHA256', '/tr', $TimestampUrl, '/td', 'SHA256') + $descArgs + @($cible))
        }

        if ($DryRun) {
            foreach ($p in $passes) {
                $affiche = $p | ForEach-Object { if ($materiel.Secret -and $_ -eq $materiel.Secret) { '***' } else { $_ } }
                Write-Info "  DRYRUN: `"$($script:SignTool)`" $($affiche -join ' ')"
            }
            continue
        }

        try {
            foreach ($p in $passes) { Invoke-SignToolSign -SignArgs $p -Target $cible }
            Test-SignedFile -File $cible
            $signes++
        }
        catch {
            Write-Host "[sign] ÉCHEC sur $cible : $($_.Exception.Message)" -ForegroundColor Red
            $echecs += $cible
        }
    }
}
catch {
    Write-Host "[sign] ERREUR : $($_.Exception.Message)" -ForegroundColor Red
    exit 1
}
finally {
    # Le PFX temporaire (secret base64) ne survit jamais au script.
    if ($materiel -and $materiel.TempPfx -and (Test-Path -LiteralPath $materiel.TempPfx)) {
        Remove-Item -LiteralPath $materiel.TempPfx -Force
    }
}

if ($echecs.Count -gt 0) {
    Write-Host ("[sign] TERMINÉ AVEC ÉCHECS : {0} signé(s), {1} ignoré(s), {2} en échec : {3}" -f `
            $signes, $ignores, $echecs.Count, ($echecs -join ' ; ')) -ForegroundColor Red
    exit 1
}
Write-Info ("Terminé : {0} signé(s), {1} ignoré(s) (déjà signés)." -f $signes, $ignores)
exit 0
