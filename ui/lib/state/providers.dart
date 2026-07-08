/// Providers Riverpod partagés par les écrans.
///
/// Conformément au plan 10 §10.2.3, ces providers ne portent **aucune logique
/// métier** : ils exposent la façade [NativeApi] et lisent l'**état persistant**
/// du cœur (identité locale, carnet, réglages, historique, enregistrements,
/// accès non surveillé). Sous mock, cet état persiste en mémoire — le parcours
/// reste entièrement démontrable sans le cœur natif.
library;

import 'dart:math';

import 'package:flutter/material.dart' show ThemeMode;
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../bridge/mock_api.dart';
import '../bridge/native_api.dart';

/// Point d'accès unique à la façade `nd-ffi`.
///
/// Par défaut : [MockNativeApi] (UI navigable sans le cœur Rust). `main()`
/// remplace cette valeur par `FrbNativeApi()` quand la bibliothèque native est
/// chargée (override du provider), sans toucher aux écrans.
final nativeApiProvider = Provider<NativeApi>((ref) => MockNativeApi());

/// Thème de l'application : clair / sombre / système (plan 10 §10.7.3).
///
/// Piloté immédiatement par l'UI **et** persisté via `setSetting("theme", …)`.
/// `main()` surcharge la valeur initiale avec le réglage `theme` relu au
/// démarrage.
final themeModeProvider = StateProvider<ThemeMode>((ref) => ThemeMode.system);

// ---------------------------------------------------------------------------
// Identité locale (persistée par le cœur : `local_identity`)
// ---------------------------------------------------------------------------

/// Identité locale persistante (ID, ID formaté, empreinte). Créée et persistée
/// au premier appel côté cœur, rechargée à l'identique ensuite.
final localIdentityProvider = FutureProvider<LocalIdentityDto>((ref) {
  return ref.watch(nativeApiProvider).localIdentity();
});

/// ID NovaDesk du poste local, dérivé de [localIdentityProvider].
///
/// Repli transitoire tant que l'identité n'est pas résolue (premiers frames) :
/// remplacé par la vraie valeur dès la résolution du futur. N'est plus une
/// donnée en dur — la source de vérité est `local_identity`.
final idLocalProvider = Provider<int>((ref) {
  return ref.watch(localIdentityProvider).maybeWhen(
        data: (identite) => identite.id,
        orElse: () => 936271048,
      );
});

/// ID local au format d'affichage groupé (« 936 271 048 »), depuis l'identité.
final idLocalFormateProvider = FutureProvider<String>((ref) async {
  final identite = await ref.watch(localIdentityProvider.future);
  return identite.idFormate;
});

/// Formatage d'un ID quelconque (sessions récentes, carnet…).
final idFormateProvider = FutureProvider.family<String, int>((ref, id) {
  final api = ref.watch(nativeApiProvider);
  return api.formatNovaId(id: id);
});

/// Mot de passe éphémère du poste local, issu de `generate_ephemeral_password`
/// et régénérable depuis l'accueil.
final motDePasseEphemereProvider =
    AsyncNotifierProvider<MotDePasseEphemereNotifier, String>(
        MotDePasseEphemereNotifier.new);

/// Notifier du mot de passe éphémère : génère à la construction et sait
/// **régénérer** (rotation ponctuelle) via la façade.
class MotDePasseEphemereNotifier extends AsyncNotifier<String> {
  @override
  Future<String> build() =>
      ref.watch(nativeApiProvider).generateEphemeralPassword();

  /// Régénère un nouveau mot de passe éphémère.
  Future<void> regenerer() async {
    state = const AsyncLoading<String>();
    state = await AsyncValue.guard(
        () => ref.read(nativeApiProvider).generateEphemeralPassword());
  }
}

/// Informations « À propos » (version du moteur).
final appInfoProvider = FutureProvider<AppInfo>((ref) {
  return ref.watch(nativeApiProvider).appInfo();
});

// ---------------------------------------------------------------------------
// Réglages (persistés par le cœur : `get_settings` / `set_setting`)
// ---------------------------------------------------------------------------

/// Réglages effectifs, indexés par clé (défauts fusionnés avec les surcharges
/// persistées).
final settingsProvider = FutureProvider<Map<String, String>>((ref) async {
  final settings = await ref.watch(nativeApiProvider).getSettings();
  return {for (final s in settings) s.cle: s.valeur};
});

/// Adresse du serveur de rendez-vous (`nd-signaling`, « ip:port »), lue depuis
/// le réglage `serveur_rendezvous` (repli local raisonnable si vide).
final rendezvousProvider = Provider<String>((ref) {
  final v =
      ref.watch(settingsProvider).valueOrNull?['serveur_rendezvous']?.trim();
  return (v == null || v.isEmpty) ? '127.0.0.1:9000' : v;
});

/// Serveurs STUN (« ip:port »), lus depuis le réglage `serveurs_stun`
/// (séparés par virgule ou espace). Liste vide acceptée.
final stunServersProvider = Provider<List<String>>((ref) {
  final v = ref.watch(settingsProvider).valueOrNull?['serveurs_stun'] ?? '';
  return v
      .split(RegExp(r'[,\s]+'))
      .map((e) => e.trim())
      .where((e) => e.isNotEmpty)
      .toList();
});

/// Relais de repli (`nd-relay`, « ip:port »), lu depuis le réglage
/// `serveur_relais` ; `null` = pas de repli.
final relayProvider = Provider<String?>((ref) {
  final v = ref.watch(settingsProvider).valueOrNull?['serveur_relais']?.trim() ??
      '';
  return v.isEmpty ? null : v;
});

/// Convertit le réglage `theme` persisté en [ThemeMode].
ThemeMode themeDepuisReglage(String? valeur) {
  switch (valeur) {
    case 'clair':
    case 'light':
      return ThemeMode.light;
    case 'sombre':
    case 'dark':
      return ThemeMode.dark;
    default:
      return ThemeMode.system;
  }
}

/// Valeur textuelle persistée pour un [ThemeMode].
String reglageDepuisTheme(ThemeMode mode) => switch (mode) {
      ThemeMode.light => 'clair',
      ThemeMode.dark => 'sombre',
      ThemeMode.system => 'systeme',
    };

// ---------------------------------------------------------------------------
// Historique, enregistrements, accès non surveillé (persistés par le cœur)
// ---------------------------------------------------------------------------

/// Sessions récentes (du plus récent au plus ancien) : `recent_sessions`.
final recentSessionsProvider = FutureProvider<List<RecentSessionDto>>((ref) {
  return ref.watch(nativeApiProvider).recentSessions();
});

/// Enregistrements présents sur le disque : `list_recordings`.
final recordingsProvider = FutureProvider<List<RecordingDto>>((ref) {
  return ref.watch(nativeApiProvider).listRecordings();
});

/// Configuration d'accès non surveillé : `unattended_config`.
final unattendedConfigProvider = FutureProvider<UnattendedConfigDto>((ref) {
  return ref.watch(nativeApiProvider).unattendedConfig();
});

/// Journal des accès non surveillés (du plus récent au plus ancien) :
/// `access_log`.
final accessLogProvider = FutureProvider<List<AccessLogEntryDto>>((ref) {
  return ref.watch(nativeApiProvider).accessLog();
});

// ---------------------------------------------------------------------------
// Carnet d'adresses (persisté par le cœur : `list_contacts`, `add_contact`, …)
// ---------------------------------------------------------------------------

/// Groupes déclarés du carnet : `list_groups`. Invalidé par les mutations du
/// carnet qui créent un groupe (voir [CarnetNotifier]).
final groupesProvider = FutureProvider<List<String>>((ref) {
  return ref.watch(nativeApiProvider).listGroups();
});

/// Carnet d'adresses, chargé depuis `list_contacts` et converti en
/// [EntreeCarnet] de présentation. Les actions (ajout, mise à jour, retrait,
/// favori, groupe) appellent les vraies fonctions puis **rechargent**.
final carnetProvider =
    AsyncNotifierProvider<CarnetNotifier, List<EntreeCarnet>>(
        CarnetNotifier.new);

/// Notifier du carnet : lit la façade et expose des actions persistantes.
class CarnetNotifier extends AsyncNotifier<List<EntreeCarnet>> {
  NativeApi get _api => ref.read(nativeApiProvider);

  @override
  Future<List<EntreeCarnet>> build() async {
    final contacts = await ref.watch(nativeApiProvider).listContacts();
    return contacts.map(EntreeCarnet.depuisContact).toList();
  }

  Future<List<EntreeCarnet>> _charger() async {
    final contacts = await _api.listContacts();
    return contacts.map(EntreeCarnet.depuisContact).toList();
  }

  Future<void> _recharger() async {
    state = await AsyncValue.guard(_charger);
  }

  /// Ajoute un contact (`add_contact`) puis recharge. Peut lever
  /// [NovaApiException] (ID déjà présent).
  Future<void> ajouter({
    required String alias,
    required int id,
    required String groupe,
    required List<String> etiquettes,
  }) async {
    await _api.addContact(
        alias: alias, id: id, groupe: groupe, etiquettes: etiquettes);
    ref.invalidate(groupesProvider);
    await _recharger();
  }

  /// Met à jour un contact (`update_contact`) puis recharge.
  Future<void> modifier({
    required int id,
    required String alias,
    required String groupe,
    required List<String> etiquettes,
  }) async {
    await _api.updateContact(
        id: id, alias: alias, groupe: groupe, etiquettes: etiquettes);
    ref.invalidate(groupesProvider);
    await _recharger();
  }

  /// Retire un contact (`remove_contact`) puis recharge.
  Future<void> supprimer(int id) async {
    await _api.removeContact(id: id);
    await _recharger();
  }

  /// Bascule le favori d'un contact (`set_favorite`) puis recharge.
  Future<void> basculerFavori(int id, bool favori) async {
    await _api.setFavorite(id: id, favori: favori);
    await _recharger();
  }

  /// Ajoute un groupe (`add_group`) et rafraîchit la liste des groupes.
  Future<void> ajouterGroupe(String nom) async {
    await _api.addGroup(nom: nom);
    ref.invalidate(groupesProvider);
  }
}

/// Génère un mot de passe aléatoire lisible (sans caractères ambigus).
String genererMotDePasse(int longueur) {
  const alphabet =
      'ABCDEFGHJKLMNPQRSTUVWXYZabcdefghjkmnpqrstuvwxyz23456789!#%+=?';
  final alea = Random.secure();
  return List.generate(longueur, (_) => alphabet[alea.nextInt(alphabet.length)])
      .join();
}

/// Système d'exploitation d'un poste (choix de l'icône OS au carnet).
enum OsAppareil { windows, linux, android, macos }

/// Déduit l'OS d'un contact de ses étiquettes (heuristique de présentation,
/// l'état persistant ne stocke pas d'OS explicite).
OsAppareil osDepuisEtiquettes(List<String> etiquettes) {
  final tags = etiquettes.map((e) => e.toLowerCase()).toSet();
  if (tags.contains('linux') ||
      tags.contains('nas') ||
      tags.contains('serveur') ||
      tags.contains('ci')) {
    return OsAppareil.linux;
  }
  if (tags.contains('android')) return OsAppareil.android;
  if (tags.contains('macos') || tags.contains('mac')) return OsAppareil.macos;
  return OsAppareil.windows;
}

/// Libellé relatif français d'un horodatage Unix (secondes) : « il y a 2 h »,
/// « hier », « 3 juil. »… `null` → « jamais ».
String formaterHorodatageRelatif(int? unixSecondes) {
  if (unixSecondes == null) return 'jamais';
  final date = DateTime.fromMillisecondsSinceEpoch(unixSecondes * 1000);
  final diff = DateTime.now().difference(date);
  if (diff.isNegative) return "à l'instant";
  if (diff.inSeconds < 60) return "à l'instant";
  if (diff.inMinutes < 60) return 'il y a ${diff.inMinutes} min';
  if (diff.inHours < 24) return 'il y a ${diff.inHours} h';
  if (diff.inDays == 1) return 'hier';
  if (diff.inDays < 7) return 'il y a ${diff.inDays} j';
  const mois = [
    'janv.', 'févr.', 'mars', 'avr.', 'mai', 'juin', //
    'juil.', 'août', 'sept.', 'oct.', 'nov.', 'déc.'
  ];
  return '${date.day} ${mois[date.month - 1]}';
}

/// Entrée du carnet d'adresses / des sessions récentes (modèle de présentation
/// dérivé d'[AddressBookEntryDto]).
class EntreeCarnet {
  const EntreeCarnet({
    required this.id,
    required this.alias,
    required this.derniereConnexion,
    this.favori = false,
    this.enLigne = false,
    this.groupe = 'Travail',
    this.etiquettes = const [],
    this.os = OsAppareil.windows,
  });

  /// Construit une entrée de présentation depuis un contact persistant.
  factory EntreeCarnet.depuisContact(AddressBookEntryDto dto) {
    final horodatage = dto.derniereConnexion;
    // Présence dérivée (l'état persistant ne fournit pas de présence en direct) :
    // considéré « en ligne » si connecté dans les dernières 24 h.
    final enLigne = horodatage != null &&
        DateTime.now()
                .difference(
                    DateTime.fromMillisecondsSinceEpoch(horodatage * 1000))
                .inHours <
            24;
    return EntreeCarnet(
      id: dto.id,
      alias: dto.alias,
      derniereConnexion: formaterHorodatageRelatif(horodatage),
      favori: dto.favori,
      enLigne: enLigne,
      groupe: dto.groupe.isEmpty ? 'Sans groupe' : dto.groupe,
      etiquettes: dto.etiquettes,
      os: osDepuisEtiquettes(dto.etiquettes),
    );
  }

  /// ID NovaDesk du poste distant.
  final int id;

  /// Alias lisible choisi par l'utilisateur.
  final String alias;

  /// Libellé relatif de la dernière connexion (« il y a 2 h », « hier »…).
  final String derniereConnexion;

  /// Marqué d'une étoile dans le carnet.
  final bool favori;

  /// Présence dérivée du pair (pastille verte sur la vignette).
  final bool enLigne;

  /// Groupe du carnet (« Travail », « Serveurs », « Perso »…).
  final String groupe;

  /// Étiquettes libres (affichées en pastilles bleues au carnet).
  final List<String> etiquettes;

  /// Système d'exploitation (icône OS du carnet).
  final OsAppareil os;
}
