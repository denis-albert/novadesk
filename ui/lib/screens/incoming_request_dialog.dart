/// Dialogue d'acceptation d'une connexion **entrante** (côté poste contrôlé,
/// doc 03 §5.7) : identité + empreinte du connecteur, profil de permissions
/// (Par défaut / Partage d'écran / Contrôle total / Non surveillé), cases de
/// permissions ajustables, boutons Refuser / Accepter (vert « accordé » —
/// le rouge de marque reste réservé).
///
/// Purement visuel pour l'instant : le câblage réel (requête du cœur via
/// Stream FRB, réponse `accept/deny`) appartient au lot 04. Un bouton de
/// démonstration l'ouvre depuis Réglages → Sécurité.
library;

import 'package:flutter/material.dart';

import '../theme/nova_theme.dart';
import '../widgets/nova_icons.dart';

/// Profils de permissions proposés à l'acceptation (vocabulaire AnyDesk).
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
    this.empreinte = '3C:9A:F2:04:6B:D8:33:71',
  });

  final String alias;
  final String idFormate;
  final String empreinte;

  /// Ouvre le dialogue et renvoie la décision (ou `null` si écarté).
  static Future<ReponseEntrante?> montrer(
    BuildContext context, {
    String alias = 'pc-marie',
    String idFormate = '555 240 173',
    String empreinte = '3C:9A:F2:04:6B:D8:33:71',
  }) {
    return showDialog<ReponseEntrante>(
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
  ProfilPermissions _profil = ProfilPermissions.parDefaut;

  late final Map<String, bool> _permissions = {
    "Afficher l'écran": true,
    'Clavier et souris': true,
    'Presse-papiers': true,
    'Transfert de fichiers': false,
    'Transmettre le son': false,
    "Bloquer l'entrée locale": false,
  };

  /// Précoche les cases selon le profil retenu.
  void _appliquerProfil(ProfilPermissions profil) {
    setState(() {
      _profil = profil;
      switch (profil) {
        case ProfilPermissions.parDefaut:
          _permissions
            ..["Afficher l'écran"] = true
            ..['Clavier et souris'] = true
            ..['Presse-papiers'] = true
            ..['Transfert de fichiers'] = false
            ..['Transmettre le son'] = false
            ..["Bloquer l'entrée locale"] = false;
        case ProfilPermissions.partageEcran:
          _permissions.updateAll((_, __) => false);
          _permissions["Afficher l'écran"] = true;
        case ProfilPermissions.controleTotal:
          _permissions.updateAll((_, __) => true);
          _permissions["Bloquer l'entrée locale"] = false;
        case ProfilPermissions.nonSurveille:
          _permissions.updateAll((_, __) => true);
      }
    });
  }

  @override
  Widget build(BuildContext context) {
    final t = NovaTokens.of(context);

    return Dialog(
      child: ConstrainedBox(
        constraints: const BoxConstraints(maxWidth: 420),
        child: Padding(
          padding: const EdgeInsets.fromLTRB(22, 20, 22, 16),
          child: Column(
            mainAxisSize: MainAxisSize.min,
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              // Identité du connecteur.
              Row(
                children: [
                  Container(
                    width: 40,
                    height: 40,
                    alignment: Alignment.center,
                    decoration: BoxDecoration(
                      color: t.champ,
                      borderRadius: BorderRadius.circular(9),
                      border: Border.all(color: t.filet),
                    ),
                    child:
                        NovaIcone(NovaIcones.utilisateur, couleur: t.texte2),
                  ),
                  const SizedBox(width: 12),
                  Expanded(
                    child: Column(
                      crossAxisAlignment: CrossAxisAlignment.start,
                      children: [
                        Text(
                          widget.alias,
                          style: TextStyle(
                            fontSize: 14.5,
                            fontWeight: FontWeight.w700,
                            color: t.texte,
                          ),
                        ),
                        const SizedBox(height: 1),
                        Text(
                          '${widget.idFormate} · souhaite se connecter '
                          'à ce poste',
                          style: TextStyle(fontSize: 12, color: t.texte2),
                        ),
                      ],
                    ),
                  ),
                ],
              ),
              const SizedBox(height: 14),
              // Empreinte du connecteur (vérification hors bande).
              Container(
                padding:
                    const EdgeInsets.symmetric(horizontal: 10, vertical: 8),
                decoration: BoxDecoration(
                  color: t.panneau,
                  borderRadius: BorderRadius.circular(7),
                  border: Border.all(color: t.filet),
                ),
                child: Row(
                  children: [
                    NovaIcone(NovaIcones.cle, taille: 14, couleur: t.texte3),
                    const SizedBox(width: 8),
                    Expanded(
                      child: Text(
                        'Empreinte : ${widget.empreinte}',
                        style: TextStyle(
                          fontSize: 11.5,
                          color: t.texte2,
                          fontFeatures: const [
                            FontFeature.tabularFigures(),
                          ],
                        ),
                      ),
                    ),
                  ],
                ),
              ),
              const SizedBox(height: 16),
              // Profil de permissions.
              Row(
                children: [
                  Text(
                    'PROFIL',
                    style: TextStyle(
                      fontSize: 10.5,
                      fontWeight: FontWeight.w700,
                      letterSpacing: 1.1,
                      color: t.texte3,
                    ),
                  ),
                  const Spacer(),
                  DropdownButton<ProfilPermissions>(
                    value: _profil,
                    underline: const SizedBox.shrink(),
                    style: TextStyle(fontSize: 12.5, color: t.texte),
                    items: [
                      for (final profil in ProfilPermissions.values)
                        DropdownMenuItem(
                          value: profil,
                          child: Text(profil.libelle),
                        ),
                    ],
                    onChanged: (profil) {
                      if (profil != null) _appliquerProfil(profil);
                    },
                  ),
                ],
              ),
              const SizedBox(height: 4),
              // Cases de permissions demandées.
              for (final entree in _permissions.entries)
                SizedBox(
                  height: 32,
                  child: Row(
                    children: [
                      SizedBox(
                        width: 28,
                        child: Checkbox(
                          value: entree.value,
                          onChanged: (valeur) => setState(() =>
                              _permissions[entree.key] = valeur ?? false),
                        ),
                      ),
                      const SizedBox(width: 4),
                      Expanded(
                        child: Text(
                          entree.key,
                          style: TextStyle(fontSize: 12.5, color: t.texte),
                        ),
                      ),
                    ],
                  ),
                ),
              const SizedBox(height: 14),
              // Décision.
              Row(
                children: [
                  Expanded(
                    child: Text(
                      'Session chiffrée de bout en bout.',
                      style: TextStyle(fontSize: 11, color: t.texte3),
                    ),
                  ),
                  OutlinedButton(
                    onPressed: () => Navigator.of(context).pop(
                      ReponseEntrante(acceptee: false, profil: _profil),
                    ),
                    child: const Text('Refuser'),
                  ),
                  const SizedBox(width: 8),
                  FilledButton(
                    onPressed: () => Navigator.of(context).pop(
                      ReponseEntrante(acceptee: true, profil: _profil),
                    ),
                    style: FilledButton.styleFrom(
                      backgroundColor: kNovaVert,
                      // Le vert « accordé » garde le rouge de marque réservé.
                      overlayColor: Colors.black,
                    ),
                    child: const Text('Accepter'),
                  ),
                ],
              ),
            ],
          ),
        ),
      ),
    );
  }
}
