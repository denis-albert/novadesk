/// Lecteur d'enregistrements NovaDesk (maquette `novadesk-app.html`, vue
/// `#v-enreg` / `.player`) : liste latérale des sessions enregistrées à gauche
/// (`.reclist`) et scène de lecture à droite (`.stage`) — canevas vidéo
/// surmontant la barre de contrôles (`.pctrl`).
///
/// La liste est alimentée par l'état persistant (`list_recordings`) : nom,
/// date, durée et taille **réels** des fichiers présents. État vide si aucun
/// enregistrement.
///
/// La lecture est **réelle**, pilotée par la façade native :
/// `open_recording` à la sélection (métadonnées + identifiant de lecteur),
/// boucle cadencée au fps nominal (`Timer.periodic`) tirant chaque image via
/// `recording_next_frame`, repositionnement par `recording_seek` (scrub de la
/// timeline, rembobinage) et `close_recording` à la fermeture (changement de
/// sélection, liste vidée, `dispose`).
///
/// Rendu vidéo **100 % pur Dart** (aucun `Texture`/plugin natif), même patron
/// que la surface live de `session_screen.dart` : chaque [VideoFrameDto] est
/// convertie en `ui.Image` via `decodeImageFromPixels` (RGBA) puis peinte par
/// [_PeintreEnregistrement] en conservant le ratio (letterbox). Les images
/// précédentes sont libérées (pas de fuite).
///
/// La progression (remplissage, bouton de la timeline, compteur `mm:ss`) est
/// pilotée par les données — aucune animation décorative n'est ajoutée, le
/// mode « réduire les animations » est donc respecté par construction. Tout
/// l'habillage (filets, liste, contrôles) reste piloté par le thème
/// [NovaTokens] et le rayon `--r:4px`.
library;

import 'dart:async';
import 'dart:math' as math;
import 'dart:ui' as ui;

import 'package:flutter/foundation.dart' show ValueListenable;
import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
// Le catalogue [NovaIcones] n'expose pas de glyphe « pause » : on pioche le
// glyphe directement dans la même fonte Lucide (aucun tracé maison).
import 'package:lucide_icons_flutter/lucide_icons.dart';

import '../app_routes.dart';
import '../bridge/native_api.dart';
import '../state/providers.dart';
import '../theme/nova_theme.dart';
import '../widgets/nova_icons.dart';
import '../widgets/nova_kit.dart';

// --- Surfaces d'aperçu sombres (valeurs fixes de la maquette) --------------

/// Fond des vignettes `.th` (`#1c2430`).
const Color _fondVignette = Color(0xFF1C2430);

/// Icône de lecture dans les vignettes `.th` (`#5b6472`).
const Color _icoVignette = Color(0xFF5B6472);

/// Dégradé du canevas `.canvas` (`linear-gradient(160deg,#20293a,#12182550)`).
const Color _canevasHaut = Color(0xFF20293A);
const Color _canevasBas = Color(0xFF12182D);

/// Texte de base du canevas (`#8a94b4`).
const Color _canevasTexte = Color(0xFF8A94B4);

/// Nom en surbrillance dans le canevas (`#cdd6f2`).
const Color _canevasTitre = Color(0xFFCDD6F2);

// ===========================================================================
// Formatage des métadonnées d'enregistrement
// ===========================================================================

const List<String> _moisCourts = [
  'janv.', 'févr.', 'mars', 'avr.', 'mai', 'juin', //
  'juil.', 'août', 'sept.', 'oct.', 'nov.', 'déc.'
];

/// Date courte française d'un horodatage Unix (secondes) : « 8 juil. ».
String _formaterDateCourte(int unixSecondes) {
  final d = DateTime.fromMillisecondsSinceEpoch(unixSecondes * 1000);
  return '${d.day} ${_moisCourts[d.month - 1]}';
}

/// Heure d'un horodatage Unix (secondes) : « 14:07 ».
String _formaterHeure(int unixSecondes) {
  final d = DateTime.fromMillisecondsSinceEpoch(unixSecondes * 1000);
  return '${d.hour.toString().padLeft(2, '0')}:'
      '${d.minute.toString().padLeft(2, '0')}';
}

/// Durée « mm:ss » (ou « h:mm:ss ») depuis un nombre de secondes.
String _formaterDuree(double secondes) {
  final total = secondes.round();
  final h = total ~/ 3600;
  final m = (total % 3600) ~/ 60;
  final s = total % 60;
  final mm = m.toString().padLeft(2, '0');
  final ss = s.toString().padLeft(2, '0');
  return h > 0 ? '$h:$mm:$ss' : '$mm:$ss';
}

/// Taille de fichier lisible (Ko / Mo / Go).
String _formaterTaille(int octets) {
  if (octets < 1024) return '$octets o';
  if (octets < 1024 * 1024) return '${(octets / 1024).toStringAsFixed(0)} Ko';
  if (octets < 1024 * 1024 * 1024) {
    return '${(octets / (1024 * 1024)).toStringAsFixed(0)} Mo';
  }
  return '${(octets / (1024 * 1024 * 1024)).toStringAsFixed(1).replaceAll('.', ',')} Go';
}

/// Ligne méta d'une entrée : « 8 juil. 14:07 · 12:34 · 486 Mo ».
String _meta(RecordingDto r) =>
    '${_formaterDateCourte(r.date)} ${_formaterHeure(r.date)} · '
    '${_formaterDuree(r.dureeS)} · ${_formaterTaille(r.tailleOctets)}';

/// Détail sous le titre du canevas : « 8 juil. · 12:34 · MP4 ».
String _detailCanevas(RecordingDto r) =>
    '${_formaterDateCourte(r.date)} · ${_formaterDuree(r.dureeS)} · '
    '${_extension(r.nom)}';

/// Extension en capitales (ex. « MP4 »), ou « ENR » à défaut.
String _extension(String nom) {
  final point = nom.lastIndexOf('.');
  if (point < 0 || point == nom.length - 1) return 'ENR';
  return nom.substring(point + 1).toUpperCase();
}

// ===========================================================================
// Écran
// ===========================================================================

/// Écran « Lecteur d'enregistrements » (vue `enregistrements` du rail).
class RecordingsScreen extends ConsumerStatefulWidget {
  const RecordingsScreen({super.key});

  /// Route nommée de l'écran.
  static const String route = NovaRoutes.enregistrements;

  @override
  ConsumerState<RecordingsScreen> createState() => _RecordingsScreenState();
}

class _RecordingsScreenState extends ConsumerState<RecordingsScreen> {
  /// Index de l'enregistrement sélectionné (premier par défaut).
  int _indexSelectionne = 0;

  // -------------------------------------------------------------------------
  // État du lecteur réel (façade `open_recording` / `recording_next_frame` /
  // `recording_seek` / `close_recording`)
  // -------------------------------------------------------------------------

  /// Identifiant opaque du lecteur ouvert côté cœur (`open_recording`).
  int? _lecteurId;

  /// Métadonnées du lecteur ouvert (dimensions, fps, durée, nombre d'images).
  RecordingInfoDto? _infoLecteur;

  /// Chemin du fichier actuellement ouvert (dédoublonne les ouvertures).
  String? _cheminOuvert;

  /// Chemin en cours d'ouverture (une seule ouverture à la fois par fichier).
  String? _cheminEnOuverture;

  /// Chemin dont l'ouverture a échoué : pas de re-tentative automatique à
  /// chaque build (le toast a déjà informé) — un nouveau clic la relance.
  String? _cheminEnEchec;

  /// Génération d'ouverture : invalide les résultats d'`open_recording`
  /// devenus périmés (autre sélection, liste vidée, écran quitté).
  int _generationOuverture = 0;

  /// Image vidéo courante décodée en `ui.Image`, peinte par
  /// [_PeintreEnregistrement] (même patron que la surface live de session).
  final ValueNotifier<ui.Image?> _trameCourante = ValueNotifier<ui.Image?>(null);

  /// Position de lecture courante (µs) — pilote timeline et compteur sans
  /// reconstruire tout l'écran à chaque image.
  final ValueNotifier<int> _positionUs = ValueNotifier<int>(0);

  /// Au moins une image du fichier ouvert a été décodée (bascule le canevas
  /// du panneau descriptif vers la surface vidéo).
  bool _aRecuUneTrame = false;

  /// Décodage `decodeImageFromPixels` en cours (les images arrivant pendant
  /// ce temps sont abandonnées — même garde que la session live).
  bool _decodageEnCours = false;

  /// Appel `recording_next_frame` en vol (borne la boucle à une image à la
  /// fois : si le décodage est plus lent que la cadence, on saute des ticks).
  bool _avanceEnCours = false;

  /// Boucle de lecture cadencée au fps nominal de l'enregistrement.
  Timer? _minuterieLecture;

  /// Lecture en cours (icône pause) ou à l'arrêt (icône lecture).
  bool _enLecture = false;

  /// Fin de flux atteinte : le prochain « Lecture » rembobine d'abord.
  bool _finAtteinte = false;

  /// Dernier repositionnement demandé mais pas encore envoyé (les scrubs
  /// rapprochés sont fusionnés : un seul `recording_seek` en vol à la fois).
  int? _seekEnAttenteUs;

  /// Un `recording_seek` est en vol.
  bool _seekEnVol = false;

  NativeApi get _api => ref.read(nativeApiProvider);

  @override
  void dispose() {
    // Ferme le lecteur (annule la minuterie, libère l'`ui.Image`, puis
    // `close_recording` best-effort) avant de jeter les notifieurs — la
    // partie synchrone de `_fermerLecteur` s'exécute avant leur `dispose()`.
    unawaited(_fermerLecteur());
    _trameCourante.dispose();
    _positionUs.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    final t = NovaTokens.of(context);
    final asyncRec = ref.watch(recordingsProvider);
    final enregistrements = asyncRec.valueOrNull ?? const <RecordingDto>[];

    if (asyncRec.isLoading && enregistrements.isEmpty) {
      return const Center(
        child: SizedBox(
          width: 22,
          height: 22,
          child: CircularProgressIndicator(strokeWidth: 2),
        ),
      );
    }
    if (enregistrements.isEmpty) {
      // Plus rien à lire : referme un lecteur resté ouvert (hors passe de
      // build — la fermeture touche des notifieurs écoutés par ce sous-arbre).
      if (_lecteurId != null || _cheminEnOuverture != null) {
        unawaited(Future<void>.microtask(_fermerLecteur));
      }
      return const NovaEmptyState(
        icone: NovaIcones.enregistrements,
        titre: 'Aucun enregistrement',
        sousTitre: 'Les sessions enregistrées apparaîtront ici.',
      );
    }

    final index = _indexSelectionne.clamp(0, enregistrements.length - 1);
    _assurerOuverture(enregistrements[index]);
    return Row(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        _construireListe(t, enregistrements, index),
        Expanded(child: _construireScene(t, enregistrements[index])),
      ],
    );
  }

  // -------------------------------------------------------------------------
  // Cycle de vie du lecteur (ouverture / fermeture réelles)
  // -------------------------------------------------------------------------

  /// Garantit que l'enregistrement sélectionné est ouvert (ou en cours de
  /// l'être). Appelé à chaque build : sans effet si [enr] est déjà ouvert, en
  /// cours d'ouverture, ou en échec (re-tenté au prochain clic).
  void _assurerOuverture(RecordingDto enr) {
    if (_cheminOuvert == enr.chemin ||
        _cheminEnOuverture == enr.chemin ||
        _cheminEnEchec == enr.chemin) {
      return;
    }
    // Micro-tâche : l'ouverture ferme l'ancien lecteur et écrit dans des
    // notifieurs écoutés par ce sous-arbre — jamais pendant la passe de build.
    unawaited(Future<void>.microtask(() => _ouvrir(enr)));
  }

  /// Ferme le lecteur courant puis ouvre [enr] : `open_recording`, image
  /// d'affiche (première image, lecteur en pause), position remise à zéro.
  Future<void> _ouvrir(RecordingDto enr) async {
    if (_cheminOuvert == enr.chemin || _cheminEnOuverture == enr.chemin) {
      return;
    }
    _cheminEnOuverture = enr.chemin;
    try {
      await _fermerLecteur();
      if (!mounted) return;
      // Reflète immédiatement l'arrêt de l'ancien lecteur (icône, canevas).
      setState(() {});
      final generation = ++_generationOuverture;
      try {
        final info = await _api.openRecording(enr.chemin);
        if (!mounted || generation != _generationOuverture) {
          // Résultat périmé (écran quitté, autre sélection, liste vidée) :
          // l'identifiant fraîchement ouvert est refermé sans bruit.
          unawaited(_fermerSansBruit(info.id));
          return;
        }
        setState(() {
          _lecteurId = info.id;
          _infoLecteur = info;
          _cheminOuvert = enr.chemin;
          _cheminEnEchec = null;
        });
        _positionUs.value = 0;
        // Image d'affiche : première image décodée, sans avancer le compteur.
        unawaited(_avancerImage(compterTemps: false));
      } catch (e) {
        if (mounted && generation == _generationOuverture) {
          _cheminEnEchec = enr.chemin;
          NovaToast.montrer(
              context, 'Ouverture impossible : ${_messageErreur(e)}');
        }
      }
    } finally {
      if (_cheminEnOuverture == enr.chemin) _cheminEnOuverture = null;
    }
  }

  /// Arrête la lecture et ferme le lecteur courant : minuterie annulée,
  /// `ui.Image` libérée, position remise à zéro, `close_recording`
  /// best-effort. Invalide au passage toute ouverture encore en vol. Aucune
  /// mutation après le premier `await` (sûr depuis `dispose`).
  Future<void> _fermerLecteur() async {
    _generationOuverture++;
    _minuterieLecture?.cancel();
    _minuterieLecture = null;
    _enLecture = false;
    _finAtteinte = false;
    _seekEnAttenteUs = null;
    _aRecuUneTrame = false;
    final id = _lecteurId;
    _lecteurId = null;
    _infoLecteur = null;
    _cheminOuvert = null;
    _positionUs.value = 0;
    final ancienne = _trameCourante.value;
    _trameCourante.value = null;
    ancienne?.dispose();
    if (id != null) {
      await _fermerSansBruit(id);
    }
  }

  /// `close_recording` best-effort : l'échec de fermeture n'est pas bloquant
  /// (l'identifiant est déjà invalidé côté UI).
  Future<void> _fermerSansBruit(int id) async {
    try {
      await _api.closeRecording(id);
    } catch (_) {
      // Fermeture best-effort.
    }
  }

  String _messageErreur(Object e) =>
      e is NovaApiException ? e.message : e.toString();

  // -------------------------------------------------------------------------
  // Lecture (boucle cadencée au fps réel, play/pause, fin de flux)
  // -------------------------------------------------------------------------

  /// Intervalle entre deux images (µs) d'après la cadence nominale du fichier.
  int _intervalleImageUs(RecordingInfoDto info) =>
      1000000 ~/ math.max(1, info.fps);

  /// Lecture ↔ pause. Si la fin est atteinte, rembobine (`recording_seek` à
  /// 0) avant de relire. Si le lecteur n'est pas prêt (ouverture en cours ou
  /// échouée), retente l'ouverture de [enr].
  Future<void> _basculerLecture(RecordingDto enr) async {
    final id = _lecteurId;
    final info = _infoLecteur;
    if (id == null || info == null) {
      _cheminEnEchec = null;
      _assurerOuverture(enr);
      return;
    }
    if (_enLecture) {
      _mettreEnPause();
      return;
    }
    if (_finAtteinte) {
      try {
        await _api.recordingSeek(id, 0);
      } catch (e) {
        if (mounted) {
          NovaToast.montrer(
              context, 'Rembobinage impossible : ${_messageErreur(e)}');
        }
        return;
      }
      if (!mounted || _lecteurId != id) return;
      _positionUs.value = 0;
      _finAtteinte = false;
    }
    _minuterieLecture?.cancel();
    _minuterieLecture = Timer.periodic(
      Duration(microseconds: _intervalleImageUs(info)),
      (_) => unawaited(_avancerImage(compterTemps: true)),
    );
    setState(() => _enLecture = true);
    // Première image sans attendre la première période.
    unawaited(_avancerImage(compterTemps: true));
  }

  /// Suspend la lecture (la position et l'image courantes sont conservées).
  void _mettreEnPause() {
    _minuterieLecture?.cancel();
    _minuterieLecture = null;
    if (_enLecture) setState(() => _enLecture = false);
  }

  /// Tire la prochaine image (`recording_next_frame`) et la peint.
  /// [compterTemps] avance le compteur d'un intervalle d'image (faux pour
  /// l'image d'affiche et l'aperçu après scrub en pause). `null` = fin de
  /// flux → arrêt (le prochain « Lecture » rembobine).
  Future<void> _avancerImage({required bool compterTemps}) async {
    if (_avanceEnCours) return;
    final id = _lecteurId;
    final info = _infoLecteur;
    if (id == null || info == null) return;
    _avanceEnCours = true;
    try {
      final trame = await _api.recordingNextFrame(id);
      if (!mounted || _lecteurId != id) return;
      if (trame == null) {
        _surFinDeFlux(info);
        return;
      }
      if (compterTemps) {
        _positionUs.value = math.min(
            _positionUs.value + _intervalleImageUs(info), info.dureeUs);
      }
      _peindre(trame);
    } catch (e) {
      if (!mounted || _lecteurId != id) return;
      _mettreEnPause();
      NovaToast.montrer(
          context, 'Lecture interrompue : ${_messageErreur(e)}');
    } finally {
      _avanceEnCours = false;
    }
  }

  /// Fin de flux : arrêt de la boucle, position calée sur la durée totale.
  void _surFinDeFlux(RecordingInfoDto info) {
    _minuterieLecture?.cancel();
    _minuterieLecture = null;
    _positionUs.value = info.dureeUs;
    if (_enLecture || !_finAtteinte) {
      setState(() {
        _enLecture = false;
        _finAtteinte = true;
      });
    }
  }

  /// Décode la trame RGBA en `ui.Image` puis la publie — patron identique à
  /// la surface live de `session_screen.dart` (`_surTrameVideo`) : garde de
  /// décodage, libération de l'image précédente, premier `setState` quand la
  /// première image arrive.
  void _peindre(VideoFrameDto trame) {
    if (_decodageEnCours) return;
    _decodageEnCours = true;
    ui.decodeImageFromPixels(
      trame.rgba,
      trame.width,
      trame.height,
      ui.PixelFormat.rgba8888,
      (ui.Image image) {
        _decodageEnCours = false;
        if (!mounted) {
          image.dispose();
          return;
        }
        final ancienne = _trameCourante.value;
        _trameCourante.value = image;
        ancienne?.dispose();
        if (!_aRecuUneTrame) {
          setState(() => _aRecuUneTrame = true);
        }
      },
    );
  }

  // -------------------------------------------------------------------------
  // Scrub de la timeline (`recording_seek`)
  // -------------------------------------------------------------------------

  /// Positionne la lecture à [fraction] (0..1) de la durée : retour visuel
  /// immédiat puis `recording_seek` (fusionné si un envoi est déjà en vol).
  void _scruber(double fraction) {
    final info = _infoLecteur;
    if (_lecteurId == null || info == null || info.dureeUs <= 0) return;
    final ts = (fraction.clamp(0.0, 1.0) * info.dureeUs).round();
    _positionUs.value = ts;
    if (_finAtteinte && ts < info.dureeUs) {
      setState(() => _finAtteinte = false);
    }
    _seekEnAttenteUs = ts;
    unawaited(_pomperSeeks());
  }

  /// Envoie les repositionnements en attente, un seul à la fois : pendant un
  /// scrub à la souris, seule la dernière position est transmise. En pause,
  /// l'image-clé visée est tirée et affichée (aperçu immédiat).
  Future<void> _pomperSeeks() async {
    if (_seekEnVol) return;
    _seekEnVol = true;
    try {
      while (mounted && _seekEnAttenteUs != null) {
        final id = _lecteurId;
        if (id == null) {
          _seekEnAttenteUs = null;
          break;
        }
        final ts = _seekEnAttenteUs!;
        _seekEnAttenteUs = null;
        try {
          await _api.recordingSeek(id, ts);
        } catch (e) {
          if (mounted) {
            NovaToast.montrer(
                context, 'Positionnement impossible : ${_messageErreur(e)}');
          }
          break;
        }
        if (!mounted || _lecteurId != id) break;
        if (_seekEnAttenteUs == null && !_enLecture) {
          await _avancerImage(compterTemps: false);
        }
      }
    } finally {
      _seekEnVol = false;
    }
  }

  // -------------------------------------------------------------------------
  // Liste latérale (maquette `.reclist`)
  // -------------------------------------------------------------------------

  Widget _construireListe(
      NovaTokens t, List<RecordingDto> enregistrements, int index) {
    return Container(
      width: 228,
      decoration: BoxDecoration(
        border: Border(right: BorderSide(color: t.filet)),
      ),
      child: ListView.builder(
        padding: const EdgeInsets.all(8),
        itemCount: enregistrements.length,
        itemBuilder: (context, i) => _LigneEnregistrement(
          enregistrement: enregistrements[i],
          selectionne: i == index,
          onTap: () => setState(() {
            _cheminEnEchec = null; // un nouveau clic retente une ouverture
            _indexSelectionne = i;
          }),
        ),
      ),
    );
  }

  // -------------------------------------------------------------------------
  // Scène de lecture (maquette `.stage`)
  // -------------------------------------------------------------------------

  Widget _construireScene(NovaTokens t, RecordingDto enr) {
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        Expanded(child: _construireCanevas(enr)),
        _construireControles(t, enr),
      ],
    );
  }

  /// Canevas (maquette `.canvas`) : surface vidéo réelle dès qu'une image du
  /// fichier ouvert est décodée, sinon panneau descriptif (pendant
  /// l'ouverture ou après un échec). Le dégradé sombre de la maquette sert de
  /// fond letterbox derrière la vidéo.
  Widget _construireCanevas(RecordingDto enr) {
    final montrerVideo = _aRecuUneTrame && _cheminOuvert == enr.chemin;
    return Container(
      alignment: Alignment.center,
      decoration: const BoxDecoration(
        gradient: LinearGradient(
          begin: Alignment.topLeft,
          end: Alignment.bottomRight,
          colors: [_canevasHaut, _canevasBas],
        ),
      ),
      child: montrerVideo
          ? SizedBox.expand(
              child: RepaintBoundary(
                child: CustomPaint(
                  painter: _PeintreEnregistrement(_trameCourante),
                  size: Size.infinite,
                ),
              ),
            )
          : Column(
              mainAxisSize: MainAxisSize.min,
              crossAxisAlignment: CrossAxisAlignment.center,
              children: [
                Text.rich(
                  TextSpan(
                    text: 'Enregistrement — ',
                    style: const TextStyle(fontSize: 13, color: _canevasTexte),
                    children: [
                      TextSpan(
                        text: enr.nom,
                        style: const TextStyle(
                          color: _canevasTitre,
                          fontWeight: FontWeight.w700,
                        ),
                      ),
                    ],
                  ),
                  textAlign: TextAlign.center,
                ),
                const SizedBox(height: 4),
                Text(
                  _detailCanevas(enr),
                  textAlign: TextAlign.center,
                  style: TextStyle(
                    fontSize: 11,
                    color: _canevasTexte.withValues(alpha: 0.7),
                  ),
                ),
              ],
            ),
    );
  }

  /// Durée totale affichée (s) : celle du lecteur ouvert (`duree_us` réelle),
  /// sinon celle des métadonnées du fichier.
  double _dureeTotaleS(RecordingDto enr) {
    final info = _infoLecteur;
    return info == null ? enr.dureeS : info.dureeUs / 1e6;
  }

  /// Barre de contrôles de lecture (maquette `.pctrl`) : lecture/pause
  /// réelles, compteur `mm:ss / mm:ss` (chiffres tabulaires) et timeline
  /// scrubbable. Seul le plein écran du lecteur reste à venir (toast honnête).
  Widget _construireControles(NovaTokens t, RecordingDto enr) {
    void pleinEcranAVenir() => NovaToast.montrer(
        context, 'Plein écran du lecteur — à venir.',
        info: true);
    return Container(
      height: 50,
      padding: const EdgeInsets.symmetric(horizontal: 16),
      decoration: BoxDecoration(
        border: Border(top: BorderSide(color: t.filet)),
      ),
      child: Row(
        children: [
          _BoutonLecture(
            enLecture: _enLecture,
            onTap: () => unawaited(_basculerLecture(enr)),
          ),
          const SizedBox(width: 12),
          ValueListenableBuilder<int>(
            valueListenable: _positionUs,
            builder: (context, us, _) =>
                Text(_formaterDuree(us / 1e6), style: _styleTemps(t.texte2)),
          ),
          const SizedBox(width: 12),
          Expanded(child: _construireTimeline(t)),
          const SizedBox(width: 12),
          Text(_formaterDuree(_dureeTotaleS(enr)),
              style: _styleTemps(t.texte3)),
          const SizedBox(width: 12),
          NovaBoutonAction(
            icone: NovaIcones.agrandirCadre,
            infobulle: 'Plein écran',
            onTap: pleinEcranAVenir,
          ),
        ],
      ),
    );
  }

  /// Style commun des étiquettes de temps (12 px, chiffres tabulaires).
  TextStyle _styleTemps(Color couleur) => TextStyle(
        fontSize: 12,
        color: couleur,
        fontFeatures: const [FontFeature.tabularFigures()],
      );

  /// Ligne de temps réelle (maquette `.timeline`) : fond neutre, remplissage
  /// rouge à la fraction `position / durée` et bouton rond à son extrémité.
  /// Clic et glisser horizontaux → [_scruber] (`recording_seek`).
  Widget _construireTimeline(NovaTokens t) {
    final dureeUs = _infoLecteur?.dureeUs ?? 0;
    return SizedBox(
      height: 11,
      child: LayoutBuilder(
        builder: (context, contraintes) {
          final largeur = contraintes.maxWidth;
          double fractionDepuis(double dx) =>
              largeur <= 0 ? 0 : (dx / largeur).clamp(0.0, 1.0);
          return MouseRegion(
            cursor: SystemMouseCursors.click,
            child: GestureDetector(
              behavior: HitTestBehavior.opaque,
              onTapDown: (d) => _scruber(fractionDepuis(d.localPosition.dx)),
              onHorizontalDragStart: (d) =>
                  _scruber(fractionDepuis(d.localPosition.dx)),
              onHorizontalDragUpdate: (d) =>
                  _scruber(fractionDepuis(d.localPosition.dx)),
              child: ValueListenableBuilder<int>(
                valueListenable: _positionUs,
                builder: (context, positionUs, _) {
                  final fraction = dureeUs <= 0
                      ? 0.0
                      : (positionUs / dureeUs).clamp(0.0, 1.0);
                  return Stack(
                    clipBehavior: Clip.none,
                    alignment: Alignment.centerLeft,
                    children: [
                      // Barre de fond (4 px).
                      Container(
                        height: 4,
                        width: largeur,
                        decoration: BoxDecoration(
                          color: t.filetFort,
                          borderRadius: BorderRadius.circular(2),
                        ),
                      ),
                      // Remplissage : progression réelle de la lecture.
                      Container(
                        height: 4,
                        width: largeur * fraction,
                        decoration: BoxDecoration(
                          color: kNovaRouge,
                          borderRadius: BorderRadius.circular(2),
                        ),
                      ),
                      // Bouton (knob) centré sur l'extrémité du remplissage.
                      Positioned(
                        left: largeur * fraction - 5.5,
                        top: 0,
                        bottom: 0,
                        child: Center(
                          child: Container(
                            width: 11,
                            height: 11,
                            decoration: const BoxDecoration(
                              color: kNovaRouge,
                              shape: BoxShape.circle,
                            ),
                          ),
                        ),
                      ),
                    ],
                  );
                },
              ),
            ),
          );
        },
      ),
    );
  }
}

// ===========================================================================
// Ligne d'enregistrement (maquette `.rec`)
// ===========================================================================

/// Une entrée de la liste latérale : vignette + nom + méta, avec survol
/// (`.rec:hover`) et état sélectionné (`.rec.on`).
class _LigneEnregistrement extends StatefulWidget {
  const _LigneEnregistrement({
    required this.enregistrement,
    required this.selectionne,
    required this.onTap,
  });

  final RecordingDto enregistrement;
  final bool selectionne;
  final VoidCallback onTap;

  @override
  State<_LigneEnregistrement> createState() => _LigneEnregistrementState();
}

class _LigneEnregistrementState extends State<_LigneEnregistrement> {
  bool _survole = false;

  @override
  Widget build(BuildContext context) {
    final t = NovaTokens.of(context);
    // `.rec.on` l'emporte sur `.rec:hover` (même spécificité, règle plus tardive).
    final Color fond = widget.selectionne
        ? t.selection
        : (_survole ? t.survol : Colors.transparent);
    return MouseRegion(
      cursor: SystemMouseCursors.click,
      onEnter: (_) => setState(() => _survole = true),
      onExit: (_) => setState(() => _survole = false),
      child: GestureDetector(
        onTap: widget.onTap,
        behavior: HitTestBehavior.opaque,
        child: Container(
          padding: const EdgeInsets.all(8),
          decoration: BoxDecoration(
            color: fond,
            borderRadius: BorderRadius.circular(kNovaRayon),
          ),
          child: Row(
            children: [
              // Vignette `.th` (surface d'aperçu sombre).
              Container(
                width: 54,
                height: 32,
                alignment: Alignment.center,
                decoration: BoxDecoration(
                  color: _fondVignette,
                  borderRadius: BorderRadius.circular(3),
                ),
                child: const NovaIcone(NovaIcones.lecture,
                    taille: 16, couleur: _icoVignette),
              ),
              const SizedBox(width: 10),
              Expanded(
                child: Column(
                  crossAxisAlignment: CrossAxisAlignment.start,
                  mainAxisSize: MainAxisSize.min,
                  children: [
                    Text(
                      widget.enregistrement.nom,
                      maxLines: 1,
                      overflow: TextOverflow.ellipsis,
                      style: TextStyle(
                        fontSize: 12.5,
                        fontWeight: FontWeight.w500,
                        color: t.texte,
                      ),
                    ),
                    const SizedBox(height: 2),
                    Text(
                      _meta(widget.enregistrement),
                      maxLines: 1,
                      overflow: TextOverflow.ellipsis,
                      style: TextStyle(
                        fontSize: 11,
                        color: t.texte3,
                        fontFeatures: const [FontFeature.tabularFigures()],
                      ),
                    ),
                  ],
                ),
              ),
            ],
          ),
        ),
      ),
    );
  }
}

// ===========================================================================
// Bouton de lecture / pause (maquette `.pbtn`)
// ===========================================================================

/// Bouton rond rouge lecture ↔ pause, focusable au clavier (maquette `.pbtn`).
class _BoutonLecture extends StatelessWidget {
  const _BoutonLecture({required this.enLecture, required this.onTap});

  /// Lecture en cours : icône pause ; sinon icône lecture.
  final bool enLecture;

  final VoidCallback onTap;

  @override
  Widget build(BuildContext context) {
    return NovaActivable(
      onTap: onTap,
      rayonFocus: 16,
      label: enLecture ? 'Pause' : 'Lecture',
      builder: (context, survole, focus) => Container(
        width: 32,
        height: 32,
        alignment: Alignment.center,
        decoration: BoxDecoration(
          color: survole ? kNovaRougePresse : kNovaRouge,
          shape: BoxShape.circle,
        ),
        child: NovaIcone(
          enLecture ? LucideIcons.pause : NovaIcones.lecture,
          taille: 15,
          couleur: Colors.white,
        ),
      ),
    );
  }
}

// ===========================================================================
// Peintre de la surface vidéo du lecteur
// ===========================================================================

/// Peint l'image courante de l'enregistrement en conservant le ratio
/// (letterbox), **sans aucun plugin natif** — même patron que le
/// `_PeintreVideo` de la surface live de session (mode « adapter »).
class _PeintreEnregistrement extends CustomPainter {
  _PeintreEnregistrement(this.trame) : super(repaint: trame);

  final ValueListenable<ui.Image?> trame;

  static final Paint _peinture = Paint()
    ..filterQuality = FilterQuality.medium
    ..isAntiAlias = false;

  @override
  void paint(Canvas canvas, Size size) {
    final image = trame.value;
    if (image == null || size.isEmpty) return;
    final double iw = image.width.toDouble();
    final double ih = image.height.toDouble();
    if (iw <= 0 || ih <= 0) return;
    final echelle = math.min(size.width / iw, size.height / ih);
    final double dw = iw * echelle;
    final double dh = ih * echelle;
    final double dx = (size.width - dw) / 2;
    final double dy = (size.height - dh) / 2;
    canvas.drawImageRect(
      image,
      Rect.fromLTWH(0, 0, iw, ih),
      Rect.fromLTWH(dx, dy, dw, dh),
      _peinture,
    );
  }

  @override
  bool shouldRepaint(covariant _PeintreEnregistrement old) =>
      old.trame != trame;
}
