/// NovaDesk — point d'entrée de l'application Flutter (plan 10).
///
/// L'UI est « mince » : présentation, collecte d'intentions, composition de
/// la surface vidéo. Toute la logique réseau/crypto/média vit dans le cœur
/// Rust, atteint via la façade `nd-ffi` (voir `lib/bridge/`).
library;

import 'dart:async';
import 'dart:io' show Platform;
import 'dart:ui' show AppExitResponse;

import 'package:flutter/foundation.dart' show kIsWeb;
import 'package:flutter/material.dart';
import 'package:flutter_localizations/flutter_localizations.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
// ignore: invalid_use_of_internal_member
import 'package:flutter_rust_bridge/flutter_rust_bridge_for_generated.dart'
    show ExternalLibrary;

import 'app_routes.dart';
import 'bridge/frb_api.dart';
import 'bridge/generated/frb_generated.dart';
import 'bridge/mock_api.dart';
import 'bridge/native_api.dart';
import 'platform/window_shim.dart';
import 'screens/address_book_screen.dart';
import 'screens/home_screen.dart';
import 'screens/recordings_screen.dart';
import 'screens/session_screen.dart';
import 'screens/settings_screen.dart';
import 'screens/unattended_screen.dart';
import 'state/providers.dart';
import 'theme/motion.dart';
import 'theme/nova_theme.dart';
import 'widgets/app_frame.dart';
import 'widgets/nova_kit.dart';

Future<void> main() async {
  WidgetsFlutterBinding.ensureInitialized();

  // NOTE : une fois le binding flutter_rust_bridge généré (lib/bridge/README.md),
  // initialiser ici le runtime Rust : `await RustLib.init();`.

  // Fenêtrage desktop : taille initiale/minimale et titre.
  if (!kIsWeb && (Platform.isWindows || Platform.isMacOS || Platform.isLinux)) {
    await windowManager.ensureInitialized();
    final options = WindowOptions(
      size: const Size(1120, 720),
      minimumSize: const Size(880, 560),
      center: true,
      title: 'NovaDesk',
    );
    unawaited(windowManager.waitUntilReadyToShow(options, () async {
      await windowManager.show();
      await windowManager.focus();
    }));
  }

  // Cœur Rust : chargement de la bibliothèque native `nd-ffi` et branchement de
  // la façade réelle. Si la DLL est absente (build sans étape native), on
  // retombe proprement sur le mock pour garder l'UI entièrement navigable.
  NativeApi api = MockNativeApi();
  if (!kIsWeb) {
    try {
      await RustLib.init(
        externalLibrary: ExternalLibrary.open(_nomBibliothequeNative()),
      );
      api = const FrbNativeApi();
      // Appel réel au cœur (preuve du round-trip FFI au démarrage).
      final info = await api.appInfo();
      debugPrint('Cœur Rust nd-ffi chargé — moteur v${info.version}.');
    } catch (e) {
      debugPrint('Cœur Rust indisponible ($e) — bascule sur la façade mock.');
    }
  }

  // Thème persistant : relu depuis les réglages (`theme`) pour appliquer le bon
  // mode dès le premier frame, sans clignotement.
  ThemeMode themeInitial = ThemeMode.system;
  try {
    themeInitial = themeDepuisReglage(await api.getSetting(cle: 'theme'));
  } catch (_) {
    // Réglage indisponible : reste sur « système ».
  }

  // Identité locale persistante : résolue une fois pour renseigner l'ID local
  // synchrone (les écrans lisent aussi [localIdentityProvider] de façon réactive).
  final overrides = <Override>[
    nativeApiProvider.overrideWithValue(api),
    themeModeProvider.overrideWith((ref) => themeInitial),
  ];
  try {
    final identite = await api.localIdentity();
    overrides.add(idLocalProvider.overrideWithValue(identite.id));
  } catch (_) {
    // Identité indisponible : le provider dérive de localIdentityProvider.
  }

  runApp(
    ProviderScope(
      overrides: overrides,
      child: const NovaDeskApp(),
    ),
  );
}

/// Nom de la bibliothèque dynamique `nd-ffi` selon la plateforme (recherchée
/// dans le répertoire de l'exécutable).
String _nomBibliothequeNative() {
  if (Platform.isWindows) return 'nd_ffi.dll';
  if (Platform.isMacOS) return 'libnd_ffi.dylib';
  return 'libnd_ffi.so';
}

/// Racine de l'application : thèmes clair/sombre « parité AnyDesk »
/// (`theme/nova_theme.dart` — rouge #EF443B en accent parcimonieux, surfaces
/// neutres à plat), localisation française, table des routes.
class NovaDeskApp extends ConsumerWidget {
  const NovaDeskApp({super.key});

  @override
  Widget build(BuildContext context, WidgetRef ref) {
    final modeTheme = ref.watch(themeModeProvider);
    return MaterialApp(
      title: 'NovaDesk',
      // Navigateur racine adressable globalement : les services applicatifs
      // (hôte non surveillé) y ouvrent le dialogue d'acceptation au-dessus de
      // n'importe quel écran, session comprise.
      navigatorKey: cleNavigateurGlobale,
      debugShowCheckedModeBanner: false,
      theme: novaTheme(Brightness.light),
      darkTheme: novaTheme(Brightness.dark),
      themeMode: modeTheme,

      // Langue v1 : français (catalogues ARB multilingues à venir,
      // plan 10 §10.7.2).
      locale: const Locale('fr'),
      supportedLocales: const [Locale('fr')],
      localizationsDelegates: const [
        GlobalMaterialLocalizations.delegate,
        GlobalWidgetsLocalizations.delegate,
        GlobalCupertinoLocalizations.delegate,
      ],

      initialRoute: NovaRoutes.accueil,
      onGenerateRoute: _genererRoute,
    );
  }

  Route<dynamic> _genererRoute(RouteSettings parametres) {
    if (parametres.name == NovaRoutes.session) {
      final arguments = parametres.arguments;
      if (arguments is! SessionScreenArgs) {
        // Une session ne s'ouvre jamais sans configuration validée : on
        // retombe proprement sur la coquille.
        return _routeCoquille(parametres);
      }
      // Ouverture de la session : fondu + léger zoom (retour symétrique).
      return PageRouteBuilder<void>(
        settings: parametres,
        transitionDuration: NovaMotion.session,
        reverseTransitionDuration: NovaMotion.session,
        pageBuilder: (context, animation, secondaryAnimation) =>
            SessionScreen(args: arguments),
        transitionsBuilder: (context, animation, secondaryAnimation, child) {
          if (NovaMotion.animationsReduites(context)) return child;
          final courbe = CurvedAnimation(
            parent: animation,
            curve: NovaMotion.sessionCourbe,
            reverseCurve: NovaMotion.sessionCourbe.flipped,
          );
          return FadeTransition(
            opacity: courbe,
            child: ScaleTransition(
              scale: Tween<double>(begin: NovaMotion.sessionZoomInitial, end: 1)
                  .animate(courbe),
              child: child,
            ),
          );
        },
      );
    }
    // Accueil (et repli de sécurité pour toute autre route nommée) : la coquille
    // persistante gère les cinq sections en interne, sans transition de page.
    return _routeCoquille(parametres);
  }

  /// Route de base : la coquille persistante, sans transition propre (l'animation
  /// des sections vit dans la coquille elle-même).
  Route<dynamic> _routeCoquille(RouteSettings parametres) {
    return PageRouteBuilder<void>(
      settings: parametres,
      transitionDuration: Duration.zero,
      reverseTransitionDuration: Duration.zero,
      pageBuilder: (context, animation, secondaryAnimation) =>
          const NovaCoquille(),
    );
  }
}

// ---------------------------------------------------------------------------
// Coquille persistante : barre de titre + rail + barre d'état conservés ; seul
// le contenu des cinq sections change (IndexedStack animé d'un fondu doux).
// ---------------------------------------------------------------------------

/// Habillage racine des sections principales. Le rail et l'onglet « Accueil »
/// n'empilent plus de routes : ils modifient [sectionCouranteProvider], et le
/// contenu bascule en place via [_ContenuSections].
///
/// La coquille porte aussi les responsabilités **de niveau application** de
/// l'hôte non surveillé : démarrage automatique au lancement quand un mot de
/// passe permanent est configuré (parité AnyDesk), toasts des erreurs de fond
/// et arrêt best-effort à la sortie — voir [hoteNonSurveilleProvider].
class NovaCoquille extends ConsumerStatefulWidget {
  const NovaCoquille({super.key});

  @override
  ConsumerState<NovaCoquille> createState() => _NovaCoquilleState();
}

class _NovaCoquilleState extends ConsumerState<NovaCoquille> {
  /// Écouteur du cycle de vie : tente un arrêt propre de l'hôte quand la
  /// sortie de l'application est demandée. Best-effort — l'hôte vit dans le
  /// même processus et disparaît de toute façon avec lui.
  late final AppLifecycleListener _cycleDeVie;

  @override
  void initState() {
    super.initState();
    _cycleDeVie = AppLifecycleListener(onExitRequested: _surDemandeDeSortie);
    // Démarrage automatique de l'hôte non surveillé : dès le lancement si un
    // mot de passe permanent est configuré — recevoir ne dépend plus de
    // l'ouverture de l'onglet « Non surveillé ».
    WidgetsBinding.instance.addPostFrameCallback((_) {
      if (mounted) unawaited(_demarrageAutomatiqueHote());
    });
  }

  @override
  void dispose() {
    _cycleDeVie.dispose();
    super.dispose();
  }

  Future<AppExitResponse> _surDemandeDeSortie() async {
    try {
      await ref.read(hoteNonSurveilleProvider.notifier).desactiver();
    } catch (_) {
      // Ne bloque jamais la sortie.
    }
    return AppExitResponse.exit;
  }

  Future<void> _demarrageAutomatiqueHote() async {
    final active =
        await ref.read(hoteNonSurveilleProvider.notifier).activerSiMotDePasse();
    if (active && mounted) {
      NovaToast.montrer(context, 'Accès non surveillé activé automatiquement');
    }
  }

  @override
  Widget build(BuildContext context) {
    // Erreurs de fond de l'hôte non surveillé (flux des demandes
    // interrompu…) : toast global, visible quelle que soit la section.
    ref.listen<EtatHoteNonSurveille>(hoteNonSurveilleProvider, (avant, apres) {
      final message = apres.derniereErreur;
      if (message != null &&
          apres.erreurCompteur != (avant?.erreurCompteur ?? 0)) {
        NovaToast.montrer(context, message, info: true);
      }
    });
    final vue = ref.watch(sectionCouranteProvider);
    return Scaffold(
      body: NovaAppFrame(
        vue: vue,
        corps: _ContenuSections(index: _indexSection(vue)),
      ),
    );
  }
}

/// Index de section dans l'IndexedStack (l'ordre suit le rail).
int _indexSection(NovaVue vue) => switch (vue) {
      NovaVue.accueil => 0,
      NovaVue.carnet => 1,
      NovaVue.enregistrements => 2,
      NovaVue.nonSurveille => 3,
      NovaVue.reglages => 4,
      NovaVue.session => 0,
    };

/// Écran de la section [index].
Widget _sectionPour(int index) => switch (index) {
      0 => const HomeScreen(),
      1 => const AddressBookScreen(),
      2 => const RecordingsScreen(),
      3 => const UnattendedScreen(),
      _ => const SettingsScreen(),
    };

/// Contenu des sections : un [IndexedStack] **persistant** (chaque section
/// conserve son état — défilement, saisies, squelette de chargement joué une
/// seule fois) que l'on **rejoue** en fondu + léger glissement à chaque
/// changement d'index.
///
/// Choix délibéré d'un contrôleur explicite plutôt qu'un `AnimatedSwitcher`
/// classique : en changeant la clé de l'IndexedStack, ce dernier recréerait tout
/// le sous-arbre à chaque bascule et **réinitialiserait** l'état des sections
/// (défilements perdus, squelette rejoué). Ici l'IndexedStack reste unique — les
/// états sont donc préservés — et seule la fine couche fondu/glissement est
/// réanimée : un « fade-through » sobre, adapté aux destinations de rail.
///
/// Les sections sont construites **paresseusement** (une section jamais visitée
/// ne coûte rien) puis conservées. Le réglage « animations réduites » supprime
/// la transition.
class _ContenuSections extends StatefulWidget {
  const _ContenuSections({required this.index});

  final int index;

  @override
  State<_ContenuSections> createState() => _ContenuSectionsState();
}

class _ContenuSectionsState extends State<_ContenuSections>
    with SingleTickerProviderStateMixin {
  late final AnimationController _controleur = AnimationController(
    vsync: this,
    duration: NovaMotion.sections,
    value: 1,
  );
  late final CurvedAnimation _courbe =
      CurvedAnimation(parent: _controleur, curve: NovaMotion.sectionsCourbe);

  /// Sections déjà visitées : construites une fois, puis maintenues en vie.
  late final Set<int> _construites = {widget.index};

  @override
  void didUpdateWidget(covariant _ContenuSections ancien) {
    super.didUpdateWidget(ancien);
    if (ancien.index != widget.index) {
      _construites.add(widget.index);
      if (NovaMotion.animationsReduites(context)) {
        _controleur.value = 1;
      } else {
        _controleur.forward(from: 0);
      }
    }
  }

  @override
  void dispose() {
    _courbe.dispose();
    _controleur.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    return FadeTransition(
      opacity: _courbe,
      child: AnimatedBuilder(
        animation: _courbe,
        builder: (context, child) => Transform.translate(
          offset: Offset(0, (1 - _courbe.value) * NovaMotion.sectionsDecalage),
          child: child,
        ),
        child: IndexedStack(
          index: widget.index,
          sizing: StackFit.expand,
          children: [
            for (var i = 0; i < 5; i++)
              _construites.contains(i)
                  ? _sectionPour(i)
                  : const SizedBox.shrink(),
          ],
        ),
      ),
    );
  }
}
