/// Test de fumée : l'application démarre sur l'accueil avec l'ID local
/// formaté par la façade (mock `MockNativeApi`).
library;

import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:novadesk_ui/main.dart';

void main() {
  testWidgets("l'accueil affiche l'ID local formaté par groupes de 3",
      (tester) async {
    await tester.pumpWidget(const ProviderScope(child: NovaDeskApp()));
    await tester.pumpAndSettle();

    expect(find.text('NovaDesk'), findsOneWidget);
    // 936271048 -> « 936 271 048 » (même rendu que format_nova_id côté Rust).
    expect(find.text('936 271 048'), findsOneWidget);
    expect(find.text('Se connecter à un poste distant'), findsOneWidget);
  });
}
