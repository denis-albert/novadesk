/// Écran des **Réglages** — reproduction fidèle de la vue `#v-reglages` de la
/// maquette validée (`novadesk-app.html`, objet `REG` + `ctrl()` + `renderReg()`) :
/// un rail vertical d'onglets à gauche (`.stabs`, 186 px, 13 onglets) et un
/// panneau de réglages à droite (`.spane`) listant des lignes titre/sous-titre +
/// contrôle (`.set`).
///
/// Seuls deux réglages sont **réellement câblés** : le **Thème** (segmenté
/// Clair/Sombre/Système piloté par [themeModeProvider]) et la **Version** de
/// l'onglet « À propos » (lue via [appInfoProvider]). Tous les autres contrôles
/// sont de l'état de présentation local (interrupteurs, sélecteurs, champs) —
/// la persistance appartiendra au cœur Rust via la façade `nd-ffi`.
library;

import 'dart:async';

import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../app_routes.dart';
import '../state/providers.dart';
import '../theme/nova_theme.dart';
import '../widgets/nova_icons.dart';
import '../widgets/nova_kit.dart';

// ===========================================================================
// Modèle de données des onglets (calqué 1:1 sur l'objet `REG` de la maquette)
// ===========================================================================

/// Un onglet de réglages : clé (= titre affiché), icône, description et lignes.
class _Onglet {
  const _Onglet(this.cle, this.icone, this.description, this.lignes);

  final String cle;
  final IconData icone;
  final String description;
  final List<_Ligne> lignes;
}

/// Une ligne de réglage (`.set`) : titre, sous-titre optionnel, type de contrôle
/// (`on`/`off`/`seg`/`sel`/`field`/`fp`/`btn`/`txt`), valeur pré-remplie et,
/// pour les lignes **câblées à l'état persistant**, la clé de réglage
/// [cleReglage] lue/écrite via `get_setting`/`set_setting`.
class _Ligne {
  const _Ligne(this.titre, this.sousTitre, this.type,
      {this.preset = '', this.cleReglage});

  final String titre;
  final String? sousTitre;
  final String type;
  final String preset;

  /// Clé de réglage persistée (`null` = ligne de présentation locale).
  final String? cleReglage;
}

// ===========================================================================
// Écran
// ===========================================================================

class SettingsScreen extends ConsumerStatefulWidget {
  const SettingsScreen({super.key});

  static const String route = NovaRoutes.reglages;

  @override
  ConsumerState<SettingsScreen> createState() => _SettingsScreenState();
}

class _SettingsScreenState extends ConsumerState<SettingsScreen> {
  /// Les 13 onglets, dans l'ordre de la maquette (`Object.keys(REG)`).
  static const List<_Onglet> _onglets = [
    _Onglet('Interface', NovaIcones.accueil, 'Apparence et comportement.', [
      _Ligne('Thème', null, 'seg'),
      _Ligne('Langue', null, 'sel', cleReglage: 'langue'),
      _Ligne('Démarrer avec le système', null, 'on',
          cleReglage: 'demarrer_avec_systeme'),
      _Ligne('Réduire dans la zone de notification', null, 'on'),
      _Ligne('Densité compacte', null, 'off'),
    ]),
    _Onglet('Sécurité', NovaIcones.bouclier, 'Contrôle d’accès.', [
      _Ligne('Liste blanche (ACL)',
          'N’autoriser que certains identifiants.', 'field'),
      _Ligne('Double authentification (TOTP)', null, 'off'),
      _Ligne('Toujours confirmer les sessions entrantes', null, 'on'),
      _Ligne('Profils de permissions', '4 profils.', 'btn'),
      _Ligne('Empreinte de ce poste', null, 'fp'),
    ]),
    _Onglet('Connexion', NovaIcones.reseau, 'Serveurs de mise en relation.', [
      _Ligne('Serveur de rendez-vous', null, 'field',
          cleReglage: 'serveur_rendezvous'),
      _Ligne('Serveur de relais', null, 'field', cleReglage: 'serveur_relais'),
      _Ligne('Serveurs STUN', null, 'field', cleReglage: 'serveurs_stun'),
      _Ligne('Proxy', null, 'sel'),
    ]),
    _Onglet('Affichage', NovaIcones.affichage, 'Qualité par défaut.', [
      _Ligne('Mode par défaut', null, 'sel', cleReglage: 'prereglage_qualite'),
      _Ligne('Images/s cible', null, 'sel'),
      _Ligne('Débit maximum', null, 'field'),
      _Ligne('Accélération NVENC', null, 'on'),
    ]),
    _Onglet('Audio', NovaIcones.audio, 'Transmission du son.', [
      _Ligne('Transmettre le son distant', null, 'on'),
      _Ligne('Périphérique de sortie', null, 'sel'),
      _Ligne('Transmettre mon micro', null, 'off'),
    ]),
    _Onglet('Imprimante', NovaIcones.imprimante, 'Impression à distance.', [
      _Ligne('Autoriser l’impression distante', null, 'on'),
      _Ligne('Imprimante locale', null, 'sel'),
    ]),
    _Onglet('Capture', NovaIcones.capturePartage, 'Ce qui est partagé.', [
      _Ligne('Écrans à partager', null, 'sel'),
      _Ligne('Cadre d’écran', null, 'off'),
      _Ligne('Masquer le fond d’écran', null, 'off'),
    ]),
    _Onglet('Transfert de fichiers', NovaIcones.dossierSync,
        'Réglages de transfert.', [
      _Ligne('Dossier par défaut', null, 'field'),
      _Ligne('Écraser les fichiers', null, 'off'),
      _Ligne('Reprise automatique', null, 'on'),
    ]),
    _Onglet('Enregistrement', NovaIcones.enregistrements,
        'Enregistrement des sessions.', [
      _Ligne('Enregistrer automatiquement', null, 'off'),
      _Ligne('Dossier', null, 'field', cleReglage: 'dossier_enregistrement'),
      _Ligne('Format', null, 'sel'),
    ]),
    _Onglet('Wake-on-LAN', NovaIcones.alimentation, 'Réveil des appareils.', [
      _Ligne('Activer Wake-on-LAN', null, 'on'),
      _Ligne('Pair relais', null, 'sel'),
    ]),
    _Onglet('Confidentialité', NovaIcones.confidentialite, 'Protection.', [
      _Ligne('Écran noir distant', null, 'off'),
      _Ligne('Bloquer les entrées locales', null, 'off'),
    ]),
    _Onglet('Raccourcis', NovaIcones.clavier, 'Combinaisons clavier.', [
      _Ligne('Plein écran', null, 'field', preset: 'F11'),
      _Ligne('Libérer la souris', null, 'field',
          preset: 'Ctrl+Alt+Maj+Espace'),
      _Ligne('Ctrl+Alt+Suppr distant', null, 'field'),
    ]),
    _Onglet('À propos', NovaIcones.info, 'Version et licence.', [
      _Ligne('Version', null, 'txt', preset: 'NovaDesk 0.1 (build 240)'),
      _Ligne('Licence', null, 'txt', preset: 'Édition libre'),
      _Ligne('Mises à jour', null, 'on'),
      _Ligne('Empreinte', null, 'fp'),
    ]),
  ];

  /// Options des sélecteurs câblés à un réglage : (valeur persistée, libellé).
  static const Map<String, List<(String, String)>> _optionsSel = {
    'langue': [('fr', 'Français'), ('en', 'English')],
    'prereglage_qualite': [
      ('qualite', 'Meilleure qualité'),
      ('equilibre', 'Équilibré'),
      ('performance', 'Meilleures performances'),
    ],
  };

  /// Onglet sélectionné (défaut : Interface).
  int _ongletActif = 0;

  // État de présentation local, indexé par clé stable « onglet::ligne ».
  final Map<String, bool> _interrupteurs = {};
  final Map<String, String> _selections = {};
  final Map<String, TextEditingController> _champs = {};

  /// Réglages persistés chargés depuis `get_settings` (source des contrôles
  /// câblés). Rempli une fois au démarrage de l'écran.
  final Map<String, String> _reglages = {};

  @override
  void initState() {
    super.initState();
    // Amorçage des états locaux depuis la définition des onglets.
    for (final onglet in _onglets) {
      for (final ligne in onglet.lignes) {
        final cle = _cle(onglet, ligne);
        switch (ligne.type) {
          case 'on':
          case 'off':
            _interrupteurs[cle] = ligne.type == 'on';
          case 'sel':
            _selections[cle] = 'Automatique';
          case 'field':
            _champs[cle] = TextEditingController(text: ligne.preset);
        }
      }
    }
    unawaited(_chargerReglages());
  }

  /// Charge les réglages persistés et hydrate les contrôles câblés.
  Future<void> _chargerReglages() async {
    try {
      final settings = await ref.read(nativeApiProvider).getSettings();
      final map = {for (final s in settings) s.cle: s.valeur};
      if (!mounted) return;
      setState(() {
        _reglages
          ..clear()
          ..addAll(map);
        for (final onglet in _onglets) {
          for (final ligne in onglet.lignes) {
            final cleReglage = ligne.cleReglage;
            if (cleReglage == null) continue;
            final valeur = map[cleReglage];
            if (valeur == null) continue;
            final cleLocale = _cle(onglet, ligne);
            switch (ligne.type) {
              case 'field':
                _champs[cleLocale]?.text = valeur;
              case 'on':
              case 'off':
                _interrupteurs[cleLocale] = valeur == 'true';
            }
          }
        }
      });
    } catch (_) {
      // Réglages indisponibles : on conserve les valeurs par défaut locales.
    }
  }

  /// Persiste un réglage (`set_setting`) et invalide la vue des réglages afin
  /// que les providers dérivés (rendez-vous, STUN, relais) se rafraîchissent.
  void _definirReglage(String cle, String valeur) {
    setState(() => _reglages[cle] = valeur);
    unawaited(ref.read(nativeApiProvider).setSetting(cle: cle, valeur: valeur));
    ref.invalidate(settingsProvider);
  }

  @override
  void dispose() {
    for (final controleur in _champs.values) {
      controleur.dispose();
    }
    super.dispose();
  }

  /// Clé stable d'une ligne (titres uniques par onglet, onglets uniques).
  String _cle(_Onglet onglet, _Ligne ligne) => '${onglet.cle}::${ligne.titre}';

  @override
  Widget build(BuildContext context) {
    final t = NovaTokens.of(context);
    final onglet = _onglets[_ongletActif];
    return Row(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        _railOnglets(t),
        Expanded(child: _panneau(t, onglet)),
      ],
    );
  }

  // -------------------------------------------------------------------------
  // Rail vertical d'onglets (`.stabs`)
  // -------------------------------------------------------------------------

  Widget _railOnglets(NovaTokens t) {
    return Container(
      width: 186,
      decoration: BoxDecoration(
        border: Border(right: BorderSide(color: t.filet)),
      ),
      child: SingleChildScrollView(
        padding: const EdgeInsets.symmetric(horizontal: 8, vertical: 10),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.stretch,
          children: [
            for (var i = 0; i < _onglets.length; i++)
              _StabItem(
                icone: _onglets[i].icone,
                libelle: _onglets[i].cle,
                actif: i == _ongletActif,
                onTap: () => setState(() => _ongletActif = i),
              ),
          ],
        ),
      ),
    );
  }

  // -------------------------------------------------------------------------
  // Panneau de réglages (`.spane`)
  // -------------------------------------------------------------------------

  Widget _panneau(NovaTokens t, _Onglet onglet) {
    return SingleChildScrollView(
      padding: const EdgeInsets.symmetric(horizontal: 26, vertical: 22),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          // .stitle
          Text(
            onglet.cle,
            style: TextStyle(
                fontSize: 16, fontWeight: FontWeight.w600, color: t.texte),
          ),
          const SizedBox(height: 3),
          // .sdesc
          Text(
            onglet.description,
            style: TextStyle(fontSize: 12, color: t.texte3),
          ),
          const SizedBox(height: 16),
          for (var i = 0; i < onglet.lignes.length; i++)
            _ligneReglage(t, onglet, onglet.lignes[i],
                dernier: i == onglet.lignes.length - 1),
        ],
      ),
    );
  }

  /// Une ligne de réglage (`.set`) : textes à gauche, contrôle à droite,
  /// filet inférieur sauf pour la dernière ligne.
  Widget _ligneReglage(NovaTokens t, _Onglet onglet, _Ligne ligne,
      {required bool dernier}) {
    return Container(
      decoration: BoxDecoration(
        border:
            dernier ? null : Border(bottom: BorderSide(color: t.filet)),
      ),
      padding: const EdgeInsets.symmetric(vertical: 13),
      child: Row(
        children: [
          // .txt (Expanded)
          Expanded(
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                // .a
                Text(
                  ligne.titre,
                  style: TextStyle(
                      fontSize: 13,
                      fontWeight: FontWeight.w500,
                      color: t.texte),
                ),
                // .b
                if (ligne.sousTitre != null) ...[
                  const SizedBox(height: 2),
                  ConstrainedBox(
                    constraints: const BoxConstraints(maxWidth: 430),
                    child: Text(
                      ligne.sousTitre!,
                      style: TextStyle(fontSize: 11.5, color: t.texte3),
                    ),
                  ),
                ],
              ],
            ),
          ),
          const SizedBox(width: 16),
          _construireControle(t, onglet, ligne),
        ],
      ),
    );
  }

  // -------------------------------------------------------------------------
  // Rendu des contrôles (`ctrl(k)` de la maquette)
  // -------------------------------------------------------------------------

  Widget _construireControle(NovaTokens t, _Onglet onglet, _Ligne ligne) {
    final cle = _cle(onglet, ligne);
    final cleReglage = ligne.cleReglage;
    switch (ligne.type) {
      // Interrupteurs (`.sw`) — persistés si câblés (ex. démarrer-avec-système).
      case 'on':
      case 'off':
        final actif = _interrupteurs[cle] ?? (ligne.type == 'on');
        return NovaSwitch(
          actif: actif,
          label: ligne.titre,
          onChanged: (v) {
            setState(() => _interrupteurs[cle] = v);
            if (cleReglage != null) {
              _definirReglage(cleReglage, v ? 'true' : 'false');
            }
          },
        );

      // Segmenté (`.segb`) — le Thème : relié à [themeModeProvider] (effet
      // immédiat) ET persisté via `set_setting("theme", …)`.
      case 'seg':
        final mode = ref.watch(themeModeProvider);
        return NovaSegmented<ThemeMode>(
          valeurs: const [
            (ThemeMode.light, 'Clair'),
            (ThemeMode.dark, 'Sombre'),
            (ThemeMode.system, 'Système'),
          ],
          selection: mode,
          onChanged: (v) {
            ref.read(themeModeProvider.notifier).state = v;
            _definirReglage('theme', reglageDepuisTheme(v));
          },
        );

      // Sélecteur compact (`.selc`) — persisté si câblé (langue, qualité).
      case 'sel':
        if (cleReglage != null && _optionsSel.containsKey(cleReglage)) {
          final options = _optionsSel[cleReglage]!;
          final valeurBrute = _reglages[cleReglage] ?? options.first.$1;
          final labelCourant = options
              .firstWhere((o) => o.$1 == valeurBrute,
                  orElse: () => options.first)
              .$2;
          return _Selecteur(
            valeur: labelCourant,
            options: [for (final o in options) o.$2],
            onChanged: (label) {
              final brute = options
                  .firstWhere((o) => o.$2 == label,
                      orElse: () => options.first)
                  .$1;
              _definirReglage(cleReglage, brute);
            },
          );
        }
        final valeur = _selections[cle] ?? 'Automatique';
        return _Selecteur(
          valeur: valeur,
          options: const ['Automatique', 'Manuel'],
          onChanged: (v) => setState(() => _selections[cle] = v),
        );

      // Champ de saisie (`.field`) — persisté si câblé (serveurs, dossiers).
      case 'field':
        return _champ(
          t,
          _champs[cle]!,
          onChanged:
              cleReglage == null ? null : (v) => _definirReglage(cleReglage, v),
        );

      // Empreinte monospace (`.fp`) — issue de local_identity.
      case 'fp':
        return _empreinte(t);

      // Bouton secondaire (`.btn`).
      case 'btn':
        return NovaBoutonSecondaire(
          libelle: 'Gérer',
          onPressed: () => NovaToast.montrer(
              context, 'Gestion des profils de permissions — à venir.'),
        );

      // Texte simple — la Version est lue depuis le moteur (`appInfoProvider`).
      case 'txt':
        final valeur = ligne.titre == 'Version'
            ? ref.watch(appInfoProvider).maybeWhen(
                data: (info) => 'NovaDesk ${info.version} (build 240)',
                orElse: () => ligne.preset,
              )
            : ligne.preset;
        return Text(
          valeur,
          style: TextStyle(fontSize: 12.5, color: t.texte2),
        );

      default:
        return const SizedBox.shrink();
    }
  }

  /// Champ de saisie 210×32 (`.field`) — habillage hérité du thème. Pour les
  /// champs câblés, [onChanged] persiste la valeur via `set_setting`.
  Widget _champ(NovaTokens t, TextEditingController controleur,
      {ValueChanged<String>? onChanged}) {
    return SizedBox(
      width: 210,
      height: 32,
      child: TextField(
        controller: controleur,
        onChanged: onChanged,
        textAlignVertical: TextAlignVertical.center,
        cursorColor: kNovaRouge,
        style: TextStyle(fontSize: 12.5, color: t.texte),
        decoration: const InputDecoration(
          isDense: true,
          contentPadding: EdgeInsets.symmetric(horizontal: 10, vertical: 6),
        ),
      ),
    );
  }

  /// Empreinte monospace (`.fp`) — police Cascadia Code / Consolas, issue de
  /// `local_identity` (8 premières paires hexadécimales).
  Widget _empreinte(NovaTokens t) {
    final identite = ref.watch(localIdentityProvider);
    final style = TextStyle(
      fontFamily: 'Cascadia Code',
      fontFamilyFallback: const ['Consolas', 'monospace'],
      fontSize: 12,
      letterSpacing: 1,
      color: t.texte2,
    );
    return identite.when(
      data: (i) => Text(_empreinteFormatee(i.empreinte), style: style),
      loading: () => const NovaSkeleton(largeur: 150, hauteur: 12),
      error: (e, _) => Text('—', style: style),
    );
  }

  /// Formate une empreinte hexadécimale en paires « AB·CD·… » (8 paires max).
  static String _empreinteFormatee(String empreinte) {
    final hex = empreinte.toUpperCase();
    final n = hex.length < 16 ? hex.length : 16;
    final paires = <String>[];
    for (var i = 0; i + 2 <= n; i += 2) {
      paires.add(hex.substring(i, i + 2));
    }
    return paires.join('·');
  }
}

// ===========================================================================
// Élément d'onglet du rail (`.stab`) — survol + état sélectionné
// ===========================================================================

class _StabItem extends StatelessWidget {
  const _StabItem({
    required this.icone,
    required this.libelle,
    required this.actif,
    required this.onTap,
  });

  final IconData icone;
  final String libelle;
  final bool actif;
  final VoidCallback onTap;

  @override
  Widget build(BuildContext context) {
    final t = NovaTokens.of(context);
    final couleur = actif ? t.texte : t.texte2;
    return NovaActivable(
      onTap: onTap,
      label: libelle,
      builder: (context, survole, focus) => Container(
        padding: const EdgeInsets.symmetric(horizontal: 11, vertical: 8),
        decoration: BoxDecoration(
          color: actif
              ? t.selection
              : (survole ? t.survol : Colors.transparent),
          borderRadius: BorderRadius.circular(kNovaRayon),
        ),
        child: Row(
          children: [
            NovaIcone(icone, taille: 15, couleur: couleur),
            const SizedBox(width: 9),
            Expanded(
              child: Text(
                libelle,
                overflow: TextOverflow.ellipsis,
                style: TextStyle(
                  fontSize: 12.5,
                  fontWeight: actif ? FontWeight.w500 : FontWeight.w400,
                  color: couleur,
                ),
              ),
            ),
          ],
        ),
      ),
    );
  }
}

// ===========================================================================
// Sélecteur compact (`.selc`) — valeur + chevron, menu déroulant thémé
// ===========================================================================

class _Selecteur extends StatelessWidget {
  const _Selecteur({
    required this.valeur,
    required this.options,
    required this.onChanged,
  });

  final String valeur;
  final List<String> options;
  final ValueChanged<String> onChanged;

  @override
  Widget build(BuildContext context) {
    final t = NovaTokens.of(context);
    return PopupMenuButton<String>(
      tooltip: '',
      padding: EdgeInsets.zero,
      position: PopupMenuPosition.under,
      onSelected: onChanged,
      itemBuilder: (context) => [
        for (final option in options)
          PopupMenuItem<String>(
            value: option,
            height: 34,
            child: Text(
              option,
              style: TextStyle(fontSize: 12.5, color: t.texte),
            ),
          ),
      ],
      child: MouseRegion(
        cursor: SystemMouseCursors.click,
        child: Container(
          width: 150,
          height: 32,
          padding: const EdgeInsets.symmetric(horizontal: 10),
          decoration: BoxDecoration(
            color: t.champ,
            border: Border.all(color: t.filetFort),
            borderRadius: BorderRadius.circular(kNovaRayon),
          ),
          child: Row(
            children: [
              Expanded(
                child: Text(
                  valeur,
                  overflow: TextOverflow.ellipsis,
                  style: TextStyle(fontSize: 12.5, color: t.texte),
                ),
              ),
              NovaIcone(NovaIcones.chevronBas, taille: 15, couleur: t.texte2),
            ],
          ),
        ),
      ),
    );
  }
}
