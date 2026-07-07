# Windows — installeur MSI (WiX v4) + CLI de déploiement + signature

## Contenu

| Fichier | Rôle |
|---|---|
| `wix/NovaDesk.wxs` | Source WiX v4 de l'installeur **MSI** (app + service + raccourcis + ressources). |
| `wix/License.rtf` | EULA **placeholder** (à remplacer avant distribution). |
| `novadesk-cli.ps1` | **CLI de déploiement** parité AnyDesk (install/remove/get-id/set-password/register-license). |
| `sign.ps1` | Signature **Authenticode** SHA-256 + horodatage (placeholders + secrets CI). |

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

```powershell
# Certificat EV sur HSM (recommandé) : empreinte dans le magasin.
$env:NOVADESK_SIGN_THUMBPRINT = '<empreinte SHA1 du certificat>'
./packaging/windows/sign.ps1 -Path dist\NovaDesk-0.1.0-x86_64.msi

# …ou PFX fourni par un secret CI (base64), supprimé après usage :
$env:NOVADESK_SIGN_PFX_BASE64  = $env:SECRET_PFX_B64
$env:NOVADESK_SIGN_PFX_PASSWORD = $env:SECRET_PFX_PWD
./packaging/windows/sign.ps1 -Path dist\NovaDesk-0.1.0-x86_64.msi
```

`sign.ps1 -DryRun` imprime la commande `signtool` sans rien signer (validation
sans certificat). Signer **tous** les binaires livrés (l'.exe et les DLL du
runner) avant de les empaqueter, puis le MSI final.

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
- **À valider sur machine Windows + droits** : `wix build` réel, la pose
  `msiexec`, l'enregistrement du service, et la signature `signtool` avec un vrai
  certificat. Aucun de ces outils/certificats n'existe sur le poste de dev.
