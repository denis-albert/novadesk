/// Tests de fumée : navigabilité sous mock ([MockNativeApi]) de l'UI
/// « novadesk-app » — accueil (ID local formaté, liste après squelette),
/// connexion → session → retour, réglages en onglets, décodage vidéo pur Dart,
/// et hôte non surveillé (dialogue d'acceptation → `approve_incoming`).
///
/// Note : les squelettes de chargement et les flux de session sont des
/// animations/minuteurs perpétuels ; on avance donc par `pump` bornés (jamais
/// `pumpAndSettle` tant qu'une animation perpétuelle est à l'écran).
library;

import 'dart:async';
import 'dart:ui' as ui;

import 'package:flutter/material.dart' show TextField, Widget, Size;
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:novadesk_ui/bridge/mock_api.dart';
import 'package:novadesk_ui/bridge/native_api.dart';
import 'package:novadesk_ui/main.dart';
import 'package:novadesk_ui/state/providers.dart';
import 'package:novadesk_ui/widgets/nova_kit.dart';

/// Avance au-delà du squelette de chargement de l'accueil (~780 ms) et de la
/// résolution des `FutureProvider`, sans `pumpAndSettle` (shimmer perpétuel).
Future<void> _demarrer(WidgetTester tester, {Widget? app}) async {
  // Fenêtre desktop réaliste (cible NovaDesk ≥ 1120 px de large).
  tester.view.physicalSize = const Size(1280, 800);
  tester.view.devicePixelRatio = 1.0;
  addTearDown(tester.view.resetPhysicalSize);
  addTearDown(tester.view.resetDevicePixelRatio);
  await tester.pumpWidget(app ?? const ProviderScope(child: NovaDeskApp()));
  for (var i = 0; i < 10; i++) {
    await tester.pump(const Duration(milliseconds: 120));
  }
}

void main() {
  testWidgets("l'accueil affiche l'ID local formaté par groupes de 3",
      (tester) async {
    await _demarrer(tester);

    // Barre de titre + onglet.
    expect(find.text('NovaDesk'), findsOneWidget);
    expect(find.text('Accueil'), findsWidgets);
    // Colonne « Poste distant » + bouton rouge.
    expect(find.text('Poste distant'), findsOneWidget);
    expect(find.text('Se connecter'), findsOneWidget);
    // Colonne « Ce poste » : 936271048 -> « 936 271 048 ».
    expect(find.text('Ce poste'), findsOneWidget);
    expect(find.text('936 271 048'), findsOneWidget);
  });

  testWidgets('la connexion ouvre la session (mock) puis revient à l’accueil',
      (tester) async {
    await _demarrer(tester);

    await tester.enterText(find.byType(TextField).first, '421887330');
    await tester.tap(find.text('Se connecter'));
    // Le flux d'états du mock atteint « active » en ~1,3 s ; on pompe par
    // durées bornées (flux vidéo perpétuel).
    for (var i = 0; i < 24; i++) {
      await tester.pump(const Duration(milliseconds: 120));
    }

    // Onglet de session + bouton « Terminer » de la barre d'outils.
    expect(find.text('poste-bureau'), findsWidgets);
    expect(find.byTooltip('Terminer'), findsOneWidget);

    await tester.ensureVisible(find.byTooltip('Terminer'));
    await tester.pump();
    await tester.tap(find.byTooltip('Terminer'));
    for (var i = 0; i < 8; i++) {
      await tester.pump(const Duration(milliseconds: 150));
    }
    // Vide le minuteur du toast « Connecté » (3 s) avant la fin du test.
    await tester.pump(const Duration(seconds: 3));
    await tester.pump(const Duration(milliseconds: 300));

    expect(find.text('Poste distant'), findsOneWidget);
  });

  testWidgets('les réglages présentent les onglets (rail + volet)',
      (tester) async {
    await _demarrer(tester);

    // Réglages via le rail de navigation.
    await tester.tap(find.byTooltip('Réglages'));
    await tester.pumpAndSettle();
    expect(find.text('Interface'), findsWidgets);

    // Onglet Sécurité : le volet affiche ses lignes de réglage.
    await tester.tap(find.text('Sécurité'));
    await tester.pumpAndSettle();
    expect(find.text('Double authentification (TOTP)'), findsOneWidget);
  });

  testWidgets('le flux vidéo du mock se décode en ui.Image (rendu pur Dart)',
      (tester) async {
    final api = MockNativeApi();
    final id = await api.startSession(
      config: SessionConfigDto(
        role: SessionRoleDto.controller,
        localId: 1,
        peerId: 2,
        permissions: PermissionsDto.full(),
      ),
      endpoint: const SessionEndpointLoopback(),
    );

    final trames =
        await api.collectVideoFrames(id, maxFrames: 1, timeoutMs: 100);
    expect(trames, isNotEmpty);
    final trame = trames.first;
    expect(trame.width, 320);
    expect(trame.height, 180);
    expect(trame.rgba.length, 320 * 180 * 4);

    await tester.runAsync(() async {
      final completer = Completer<ui.Image>();
      ui.decodeImageFromPixels(
        trame.rgba,
        trame.width,
        trame.height,
        ui.PixelFormat.rgba8888,
        completer.complete,
      );
      final image = await completer.future;
      expect(image.width, 320);
      expect(image.height, 180);
      image.dispose();
    });

    await api.stopSession(id);
  });

  testWidgets(
      "l'hôte non surveillé ouvre le dialogue entrant et « Accepter » "
      'appelle approveIncoming',
      (tester) async {
    final mock = MockNativeApi();
    await _demarrer(
      tester,
      app: ProviderScope(
        overrides: [nativeApiProvider.overrideWithValue(mock)],
        child: const NovaDeskApp(),
      ),
    );

    // Ouvre l'écran depuis le lien « Accès non surveillé » de l'accueil.
    await tester.tap(find.text('Accès non surveillé'));
    await tester.pumpAndSettle();

    // Active l'hôte via l'interrupteur (start_unattended_host + abonnement).
    await tester.tap(find.byType(NovaSwitch).first);
    await tester.pump();
    await tester.pump(const Duration(milliseconds: 50));

    // La demande entrante factice arrive ~2 s après l'abonnement.
    await tester.pump(const Duration(milliseconds: 2200));
    await tester.pump();
    await tester.pump(const Duration(milliseconds: 300));
    expect(find.text('Accepter'), findsOneWidget);
    expect(find.text('Refuser'), findsOneWidget);

    // Accepter → approve_incoming(accepter: true) sur le bon pair.
    await tester.tap(find.text('Accepter'));
    await tester.pump();
    await tester.pump(const Duration(milliseconds: 300));
    expect(mock.approbations, isNotEmpty);
    expect(mock.approbations.last.accepter, isTrue);
    expect(mock.approbations.last.peerId, 555240173);

    // Désactive proprement l'hôte (annule le flux mock → aucun timer pendant).
    await tester.tap(find.byType(NovaSwitch).first);
    await tester.pump();
    await tester.pump(const Duration(milliseconds: 100));
    // Vide les minuteurs de toast (3 s).
    await tester.pump(const Duration(seconds: 3));
    await tester.pump(const Duration(milliseconds: 300));
  });
}
