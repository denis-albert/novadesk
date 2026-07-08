/// Dialogue d'acceptation d'une connexion **entrante** (côté poste contrôlé,
/// maquette `novadesk-app.html`, `#bdDlg`) : avatar + identité du connecteur,
/// empreinte à vérifier hors bande, profil de permissions, cases de permissions
/// (coche verte « accordé »), boutons Refuser / Accepter (vert « accordé » — le
/// rouge de marque reste réservé).
///
/// Ouvert par le flux réel des demandes entrantes de l'hôte non surveillé
/// (`unattended_incoming_stream`) ; sa décision est transmise à
/// `approve_incoming`.
library;

import 'package:flutter/material.dart';

import '../theme/motion.dart';
import '../theme/nova_theme.dart';
import '../widgets/nova_icons.dart';
import '../widgets/nova_kit.dart';

/// Profils de permissions proposés à l'acceptation.
enum ProfilPermissions { parDefaut, partageEcran, controleTotal, nonSurveille }

extension _ProfilX on ProfilPermissions {
  String get libelle => switch (this) {
        ProfilPermissions.parDefaut => 'Par défaut',
        ProfilPermissions.partageEcran => "Partage d'écran",
        ProfilPermissions.controleTotal => 'Contrôle total',
        ProfilPermissions.nonSurveille => 'Non surveillé',
      };
}

/// Résultat rendu par [IncomingRequestDialog.montrer].
class ReponseEntrante {
  const ReponseEntrante({required this.acceptee, required this.profil});

  final bool acceptee;
  final ProfilPermissions profil;
}

class IncomingRequestDialog extends StatefulWidget {
  const IncomingRequestDialog({
    super.key,
    this.alias = 'pc-marie',
    this.idFormate = '555 240 173',
    this.empreinte = '3F·A9·7C·22·E1·08',
  });

  final String alias;
  final String idFormate;
  final String empreinte;

  static Future<ReponseEntrante?> montrer(
    BuildContext context, {
    String alias = 'pc-marie',
    String idFormate = '555 240 173',
    String empreinte = '3F·A9·7C·22·E1·08',
  }) {
    return montrerDialogueNova<ReponseEntrante>(
      context: context,
      builder: (context) => IncomingRequestDialog(
        alias: alias,
        idFormate: idFormate,
        empreinte: empreinte,
      ),
    );
  }

  @override
  State<IncomingRequestDialog> createState() => _IncomingRequestDialogState();
}

class _IncomingRequestDialogState extends State<IncomingRequestDialog> {
  ProfilPermissions _profil = ProfilPermissions.controleTotal;

  late final Map<String, bool> _permissions = {
    'Contrôler le clavier et la souris': true,
    'Accéder au presse-papiers': true,
    'Transférer des fichiers': false,
    'Transmettre le son': false,
    "Bloquer l'entrée locale": false,
  };

  void _appliquerProfil(ProfilPermissions profil) {
    setState(() {
      _profil = profil;
      switch (profil) {
        case ProfilPermissions.parDefaut:
          _permissions
            ..['Contrôler le clavier et la souris'] = true
            ..['Accéder au presse-papiers'] = true
            ..['Transférer des fichiers'] = false
            ..['Transmettre le son'] = false
            ..["Bloquer l'entrée locale"] = false;
        case ProfilPermissions.partageEcran:
          _permissions.updateAll((_, __) => false);
        case ProfilPermissions.controleTotal:
          _permissions
            ..['Contrôler le clavier et la souris'] = true
            ..['Accéder au presse-papiers'] = true
            ..['Transférer des fichiers'] = true
            ..['Transmettre le son'] = true
            ..["Bloquer l'entrée locale"] = false;
        case ProfilPermissions.nonSurveille:
          _permissions.updateAll((_, __) => true);
      }
    });
  }

  String get _initiales {
    final mots = widget.alias
        .split(RegExp(r'[\s\-_.]+'))
        .where((m) => m.isNotEmpty)
        .toList();
    if (mots.isEmpty) return '?';
    if (mots.length == 1) {
      return mots.first
          .substring(0, mots.first.length >= 2 ? 2 : 1)
          .toUpperCase();
    }
    return (mots[0][0] + mots[1][0]).toUpperCase();
  }

  @override
  Widget build(BuildContext context) {
    final t = NovaTokens.of(context);
    return Dialog(
      child: ConstrainedBox(
        constraints: const BoxConstraints(maxWidth: 400),
        child: Column(
          mainAxisSize: MainAxisSize.min,
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            // Bandeau supérieur : avatar + identité.
            Padding(
              padding: const EdgeInsets.fromLTRB(20, 17, 20, 17),
              child: Row(
                children: [
                  Container(
                    width: 42,
                    height: 42,
                    alignment: Alignment.center,
                    decoration: BoxDecoration(
                      color: kNovaRouge,
                      borderRadius: BorderRadius.circular(kNovaRayon),
                    ),
                    child: Text(
                      _initiales,
                      style: const TextStyle(
                          fontSize: 15,
                          fontWeight: FontWeight.w700,
                          color: Colors.white),
                    ),
                  ),
                  const SizedBox(width: 13),
                  Expanded(
                    child: Column(
                      crossAxisAlignment: CrossAxisAlignment.start,
                      children: [
                        Text('Demande de connexion',
                            style: TextStyle(
                                fontSize: 15,
                                fontWeight: FontWeight.w600,
                                color: t.texte)),
                        const SizedBox(height: 1),
                        Text('${widget.alias} · ${widget.idFormate}',
                            style: TextStyle(fontSize: 12, color: t.texte3)),
                      ],
                    ),
                  ),
                ],
              ),
            ),
            Divider(height: 1, color: t.filet),
            Padding(
              padding: const EdgeInsets.fromLTRB(20, 16, 20, 8),
              child: Column(
                crossAxisAlignment: CrossAxisAlignment.start,
                children: [
                  // Empreinte.
                  Row(
                    children: [
                      NovaIcone(NovaIcones.bouclierCoche,
                          taille: 14, couleur: t.vert),
                      const SizedBox(width: 8),
                      Text('Empreinte : ',
                          style: TextStyle(fontSize: 11.5, color: t.texte2)),
                      Flexible(
                        child: Text(
                          widget.empreinte,
                          maxLines: 1,
                          overflow: TextOverflow.ellipsis,
                          style: TextStyle(
                            fontSize: 12,
                            color: t.texte2,
                            letterSpacing: 1,
                            fontFamily: 'Cascadia Code',
                            fontFamilyFallback: const ['Consolas', 'monospace'],
                          ),
                        ),
                      ),
                    ],
                  ),
                  const SizedBox(height: 14),
                  // Profil.
                  Row(
                    children: [
                      const NovaSectionLabel('Profil'),
                      const Spacer(),
                      DropdownButton<ProfilPermissions>(
                        value: _profil,
                        underline: const SizedBox.shrink(),
                        style: TextStyle(fontSize: 12.5, color: t.texte),
                        items: [
                          for (final p in ProfilPermissions.values)
                            DropdownMenuItem(value: p, child: Text(p.libelle)),
                        ],
                        onChanged: (p) {
                          if (p != null) _appliquerProfil(p);
                        },
                      ),
                    ],
                  ),
                  const SizedBox(height: 2),
                  for (final entree in _permissions.entries)
                    _lignePermission(t, entree.key, entree.value,
                        (v) => setState(() => _permissions[entree.key] = v)),
                ],
              ),
            ),
            Divider(height: 1, color: t.filet),
            // Décision.
            Padding(
              padding: const EdgeInsets.fromLTRB(20, 14, 20, 14),
              child: Row(
                children: [
                  Expanded(
                    child: Text('Session chiffrée de bout en bout.',
                        style: TextStyle(fontSize: 11, color: t.texte3)),
                  ),
                  NovaBoutonSecondaire(
                    libelle: 'Refuser',
                    hauteur: 38,
                    onPressed: () => Navigator.of(context)
                        .pop(ReponseEntrante(acceptee: false, profil: _profil)),
                  ),
                  const SizedBox(width: 10),
                  _BoutonAccepter(
                    onTap: () => Navigator.of(context)
                        .pop(ReponseEntrante(acceptee: true, profil: _profil)),
                  ),
                ],
              ),
            ),
          ],
        ),
      ),
    );
  }

  Widget _lignePermission(
      NovaTokens t, String libelle, bool accorde, ValueChanged<bool> onChanged) {
    return InkWell(
      onTap: () => onChanged(!accorde),
      child: Padding(
        padding: const EdgeInsets.symmetric(vertical: 6),
        child: Row(
          children: [
            Container(
              width: 17,
              height: 17,
              alignment: Alignment.center,
              decoration: BoxDecoration(
                color: accorde ? t.vert : t.filetFort,
                borderRadius: BorderRadius.circular(3),
              ),
              child: accorde
                  ? const NovaIcone(NovaIcones.coche,
                      taille: 12, couleur: Colors.white)
                  : null,
            ),
            const SizedBox(width: 9),
            Expanded(
              child: Text(libelle,
                  style: TextStyle(fontSize: 12.5, color: t.texte)),
            ),
          ],
        ),
      ),
    );
  }
}

/// Bouton « Accepter » vert (le rouge de marque reste réservé).
class _BoutonAccepter extends StatefulWidget {
  const _BoutonAccepter({required this.onTap});

  final VoidCallback onTap;

  @override
  State<_BoutonAccepter> createState() => _BoutonAccepterState();
}

class _BoutonAccepterState extends State<_BoutonAccepter> {
  bool _survole = false;

  @override
  Widget build(BuildContext context) {
    final t = NovaTokens.of(context);
    return MouseRegion(
      cursor: SystemMouseCursors.click,
      onEnter: (_) => setState(() => _survole = true),
      onExit: (_) => setState(() => _survole = false),
      child: GestureDetector(
        onTap: widget.onTap,
        child: Container(
          height: 38,
          padding: const EdgeInsets.symmetric(horizontal: 16),
          alignment: Alignment.center,
          decoration: BoxDecoration(
            color: _survole
                ? Color.alphaBlend(Colors.black.withValues(alpha: 0.12), t.vert)
                : t.vert,
            borderRadius: BorderRadius.circular(kNovaRayon),
          ),
          child: const Text('Accepter',
              style: TextStyle(
                  fontSize: 13,
                  fontWeight: FontWeight.w600,
                  color: Colors.white)),
        ),
      ),
    );
  }
}
