/// Thème NovaDesk — tokens **exacts** de la maquette validée
/// (`novadesk-app.html`, variables CSS `:root` clair + `[data-theme="dark"]`) :
/// géométrie très carrée (`--r` = 4 px), surfaces neutres à plat, filets 1 px,
/// densité compacte, typographie Segoe UI à chiffres tabulaires.
///
/// Le rouge de marque `#EF443B` reste **strictement parcimonieux** : logo,
/// bouton « Se connecter », onglet actif, indicateur de rail, survol du bouton
/// fermer, « Terminer » en session, avatar de pair. Tout le reste est neutre ;
/// le vert signale l'accès/présence, le bleu les liens et la sélection, l'ambre
/// les favoris.
library;

import 'package:flutter/material.dart';

/// Rayon de base de la maquette (`--r:4px`) — géométrie carrée.
const double kNovaRayon = 4;

/// Rouge de marque (`--red`).
const Color kNovaRouge = Color(0xFFEF443B);

/// Rouge pressé / survol du bouton principal (`--redp`).
const Color kNovaRougePresse = Color(0xFFD93B33);

/// Vert « en ligne / accès accordé » (`--green`, valeur claire ; la variante
/// sombre `#3FB457` est portée par [NovaTokens.vert]).
const Color kNovaVert = Color(0xFF37A24B);

/// Bleu « lien / sélection » (`--blue`, valeur claire).
const Color kNovaBleu = Color(0xFF2F6FE0);

/// Ambre « favori » (`--amber`).
const Color kNovaAmbre = Color(0xFFD98A1F);

/// Jeu de couleurs hors `ColorScheme` (filets, champs, barres, sélection…),
/// calqué 1:1 sur les variables CSS de la maquette.
@immutable
class NovaTokens extends ThemeExtension<NovaTokens> {
  const NovaTokens({
    required this.fenetre,
    required this.panneau,
    required this.panneau2,
    required this.barre,
    required this.filet,
    required this.filetFort,
    required this.texte,
    required this.texte2,
    required this.texte3,
    required this.survol,
    required this.champ,
    required this.champBordure,
    required this.logo,
    required this.selection,
    required this.vert,
    required this.bleu,
    required this.ambre,
    required this.vignette1,
    required this.vignette2,
    required this.vignette3,
  });

  /// Clair — `:root` de la maquette.
  const NovaTokens.clair()
      : this(
          fenetre: const Color(0xFFFFFFFF), // --win
          panneau: const Color(0xFFF5F6F8), // --panel
          panneau2: const Color(0xFFEEF0F3), // --panel2
          barre: const Color(0xFFF5F6F8), // --panel (tbar / status / rail)
          filet: const Color(0xFFE1E4E8), // --line
          filetFort: const Color(0xFFD2D6DC), // --line2
          texte: const Color(0xFF1F2328), // --t1
          texte2: const Color(0xFF5B636D), // --t2
          texte3: const Color(0xFF8B929B), // --t3
          survol: const Color(0xFFECEEF1), // --hover
          champ: const Color(0xFFFFFFFF), // --win
          champBordure: const Color(0xFFD2D6DC), // --line2
          logo: const Color(0xFF1F2328), // hérite --t1
          selection: const Color(0xFFE9F0FB), // --sel
          vert: const Color(0xFF37A24B), // --green
          bleu: const Color(0xFF2F6FE0), // --blue
          ambre: const Color(0xFFD98A1F), // --amber
          vignette1: const Color(0xFFD8DCE2),
          vignette2: const Color(0xFFD2D6DC),
          vignette3: const Color(0xFFDCE0E5),
        );

  /// Sombre — `:root[data-theme="dark"]` de la maquette.
  const NovaTokens.sombre()
      : this(
          fenetre: const Color(0xFF181B21), // --win
          panneau: const Color(0xFF1E222A), // --panel
          panneau2: const Color(0xFF22272F), // --panel2
          barre: const Color(0xFF1E222A), // --panel
          filet: const Color(0xFF282E37), // --line
          filetFort: const Color(0xFF343B45), // --line2
          texte: const Color(0xFFE6E9ED), // --t1
          texte2: const Color(0xFF9DA5AF), // --t2
          texte3: const Color(0xFF69727C), // --t3
          survol: const Color(0xFF252B34), // --hover
          champ: const Color(0xFF181B21), // --win
          champBordure: const Color(0xFF343B45), // --line2
          logo: const Color(0xFFE6E9ED),
          selection: const Color(0xFF1C2838), // --sel
          vert: const Color(0xFF3FB457), // --green (dark)
          bleu: const Color(0xFF5B93F0), // --blue (dark)
          ambre: const Color(0xFFD98A1F), // --amber
          vignette1: const Color(0xFF282E37),
          vignette2: const Color(0xFF22272F),
          vignette3: const Color(0xFF2A313B),
        );

  /// Fond de fenêtre / contenu principal (`--win`).
  final Color fenetre;

  /// Fond secondaire (`--panel`).
  final Color panneau;

  /// Fond tertiaire (`--panel2`) — pastilles OS, bulles de discussion.
  final Color panneau2;

  /// Fond des barres de titre, d'état et du rail (`--panel`).
  final Color barre;

  /// Filet séparateur 1 px (`--line`).
  final Color filet;

  /// Filet appuyé — bordures de champ/fenêtre (`--line2`).
  final Color filetFort;

  /// Texte primaire (`--t1`).
  final Color texte;

  /// Texte secondaire (`--t2`).
  final Color texte2;

  /// Texte tertiaire / libellés discrets (`--t3`).
  final Color texte3;

  /// Fond de survol des contrôles neutres (`--hover`).
  final Color survol;

  /// Fond des champs de saisie (`--win`).
  final Color champ;

  /// Bordure des champs de saisie (`--line2`).
  final Color champBordure;

  /// Couleur du mot-symbole « NovaDesk ».
  final Color logo;

  /// Fond de sélection (lignes/onglets/groupes sélectionnés — `--sel`).
  final Color selection;

  /// Vert « accès / présence » adapté au thème (`--green`).
  final Color vert;

  /// Bleu « lien / sélection / mode » adapté au thème (`--blue`).
  final Color bleu;

  /// Ambre « favori » (`--amber`).
  final Color ambre;

  /// Fonds désaturés des vignettes d'aperçu.
  final Color vignette1;
  final Color vignette2;
  final Color vignette3;

  /// Raccourci : `NovaTokens.of(context)`.
  static NovaTokens of(BuildContext context) =>
      Theme.of(context).extension<NovaTokens>()!;

  @override
  NovaTokens copyWith({
    Color? fenetre,
    Color? panneau,
    Color? panneau2,
    Color? barre,
    Color? filet,
    Color? filetFort,
    Color? texte,
    Color? texte2,
    Color? texte3,
    Color? survol,
    Color? champ,
    Color? champBordure,
    Color? logo,
    Color? selection,
    Color? vert,
    Color? bleu,
    Color? ambre,
    Color? vignette1,
    Color? vignette2,
    Color? vignette3,
  }) {
    return NovaTokens(
      fenetre: fenetre ?? this.fenetre,
      panneau: panneau ?? this.panneau,
      panneau2: panneau2 ?? this.panneau2,
      barre: barre ?? this.barre,
      filet: filet ?? this.filet,
      filetFort: filetFort ?? this.filetFort,
      texte: texte ?? this.texte,
      texte2: texte2 ?? this.texte2,
      texte3: texte3 ?? this.texte3,
      survol: survol ?? this.survol,
      champ: champ ?? this.champ,
      champBordure: champBordure ?? this.champBordure,
      logo: logo ?? this.logo,
      selection: selection ?? this.selection,
      vert: vert ?? this.vert,
      bleu: bleu ?? this.bleu,
      ambre: ambre ?? this.ambre,
      vignette1: vignette1 ?? this.vignette1,
      vignette2: vignette2 ?? this.vignette2,
      vignette3: vignette3 ?? this.vignette3,
    );
  }

  @override
  NovaTokens lerp(NovaTokens? other, double t) {
    if (other == null) return this;
    Color m(Color a, Color b) => Color.lerp(a, b, t)!;
    return NovaTokens(
      fenetre: m(fenetre, other.fenetre),
      panneau: m(panneau, other.panneau),
      panneau2: m(panneau2, other.panneau2),
      barre: m(barre, other.barre),
      filet: m(filet, other.filet),
      filetFort: m(filetFort, other.filetFort),
      texte: m(texte, other.texte),
      texte2: m(texte2, other.texte2),
      texte3: m(texte3, other.texte3),
      survol: m(survol, other.survol),
      champ: m(champ, other.champ),
      champBordure: m(champBordure, other.champBordure),
      logo: m(logo, other.logo),
      selection: m(selection, other.selection),
      vert: m(vert, other.vert),
      bleu: m(bleu, other.bleu),
      ambre: m(ambre, other.ambre),
      vignette1: m(vignette1, other.vignette1),
      vignette2: m(vignette2, other.vignette2),
      vignette3: m(vignette3, other.vignette3),
    );
  }
}

/// `ColorScheme` construit à la main (pas de `fromSeed` : un seed rouge
/// teinterait toute l'UI en rosé, à l'inverse de la cible neutre).
ColorScheme _schema(Brightness brillance, NovaTokens t) {
  if (brillance == Brightness.light) {
    return ColorScheme.light(
      primary: kNovaRouge,
      onPrimary: Colors.white,
      primaryContainer: const Color(0xFFFDEAE9),
      onPrimaryContainer: const Color(0xFF7A1712),
      secondary: t.texte2,
      onSecondary: Colors.white,
      secondaryContainer: t.survol,
      onSecondaryContainer: t.texte,
      tertiary: t.vert,
      onTertiary: Colors.white,
      error: kNovaRouge,
      onError: Colors.white,
      errorContainer: const Color(0xFFFDEAE9),
      onErrorContainer: const Color(0xFF7A1712),
      surface: t.fenetre,
      onSurface: t.texte,
      onSurfaceVariant: t.texte2,
      surfaceContainerLowest: t.fenetre,
      surfaceContainerLow: t.barre,
      surfaceContainer: t.panneau,
      surfaceContainerHigh: t.survol,
      surfaceContainerHighest: t.panneau2,
      outline: t.filetFort,
      outlineVariant: t.filet,
      shadow: Colors.black,
      inverseSurface: const Color(0xFF2A2D31),
      onInverseSurface: const Color(0xFFF1F3F5),
      inversePrimary: const Color(0xFFF9B4B0),
    );
  }
  return ColorScheme.dark(
    primary: kNovaRouge,
    onPrimary: Colors.white,
    primaryContainer: const Color(0xFF4A211E),
    onPrimaryContainer: const Color(0xFFF9B4B0),
    secondary: t.texte2,
    onSecondary: const Color(0xFF15181D),
    secondaryContainer: t.survol,
    onSecondaryContainer: t.texte,
    tertiary: t.vert,
    onTertiary: Colors.white,
    error: kNovaRouge,
    onError: Colors.white,
    errorContainer: const Color(0xFF4A211E),
    onErrorContainer: const Color(0xFFF9B4B0),
    surface: t.fenetre,
    onSurface: t.texte,
    onSurfaceVariant: t.texte2,
    surfaceContainerLowest: t.panneau,
    surfaceContainerLow: t.barre,
    surfaceContainer: t.panneau,
    surfaceContainerHigh: t.survol,
    surfaceContainerHighest: t.panneau2,
    outline: t.filetFort,
    outlineVariant: t.filet,
    shadow: Colors.black,
    inverseSurface: const Color(0xFFE6E9ED),
    onInverseSurface: const Color(0xFF181B21),
    inversePrimary: kNovaRougePresse,
  );
}

/// Thème Material 3 complet (clair ou sombre) : à plat, dense, filets 1 px,
/// rayons carrés à 4 px.
ThemeData novaTheme(Brightness brillance) {
  final tokens = brillance == Brightness.light
      ? const NovaTokens.clair()
      : const NovaTokens.sombre();
  final schema = _schema(brillance, tokens);

  const rayon = BorderRadius.all(Radius.circular(kNovaRayon));

  return ThemeData(
    useMaterial3: true,
    colorScheme: schema,
    extensions: [tokens],

    // Densité compacte + police système Windows.
    visualDensity: const VisualDensity(horizontal: -1, vertical: -1),
    materialTapTargetSize: MaterialTapTargetSize.shrinkWrap,
    fontFamily: 'Segoe UI',
    scaffoldBackgroundColor: tokens.fenetre,
    canvasColor: tokens.fenetre,
    dividerColor: tokens.filet,

    // Desktop : pas d'effet d'encre, survols discrets.
    splashFactory: NoSplash.splashFactory,
    hoverColor: tokens.survol,
    highlightColor: tokens.survol,
    focusColor: tokens.survol,

    textTheme: const TextTheme(
      headlineMedium: TextStyle(fontSize: 24, fontWeight: FontWeight.w600),
      titleLarge: TextStyle(fontSize: 16, fontWeight: FontWeight.w600),
      titleMedium: TextStyle(fontSize: 13.5, fontWeight: FontWeight.w600),
      titleSmall: TextStyle(fontSize: 12.5, fontWeight: FontWeight.w600),
      bodyLarge: TextStyle(fontSize: 14),
      bodyMedium: TextStyle(fontSize: 13),
      bodySmall: TextStyle(fontSize: 11.5),
      labelLarge: TextStyle(fontSize: 13, fontWeight: FontWeight.w600),
      labelMedium: TextStyle(fontSize: 12, fontWeight: FontWeight.w500),
      labelSmall: TextStyle(fontSize: 11, fontWeight: FontWeight.w500),
    ),

    appBarTheme: AppBarTheme(
      backgroundColor: tokens.barre,
      foregroundColor: tokens.texte,
      elevation: 0,
      scrolledUnderElevation: 0,
      centerTitle: false,
      toolbarHeight: 44,
      titleTextStyle: TextStyle(
        fontSize: 14,
        fontWeight: FontWeight.w600,
        color: tokens.texte,
        fontFamily: 'Segoe UI',
      ),
      iconTheme: IconThemeData(color: tokens.texte2, size: 18),
      shape: LinearBorder.bottom(side: BorderSide(color: tokens.filet)),
    ),

    // Cartes à plat : bordure 1 px, rayon 4, aucune ombre.
    cardTheme: CardThemeData(
      elevation: 0,
      color: tokens.fenetre,
      surfaceTintColor: Colors.transparent,
      margin: EdgeInsets.zero,
      shape: RoundedRectangleBorder(
        borderRadius: rayon,
        side: BorderSide(color: tokens.filet),
      ),
    ),

    dividerTheme: DividerThemeData(
      color: tokens.filet,
      thickness: 1,
      space: 1,
    ),

    iconTheme: IconThemeData(color: tokens.texte2, size: 18),

    // Bouton plein neutre par défaut (le rouge reste réservé au « Se connecter »).
    filledButtonTheme: FilledButtonThemeData(
      style: ButtonStyle(
        backgroundColor: WidgetStateProperty.resolveWith(
          (etats) => etats.contains(WidgetState.disabled)
              ? tokens.champBordure
              : etats.contains(WidgetState.hovered) ||
                      etats.contains(WidgetState.pressed)
                  ? tokens.texte.withValues(alpha: 0.85)
                  : tokens.texte,
        ),
        foregroundColor: WidgetStatePropertyAll(tokens.fenetre),
        overlayColor: const WidgetStatePropertyAll(Colors.transparent),
        elevation: const WidgetStatePropertyAll(0),
        minimumSize: const WidgetStatePropertyAll(Size(64, 34)),
        padding: const WidgetStatePropertyAll(
          EdgeInsets.symmetric(horizontal: 14),
        ),
        textStyle: const WidgetStatePropertyAll(
          TextStyle(
              fontSize: 12.5,
              fontWeight: FontWeight.w600,
              fontFamily: 'Segoe UI'),
        ),
        shape: const WidgetStatePropertyAll(
          RoundedRectangleBorder(borderRadius: rayon),
        ),
      ),
    ),

    // Bouton fantôme : bordure filet, texte primaire (maquette `.btn`).
    outlinedButtonTheme: OutlinedButtonThemeData(
      style: OutlinedButton.styleFrom(
        foregroundColor: tokens.texte,
        backgroundColor: tokens.fenetre,
        side: BorderSide(color: tokens.filetFort),
        minimumSize: const Size(0, 32),
        padding: const EdgeInsets.symmetric(horizontal: 12),
        textStyle: const TextStyle(
            fontSize: 12.5, fontWeight: FontWeight.w500, fontFamily: 'Segoe UI'),
        shape: const RoundedRectangleBorder(borderRadius: rayon),
      ).copyWith(
        backgroundColor: WidgetStateProperty.resolveWith(
          (etats) => etats.contains(WidgetState.hovered)
              ? tokens.survol
              : tokens.fenetre,
        ),
      ),
    ),

    textButtonTheme: TextButtonThemeData(
      style: TextButton.styleFrom(
        foregroundColor: tokens.texte2,
        minimumSize: const Size(0, 32),
        padding: const EdgeInsets.symmetric(horizontal: 10),
        textStyle: const TextStyle(
            fontSize: 12.5, fontWeight: FontWeight.w500, fontFamily: 'Segoe UI'),
        shape: const RoundedRectangleBorder(borderRadius: rayon),
      ),
    ),

    // Champs : fond « champ », bordure 1 px, focus = liseré rouge (maquette).
    inputDecorationTheme: InputDecorationTheme(
      isDense: true,
      filled: true,
      fillColor: tokens.champ,
      hoverColor: tokens.champ,
      hintStyle: TextStyle(color: tokens.texte3, fontSize: 13),
      labelStyle: TextStyle(color: tokens.texte2, fontSize: 13),
      contentPadding: const EdgeInsets.symmetric(horizontal: 11, vertical: 10),
      enabledBorder: OutlineInputBorder(
        borderRadius: rayon,
        borderSide: BorderSide(color: tokens.champBordure),
      ),
      focusedBorder: const OutlineInputBorder(
        borderRadius: rayon,
        borderSide: BorderSide(color: kNovaRouge, width: 1.4),
      ),
      border: OutlineInputBorder(
        borderRadius: rayon,
        borderSide: BorderSide(color: tokens.champBordure),
      ),
      disabledBorder: OutlineInputBorder(
        borderRadius: rayon,
        borderSide: BorderSide(color: tokens.filet),
      ),
    ),

    // Interrupteurs : vert « accès » adapté au thème.
    switchTheme: SwitchThemeData(
      trackColor: WidgetStateProperty.resolveWith(
        (etats) => etats.contains(WidgetState.selected)
            ? tokens.vert
            : tokens.filetFort,
      ),
      thumbColor: const WidgetStatePropertyAll(Colors.white),
      trackOutlineColor: const WidgetStatePropertyAll(Colors.transparent),
      trackOutlineWidth: const WidgetStatePropertyAll(0),
    ),
    checkboxTheme: CheckboxThemeData(
      fillColor: WidgetStateProperty.resolveWith(
        (etats) => etats.contains(WidgetState.selected)
            ? tokens.vert
            : Colors.transparent,
      ),
      checkColor: const WidgetStatePropertyAll(Colors.white),
      side: BorderSide(color: tokens.champBordure, width: 1.4),
      shape: const RoundedRectangleBorder(
        borderRadius: BorderRadius.all(Radius.circular(3)),
      ),
    ),
    radioTheme: RadioThemeData(
      fillColor: WidgetStateProperty.resolveWith(
        (etats) => etats.contains(WidgetState.selected)
            ? tokens.vert
            : tokens.champBordure,
      ),
    ),

    popupMenuTheme: PopupMenuThemeData(
      color: tokens.fenetre,
      surfaceTintColor: Colors.transparent,
      elevation: 8,
      shadowColor: Colors.black.withValues(alpha: 0.22),
      textStyle: TextStyle(
          fontSize: 12.5, color: tokens.texte, fontFamily: 'Segoe UI'),
      labelTextStyle: WidgetStatePropertyAll(
        TextStyle(fontSize: 12.5, color: tokens.texte, fontFamily: 'Segoe UI'),
      ),
      shape: RoundedRectangleBorder(
        borderRadius: const BorderRadius.all(Radius.circular(6)),
        side: BorderSide(color: tokens.filetFort),
      ),
    ),
    menuTheme: MenuThemeData(
      style: MenuStyle(
        backgroundColor: WidgetStatePropertyAll(tokens.fenetre),
        surfaceTintColor: const WidgetStatePropertyAll(Colors.transparent),
        elevation: const WidgetStatePropertyAll(8),
        shape: WidgetStatePropertyAll(
          RoundedRectangleBorder(
            borderRadius: const BorderRadius.all(Radius.circular(6)),
            side: BorderSide(color: tokens.filetFort),
          ),
        ),
      ),
    ),

    dialogTheme: DialogThemeData(
      backgroundColor: tokens.fenetre,
      surfaceTintColor: Colors.transparent,
      elevation: 10,
      shadowColor: Colors.black.withValues(alpha: 0.4),
      shape: RoundedRectangleBorder(
        borderRadius: const BorderRadius.all(Radius.circular(8)),
        side: BorderSide(color: tokens.filetFort),
      ),
      titleTextStyle: TextStyle(
        fontSize: 15,
        fontWeight: FontWeight.w600,
        color: tokens.texte,
        fontFamily: 'Segoe UI',
      ),
    ),

    tabBarTheme: TabBarThemeData(
      labelColor: tokens.texte,
      unselectedLabelColor: tokens.texte2,
      indicatorColor: kNovaRouge,
      indicatorSize: TabBarIndicatorSize.label,
      dividerColor: tokens.filet,
      dividerHeight: 1,
      overlayColor: WidgetStatePropertyAll(tokens.survol),
      labelStyle: const TextStyle(
          fontSize: 12.5, fontWeight: FontWeight.w600, fontFamily: 'Segoe UI'),
      unselectedLabelStyle: const TextStyle(
          fontSize: 12.5, fontWeight: FontWeight.w400, fontFamily: 'Segoe UI'),
    ),

    segmentedButtonTheme: SegmentedButtonThemeData(
      style: ButtonStyle(
        backgroundColor: WidgetStateProperty.resolveWith(
          (etats) => etats.contains(WidgetState.selected)
              ? kNovaRouge
              : tokens.champ,
        ),
        foregroundColor: WidgetStateProperty.resolveWith(
          (etats) => etats.contains(WidgetState.selected)
              ? Colors.white
              : tokens.texte2,
        ),
        side: WidgetStatePropertyAll(BorderSide(color: tokens.filetFort)),
        textStyle: const WidgetStatePropertyAll(
          TextStyle(
              fontSize: 12, fontWeight: FontWeight.w500, fontFamily: 'Segoe UI'),
        ),
        visualDensity: const VisualDensity(horizontal: -2, vertical: -2),
        shape: const WidgetStatePropertyAll(
          RoundedRectangleBorder(borderRadius: rayon),
        ),
      ),
    ),

    listTileTheme: ListTileThemeData(
      dense: true,
      iconColor: tokens.texte2,
      textColor: tokens.texte,
      titleTextStyle: TextStyle(
          fontSize: 13,
          fontWeight: FontWeight.w500,
          color: tokens.texte,
          fontFamily: 'Segoe UI'),
      subtitleTextStyle: TextStyle(
          fontSize: 11.5, color: tokens.texte3, fontFamily: 'Segoe UI'),
      contentPadding: const EdgeInsets.symmetric(horizontal: 16),
    ),

    tooltipTheme: TooltipThemeData(
      waitDuration: const Duration(milliseconds: 350),
      decoration: BoxDecoration(
        color: const Color(0xFF05070A),
        borderRadius: BorderRadius.circular(3),
      ),
      textStyle: const TextStyle(
          color: Colors.white,
          fontSize: 10.5,
          fontWeight: FontWeight.w500,
          fontFamily: 'Segoe UI'),
      padding: const EdgeInsets.symmetric(horizontal: 7, vertical: 3),
    ),

    snackBarTheme: SnackBarThemeData(
      behavior: SnackBarBehavior.floating,
      backgroundColor: brillance == Brightness.light
          ? const Color(0xFF2A2D31)
          : const Color(0xFF33373C),
      contentTextStyle: const TextStyle(
          color: Colors.white, fontSize: 12.5, fontFamily: 'Segoe UI'),
      shape: RoundedRectangleBorder(borderRadius: BorderRadius.circular(6)),
    ),

    progressIndicatorTheme: ProgressIndicatorThemeData(
      color: tokens.vert,
      linearTrackColor: tokens.filetFort,
      circularTrackColor: tokens.champ,
    ),

    dropdownMenuTheme: DropdownMenuThemeData(
      textStyle:
          TextStyle(fontSize: 13, color: tokens.texte, fontFamily: 'Segoe UI'),
    ),

    scrollbarTheme: ScrollbarThemeData(
      thumbColor: WidgetStatePropertyAll(
        tokens.filetFort.withValues(alpha: 0.9),
      ),
      radius: const Radius.circular(6),
      thickness: const WidgetStatePropertyAll(6),
    ),

    expansionTileTheme: ExpansionTileThemeData(
      iconColor: tokens.texte2,
      collapsedIconColor: tokens.texte3,
      textColor: tokens.texte,
      collapsedTextColor: tokens.texte,
      shape: const Border(),
      collapsedShape: const Border(),
    ),
  );
}
