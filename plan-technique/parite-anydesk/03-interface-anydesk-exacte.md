# 03 — Interface AnyDesk exacte : spécification visuelle + tableau d'écart

> **Objectif.** Fournir à un agent Fable tout le nécessaire pour reproduire l'apparence d'AnyDesk **sans avoir à deviner** : couleurs hex, dimensions, composants, comportements, et un **tableau d'écart écran par écran** avec l'UI Flutter actuelle.
>
> **Honnêteté & sourçage.** AnyDesk ne publie **aucune** charte graphique ni design-system. Les valeurs ci-dessous combinent : (a) faits sourcés (couleur de marque #EF443B, structure de la fenêtre principale et de la barre de session — voir [`17-anydesk-realite.md`](../17-anydesk-realite.md) §7-8 et la base de connaissances AnyDesk) ; (b) une **reconstruction fidèle** des dimensions/rayons/typo à partir de captures publiques du client v9.x. Les valeurs reconstruites sont marquées **(R)** : elles visent le rendu, pas une conformité pixel certifiée. Là où NovaDesk a intérêt à diverger (voir §7, décision de marque), c'est signalé.

---

## 0. Décision préalable OBLIGATOIRE — identité de marque

L'UI actuelle est bâtie sur un **seed indigo `#4C5FD5`** (`ui/lib/main.dart:94`), qui découle du plan 10 (marque NovaDesk propre). La demande « exactement la même interface qu'AnyDesk » implique de **basculer sur le rouge AnyDesk `#EF443B`** et d'adopter le chrome/densité AnyDesk. Ces deux intentions sont **contradictoires**. Ce document spécifie la **cible « clone AnyDesk »** ; l'arbitrage (cloner à l'identique vs. s'en inspirer avec la marque NovaDesk) est listé comme risque P0 dans [`00-synthese-et-roadmap.md`](00-synthese-et-roadmap.md). **Tout le reste de ce fichier suppose l'option « clone visuel AnyDesk ».**

---

## 1. Fondations visuelles (design tokens)

### 1.1 Palette de couleurs

| Rôle | Clair | Sombre | Source |
|---|---|---|---|
| **Accent / marque (primary)** | `#EF443B` | `#EF443B` | ✅ officiel (logo) |
| Accent pressé / hover | `#D63A32` (R) | `#F25C54` (R) | R |
| Fond fenêtre (surface) | `#FFFFFF` | `#1C1C1E` (R) | R |
| Fond secondaire / panneaux | `#F5F5F7` (R) | `#262629` (R) | R |
| Fond barre de titre | `#FFFFFF` / `#F0F0F0` (R) | `#141416` (R) | R |
| Surface vidéo (session) | `#000000` | `#000000` | ✅ (fond noir en session) |
| Bordures / séparateurs | `#E3E3E6` (R) | `#37373A` (R) | R |
| Texte primaire | `#1A1A1A` (R) | `#F2F2F2` (R) | R |
| Texte secondaire / labels | `#6B6B70` (R) | `#9A9AA0` (R) | R |
| Succès / en ligne (P2P direct) | `#2FB457` (R) | `#37C766` (R) | R |
| Avertissement (relayé) | `#F0A020` (R) | `#F5B24A` (R) | R |
| Erreur / déconnexion | `#EF443B` | `#EF443B` | — |

> **Remarque densité de couleur.** AnyDesk est **majoritairement neutre** (blanc/anthracite) avec le rouge en **accent parcimonieux** (bouton « Se connecter », liens, indicateur de session, logo). Ne pas « rougir » toute l'UI : le rouge ponctue, il n'inonde pas. C'est l'inverse d'un `ColorScheme.fromSeed(rouge)` Material 3 qui teinte tout en rosé — voir §3.

### 1.2 Typographie

| Élément | Valeur cible (R) | Actuel NovaDesk |
|---|---|---|
| Police UI | **Sans-serif système** : Segoe UI (Windows), SF Pro (macOS), Roboto (Linux) | Roboto (défaut M3), aucune police définie |
| ID AnyDesk (grand) | ~28–32 px, poids 300–400, chiffres à chasse fixe, groupés `123 456 789` | `headlineMedium` (~28px) + `FontFeature.tabularFigures()` + `letterSpacing:2` — **déjà proche** (`home_screen.dart:202`) |
| Titres de section | ~13–14 px, poids 600, souvent MAJUSCULES discrètes ou capitalisé | `titleMedium` (~16px, poids 500) |
| Corps | ~13 px | `bodyMedium` (~14px) |
| Labels / légendes | ~11–12 px, texte secondaire | `bodySmall`/`labelMedium` |

> Densité globale AnyDesk **plus compacte** que Material 3 par défaut : viser `VisualDensity(-1, -1)` ou `.compact`, pas `.adaptivePlatformDensity`.

### 1.3 Formes, rayons, ombres, densité

| Token | Cible AnyDesk (R) | Actuel NovaDesk |
|---|---|---|
| Rayon boutons | 3–4 px (quasi rectangulaire) | ~20px (FilledButton M3 « stadium ») |
| Rayon cartes/panneaux | 6–8 px | ~12px (Card M3) |
| Rayon champs de saisie | 3–4 px | M3 (~4px OutlineInputBorder — proche) |
| Élévation / ombres | **plates**, séparateurs 1px plutôt qu'ombres portées | Cards elevation 1 (ombre douce) |
| Coins fenêtre | droits, chrome custom sans bordure | chrome OS par défaut (shim no-op) |
| Espacements | denses (8/12/16) | aérés (16/20/24) |

---

## 2. Fenêtre principale AnyDesk — spécification

### 2.1 Structure générale (✅ structure officielle, layout reconstruit)

AnyDesk = **fenêtre unique**, chrome custom, avec :

```
┌───────────────────────────────────────────────────────────────┐
│ [≡] AnyDesk            [onglets de session]         — □ ✕      │  ← barre de titre custom (30–36px)
├───────────────┬───────────────────────────────────────────────┤
│  SIDEBAR      │  ZONE PRINCIPALE                               │
│  (~200–240px) │                                                │
│  ⌂ Accueil    │   ┌─ This Desk ──────────────────────────┐    │
│  ★ Favoris    │   │  Votre adresse : 123 456 789   [copier]│    │
│  🕑 Récents   │   │  Alias : nom@ad                        │    │
│  📖 Carnet    │   └────────────────────────────────────────┘    │
│  🔍 Découverte│                                                │
│               │   ┌─ Remote Desk ───────────────────────────┐  │
│  ─────────    │   │  [ Saisir l'adresse distante… ]  [→]     │  │
│  ⚙ Réglages   │   └──────────────────────────────────────────┘  │
│  👤 Compte    │                                                │
│               │   Récents / Favoris  (vignettes speed-dial)    │
│               │   ▢ ▢ ▢ ▢   ← cartes 140×90 avec aperçu+alias │
└───────────────┴───────────────────────────────────────────────┘
```

- **Barre de titre custom** : hauteur ~32px (R), fond clair/anthracite, logo AnyDesk (rouge) à gauche, **onglets de session** au centre (chaque connexion ouvre un onglet), boutons min/max/close à droite. Windows : pas la barre native.
- **Sidebar de navigation** : ~200–240px (R), items : Accueil, Favoris, Récents, Carnet d'adresses, Découverte (LAN), + bas : Réglages, Compte. Item actif souligné/fond accentué.
- **Panneau « This Desk »** (✅) : libellé « This Desk » / « Votre adresse » ; **ID à 9-10 chiffres** en grand, bouton copier ; alias `nom@ad` ; (accès non-surveillé : mot de passe si activé).
- **Panneau « Remote Desk »** (✅) : champ de saisie proéminent « Remote Desk » avec placeholder, bouton de connexion (flèche/`Connect`), **historique déroulant** des adresses récentes sous le champ.
- **Speed-dial / vignettes** (✅ Récents/Favoris/Découverte) : **cartes** ~140×90 (R) avec **miniature d'aperçu**, alias, indicateur en ligne, menu contextuel (connecter, favori, renommer, supprimer). C'est l'élément le plus caractéristique et **absent** de NovaDesk.

### 2.2 Comportements

- Clic sur une vignette → connexion ; double-clic idem ; menu `⋯` par vignette.
- Champ Remote Desk : `Enter` connecte ; le champ suggère les IDs récents (autocomplete).
- Découverte : rafraîchissement auto des pairs LAN (multicast UDP, [`17`](../17-anydesk-realite.md) §3).
- Onglets : plusieurs sessions simultanées, chacune dans un onglet de la barre de titre.

---

## 3. Barre d'outils **en session** AnyDesk — spécification (✅ items officiels)

Barre **horizontale en haut** de la fenêtre de session (fond anthracite semi-opaque, se masque en plein écran). De gauche à droite, groupes d'icônes (source : base de connaissances AnyDesk « session-settings », [`17`](../17-anydesk-realite.md) §7-8) :

| # | Icône / groupe | Contenu / menu | Présent NovaDesk ? |
|---|---|---|---|
| 1 | **Indicateur de connexion + sécurité** | mode (direct/relayé), chiffrement, **empreinte** (caller-ID), état vérifié | Partiel (texte en barre d'état basse, pas d'icône dédiée) |
| 2 | **Réception d'image / stats** | indicateur de flux, éventuel HUD | Non |
| 3 | **Moniteurs** | sélecteur numéroté (1 écran / tous / choix) | Oui (2 écrans en dur) |
| 4 | **Display / Qualité** | préréglages **Meilleure qualité / Équilibré / Meilleures perfs**, résolution, **plein écran**, échelle, qualité couleur | Partiel (Auto/Fluidité/Netteté + plein écran) |
| 5 | **Favori** (étoile) | ajouter/retirer des favoris | Non (existe sur l'accueil) |
| 6 | **Gestionnaire de fichiers** | ouvre le double-panneau | Partiel (bottom-sheet fictif) |
| 7 | **Chat** | volet de discussion | Oui (drawer, local) |
| 8 | **Actions** (⋯ / menu) | Demander élévation, **Ctrl+Alt+Suppr**, verrouiller, changer d'utilisateur, déconnecter la session, **capture d'écran**, redémarrer, **tunnel TCP**, coller le presse-papiers | Partiel (Ctrl+Alt+Suppr seul, bouton dédié) |
| 9 | **Clavier / saisie** | disposition clavier, **mode de transmission** des touches | Non |
| 10 | **Permissions** | activer/désactiver : audio, souris/clavier, presse-papiers, bloquer l'entrée distante, verrouiller le compte en fin, mode confidentialité | **Non** (permissions figées à la connexion) |
| 11 | **Enregistrement** | démarrer/arrêter l'enregistrement de session | **Non** |
| 12 | **Tableau blanc / annotation** | dessiner sur l'écran distant | **Non** |
| 13 | **Mode confidentialité** | noircir le moniteur physique distant | **Non** |

> **Barre d'état basse** AnyDesk : discrète ; l'info sécurité/empreinte est surtout dans l'indicateur (#1). L'UI actuelle met tout en bas (badge + « TLS 1.3 + Noise_IK » + SAS) — à réorganiser vers le modèle « indicateur en barre d'outils ».

---

## 4. Dialogues et écrans secondaires

| Écran/dialogue AnyDesk | Rôle | Présent NovaDesk ? |
|---|---|---|
| **Fenêtre d'acceptation** (côté contrôlé) | Accepter/refuser une connexion entrante, cocher les **permissions** demandées, voir l'empreinte du connecteur | **Absent** (seul un toggle « confirmation requise ») |
| **Gestionnaire de fichiers** double-panneau | Arbre local ↔ distant, glisser-déposer, file de transferts | **Absent** (bottom-sheet fictif) |
| **Réglages** (multi-onglets) | Interface, Sécurité (profils de permissions, ACL), Connexion, Affichage, Enregistrement, À propos/Empreinte | Présent, mono-page, état volatile |
| **Accès non-surveillé** | Mot de passe permanent, appareils de confiance, TOTP | Présent, abouti (mais simulé) |
| **Carnet d'adresses** | Contacts, tags, groupes | Fusionné dans « Récents & carnet » |

---

## 5. Tableau d'écart ÉCRAN PAR ÉCRAN (cible clone AnyDesk ↔ Flutter actuel)

Format : **Cible AnyDesk** → **Actuel** → **Changements exacts** (fichiers à toucher).

### 5.1 Chrome global / thème (`ui/lib/main.dart`)

| Aspect | Cible | Actuel | Changement exact |
|---|---|---|---|
| Couleur primaire | `#EF443B` accent parcimonieux | seed `#4C5FD5` (indigo) tout teinté | Remplacer `ColorScheme.fromSeed(0xFF4C5FD5)` par un `ColorScheme` **construit à la main** (surfaces neutres + `primary=#EF443B`). Ne pas dériver toute la palette du rouge. |
| Densité | compacte | `adaptivePlatformDensity` | `visualDensity: VisualDensity(-1,-1)` |
| Police | système (Segoe/SF/Roboto) | Roboto défaut | Définir `fontFamily` par plateforme ; `TextTheme` compact |
| Formes | rayons 3–8px, plat | M3 arrondi + ombres | `cardTheme`, `filledButtonTheme`, `inputDecorationTheme` avec petits rayons, elevation 0, bordures 1px |
| Chrome fenêtre | barre titre custom + onglets | chrome OS (shim no-op) | Voir §5.6 (contrainte no-admin) |

### 5.2 Écran d'accueil (`ui/lib/screens/home_screen.dart`)

| Aspect | Cible | Actuel | Changement exact |
|---|---|---|---|
| Navigation | **sidebar** gauche 200–240px (Accueil/Favoris/Récents/Carnet/Découverte/Réglages/Compte) | `AppBar` + 3 cartes empilées | Introduire un `Row[ NavigationRail/sidebar custom , contenu ]` ; retirer l'`AppBar` générique |
| This Desk | panneau « Votre adresse » + ID + alias + (mdp non-surveillé) | carte « Ce poste » OK conceptuellement | Ré-habiller en panneau plat, libellés AnyDesk, densité compacte |
| Remote Desk | champ proéminent + historique déroulant + bouton connecter accentué | carte + `SegmentedButton` modes + bouton | Champ plus grand, autocomplete IDs récents ; déplacer les modes (Contrôle/Observation) dans un menu secondaire (AnyDesk les met dans la fenêtre d'acceptation/permissions, pas en façade) |
| Speed-dial | **vignettes 140×90 avec aperçu** (Récents/Favoris/Découverte, onglets) | `ListTile` simples fusionnés | Créer un `GridView`/`Wrap` de cartes vignettes + onglets Récents/Favoris/Découverte ; **nouveau widget** `SessionThumbnail` |
| Découverte LAN | onglet listant les pairs du réseau | absent | Ajouter l'onglet (données via cœur, voir prompts) |

### 5.3 Écran de session (`ui/lib/screens/session_screen.dart`)

| Aspect | Cible | Actuel | Changement exact |
|---|---|---|---|
| **Rendu vidéo** | surface live plein cadre | `Texture` conditionnée à `_textureId` **toujours null** → placeholder | **P0 absolu** : brancher `_textureId` sur la texture publiée par le cœur (voir [`01`](01-analyse-ecarts.md) + prompts FFI/texture). Fond `#000000` (déjà `#101014`, à passer noir) |
| Barre d'outils | 13 groupes (§3) | 7 boutons | Ajouter : Permissions (menu), Actions (menu groupé : élévation, verrouiller, redémarrer, capture, tunnel), Enregistrement, Clavier/saisie, indicateur sécurité/empreinte en haut, favori |
| Qualité | Meilleure qualité/Équilibré/Meilleures perfs | Auto/Fluidité/Netteté | Renommer pour coller au vocabulaire AnyDesk (ou garder + libellés FR équivalents) |
| Chat | volet réel (canal cœur) | drawer **local** | Brancher sur le canal chat du cœur (prompt features) |
| Transfert | double-panneau | bottom-sheet fictif | Écran/volet dédié (prompt files+UI) |
| Barre d'état | discrète ; sécurité dans l'indicateur haut | tout en bas, SAS/TLS en dur | Réorganiser ; alimenter par le vrai statut (StreamSink) |

### 5.4 Réglages (`ui/lib/screens/settings_screen.dart`)

| Aspect | Cible | Actuel | Changement exact |
|---|---|---|---|
| Structure | multi-onglets (Interface/Sécurité/Connexion/Affichage/Enregistrement/À propos) | mono-page, sections | Passer en onglets ; **Profils de permissions** (Default/Screen-Sharing/Full/Unattended) ; ACL liste blanche à joker |
| Empreinte | réelle + QR | en dur `9A:F2:…`, QR vide | Alimenter par le cœur ; générer le QR |
| État | persistant | volatile (mémoire) | Persistance (prompt réglages) |

### 5.5 Accès non-surveillé (`ui/lib/screens/unattended_screen.dart`)

Déjà le plus abouti. Écart : **mot de passe par profil de permissions** (idée AnyDesk, [`17`](../17-anydesk-realite.md) §7), persistance réelle, câblage cœur. Réhabillage densité/couleur.

### 5.6 Chrome fenêtre / barre de titre (contrainte no-admin)

- **Cible** : barre de titre sans bordure + onglets de session + boutons custom.
- **Contrainte** : `window_manager` (plugin natif) est **interdit ici** (pas d'admin/symlinks) ; remplacé par un shim no-op → le fenêtrage n'a **aucun effet** (`ui/lib/platform/window_shim.dart`).
- **Option viable sans plugin natif** : `bitsdojo_window` est aussi natif (exclu). Rester en **chrome OS** pour l'instant et simuler une **barre d'onglets applicative** _sous_ la barre native (un `Row` d'onglets custom en haut du `Scaffold`). La vraie fenêtre sans bordure est un chantier **packaging** (build sur machine avec droits), documenté P2.

### 5.7 Dialogue d'acceptation entrante — **à créer**

Nouveau `screens/incoming_request_dialog.dart` : identité du connecteur + empreinte, cases de permissions (audio/clavier/souris/presse-papiers/fichiers/…), boutons Accepter/Refuser, profils. C'est un manque fonctionnel **et** visuel majeur côté « poste contrôlé ».

---

## 6. Les 5 écarts visuels prioritaires (rappel synthétique)

1. **Couleur de marque** : indigo `#4C5FD5` → rouge `#EF443B` en accent (refonte du `ColorScheme`, sans tout rougir).
2. **Rendu vidéo absent** : la session est un placeholder noir ; c'est le P0 transverse (UI+FFI+cœur).
3. **Pas de sidebar ni de speed-dial à vignettes** : layout « cartes empilées » générique → sidebar + vignettes d'aperçu + onglets Récents/Favoris/Découverte.
4. **Chrome/densité M3 générique** : boutons stadium, cards ombrées, aéré → plat, dense, rayons 3–8px, séparateurs 1px, police système.
5. **Barre d'outils de session incomplète** + **dialogue d'acceptation absent** : ajouter Permissions/Actions/Enregistrement/indicateur sécurité + créer la fenêtre d'acceptation entrante.

---

## 7. Design tokens Flutter concrets (à implémenter)

Extrait cible pour `main.dart` (option clone AnyDesk) — **à affiner par l'agent UI** :

```dart
// Palette AnyDesk (clone). Rouge = accent parcimonieux, surfaces neutres.
const kAnyRed = Color(0xFFEF443B);
ColorScheme _scheme(Brightness b) => (b == Brightness.light)
  ? const ColorScheme.light(
      primary: kAnyRed, onPrimary: Colors.white,
      surface: Color(0xFFFFFFFF), onSurface: Color(0xFF1A1A1A),
      surfaceContainerHighest: Color(0xFFF5F5F7),
      outline: Color(0xFFE3E3E6), error: kAnyRed,
    )
  : const ColorScheme.dark(
      primary: kAnyRed, onPrimary: Colors.white,
      surface: Color(0xFF1C1C1E), onSurface: Color(0xFFF2F2F2),
      surfaceContainerHighest: Color(0xFF262629),
      outline: Color(0xFF37373A), error: kAnyRed,
    );
ThemeData _theme(Brightness b) => ThemeData(
    useMaterial3: true,
    colorScheme: _scheme(b),
    visualDensity: const VisualDensity(horizontal: -1, vertical: -1),
    cardTheme: const CardThemeData(elevation: 0, shape: RoundedRectangleBorder(
        borderRadius: BorderRadius.all(Radius.circular(8)))),
    filledButtonTheme: FilledButtonThemeData(style: FilledButton.styleFrom(
        shape: const RoundedRectangleBorder(
            borderRadius: BorderRadius.all(Radius.circular(4))))),
    inputDecorationTheme: const InputDecorationTheme(isDense: true,
        border: OutlineInputBorder(borderRadius: BorderRadius.all(Radius.circular(4)))),
  );
```

> Ces valeurs sont un **point de départ vérifiable** (`flutter analyze` sans erreur, rendu clair/sombre), pas une fin : l'agent UI itère sur captures AnyDesk. Voir le prompt UI dédié dans [`04-prompts-fable/`](04-prompts-fable/).
