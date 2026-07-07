/// Providers Riverpod partagés par les écrans.
///
/// Conformément au plan 10 §10.2.3, ces providers ne contiennent **aucune
/// logique métier** : ils exposent la façade [NativeApi] et de l'état de
/// présentation (thème, données fictives du poste local en attendant les
/// flux du cœur Rust).
library;

import 'dart:math';

import 'package:flutter/material.dart' show ThemeMode;
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../bridge/mock_api.dart';
import '../bridge/native_api.dart';

/// Point d'accès unique à la façade `nd-ffi`.
///
/// Aujourd'hui : [MockNativeApi] (UI navigable sans le cœur Rust).
/// Après génération du binding : remplacer par `FrbNativeApi()`
/// (voir `lib/bridge/README.md`).
final nativeApiProvider = Provider<NativeApi>((ref) => MockNativeApi());

/// Thème de l'application : clair / sombre / système (plan 10 §10.7.3).
final themeModeProvider = StateProvider<ThemeMode>((ref) => ThemeMode.system);

/// ID NovaDesk du poste local.
///
/// FICTIF : sera fourni par le cœur (identité provisionnée au premier
/// lancement, voir plan 11) via un futur appel de la façade.
final idLocalProvider = Provider<int>((ref) => 936271048);

/// ID local, au format d'affichage groupé (`936 271 048`).
final idLocalFormateProvider = FutureProvider<String>((ref) {
  final api = ref.watch(nativeApiProvider);
  return api.formatNovaId(id: ref.watch(idLocalProvider));
});

/// Formatage d'un ID quelconque (sessions récentes, carnet…).
final idFormateProvider = FutureProvider.family<String, int>((ref, id) {
  final api = ref.watch(nativeApiProvider);
  return api.formatNovaId(id: id);
});

/// Informations « À propos » (version du moteur).
final appInfoProvider = FutureProvider<AppInfo>((ref) {
  return ref.watch(nativeApiProvider).appInfo();
});

/// Mot de passe éphémère du poste local, régénérable depuis l'accueil.
///
/// FICTIF : la génération/rotation réelle appartiendra au cœur (plan 06).
final motDePasseEphemereProvider =
    StateProvider<String>((ref) => genererMotDePasse(10));

/// Génère un mot de passe aléatoire lisible (sans caractères ambigus).
String genererMotDePasse(int longueur) {
  const alphabet =
      'ABCDEFGHJKLMNPQRSTUVWXYZabcdefghjkmnpqrstuvwxyz23456789!#%+=?';
  final alea = Random.secure();
  return List.generate(longueur, (_) => alphabet[alea.nextInt(alphabet.length)])
      .join();
}

/// Entrée du carnet d'adresses / des sessions récentes.
class EntreeCarnet {
  const EntreeCarnet({
    required this.id,
    required this.alias,
    required this.derniereConnexion,
    this.favori = false,
    this.enLigne = false,
  });

  /// ID NovaDesk du poste distant.
  final int id;

  /// Alias lisible choisi par l'utilisateur.
  final String alias;

  /// Libellé relatif de la dernière connexion (« il y a 2 h », « hier »…).
  final String derniereConnexion;

  /// Marqué d'une étoile dans le carnet.
  final bool favori;

  /// Présence du pair (pastille verte sur la vignette).
  /// FICTIF : viendra du service de rendez-vous (plan 11).
  final bool enLigne;

  EntreeCarnet copyWith({String? alias, bool? favori}) => EntreeCarnet(
        id: id,
        alias: alias ?? this.alias,
        derniereConnexion: derniereConnexion,
        favori: favori ?? this.favori,
        enLigne: enLigne,
      );
}

/// Sessions récentes + carnet d'adresses. `StateProvider` : les vignettes
/// permettent favori / renommage / suppression en local (état de
/// présentation).
///
/// FICTIF : sera synchronisé via le backend (plan 11), entrées chiffrées
/// côté client (plan 06).
final carnetProvider = StateProvider<List<EntreeCarnet>>((ref) {
  return const [
    EntreeCarnet(
      id: 421887330,
      alias: 'poste-bureau',
      derniereConnexion: 'il y a 2 h',
      favori: true,
      enLigne: true,
    ),
    EntreeCarnet(
      id: 730118902,
      alias: 'serveur-nas',
      derniereConnexion: 'hier',
      enLigne: true,
    ),
    EntreeCarnet(
      id: 555240173,
      alias: 'pc-marie',
      derniereConnexion: 'lun. 14:07',
    ),
    EntreeCarnet(
      id: 190774025,
      alias: 'atelier-01',
      derniereConnexion: 'mer. 09:42',
      favori: true,
      enLigne: true,
    ),
  ];
});
