# Windows — installeur MSI (WiX v4) + CLI de déploiement + signature

## Contenu

| Fichier | Rôle |
|---|---|
| `wix/NovaDesk.wxs` | Source WiX v4 de l'installeur **MSI** (app + service + raccourcis + ressources). |
| `wix/License.rtf` | EULA **placeholder** (à remplacer avant distribution). |
| `novadesk-cli.ps1` | **CLI de déploiement** parité AnyDesk (install/remove/get-id/set-password/register-license). |
| `sign.ps1` | Signature **Authenticode** SHA-256 + horodatage RFC 3161 + vérification (paramétrable : empreinte magasin/jeton, PFX, secrets CI). |
| `CERTIFICAT.md` | **Obtention et usage d'un vrai certificat** (OV vs EV, achat, jeton/HSM, SmartScreen, CI). |

## Prérequis (machine de build Windows, droits admin)

- **WiX Toolset v4** : `dotnet tool install -g wix` puis `wix extension add WixToolset.UI.wixext`.
- **Windows SDK** (fournit `signtool.exe`).
- La **sortie du build Flutter release** de `ui/` :
  `ui\build\windows\x64\runner\Release\` (contient `novadesk.exe`,
  `flutter_windows.dll`, `nd_ffi.dll`, `data\`). ⚠ Non produite ici : le runner
  Flutter desktop n'est pas généré sur ce poste (pas de droits/SDK — voir la note
  mémoire « no-admin / no-symlink »).

## Construire le MSI

```powershell
wix build packaging\windows\wix\NovaDesk.wxs -arch x64 `
    -d StageDir=ui\build\windows\x64\runner\Release `
    -ext WixToolset.UI.wixext `
    -o dist\NovaDesk-0.1.0-x86_64.msi
```

Le `-d StageDir=…` pointe le dossier de sortie Flutter : l'élément `<Files>`
**moissonne** automatiquement DLL, plugins et `data\` (l'exécutable est déclaré
à part pour porter les raccourcis).

### Service Windows d'accès non surveillé

Le MSI enregistre le service `novadesk-svc` en compte **LocalSystem**, démarrage
**manuel** (dormant) : présent mais inactif tant que l'accès non surveillé n'est
pas configuré — comme AnyDesk. Le nom se surcharge : `wix build … -d …` puis à la
pose `msiexec /i … SERVICENAME=mon-service`. L'exécutable client doit honorer son
argument de service (`--service`) : **ce câblage relève du lot client** (le
runner Flutter n'expose pas encore ce drapeau).

## Signer (Authenticode)

`sign.ps1` signe EXE/DLL/MSI en **SHA-256** avec **horodatage RFC 3161**
(la signature survit à l'expiration du certificat), vérifie chaque fichier
(`signtool verify /pa /all` + `Get-AuthenticodeSignature`), est **idempotent**
(fichier déjà signé par le même certificat → ignoré, re-signer avec `-Force`)
et renvoie un code non nul au moindre échec.

```powershell
# Vrai certificat (magasin Windows / jeton EV / HSM cloud) — voie recommandée :
./packaging/windows/sign.ps1 -Files dist\NovaDesk-0.1.0-x86_64.msi `
    -Thumbprint '<empreinte SHA1 du certificat>'

# Plusieurs cibles : fichiers, jokers, dossiers (récursif exe/dll/msi) :
./packaging/windows/sign.ps1 -Files ui\build\windows\x64\runner\Release, dist\*.msi `
    -Thumbprint '<empreinte>'

# PFX (test / CA interne) :
./packaging/windows/sign.ps1 -Files dist\*.msi -PfxPath .\cert.pfx -Password $pwd

# Secrets CI (aucun paramètre) : NOVADESK_SIGN_THUMBPRINT, ou
# NOVADESK_SIGN_PFX_BASE64 (+ NOVADESK_SIGN_PFX_PASSWORD), NOVADESK_TIMESTAMP_URL.
$env:NOVADESK_SIGN_THUMBPRINT = '<empreinte>'
./packaging/windows/sign.ps1 -Files dist\NovaDesk-0.1.0-x86_64.msi
```

Options : `-TimestampUrl` (défaut DigiCert), `-DualSign` (SHA-1 + SHA-256 `/as`,
compat héritée, PE uniquement), `-Description`/`-DescriptionUrl` (`/d`, `/du`),
`-AllowUntrusted` (tolère l'échec de chaîne du **certificat auto-signé de
test** uniquement), `-DryRun` (imprime les commandes sans signer).

Signer **tous** les binaires livrés (l'.exe et les DLL du runner) avant de les
empaqueter, puis le MSI final. Pour l'obtention d'un **vrai certificat**
(OV/EV, jeton/HSM, SmartScreen, CI) : voir **`CERTIFICAT.md`**.

## CLI de déploiement (parité AnyDesk)

```powershell
# Pose silencieuse d'entreprise (équivaut à « --install --silent ») :
./novadesk-cli.ps1 --install --silent --msi dist\NovaDesk-0.1.0-x86_64.msi
./novadesk-cli.ps1 --get-id                 # ID du poste
./novadesk-cli.ps1 --set-password 'S3cret'  # accès non surveillé
./novadesk-cli.ps1 --register-license 'CLE-XXXX'
./novadesk-cli.ps1 --remove --silent        # désinstallation
```

- `--install` / `--remove` pilotent **msiexec** (pleinement fonctionnels ;
  `--remove` retrouve le ProductCode via les clés de désinstallation).
- `--get-id` / `--set-password` / `--register-license` **délèguent au client**
  `novadesk.exe` une fois installé. Tant que le client n'expose pas ces drapeaux,
  un repli documenté écrit/lit l'état sous `%ProgramData%\NovaDesk\` et le
  signale. **À câbler côté client** (lot UI/FFI).

## Vérifiable ici vs sur machine de build

- **Vérifié ici** : bonne formation XML du `.wxs` (namespaces WiX v4), analyse et
  exécution à blanc des scripts PowerShell (`sign.ps1 -DryRun`, `novadesk-cli.ps1
  --help/--get-id`). Les `.ps1` sont encodés **UTF-8 avec BOM** (sinon Windows
  PowerShell 5.1 corrompt les accents).
- **Vérifié ici (2026-07-10) — pipeline de signature complet** : `sign.ps1` a
  réellement signé et **horodaté (RFC 3161, DigiCert)** les 5 artefacts
  (`novadesk_ui.exe` Release/Debug, `nd_ffi.dll`, `novadesk-svc.exe`, MSI) avec
  le certificat **auto-signé de test** (`FC32C2EB4EDB5F19BCD1D97D87B03EC2890830F0`,
  option `-AllowUntrusted`). `Get-AuthenticodeSignature` confirme le
  `TimeStamperCertificate` DigiCert ; le statut `UnknownError` (chaîne non
  approuvée) est **attendu** en auto-signé — voir `CERTIFICAT.md`.
- **À valider sur machine de build / restant** : `wix build` réel, la pose
  `msiexec`, l'enregistrement du service, et la signature avec un **vrai
  certificat d'autorité** (achat OV/EV — parcours documenté dans
  `CERTIFICAT.md` ; le pipeline s'utilise ensuite tel quel).
