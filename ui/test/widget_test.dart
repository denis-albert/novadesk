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

  // -------------------------------------------------------------------------
  // Canaux média annexes du mock (lot « session media »)
  // -------------------------------------------------------------------------

  test('discussion mock : sendChat livre l’écho local puis la réponse distante',
      () async {
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
    final recu = <ChatMessageDto>[];
    final sub = api.sessionChatStream(id).listen(recu.add);

    await api.sendChat(id, 'Bonjour');
    // Écho local immédiat (fromRemote faux).
    await Future<void>.delayed(const Duration(milliseconds: 50));
    expect(recu.where((m) => !m.fromRemote).map((m) => m.text), ['Bonjour']);

    // Réponse distante de synthèse ~1,5 s plus tard (fromRemote vrai).
    await Future<void>.delayed(const Duration(milliseconds: 1600));
    final distants = recu.where((m) => m.fromRemote).toList();
    expect(distants, hasLength(1));
    expect(distants.single.text, contains('Bonjour'));

    await sub.cancel();
    await api.stopSession(id);
  });

  test('transfert mock : sendFiles émet une progression jusqu’à « finished »',
      () async {
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
    final evts = <TransferEventDto>[];
    final sub = api.sessionTransferStream(id).listen(evts.add);

    await api.sendFiles(id, [r'C:\demo\a.bin', r'C:\demo\b.bin']);

    // Attend la fin de la file (borne de sécurité de 8 s).
    final limite = DateTime.now().add(const Duration(seconds: 8));
    while (!evts.any((e) => e.kind == 'finished') &&
        DateTime.now().isBefore(limite)) {
      await Future<void>.delayed(const Duration(milliseconds: 100));
    }

    expect(evts.first.kind, 'started');
    expect(evts.any((e) => e.kind == 'progress'), isTrue);
    expect(evts.where((e) => e.kind == 'completed'), hasLength(2));
    expect(evts.last.kind, 'finished');
    // Le pourcentage de session progresse et atteint 100 %.
    final pourcents =
        evts.where((e) => e.percent != null).map((e) => e.percent!).toList();
    expect(pourcents, isNotEmpty);
    expect(pourcents.last, closeTo(100.0, 0.001));

    await sub.cancel();
    await api.stopSession(id);
  });

  // -------------------------------------------------------------------------
  // État persistant du mock (lot « état persistant ») — persistance mémoire
  // -------------------------------------------------------------------------

  test('identité mock : localIdentity est stable et formatée', () async {
    final api = MockNativeApi();
    final a = await api.localIdentity();
    final b = await api.localIdentity();
    expect(a.id, 936271048);
    expect(a.idFormate, '936 271 048');
    expect(a.empreinte.length, 64);
    // Rechargée à l'identique.
    expect(b, a);
  });

  test('carnet mock : addContact → listContacts contient le contact', () async {
    final api = MockNativeApi();
    final avant = await api.listContacts();
    final entree = await api.addContact(
        alias: 'nouveau-poste',
        id: 123456789,
        groupe: 'Test',
        etiquettes: const ['demo']);
    expect(entree.id, 123456789);
    expect(entree.alias, 'nouveau-poste');

    final apres = await api.listContacts();
    expect(apres.length, avant.length + 1);
    expect(apres.any((c) => c.id == 123456789 && c.alias == 'nouveau-poste'),
        isTrue);
    // Le groupe non vide est déclaré.
    expect(await api.listGroups(), contains('Test'));

    // Favori puis mise à jour persistent, ID en double refusé.
    await api.setFavorite(id: 123456789, favori: true);
    expect(
        (await api.listContacts()).firstWhere((c) => c.id == 123456789).favori,
        isTrue);
    await api.updateContact(
        id: 123456789,
        alias: 'poste-renomme',
        groupe: 'Test',
        etiquettes: const ['demo']);
    expect(
        (await api.listContacts()).firstWhere((c) => c.id == 123456789).alias,
        'poste-renomme');
    await expectLater(
      api.addContact(
          alias: 'x', id: 123456789, groupe: '', etiquettes: const []),
      throwsA(isA<NovaApiException>()),
    );

    // Retrait effectif.
    await api.removeContact(id: 123456789);
    expect((await api.listContacts()).any((c) => c.id == 123456789), isFalse);
  });

  test('réglages mock : setSetting → getSetting renvoie la valeur', () async {
    final api = MockNativeApi();
    expect(await api.getSetting(cle: 'serveur_rendezvous'), '127.0.0.1:9000');
    await api.setSetting(cle: 'serveur_rendezvous', valeur: '203.0.113.7:9000');
    expect(await api.getSetting(cle: 'serveur_rendezvous'), '203.0.113.7:9000');
    // Reflété dans getSettings.
    final tous = await api.getSettings();
    expect(tous.firstWhere((s) => s.cle == 'serveur_rendezvous').valeur,
        '203.0.113.7:9000');
    // Clé vide refusée.
    await expectLater(
      api.setSetting(cle: '', valeur: 'x'),
      throwsA(isA<NovaApiException>()),
    );
  });

  test('accès non surveillé mock : setUnattendedPassword → verify', () async {
    final api = MockNativeApi();
    expect((await api.unattendedConfig()).aMotDePasse, isFalse);
    expect(await api.verifyUnattendedPassword(pwd: 'secret'), isFalse);

    await api.setUnattendedPassword(pwd: 'secret-permanent');
    expect((await api.unattendedConfig()).aMotDePasse, isTrue);
    expect(await api.verifyUnattendedPassword(pwd: 'secret-permanent'), isTrue);
    expect(await api.verifyUnattendedPassword(pwd: 'mauvais'), isFalse);

    // Mot de passe vide : efface la configuration.
    await api.setUnattendedPassword(pwd: '');
    expect((await api.unattendedConfig()).aMotDePasse, isFalse);

    // Appareils de confiance : add / remove.
    await api.addTrustedDevice(id: 111222333);
    expect((await api.unattendedConfig()).appareilsDeConfiance,
        contains(111222333));
    await api.removeTrustedDevice(id: 111222333);
    expect((await api.unattendedConfig()).appareilsDeConfiance,
        isNot(contains(111222333)));

    // Journal des accès : recordAccess ajoute en tête.
    final avant = (await api.accessLog()).length;
    await api.recordAccess(peerId: 111222333, accepte: true);
    final apres = await api.accessLog();
    expect(apres.length, avant + 1);
    expect(apres.first.peerId, 111222333);
    expect(apres.first.accepte, isTrue);
  });

  test('historique mock : recordSession place la session en tête', () async {
    final api = MockNativeApi();
    await api.recordSession(id: 987654321, alias: 'poste-test');
    final recentes = await api.recentSessions();
    expect(recentes.first.id, 987654321);
    expect(recentes.first.alias, 'poste-test');
    // Dédupliqué par id : un second enregistrement ne crée pas de doublon.
    await api.recordSession(id: 987654321, alias: 'poste-test');
    final apres = await api.recentSessions();
    expect(apres.where((s) => s.id == 987654321).length, 1);
  });

  test('enregistrements mock : listRecordings renvoie des métadonnées triées',
      () async {
    final api = MockNativeApi();
    final recs = await api.listRecordings();
    expect(recs, isNotEmpty);
    // Triés du plus récent au plus ancien.
    for (var i = 1; i < recs.length; i++) {
      expect(recs[i - 1].date >= recs[i].date, isTrue);
    }
    expect(recs.first.nom, isNotEmpty);
    expect(recs.first.tailleOctets, greaterThan(0));
  });
}
