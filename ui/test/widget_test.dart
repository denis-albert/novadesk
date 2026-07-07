/// Test de fumée : l'application démarre sur l'accueil « parité AnyDesk »
/// (fenêtre à onglets + deux colonnes) avec l'ID local formaté par la
/// façade (mock `MockNativeApi`).
library;

import 'package:flutter/material.dart' show TextField;
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:novadesk_ui/main.dart';

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
    // Déroule la machine à états simulée (résolution → … → active).
    await tester.pumpAndSettle();

    // Onglet de session + bloc pair de la barre d'outils flottante.
    expect(find.text('poste-bureau'), findsWidgets);
    expect(find.text('active'), findsOneWidget); // badge de la barre d'état
    expect(find.byTooltip('Terminer'), findsOneWidget);

    // Terminer : état « terminée » puis retour à l'accueil (350 ms).
    // (La barre d'outils défile horizontalement si la fenêtre est étroite.)
    await tester.ensureVisible(find.byTooltip('Terminer'));
    await tester.pumpAndSettle();
    await tester.tap(find.byTooltip('Terminer'));
    await tester.pump();
    await tester.pump(const Duration(milliseconds: 400));
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

    // Écran « Accès non surveillé ».
    await tester.tap(find.text('Accès non surveillé'));
    await tester.pumpAndSettle();
    expect(find.text("Autoriser l'accès non-surveillé"), findsOneWidget);
  });
}
