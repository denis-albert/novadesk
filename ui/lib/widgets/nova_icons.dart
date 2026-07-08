/// Jeu d'icônes NovaDesk — **police vectorielle professionnelle Lucide**
/// (paquet pur-Dart `lucide_icons`, aucune icône dessinée à la main, aucun
/// plugin natif). Les tracés sont exactement ceux de la maquette validée
/// (`novadesk-app.html`, qui charge `lucide@latest`), pour un rendu 1:1.
///
/// Compatibilité : [NovaIconeData] est un simple alias de [IconData] et le
/// widget [NovaIcone] conserve sa signature historique (`taille`, `couleur`),
/// si bien que tous les appels existants `NovaIcone(NovaIcones.x, taille: …)`
/// continuent de fonctionner — désormais adossés à la fonte Lucide.
library;

import 'package:flutter/widgets.dart';
import 'package:lucide_icons_flutter/lucide_icons.dart';

/// Une icône NovaDesk est un [IconData] Lucide (alias de compatibilité).
typedef NovaIconeData = IconData;

/// Icône vectorielle NovaDesk (glyphe Lucide). Couleur héritée de [IconTheme]
/// si [couleur] est absente ; taille par défaut 18 px (barre d'outils).
class NovaIcone extends StatelessWidget {
  const NovaIcone(this.icone, {super.key, this.taille = 18, this.couleur});

  final IconData icone;
  final double taille;
  final Color? couleur;

  @override
  Widget build(BuildContext context) {
    return Icon(icone, size: taille, color: couleur);
  }
}

/// Catalogue des icônes utilisées par l'UI, adossé à la fonte Lucide.
///
/// Les noms **français historiques** sont conservés (compatibilité ascendante
/// des écrans existants) et complétés par les noms de la maquette. Tout pointe
/// vers un glyphe Lucide — jamais un tracé maison.
abstract final class NovaIcones {
  // --- Contrôles de fenêtre / onglets ---------------------------------------
  static const IconData reduire = LucideIcons.minus;
  static const IconData agrandir = LucideIcons.square;
  static const IconData fermer = LucideIcons.x;
  static const IconData plus = LucideIcons.plus;

  // --- Navigation (rail) ----------------------------------------------------
  static const IconData accueil = LucideIcons.home;
  static const IconData carnet = LucideIcons.bookOpen;
  static const IconData enregistrements = LucideIcons.clapperboard;
  static const IconData reglages = LucideIcons.settings;

  // --- Accueil : poste distant / ce poste -----------------------------------
  static const IconData adresse = LucideIcons.arrowRightLeft;
  static const IconData flecheDroite = LucideIcons.arrowRight;
  static const IconData controle = LucideIcons.mousePointer2;
  static const IconData observation = LucideIcons.eye;
  static const IconData fichiers = LucideIcons.folder;
  static const IconData cast = LucideIcons.cast;
  static const IconData tag = LucideIcons.tag;
  static const IconData copier = LucideIcons.copy;
  static const IconData lien = LucideIcons.link;
  static const IconData inviter = LucideIcons.link;
  static const IconData partager = LucideIcons.share2;
  static const IconData info = LucideIcons.info;

  // --- Écrans / affichage ---------------------------------------------------
  static const IconData moniteur = LucideIcons.monitor;
  static const IconData moniteurs = LucideIcons.monitor;
  static const IconData tousEcrans = LucideIcons.layoutGrid;
  static const IconData qualite = LucideIcons.slidersHorizontal;
  static const IconData affichage = LucideIcons.scaling;
  static const IconData pleinEcran = LucideIcons.maximize;
  static const IconData quitterPleinEcran = LucideIcons.minimize;
  static const IconData cadre = LucideIcons.frame;
  static const IconData image = LucideIcons.image;
  static const IconData disque = LucideIcons.hardDrive;

  // --- Entrées / session ----------------------------------------------------
  static const IconData clavier = LucideIcons.keyboard;
  static const IconData ctrlAltSuppr = LucideIcons.delete;
  static const IconData pressePapiers = LucideIcons.clipboard;
  static const IconData souris = LucideIcons.mousePointer2;
  static const IconData bloquer = LucideIcons.ban;
  static const IconData changerCote = LucideIcons.chevronsLeftRight;

  // --- Outils de session ----------------------------------------------------
  static const IconData dossier = LucideIcons.folder;
  static const IconData dossierSync = LucideIcons.folderSync;
  static const IconData discussion = LucideIcons.messageSquare;
  static const IconData tableauBlanc = LucideIcons.penTool;
  static const IconData enregistrer = LucideIcons.circleDot;
  static const IconData troisPoints = LucideIcons.moreHorizontal;

  // --- Whiteboard mini-outils ----------------------------------------------
  static const IconData crayonOutil = LucideIcons.penTool;
  static const IconData carre = LucideIcons.square;
  static const IconData flecheDiagonale = LucideIcons.arrowUpRight;
  static const IconData cercle = LucideIcons.circle;
  static const IconData gomme = LucideIcons.eraser;

  // --- Transfert de fichiers ------------------------------------------------
  static const IconData fichierTexte = LucideIcons.fileText;
  static const IconData fichierArchive = LucideIcons.fileArchive;
  static const IconData telecharger = LucideIcons.arrowDown;
  static const IconData serveur = LucideIcons.server;

  // --- Sécurité -------------------------------------------------------------
  static const IconData cadenas = LucideIcons.lock;
  static const IconData bouclier = LucideIcons.shield;
  static const IconData bouclierCoche = LucideIcons.shieldCheck;
  static const IconData cle = LucideIcons.key;
  static const IconData confidentialite = LucideIcons.eyeOff;
  static const IconData globe = LucideIcons.globe;

  // --- Actions à distance ---------------------------------------------------
  static const IconData eclair = LucideIcons.zap;
  static const IconData capture = LucideIcons.camera;
  static const IconData recharger = LucideIcons.refreshCw;
  static const IconData redemarrer = LucideIcons.rotateCw;
  static const IconData terminal = LucideIcons.terminal;
  static const IconData alimentation = LucideIcons.power;

  // --- Favoris / édition ----------------------------------------------------
  static const IconData etoile = LucideIcons.star;
  static const IconData etoilePleine = LucideIcons.star;
  static const IconData crayon = LucideIcons.pencil;
  static const IconData corbeille = LucideIcons.trash2;
  static const IconData coche = LucideIcons.check;

  // --- Divers ---------------------------------------------------------------
  static const IconData utilisateur = LucideIcons.user;
  static const IconData horloge = LucideIcons.clock;
  static const IconData audio = LucideIcons.volume2;
  static const IconData imprimante = LucideIcons.printer;
  static const IconData capturePartage = LucideIcons.crop;
  static const IconData reseau = LucideIcons.wifi;
  static const IconData avertissement = LucideIcons.alertTriangle;
  static const IconData oeil = LucideIcons.eye;
  static const IconData oeilBarre = LucideIcons.eyeOff;
  static const IconData chevronBas = LucideIcons.chevronDown;
  static const IconData chevronDroit = LucideIcons.chevronRight;
  static const IconData lienCoupe = LucideIcons.unlink;
  static const IconData lune = LucideIcons.moon;
  static const IconData liste = LucideIcons.list;
  static const IconData radar = LucideIcons.radar;
  static const IconData rechercher = LucideIcons.search;
  static const IconData importer = LucideIcons.download;
  static const IconData exporter = LucideIcons.upload;
  static const IconData lecture = LucideIcons.play;
  static const IconData agrandirCadre = LucideIcons.maximize;
}
