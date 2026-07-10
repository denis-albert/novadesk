# Certificat de signature de code — obtention et usage (NovaDesk)

Ce document décrit le parcours complet pour passer du **certificat auto-signé
de test** (qui prouve le pipeline) à un **vrai certificat d'autorité**
distribuable. Le pipeline technique (`sign.ps1`) est déjà prêt et vérifié :
seul l'achat du certificat reste à faire.

---

## 1. Pourquoi signer les binaires

- **SmartScreen** : un exécutable/installeur non signé (ou signé par un
  certificat inconnu) déclenche l'écran bleu « Windows a protégé votre
  ordinateur » au téléchargement — rédhibitoire pour un outil de bureau à
  distance grand public.
- **Antivirus / EDR** : les heuristiques pénalisent lourdement les binaires
  non signés, surtout ceux qui capturent l'écran et injectent des entrées
  (exactement le profil de NovaDesk).
- **Intégrité** : la signature garantit que le binaire n'a pas été altéré
  entre la build et le poste client.
- **UAC** : l'invite d'élévation affiche « Éditeur vérifié : <société> » au
  lieu de « Éditeur inconnu » en jaune.

## 2. Pourquoi l'auto-signé ne suffit pas

Le certificat de test (`CN=NovaDesk (auto-signe test)`, empreinte
`FC32C2EB4EDB5F19BCD1D97D87B03EC2890830F0`) **prouve que le pipeline
fonctionne** : `sign.ps1` signe réellement les EXE/DLL/MSI en SHA-256 et
obtient un **horodatage RFC 3161 authentique** de DigiCert
(`Get-AuthenticodeSignature` renvoie bien un `TimeStamperCertificate`).

Mais il n'est **pas distribuable** :

- sa chaîne se termine sur une racine que **personne n'approuve** →
  `signtool verify` échoue (« terminated in a root certificate which is not
  trusted ») et `Get-AuthenticodeSignature` rend `UnknownError` sur tout
  poste, y compris ici — c'est **attendu** ;
- SmartScreen le traite comme non signé (aucune réputation possible) ;
- installer sa racine dans « Autorités racines de confiance » de chaque
  client est inacceptable en distribution publique (et dangereux).

Seul un certificat émis par une **autorité publiquement approuvée** (racine
présente dans le magasin Windows) supprime ces blocages.

## 3. Types de certificats : OV vs EV

| | **OV** (Organization Validation) | **EV** (Extended Validation) |
|---|---|---|
| Vérification | Existence légale de l'organisation (ou identité pour un individu) | Vérification étendue : existence légale + opérationnelle + physique |
| Réputation SmartScreen | **Se construit avec le volume** de téléchargements sains (semaines/mois d'avertissements au début) | **Immédiate ou quasi immédiate** — SmartScreen accorde d'emblée la réputation |
| Clé privée | Matériel obligatoire (voir §5) | Matériel obligatoire, exigences renforcées |
| Prix indicatif / an | ≈ 200–500 € | ≈ 300–700 € |
| Signature noyau (drivers) | Non | Requis pour le portail Microsoft Hardware Dev |
| **Recommandation NovaDesk** | Acceptable si budget serré | **Recommandé** : un outil de prise de contrôle à distance sans réputation SmartScreen sera massivement bloqué/supprimé au téléchargement |

> **Important (règle CA/Browser Forum, effective depuis le 1ᵉʳ juin 2023)** :
> **tous** les certificats de signature de code publics — OV compris — doivent
> désormais avoir leur clé privée générée et conservée dans un **matériel
> certifié FIPS 140-2 niveau 2+ / Common Criteria EAL4+** (jeton USB ou HSM).
> Les autorités **ne livrent plus de fichier `.pfx` logiciel**. Le mode
> `-PfxPath` de `sign.ps1` reste utile pour les certificats de **test** ou
> d'**autorité interne d'entreprise**, pas pour un certificat public récent.

## 4. Où l'acheter

| Autorité | Notes |
|---|---|
| **DigiCert** | Référence du marché ; HSM cloud « KeyLocker » ; serveur d'horodatage utilisé par défaut dans `sign.ps1`. |
| **Sectigo** (ex-Comodo) | Souvent le moins cher ; revendeurs (SSL.com, KSoftware…) encore moins chers. |
| **GlobalSign** | Bon support entreprise ; HSM cloud « Aegis ». |
| **Entrust** | Orienté grandes organisations. |
| **SSL.com** | Revendeur + AC ; option « eSigner » (signature cloud). |
| **Certum** | Le plus accessible pour un **développeur individuel** (« Open Source Code Signing » sur carte cryptographique). |
| **Azure Trusted Signing** (Microsoft) | Alternative moderne **par abonnement** (~10 $/mois) : certificats à rotation courte gérés par Microsoft, très bonne réputation SmartScreen, intégration `signtool /dlib` et GitHub Actions. Exige une entité vérifiable (3+ ans d'historique pour la validation publique). À sérieusement considérer pour NovaDesk. |

Documents demandés (OV/EV) : immatriculation de la société (Kbis/D-U-N-S),
numéro de téléphone vérifiable dans un annuaire public, rappel téléphonique
de validation. Compter **2–10 jours ouvrés** (OV) à **1–3 semaines** (EV).

## 5. Stockage de la clé privée

1. **Jeton USB** (livraison classique) : SafeNet eToken / Luna, envoyé par
   l'AC ou fourni par vous. Le pilote (ex. *SafeNet Authentication Client*)
   publie le certificat dans `Cert:\CurrentUser\My` ; la clé privée **ne
   quitte jamais le jeton**. La signature demande le PIN (une option
   « single logon » évite de resaisir le PIN à chaque fichier d'un lot).
2. **HSM cloud** (recommandé pour la CI) : DigiCert KeyLocker, GlobalSign
   Aegis, Azure Key Vault (+ `AzureSignTool`), Azure Trusted Signing. La clé
   vit dans le cloud ; un client CSP/KSP local la rend visible comme un
   certificat du magasin → `sign.ps1 -Thumbprint` fonctionne tel quel.
3. **Fichier PFX** : uniquement certificats de test/CA interne (voir §3).
   Ne **jamais** committer un PFX ; en CI, le passer en secret base64
   (`NOVADESK_SIGN_PFX_BASE64`), `sign.ps1` détruit le fichier temporaire.

## 6. Usage avec `sign.ps1`

Le jour où le certificat est installé (jeton branché + pilote, ou HSM cloud
configuré), récupérer l'empreinte puis signer — **rien d'autre à changer** :

```powershell
# Empreinte du certificat (le jeton/HSM le publie dans le magasin) :
Get-ChildItem Cert:\CurrentUser\My -CodeSigningCert | Format-List Subject, Thumbprint

# Signer tous les artefacts d'une release (SHA-256 + horodatage RFC 3161) :
./packaging/windows/sign.ps1 `
    -Files ui\build\windows\x64\runner\Release\novadesk_ui.exe,
           target\release\nd_ffi.dll,
           target\release\novadesk-svc.exe `
    -Thumbprint '<EMPREINTE_DU_VRAI_CERT>'

# Puis le MSI (construit à partir des binaires DÉJÀ signés) :
./packaging/windows/sign.ps1 -Files dist\NovaDesk-0.1.0-x86_64.msi `
    -Thumbprint '<EMPREINTE_DU_VRAI_CERT>'
```

Points clés :

- **Ordre** : signer les binaires **avant** de construire le MSI, puis
  signer le MSI lui-même.
- **Horodatage obligatoire** (`sign.ps1` le refuse absent) : sans lui, la
  signature devient invalide à l'expiration du certificat (1–3 ans) ; avec
  lui, elle reste valide indéfiniment. URL par défaut :
  `http://timestamp.digicert.com` (remplaçable par `-TimestampUrl` ou
  `$env:NOVADESK_TIMESTAMP_URL` ; ex. `http://timestamp.sectigo.com`,
  `http://timestamp.globalsign.com/tsa/r6advanced1`, `http://ts.ssl.com`).
- **Double signature** SHA-1 + SHA-256 (`-DualSign`) : seulement si vous
  devez supporter Vista/7 sans correctif SHA-256 — inutile pour un Windows 10+
  moderne, et impossible sur MSI (une seule signature).
- **Pas de `-AllowUntrusted`** avec un vrai certificat : ce commutateur ne
  sert qu'au certificat de test. Avec la vraie chaîne, `signtool verify
  /pa /all` doit réussir et `Get-AuthenticodeSignature` rendre `Valid` —
  `sign.ps1` échoue (code retour ≠ 0) sinon.

### Vérification manuelle

```powershell
& signtool verify /pa /all dist\NovaDesk-0.1.0-x86_64.msi     # doit réussir
Get-AuthenticodeSignature dist\NovaDesk-0.1.0-x86_64.msi |
    Format-List Status, SignerCertificate, TimeStamperCertificate  # Status = Valid
```

## 7. Réputation SmartScreen

- **EV** : réputation accordée d'emblée — plus d'écran bleu dès la première
  release.
- **OV** : la réputation se construit par volume de téléchargements sains
  **par certificat** ; les premières semaines afficheront l'avertissement.
  Ne pas changer de certificat inutilement (la réputation repart de zéro).
- Dans les deux cas : signer **tout** ce qui est livré (exe, dll, msi),
  toujours horodater, et soumettre les faux positifs à
  <https://www.microsoft.com/wdsi/filesubmission>.

## 8. Intégration CI

`sign.ps1` lit ses secrets dans l'environnement — aucun secret en dur :

| Variable | Rôle |
|---|---|
| `NOVADESK_SIGN_THUMBPRINT` | Empreinte du certificat (magasin / jeton / HSM cloud). |
| `NOVADESK_SIGN_PFX_BASE64` | PFX encodé base64 (test/CA interne) — fichier temporaire détruit après usage. |
| `NOVADESK_SIGN_PFX_PATH` / `NOVADESK_SIGN_PFX_PASSWORD` | PFX sur disque + mot de passe (jamais journalisé). |
| `NOVADESK_TIMESTAMP_URL` | Serveur d'horodatage (défaut DigiCert). |

Scénarios :

1. **Jeton USB EV** : impossible dans un runner cloud → **runner
   auto-hébergé** Windows avec le jeton branché en permanence (PIN mis en
   cache par le client SafeNet). `sign.ps1` inchangé.
2. **HSM cloud** (recommandé) : DigiCert KeyLocker (`smctl` + KSP),
   Azure Trusted Signing (action GitHub officielle ou
   `signtool /dlib Azure.CodeSigning.Dlib.dll`), ou Azure Key Vault +
   `AzureSignTool`. Selon l'outil, soit le certificat apparaît dans le
   magasin (→ `sign.ps1 -Thumbprint`), soit l'outil remplace l'appel
   `signtool` (adapter alors l'étape CI, la vérification de `sign.ps1`
   restant réutilisable).
3. **Exemple GitHub Actions** (empreinte via HSM/KSP installé sur le runner) :

```yaml
- name: Signer les artefacts
  shell: pwsh
  env:
    NOVADESK_SIGN_THUMBPRINT: ${{ secrets.NOVADESK_SIGN_THUMBPRINT }}
  run: |
    ./packaging/windows/sign.ps1 -Files dist\*.msi,
        ui\build\windows\x64\runner\Release\novadesk_ui.exe,
        target\release\nd_ffi.dll, target\release\novadesk-svc.exe
    if ($LASTEXITCODE -ne 0) { exit 1 }
```

## 9. Checklist « le certificat est arrivé »

1. Installer le pilote du jeton (ou le client HSM cloud) sur la machine de
   build ; vérifier `Get-ChildItem Cert:\CurrentUser\My -CodeSigningCert`.
2. Relever l'empreinte → secret CI `NOVADESK_SIGN_THUMBPRINT`.
3. Signer une build complète avec `sign.ps1` (sans `-AllowUntrusted`).
4. Vérifier : `signtool verify /pa /all` OK partout, `Status = Valid`,
   `TimeStamperCertificate` renseigné.
5. Tester le téléchargement du MSI depuis un poste vierge (SmartScreen).
6. Sauvegarder les procédures de révocation/renouvellement de l'AC ;
   noter la date d'expiration (l'horodatage protège les binaires déjà
   signés, pas les futurs).

## 10. État actuel — prouvé vs restant

**Prouvé ici (2026-07-10), avec le certificat auto-signé :**

- `sign.ps1` signe réellement les 5 artefacts (EXE Release/Debug, DLL,
  service, MSI) en SHA-256 ;
- l'**horodatage RFC 3161 DigiCert fonctionne** (TimeStamperCertificate =
  `CN=DigiCert SHA256 RSA4096 Timestamp Responder 2025 1`) — l'horodatage
  n'exige pas de certificat de confiance côté signataire ;
- idempotence, `-Force`, `-DryRun`, `-DualSign`, dossiers/jokers, codes
  retour d'échec : tous vérifiés ;
- l'échec de chaîne (`UnknownError` / signtool « root … not trusted ») est
  le comportement **attendu** de l'auto-signé, encadré par `-AllowUntrusted`.

**Restant (hors de portée ici) :** l'**achat** du certificat OV/EV (ou
l'abonnement Azure Trusted Signing) et la réception du jeton/accès HSM.
Une fois l'empreinte disponible, le pipeline s'utilise tel quel (§6).
