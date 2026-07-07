/// Test de fumée : l'application démarre sur l'accueil « parité AnyDesk »
/// (fenêtre à onglets + deux colonnes) avec l'ID local formaté par la
/// façade (mock `MockNativeApi`).
library;

import 'dart:async';
import 'dart:ui' as ui;

import 'package:flutter/material.dart' show TextField;
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:novadesk_ui/bridge/mock_api.dart';
import 'package:novadesk_ui/bridge/native_api.dart';
import 'package:novadesk_ui/main.dart';
import 'package:novadesk_ui/state/providers.dart';

void main() {
  testWidgets("l'accueil affiche l'ID local formaté par groupes de 3",
      (tester) async {
    await tester.pumpWidget(const ProviderScope(child: NovaDeskApp()));
    await tester.pumpAndSettle();

    // Barre de titre : marque + onglet Accueil.
    expect(find.text('NovaDesk'), findsOneWidget);
    expect(find.text('Accueil'), findsOneWidget);
    // Colonne « Poste distant » : champ d'adresse + bouton rouge.
    expect(find.text('POSTE DISTANT'), findsOneWidget);
    expect(find.text('Se connecter'), findsOneWidget);
    expect(find.text('SESSIONS RÉCENTES'), findsOneWidget);
    // Colonne « Ce poste » : 936271048 -> « 936 271 048 »
    // (même rendu que format_nova_id côté Rust).
    expect(find.text('CE POSTE'), findsOneWidget);
    expect(find.text('936 271 048'), findsOneWidget);
  });

  testWidgets('la connexion ouvre la session (mock) puis revient à l’accueil',
      (tester) async {
    await tester.pumpWidget(const ProviderScope(child: NovaDeskApp()));
    await tester.pumpAndSettle();

    await tester.enterText(find.byType(TextField).first, '421887330');
    await tester.tap(find.text('Se connecter'));
    // La session live du mock émet un flux vidéo continu (~30 IPS) : on pompe
    // par durées bornées (pas de pumpAndSettle tant que la session est ouverte,
    // sinon l'animation perpétuelle ne « settle » jamais). Le flux d'états du
    // mock atteint « active » en ~1,3 s.
    for (var i = 0; i < 20; i++) {
      await tester.pump(const Duration(milliseconds: 120));
    }

    // Onglet de session + bloc pair de la barre d'outils flottante.
    expect(find.text('poste-bureau'), findsWidgets);
    expect(find.text('active'), findsOneWidget); // badge de la barre d'état
    expect(find.byTooltip('Terminer'), findsOneWidget);

    // Terminer : arrêt du moteur puis retour à l'accueil.
    // (La barre d'outils défile horizontalement si la fenêtre est étroite.)
    await tester.ensureVisible(find.byTooltip('Terminer'));
    await tester.pump();
    await tester.tap(find.byTooltip('Terminer'));
    // Le dispose annule les flux : une fois de retour à l'accueil, plus
    // d'animation → pumpAndSettle peut de nouveau se stabiliser.
    for (var i = 0; i < 8; i++) {
      await tester.pump(const Duration(milliseconds: 150));
    }
    await tester.pumpAndSettle();
    expect(find.text('POSTE DISTANT'), findsOneWidget);
  });

  testWidgets(
      'réglages en onglets, dialogue d’acceptation et accès non surveillé',
      (tester) async {
    await tester.pumpWidget(const ProviderScope(child: NovaDeskApp()));
    await tester.pumpAndSettle();

    // Réglages (bouton discret de la barre de titre).
    await tester.tap(find.byTooltip('Réglages'));
    await tester.pumpAndSettle();
    expect(find.text('Interface'), findsOneWidget);
    expect(find.text('À propos'), findsOneWidget);

    // Onglet Sécurité : dialogue d'acceptation entrante (démo).
    await tester.tap(find.text('Sécurité'));
    await tester.pumpAndSettle();
    await tester.tap(find.text("Tester le dialogue d'acceptation"));
    await tester.pumpAndSettle();
    expect(find.text('pc-marie'), findsOneWidget);
    expect(find.text('Accepter'), findsOneWidget);
    await tester.tap(find.text('Refuser'));
    await tester.pumpAndSettle();
    // Laisse la SnackBar de confirmation se résorber (minuteur interne).
    await tester.pump(const Duration(seconds: 5));
    await tester.pumpAndSettle();

    // Écran « Accès non surveillé » : le bouton d'activation est présent.
    await tester.tap(find.text('Accès non surveillé'));
    await tester.pumpAndSettle();
    expect(find.text("Activer l'accès non surveillé"), findsOneWidget);
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

    // Le mock émet une mire 320×180 en RGBA (4 octets/pixel).
    final trames = await api.collectVideoFrames(id, maxFrames: 1, timeoutMs: 100);
    expect(trames, isNotEmpty);
    final trame = trames.first;
    expect(trame.width, 320);
    expect(trame.height, 180);
    expect(trame.rgba.length, 320 * 180 * 4);

    // Chemin exact du rendu de la surface : RGBA → ui.Image via
    // decodeImageFromPixels (runAsync car le décodage natif sort du FakeAsync).
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
      "l'hôte non surveillé mock ouvre le dialogue entrant et « Accepter » "
      'appelle approveIncoming',
      (tester) async {
    // Mock injecté pour observer les décisions transmises à approve_incoming.
    final mock = MockNativeApi();
    await tester.pumpWidget(
      ProviderScope(
        overrides: [nativeApiProvider.overrideWithValue(mock)],
        child: const NovaDeskApp(),
      ),
    );
    await tester.pumpAndSettle();

    // Ouvre l'écran depuis la colonne « Ce poste » de l'accueil.
    await tester.ensureVisible(find.text('Accès non surveillé'));
    await tester.tap(find.text('Accès non surveillé'));
    await tester.pumpAndSettle();

    // Active l'hôte : start_unattended_host renvoie un id, abonnement au flux.
    await tester.tap(find.text("Activer l'accès non surveillé"));
    await tester.pump(); // lance _activerHote
    await tester.pump(const Duration(milliseconds: 50)); // l'appel résout
    expect(find.text('Actif'), findsOneWidget);

    // La demande entrante factice arrive ~2 s après l'abonnement : le dialogue
    // d'acceptation s'ouvre. (Pas de pumpAndSettle : le polling des stats
    // reprogramme un frame toutes les 2 s tant que l'hôte est actif.)
    await tester.pump(const Duration(milliseconds: 2200));
    await tester.pump(); // livraison de l'événement + showDialog
    await tester.pump(const Duration(milliseconds: 300)); // animation d'ouverture
    expect(find.text('Accepter'), findsOneWidget);
    expect(find.text('Refuser'), findsOneWidget);

    // Accepter → approve_incoming(accepter: true) sur le bon pair.
    await tester.tap(find.text('Accepter'));
    await tester.pump(); // pop + microtâche approveIncoming
    await tester.pump(const Duration(milliseconds: 300)); // fermeture du dialogue
    expect(mock.approbations, isNotEmpty);
    expect(mock.approbations.last.accepter, isTrue);
    expect(mock.approbations.last.peerId, 555240173);

    // Désactive proprement l'hôte (annule le flux mock → aucun timer pendant).
    await tester.tap(find.text('Désactiver'));
    await tester.pump();
    await tester.pump(const Duration(milliseconds: 100));
  });
}
