/// Thème NovaDesk « parité AnyDesk » — tokens issus de la maquette validée
/// (`anydesk-reference.html`) : surfaces neutres à plat, filets 1px, densité
/// compacte, et rouge de marque en accent **strictement parcimonieux**.
///
/// Le rouge `#EF443B` est réservé à : logo, bouton « Se connecter », onglet
/// actif, survol du bouton fermer (+ avatar de pair en session, comme sur la
/// maquette). Tout le reste est neutre (clair `#FFFFFF`/`#F5F6F8`, sombre
/// `#1D1F23`/`#17181B`).
library;

import 'package:flutter/material.dart';

/// Rouge AnyDesk (couleur de marque officielle).
const Color kNovaRouge = Color(0xFFEF443B);

/// Rouge pressé / survol du bouton principal.
const Color kNovaRougePresse = Color(0xFFD83A32);

/// Vert « en ligne / accès accordé » (pastilles, interrupteurs).
const Color kNovaVert = Color(0xFF2FAE60);

/// Jeu de couleurs hors `ColorScheme` (filets, champs, barre de titre,
/// vignettes…), calqué 1:1 sur les variables CSS de la maquette.
@immutable
class NovaTokens extends ThemeExtension<NovaTokens> {
  const NovaTokens({
    required this.fenetre,
    required this.panneau,
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
    required this.vignette1,
    required this.vignette2,
    required this.vignette3,
  });

  /// Clair — `:root[data-theme="light"]` de la maquette.
  const NovaTokens.clair()
      : this(
          fenetre: const Color(0xFFFFFFFF),
          panneau: const Color(0xFFF5F6F8),
          barre: const Color(0xFFFBFBFC),
          filet: const Color(0xFFE7E9EC),
          filetFort: const Color(0xFFDADDE1),
          texte: const Color(0xFF15181D),
          texte2: const Color(0xFF565C64),
          texte3: const Color(0xFF9AA0A7),
          survol: const Color(0xFFF1F3F5),
          champ: const Color(0xFFF3F5F7),
          champBordure: const Color(0xFFDCE0E4),
          logo: const Color(0xFF22262C),
          vignette1: const Color(0xFF59606B),
          vignette2: const Color(0xFF4A5560),
          vignette3: const Color(0xFF5B5560),
        );

  /// Sombre — `:root[data-theme="dark"]` de la maquette.
  const NovaTokens.sombre()
      : this(
          fenetre: const Color(0xFF1D1F23),
          panneau: const Color(0xFF17181B),
          barre: const Color(0xFF191A1D),
          filet: const Color(0xFF2C2F34),
          filetFort: const Color(0xFF34383D),
          texte: const Color(0xFFE7E9EC),
          texte2: const Color(0xFFA0A6AD),
          texte3: const Color(0xFF6B7178),
          survol: const Color(0xFF26292E),
          champ: const Color(0xFF25282D),
          champBordure: const Color(0xFF33373C),
          logo: const Color(0xFFE7E9EC),
          vignette1: const Color(0xFF3A4149),
          vignette2: const Color(0xFF333B44),
          vignette3: const Color(0xFF3D3942),
        );

  /// Fond de fenêtre / panneaux principaux (`--win`).
  final Color fenetre;

  /// Fond secondaire (`--sub`).
  final Color panneau;

  /// Fond des barres de titre et d'état (`--bar`).
  final Color barre;

  /// Filet séparateur 1px (`--line`).
  final Color filet;

  /// Filet appuyé — bordures de fenêtre, survol de vignette (`--line-strong`).
  final Color filetFort;

  /// Texte primaire (`--txt`).
  final Color texte;

  /// Texte secondaire (`--txt2`).
  final Color texte2;

  /// Texte tertiaire / libellés discrets (`--txt3`).
  final Color texte3;

  /// Fond de survol des contrôles neutres (`--hover`).
  final Color survol;

  /// Fond des champs de saisie (`--field`).
  final Color champ;

  /// Bordure des champs de saisie (`--field-bd`).
  final Color champBordure;

  /// Couleur du mot-symbole « NovaDesk » (`--logo`).
  final Color logo;

  /// Fonds désaturés des vignettes d'aperçu (`--thumb1..3`).
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
    Color? vignette1,
    Color? vignette2,
    Color? vignette3,
  }) {
    return NovaTokens(
      fenetre: fenetre ?? this.fenetre,
      panneau: panneau ?? this.panneau,
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
      vignette1: vignette1 ?? this.vignette1,
      vignette2: vignette2 ?? this.vignette2,
      vignette3: vignette3 ?? this.vignette3,
    );
  }

  @override
  NovaTokens lerp(NovaTokens? other, double t) {
    if (other == null) return this;
    Color melanger(Color a, Color b) => Color.lerp(a, b, t)!;
    return NovaTokens(
      fenetre: melanger(fenetre, other.fenetre),
      panneau: melanger(panneau, other.panneau),
      barre: melanger(barre, other.barre),
      filet: melanger(filet, other.filet),
      filetFort: melanger(filetFort, other.filetFort),
      texte: melanger(texte, other.texte),
      texte2: melanger(texte2, other.texte2),
      texte3: melanger(texte3, other.texte3),
      survol: melanger(survol, other.survol),
      champ: melanger(champ, other.champ),
      champBordure: melanger(champBordure, other.champBordure),
      logo: melanger(logo, other.logo),
      vignette1: melanger(vignette1, other.vignette1),
      vignette2: melanger(vignette2, other.vignette2),
      vignette3: melanger(vignette3, other.vignette3),
    );
  }
}

/// `ColorScheme` **construit à la main** (pas de `fromSeed` : un seed rouge
/// teinterait toute l'UI en rosé, à l'inverse de la cible neutre — doc 03 §1.1).
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
      tertiary: kNovaVert,
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
      surfaceContainerHighest: t.champ,
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
    tertiary: kNovaVert,
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
    surfaceContainerHighest: t.champ,
    outline: t.filetFort,
    outlineVariant: t.filet,
    shadow: Colors.black,
    inverseSurface: const Color(0xFFE7E9EC),
    onInverseSurface: const Color(0xFF1D1F23),
    inversePrimary: kNovaRougePresse,
  );
}

/// Thème Material 3 complet (clair ou sombre) : à plat, dense, filets 1px.
ThemeData novaTheme(Brightness brillance) {
  final tokens = brillance == Brightness.light
      ? const NovaTokens.clair()
      : const NovaTokens.sombre();
  final schema = _schema(brillance, tokens);

  const rayonChamp = BorderRadius.all(Radius.circular(8));

  return ThemeData(
    useMaterial3: true,
    colorScheme: schema,
    extensions: [tokens],

    // Densité compacte façon AnyDesk (doc 03 §1.3) + police système Windows.
    visualDensity: const VisualDensity(horizontal: -1, vertical: -1),
    materialTapTargetSize: MaterialTapTargetSize.shrinkWrap,
    fontFamily: 'Segoe UI',
    scaffoldBackgroundColor: tokens.fenetre,
    canvasColor: tokens.fenetre,
    dividerColor: tokens.filet,

    // Desktop : pas d'effet d'encre « ripple », survols discrets.
    splashFactory: NoSplash.splashFactory,
    hoverColor: tokens.survol,
    highlightColor: tokens.survol,
    focusColor: tokens.survol,

    // Typographie compacte (~13px de corps, titres 600).
    textTheme: const TextTheme(
      headlineMedium: TextStyle(fontSize: 29, fontWeight: FontWeight.w700),
      titleLarge: TextStyle(fontSize: 16, fontWeight: FontWeight.w700),
      titleMedium: TextStyle(fontSize: 14, fontWeight: FontWeight.w600),
      titleSmall: TextStyle(fontSize: 12.5, fontWeight: FontWeight.w600),
      bodyLarge: TextStyle(fontSize: 14),
      bodyMedium: TextStyle(fontSize: 13),
      bodySmall: TextStyle(fontSize: 11.5),
      labelLarge: TextStyle(fontSize: 13, fontWeight: FontWeight.w600),
      labelMedium: TextStyle(fontSize: 12, fontWeight: FontWeight.w500),
      labelSmall: TextStyle(fontSize: 11, fontWeight: FontWeight.w500),
    ),

    // Barres plates : fond « barre », filet bas 1px, aucune ombre.
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

    // Cartes À PLAT : bordure 1px, rayon 9, AUCUNE ombre portée.
    cardTheme: CardThemeData(
      elevation: 0,
      color: tokens.fenetre,
      surfaceTintColor: Colors.transparent,
      margin: EdgeInsets.zero,
      shape: RoundedRectangleBorder(
        borderRadius: const BorderRadius.all(Radius.circular(9)),
        side: BorderSide(color: tokens.filet),
      ),
    ),

    dividerTheme: DividerThemeData(
      color: tokens.filet,
      thickness: 1,
      space: 1,
    ),

    iconTheme: IconThemeData(color: tokens.texte2, size: 18),

    // Bouton plein par défaut : NEUTRE (anthracite / clair inversé) — le
    // rouge de marque reste réservé au bouton « Se connecter », stylé
    // explicitement sur l'accueil.
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
        minimumSize: const WidgetStatePropertyAll(Size(64, 38)),
        padding: const WidgetStatePropertyAll(
          EdgeInsets.symmetric(horizontal: 16),
        ),
        textStyle: const WidgetStatePropertyAll(
          TextStyle(
              fontSize: 13.5, fontWeight: FontWeight.w600,
              fontFamily: 'Segoe UI'),
        ),
        shape: const WidgetStatePropertyAll(
          RoundedRectangleBorder(borderRadius: rayonChamp),
        ),
      ),
    ),

    // Bouton « fantôme » : bordure filet, texte secondaire (maquette .ghost).
    outlinedButtonTheme: OutlinedButtonThemeData(
      style: OutlinedButton.styleFrom(
        foregroundColor: tokens.texte2,
        side: BorderSide(color: tokens.filet),
        minimumSize: const Size(0, 32),
        padding: const EdgeInsets.symmetric(horizontal: 12),
        textStyle: const TextStyle(
            fontSize: 12, fontWeight: FontWeight.w500,
            fontFamily: 'Segoe UI'),
        shape: const RoundedRectangleBorder(
          borderRadius: BorderRadius.all(Radius.circular(7)),
        ),
      ).copyWith(
        backgroundColor: WidgetStateProperty.resolveWith(
          (etats) => etats.contains(WidgetState.hovered)
              ? tokens.survol
              : Colors.transparent,
        ),
      ),
    ),

    textButtonTheme: TextButtonThemeData(
      style: TextButton.styleFrom(
        foregroundColor: tokens.texte2,
        minimumSize: const Size(0, 32),
        padding: const EdgeInsets.symmetric(horizontal: 10),
        textStyle: const TextStyle(
            fontSize: 12.5, fontWeight: FontWeight.w500,
            fontFamily: 'Segoe UI'),
        shape: const RoundedRectangleBorder(
          borderRadius: BorderRadius.all(Radius.circular(6)),
        ),
      ),
    ),

    // Champs : fond « field », bordure 1px, focus = liseré rouge (maquette).
    inputDecorationTheme: InputDecorationTheme(
      isDense: true,
      filled: true,
      fillColor: tokens.champ,
      hoverColor: tokens.champ,
      hintStyle: TextStyle(color: tokens.texte3, fontSize: 13),
      labelStyle: TextStyle(color: tokens.texte2, fontSize: 13),
      contentPadding: const EdgeInsets.symmetric(horizontal: 12, vertical: 11),
      enabledBorder: OutlineInputBorder(
        borderRadius: rayonChamp,
        borderSide: BorderSide(color: tokens.champBordure),
      ),
      focusedBorder: const OutlineInputBorder(
        borderRadius: rayonChamp,
        borderSide: BorderSide(color: kNovaRouge, width: 1.4),
      ),
      border: OutlineInputBorder(
        borderRadius: rayonChamp,
        borderSide: BorderSide(color: tokens.champBordure),
      ),
      disabledBorder: OutlineInputBorder(
        borderRadius: rayonChamp,
        borderSide: BorderSide(color: tokens.filet),
      ),
    ),

    // Interrupteurs : vert « accès » (le rouge reste réservé).
    switchTheme: SwitchThemeData(
      trackColor: WidgetStateProperty.resolveWith(
        (etats) => etats.contains(WidgetState.selected)
            ? kNovaVert
            : tokens.champBordure,
      ),
      thumbColor: const WidgetStatePropertyAll(Colors.white),
      trackOutlineColor: const WidgetStatePropertyAll(Colors.transparent),
      trackOutlineWidth: const WidgetStatePropertyAll(0),
    ),
    checkboxTheme: CheckboxThemeData(
      fillColor: WidgetStateProperty.resolveWith(
        (etats) => etats.contains(WidgetState.selected)
            ? kNovaVert
            : Colors.transparent,
      ),
      checkColor: const WidgetStatePropertyAll(Colors.white),
      side: BorderSide(color: tokens.champBordure, width: 1.4),
      shape: const RoundedRectangleBorder(
        borderRadius: BorderRadius.all(Radius.circular(4)),
      ),
    ),
    radioTheme: RadioThemeData(
      fillColor: WidgetStateProperty.resolveWith(
        (etats) => etats.contains(WidgetState.selected)
            ? kNovaVert
            : tokens.champBordure,
      ),
    ),

    // Menus contextuels : panneau net, filet 1px, ombre courte.
    popupMenuTheme: PopupMenuThemeData(
      color: tokens.fenetre,
      surfaceTintColor: Colors.transparent,
      elevation: 6,
      shadowColor: Colors.black.withValues(alpha: 0.25),
      textStyle: TextStyle(
          fontSize: 12.5, color: tokens.texte, fontFamily: 'Segoe UI'),
      labelTextStyle: WidgetStatePropertyAll(
        TextStyle(fontSize: 12.5, color: tokens.texte,
            fontFamily: 'Segoe UI'),
      ),
      shape: RoundedRectangleBorder(
        borderRadius: const BorderRadius.all(Radius.circular(8)),
        side: BorderSide(color: tokens.filet),
      ),
    ),
    menuTheme: MenuThemeData(
      style: MenuStyle(
        backgroundColor: WidgetStatePropertyAll(tokens.fenetre),
        surfaceTintColor: const WidgetStatePropertyAll(Colors.transparent),
        elevation: const WidgetStatePropertyAll(6),
        shape: WidgetStatePropertyAll(
          RoundedRectangleBorder(
            borderRadius: const BorderRadius.all(Radius.circular(8)),
            side: BorderSide(color: tokens.filet),
          ),
        ),
      ),
    ),

    dialogTheme: DialogThemeData(
      backgroundColor: tokens.fenetre,
      surfaceTintColor: Colors.transparent,
      elevation: 10,
      shadowColor: Colors.black.withValues(alpha: 0.35),
      shape: RoundedRectangleBorder(
        borderRadius: const BorderRadius.all(Radius.circular(10)),
        side: BorderSide(color: tokens.filetFort),
      ),
      titleTextStyle: TextStyle(
        fontSize: 15,
        fontWeight: FontWeight.w700,
        color: tokens.texte,
        fontFamily: 'Segoe UI',
      ),
    ),

    // Onglets : indicateur rouge 2px (usage autorisé : onglet actif).
    tabBarTheme: TabBarThemeData(
      labelColor: tokens.texte,
      unselectedLabelColor: tokens.texte2,
      indicatorColor: kNovaRouge,
      indicatorSize: TabBarIndicatorSize.label,
      dividerColor: tokens.filet,
      dividerHeight: 1,
      overlayColor: WidgetStatePropertyAll(tokens.survol),
      labelStyle: const TextStyle(
          fontSize: 12.5, fontWeight: FontWeight.w600,
          fontFamily: 'Segoe UI'),
      unselectedLabelStyle: const TextStyle(
          fontSize: 12.5, fontWeight: FontWeight.w400,
          fontFamily: 'Segoe UI'),
    ),

    segmentedButtonTheme: SegmentedButtonThemeData(
      style: ButtonStyle(
        backgroundColor: WidgetStateProperty.resolveWith(
          (etats) => etats.contains(WidgetState.selected)
              ? tokens.fenetre
              : tokens.champ,
        ),
        foregroundColor: WidgetStateProperty.resolveWith(
          (etats) => etats.contains(WidgetState.selected)
              ? tokens.texte
              : tokens.texte2,
        ),
        side: WidgetStatePropertyAll(BorderSide(color: tokens.filet)),
        textStyle: const WidgetStatePropertyAll(
          TextStyle(fontSize: 12, fontWeight: FontWeight.w600,
              fontFamily: 'Segoe UI'),
        ),
        visualDensity: const VisualDensity(horizontal: -2, vertical: -2),
        shape: const WidgetStatePropertyAll(
          RoundedRectangleBorder(
            borderRadius: BorderRadius.all(Radius.circular(7)),
          ),
        ),
      ),
    ),

    listTileTheme: ListTileThemeData(
      dense: true,
      iconColor: tokens.texte2,
      textColor: tokens.texte,
      titleTextStyle: TextStyle(
          fontSize: 13, fontWeight: FontWeight.w500, color: tokens.texte,
          fontFamily: 'Segoe UI'),
      subtitleTextStyle: TextStyle(fontSize: 11.5, color: tokens.texte3,
          fontFamily: 'Segoe UI'),
      contentPadding: const EdgeInsets.symmetric(horizontal: 16),
    ),

    tooltipTheme: TooltipThemeData(
      waitDuration: const Duration(milliseconds: 350),
      decoration: BoxDecoration(
        color: const Color(0xFF0C0D10),
        borderRadius: BorderRadius.circular(5),
      ),
      textStyle: const TextStyle(color: Colors.white, fontSize: 11,
          fontFamily: 'Segoe UI'),
      padding: const EdgeInsets.symmetric(horizontal: 8, vertical: 4),
    ),

    snackBarTheme: SnackBarThemeData(
      behavior: SnackBarBehavior.floating,
      backgroundColor: brillance == Brightness.light
          ? const Color(0xFF2A2D31)
          : const Color(0xFF33373C),
      contentTextStyle: const TextStyle(
          color: Colors.white, fontSize: 12.5, fontFamily: 'Segoe UI'),
      shape: RoundedRectangleBorder(borderRadius: BorderRadius.circular(8)),
    ),

    progressIndicatorTheme: ProgressIndicatorThemeData(
      color: kNovaVert,
      linearTrackColor: tokens.champ,
      circularTrackColor: tokens.champ,
    ),

    dropdownMenuTheme: DropdownMenuThemeData(
      textStyle: TextStyle(fontSize: 13, color: tokens.texte,
          fontFamily: 'Segoe UI'),
    ),

    scrollbarTheme: ScrollbarThemeData(
      thumbColor: WidgetStatePropertyAll(
        tokens.texte3.withValues(alpha: 0.45),
      ),
      radius: const Radius.circular(4),
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
