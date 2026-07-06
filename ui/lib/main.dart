/// NovaDesk — point d'entrée de l'application Flutter (plan 10).
///
/// L'UI est « mince » : présentation, collecte d'intentions, composition de
/// la surface vidéo. Toute la logique réseau/crypto/média vit dans le cœur
/// Rust, atteint via la façade `nd-ffi` (voir `lib/bridge/`).
library;

import 'dart:async';
import 'dart:io' show Platform;

import 'package:flutter/foundation.dart' show kIsWeb;
import 'package:flutter/material.dart';
import 'package:flutter_localizations/flutter_localizations.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import 'platform/window_shim.dart';
import 'screens/home_screen.dart';
import 'screens/session_screen.dart';
import 'screens/settings_screen.dart';
import 'screens/unattended_screen.dart';
import 'state/providers.dart';

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

  runApp(const ProviderScope(child: NovaDeskApp()));
}

/// Racine de l'application : thèmes Material 3 clair/sombre, localisation
/// française, table des routes.
class NovaDeskApp extends ConsumerWidget {
  const NovaDeskApp({super.key});

  /// Thème Material 3 dérivé d'une couleur de base unique
  /// (design tokens centralisés, plan 10 §10.7.3).
  static ThemeData _theme(Brightness brightness) {
    final schema = ColorScheme.fromSeed(
      seedColor: const Color(0xFF4C5FD5),
      brightness: brightness,
    );
    return ThemeData(
      useMaterial3: true,
      colorScheme: schema,
      visualDensity: VisualDensity.adaptivePlatformDensity,
      snackBarTheme: const SnackBarThemeData(
        behavior: SnackBarBehavior.floating,
      ),
    );
  }

  @override
  Widget build(BuildContext context, WidgetRef ref) {
    final modeTheme = ref.watch(themeModeProvider);
    return MaterialApp(
      title: 'NovaDesk',
      debugShowCheckedModeBanner: false,
      theme: _theme(Brightness.light),
      darkTheme: _theme(Brightness.dark),
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

      initialRoute: HomeScreen.route,
      onGenerateRoute: _genererRoute,
    );
  }

  Route<dynamic>? _genererRoute(RouteSettings parametres) {
    switch (parametres.name) {
      case HomeScreen.route:
        return MaterialPageRoute(builder: (_) => const HomeScreen());
      case SessionScreen.route:
        final arguments = parametres.arguments;
        if (arguments is! SessionScreenArgs) {
          // Une session ne s'ouvre jamais sans configuration validée.
          return MaterialPageRoute(builder: (_) => const HomeScreen());
        }
        return MaterialPageRoute(
          builder: (_) => SessionScreen(args: arguments),
        );
      case SettingsScreen.route:
        return MaterialPageRoute(builder: (_) => const SettingsScreen());
      case UnattendedScreen.route:
        return MaterialPageRoute(builder: (_) => const UnattendedScreen());
    }
    return null;
  }
}
