# Prompt Fable 11 — Packaging, installeurs, signature, auto-update

**Priorité : P2** · **Cible : `packaging/` (+ `.github/workflows/`)** · **Parallélisable avec : tout.** ⚠ **Le build/signature natif exige une machine avec droits admin** (impossible sur le poste de dev actuel : pas d'admin/symlinks) — ce prompt **écrit la chaîne**, son exécution complète se fait ailleurs.

---

Projet **NovaDesk** (bureau à distance en Rust + UI Flutter). Code/commentaires/scripts en **FRANÇAIS**. **Mission** : passer le dossier `packaging/` (aujourd'hui **un seul README**) à une **vraie chaîne de packaging** : installeurs par OS, signature de code, et squelette d'auto-update.

## LOGISTIQUE
- Édite **UNIQUEMENT** sous `C:\Users\udohkak\Desktop\Anydesk\novadesk\packaging\` et, si nécessaire, `.github/workflows/` (CI).
- **AUCUN git.** Pas de build long ici : tu **écris** les manifestes/scripts et les **valides syntaxiquement** ; l'exécution réelle (compilation signée) se fait sur runners CI/machine avec droits.
- **Ne modifie aucune crate Rust ni l'UI.**

## BARRE QUALITÉ
- Scripts **shell/PowerShell** et manifestes **valides** (lint/dry-run quand possible : `yamllint`/`--dry-run`/`-WhatIf`).
- Aucune valeur secrète en clair (utiliser des placeholders + secrets CI).

## ÉTAT ACTUEL
- `packaging/README.md` seul (« rien n'est encore automatisé »). CI `release.yml` produit des **binaires serveur bruts non signés** (3 OS) dans une release brouillon ; installeurs + signature + TUF **explicitement non faits**.
- Application cliente = UI Flutter (`ui/`) + DLL `nd-ffi`. Serveurs = 4 binaires (`nd-rendezvous`, `nd-relay`, `nd-accounts`, `nd-api`).

## TÂCHE
1. **Windows** : installeur **MSI** (WiX) **ou** NSIS pour l'app cliente (UI Flutter + `nd_ffi.dll` + ressources), avec raccourcis, service optionnel (accès non-surveillé), et **CLI de déploiement** parité AnyDesk (`--install --silent`, `--get-id`, `--set-password`, `--register-license`, `--remove` — voir `../17-anydesk-realite.md` §12). Signature **Authenticode** (placeholder cert + secret CI).
2. **macOS** : `.app` + **DMG**/`pkg`, **codesign** + **notarization** (placeholders), entitlements (capture d'écran/accessibilité).
3. **Linux** : **AppImage** + **.deb** (+ éventuellement .rpm/Flatpak), dépendances déclarées.
4. **Serveurs** : **Dockerfiles** + **docker-compose** lançant les 4 services ensemble (réseau interne, volumes de persistance) — comble l'absence d'orchestration.
5. **Auto-update** : squelette **TUF-like** (métadonnées signées, canal stable/beta, vérification de signature **Ed25519/cosign** à la mise à jour — aligné `../15-deploiement-mise-a-jour.md`). Pas besoin d'un serveur complet ; livrer le format + le vérificateur + un manifeste d'exemple.
6. **CI** : étendre `release.yml` pour **produire et signer** les installeurs (matriciel 3 OS), publier les artefacts + métadonnées de MAJ. Garder le build serveur existant.
7. **Doc** : mettre à jour `packaging/README.md` avec la procédure complète et **les prérequis (droits admin, certificats)**.

## VÉRIF (obligatoire)
- Valider la **syntaxe** de chaque manifeste/script (WiX `candle -nologo` dry-run si dispo, `docker compose config`, `yamllint` sur les workflows, `bash -n`/PowerShell `-WhatIf`).
- Décrire ce qui **s'exécuterait** sur un runner (chaîne complète) vs ce qui est **vérifiable ici** (syntaxe).
- **Régression** : le `release.yml` existant reste valide (les jobs serveur ne cassent pas).

## RÉPONSE FINALE ATTENDUE
- Arborescence `packaging/` créée (par OS) + workflows modifiés.
- Ce qui est **prêt à signer/produire** vs ce qui exige une machine à droits (honnêteté).
- Résultat des validations de syntaxe.
- **Pas de git.**
