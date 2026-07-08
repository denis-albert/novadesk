# Prompt ultime — Refonte front-end « niveau AnyDesk » (Flutter)

**Cible : `ui/` (Flutter)** · **Exécution : agent Opus, en PHASES séquentielles** · Objectif : une UI **de qualité produit professionnelle**, dense, cohérente, riche en fonctionnalités — et **débarrassée de l'effet « IA banale »**.

---

Projet **NovaDesk** (bureau à distance en Rust, clone d'AnyDesk). Code/commentaires/textes en **FRANÇAIS**. Tu refonds **entièrement la présentation** du client Flutter en suivant **à la lettre** la direction artistique et l'inventaire ci-dessous. Tu **NE touches PAS** à la logique de câblage moteur (contrat `NativeApi`, rendu vidéo live, connexion-par-ID, hôte non-surveillé) : tu ré-habilles et enrichis, tu ne débranches rien.

## LOGISTIQUE
- Édite **UNIQUEMENT** sous `C:\Users\udohkak\Desktop\Anydesk\novadesk\ui\`.
- Build/analyse via `C:\Users\udohkak\flutter\bin\flutter.bat` (`analyze`, `test`).
- **AUCUN plugin natif** (pas d'admin/symlinks : `window_manager`, `irondash_texture`, `bitsdojo_window` INTERDITS ; garde le shim pur-Dart `lib/platform/window_shim.dart`). **AUCUNE police/asset web externe** (police **système Segoe UI** ; icônes = `nova_icons` maison uniquement). **AUCUN git.**
- L'app reste **navigable sous mock** (`MockNativeApi` par défaut). Ne casse pas `lib/bridge/*`, la nav `Navigator`+routes nommées, ni le rendu vidéo de `session_screen.dart`.

## BARRE QUALITÉ
- `C:\Users\udohkak\flutter\bin\flutter.bat analyze` = **0 erreur** à la fin de CHAQUE phase.
- `flutter test` vert (adapte/enrichis les tests widget).
- Zéro `Icons.*` Material résiduel (tout via `NovaIcones`). Zéro couleur en dur dans les écrans : **tout passe par le thème/tokens**.

---

## DIRECTION ARTISTIQUE — DÉCIDÉE (ne pas ré-inventer)

### Concept
« **Poste de contrôle** » : une console professionnelle d'accès distant. **Sombre par défaut** (comme les vrais outils techniques), **dense**, calme, précise. Le rouge est un **signal** (action/live/danger), jamais une décoration.

### Système à DEUX accents (le parti-pris central, anti-banal)
- **Rouge AnyDesk `#EF4B44`** = *action & live* uniquement : bouton primaire « Se connecter », onglet de session actif, pastille REC, état « en session », destructif/fermer.
- **Bleu acier `#3B82F6`** = *sélection, focus, liens, éléments interactifs neutres* (anneau de focus, ligne sélectionnée du carnet, interrupteurs « info »). Ainsi le rouge **reste rare** → l'UI respire le pro.
- Le vert n'est QUE le statut « en ligne ».

### Palette — SOMBRE (défaut)
```
fond            #0F1115   surface1 (panneaux)  #161922   surface2 (cartes/champs) #1C2029
surface3 (survol/relief) #232834   bord fin #262B36   bord fort #313846
texte1 #E8EBF0   texte2 #A2ABBA   texte3 #6B7688
rouge #EF4B44 (pressé #D8423B)   bleu #3B82F6 (pressé #2E6FE0)
vert #35C46B   ambre #E5A13A   danger = rouge
overlay/backdrop rgba(6,8,12,.62)
```
### Palette — CLAIR (miroir soigné, pas une simple inversion)
```
fond #F2F4F7   surface1 #FFFFFF   surface2 #FFFFFF   surface3 #EEF1F5
bord fin #E3E7EC   bord fort #D2D8E0
texte1 #14171C   texte2 #55607A   texte3 #8A94A6
rouge #EF4B44   bleu #2E6FE0   vert #1FA85A   ambre #C9821B
```
### Typographie (Segoe UI système)
Échelle : **11 / 12 / 13 / 15 / 18 / 24 / 32**. Graisses 400/500/600/700, interligne 1.35–1.5.
- Micro-labels de section : **11px, MAJUSCULES, letter-spacing +0.6, texte3**.
- Corps 13px texte1/texte2. Titres d'écran 18–24 semi-bold.
- **Chiffres tabulaires** (`fontFeatures: [FontFeature.tabularFigures()]`) partout où il y a des IDs/stats/latences.

### Grille, profondeur, formes
- Base **4/8 px**. Contrôles **hauteur 32–36px**. Rayons **6px** (champs/boutons), **10px** (panneaux/dialogues), **full** (pastilles).
- **Profondeur par étages de surface + filets 1px**, PAS par ombres. Ombre douce **uniquement** sur les couches flottantes (barre d'outils de session, menus, dialogues, toasts) : `0 12px 32px rgba(0,0,0,.45)` (sombre).
- Densité « outil » : listes/tableaux serrés, pas de grands vides.

### Mouvement & micro-interactions (précis)
- Survols/toggles : **130ms ease-out**. Transitions de page : **200ms** (glissement 8px + fondu). Apparition barre d'outils session : **180ms** (glisse depuis le haut + fondu). Menus/dialogues : **150ms** échelle 0.98→1 + fondu.
- **Feedback de copie** : le bouton passe « Copié ✓ » 1.1s + léger flash bleu.
- **Pastille REC** : pulsation 1.2s. **Squelettes shimmer** pour tout chargement (carnet, vignettes, stats).
- Respecte `MediaQuery.disableAnimations` (reduced-motion).

### Iconographie
`NovaIcones` uniquement, **20px, trait 1.75, bouts ronds**, poids optique constant. Complète le jeu si un pictogramme manque (même style). Jamais d'emoji.

## À BANNIR (l'effet « IA banale »)
- Cartes arrondies à grosse ombre **partout** ; tout **centré** ; héros/dégradés décoratifs ; violets ; glassmorphism gratuit ; emoji-icônes ; police « Inter par défaut » ; espacements mous/aérés « landing SaaS ».
- États vides **absents** ou niais (« Rien ici 🙁 »). **Chaque** liste/zone a un état vide, chargement (skeleton) et erreur **soignés et utiles** (titre, sous-texte, action).
- Données bidon évidentes : utilise des libellés **produit** crédibles.

---

## INVENTAIRE EXHAUSTIF — écran par écran (layout + états + interactions)

### 0. Chrome global
- **Barre de titre à onglets** : logo (pastille rouge à arc blanc) + **onglets** (Accueil + un onglet par session active affichant **alias + latence live + point d'état**, croix de fermeture au survol) + `＋` (nouvelle connexion) + contrôles fenêtre Windows discrets (─ ▢ ✕, ✕→rouge au survol). Onglet actif : soulignement rouge 2px.
- **Barre d'état** basse (28px) : état réseau (point vert/ambre/rouge + texte), chiffrement E2E, version, à droite : compteur de sessions actives.
- **Toasts** en bas-droite (succès/erreur/info), auto-dismiss, empilables.
- **Rail latéral** fin (56px) optionnel d'icônes (Accueil / Carnet / Historique / Réglages) — actif = fond surface3 + barre bleue à gauche.

### 1. Accueil
- **Deux colonnes** (filet 1px), repli mono-colonne < 860px.
- **« Poste distant »** (gauche, principal) : grand champ ID (chiffres tabulaires, formatage auto par 3), bouton rouge **« Se connecter → »**, sélecteur de **mode** (Contrôle / Observation / Transfert), et **quick-connect** : au focus, un panneau déroulant façon *command palette* filtre le carnet en direct.
- **« Ce poste »** (droite) : adresse 28px, alias, **mot de passe éphémère** (masqué, révéler/copier/régénérer), lien accès non surveillé, empreinte courte.
- **Carnet d'adresses** riche (sous les colonnes ou onglet dédié) : **tableau dense** (pas des cartes molles) — colonnes Alias / ID (tabulaire) / Groupe / Dernière connexion / État (point) / actions ; **recherche** instantanée, **filtre par groupe/tag**, **tri** cliquable, **favoris** (étoile), **menu contextuel** (Connecter, Observer, Transfert, Renommer, Déplacer vers groupe, Wake-on-LAN, Supprimer), sélection au clavier, **groupes/dossiers** repliables dans une colonne latérale. Vue alternative **vignettes** (aperçu + statut). **Découverte LAN** : section listant les postes détectés. **Sessions récentes** : bande de vignettes (aperçu desktop désaturé réaliste + alias + horodatage + reconnecter).
- États vide/chargement/erreur pour carnet, découverte, récentes.

### 2. Fenêtre de session
- Surface vidéo plein cadre `#000`, rendu live (lot 04, **inchangé**), letterbox propre.
- **Barre d'outils flottante** sombre, centrée en haut, **révélée au survol / masquée en plein écran**, défilable, groupée par séparateurs 1px :
  1. **Pair** : avatar + alias + latence live.
  2. **Sécurité** : cadenas → popover (transport, chiffrement, **empreinte + SAS**).
  3. **Affichage** : sélecteur **multi-écran** (miniatures), **qualité/vitesse** (Réactivité / Équilibré / Qualité), **résolution/échelle** (adapter/1:1/remplir), **plein écran**.
  4. **Entrées** : clavier (disposition, transmettre les raccourcis), **Ctrl+Alt+Suppr**, **presse-papiers** (sync).
  5. **Collaboration** : **transfert de fichiers** (ouvre le gestionnaire deux volets), **chat** (panneau latéral), **tableau blanc/annotation**.
  6. **Capture** : **enregistrement** (toggle + pastille REC), **capture d'écran**.
  7. **Système** : actions (verrouiller, redémarrer, élévation), **tunnel TCP**, **permissions** (menu à cases).
  8. **Terminer** (rouge).
- **HUD** discret (coin) : fps, rtt, débit, **niveau ABR**, **encodeur (NVENC/logiciel)**, entrées refusées, reconnexions — libellés honnêtes, masqués si non applicables.
- **Bandeau de reconnexion** (ambre) si lien perdu. **Overlay de connexion** (états Résolution→Connexion→Authentification) avec squelette, pas un spinner nu.
- **Gestionnaire de fichiers** deux volets (local | distant) : arborescence, glisser-déposer, file de transferts avec progression/vitesse/ETA, pause/reprise/annuler.
- **Chat** : panneau latéral, bulles, horodatage, indicateur d'écriture.

### 3. Réglages (onglets, denses, pro)
Interface (thème clair/sombre/système, langue, densité) · **Sécurité** (liste blanche **ACL** avec joker, **profils de permissions** dépliables, 2FA/TOTP, empreinte) · **Connexion** (rendez-vous / relais / **STUN**, proxy, ports) · **Affichage/Qualité** (préréglages, fps cible, débit max) · **Audio** · **Enregistrement** (dossier, format, auto) · **Confidentialité** (écran noir distant, bloquer entrée locale) · **Raccourcis** (table éditable) · **À propos** (version, licence, **mises à jour**, empreinte, mentions). Chaque réglage : libellé + aide + contrôle aligné (interrupteurs verts/bleus, pas rouges).

### 4. Accès non surveillé
Activation service (état actif/inactif), **mot de passe permanent** + **jauge de force**, **appareils de confiance** (liste + ajout validé), **dialogue d'acceptation entrante** (identité, **empreinte**, profil de permissions précoché, Accepter vert / Refuser), **journal** des accès (table). Câblé au vrai flux (`startUnattendedHost`/`unattendedIncomingStream`/`approveIncoming`) déjà présent.

---

## EXÉCUTION — PHASES SÉQUENTIELLES (chacune finit avec `flutter analyze` = 0)
1. **Design system & thème** : `theme/nova_theme.dart` refait (tokens ci-dessus via `ThemeExtension`, clair+sombre, système à deux accents, typo, motion durations). Helpers d'animation communs (durées/easing).
2. **Composants réutilisables** (`widgets/`) : boutons (primaire rouge, secondaire, fantôme, danger), champ, interrupteur (vert/bleu), pastilles d'état, `NovaCard`/panneau plat, en-tête de section (micro-label), **tableau dense** générique (tri/sélection/menu contextuel), squelettes shimmer, états vides/erreur génériques, toasts, popover/menu, onglets, barre de titre (`app_frame`) enrichie.
3. **Accueil** : deux colonnes, quick-connect, **carnet en tableau dense** (recherche/tri/filtre/groupes/favoris/menu contextuel), découverte LAN, récentes, tous états.
4. **Session** : barre d'outils complète groupée + révélation au survol, HUD, overlay de connexion, bandeau reconnexion, **gestionnaire de fichiers deux volets**, **chat**, popovers sécurité/permissions/affichage.
5. **Réglages** onglets exhaustifs + **Accès non surveillé** enrichi.
6. **Passe finale** : micro-interactions, états vides/chargement/erreur partout, cohérence icônes/tokens, responsive (étroit→large), thème clair vérifié, `flutter analyze` 0 + tests widget.

## VÉRIF (à chaque phase + finale)
- `C:\Users\udohkak\flutter\bin\flutter.bat analyze` = 0 erreur (reporte le compte).
- `flutter test` vert. Navigabilité sous mock décrite (accueil → carnet → connexion → session ; réglages ; accès non surveillé → dialogue).
- Confirme : aucun plugin natif ajouté, câblage moteur intact, zéro `Icons.*`, tokens respectés (clair+sombre).

## RÉPONSE FINALE
Fichiers créés/modifiés par phase ; résumé visuel de chaque écran ; sortie EXACTE de `flutter analyze` + tests ; captures décrites (clair et sombre). **Pas de git.**
