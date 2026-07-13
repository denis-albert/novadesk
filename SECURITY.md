# Politique de sécurité

NovaDesk est un logiciel de bureau à distance : le chiffrement de bout en bout,
l'accès non surveillé et le service Windows privilégié sont des surfaces
critiques. Merci de signaler tout problème de façon responsable.

## Signaler une vulnérabilité

- **Canal privilégié** : l'onglet **Security → « Report a vulnerability »** du
  dépôt GitHub (signalement privé, visible des seuls mainteneurs).
- **Ne pas ouvrir d'issue publique** pour une faille exploitable.
- Décrire : version ou commit affecté, scénario d'exploitation, impact, preuve
  de concept si possible.

## Engagements

- Accusé de réception sous **72 heures**.
- Correctif priorisé selon la sévérité ; les failles touchant le chiffrement de
  session, l'admission non surveillée ou l'élévation de privilèges du service
  Windows sont traitées en priorité absolue.
- Divulgation coordonnée : les détails sont publiés au plus tôt à la sortie du
  correctif, au plus tard 90 jours après le signalement.

## Périmètre

Surfaces particulièrement sensibles (rapports bienvenus) :

- chiffrement de bout en bout (Noise XX), empreintes et SAS (`nd-crypto`) ;
- admission et secrets d'accès non surveillé (`nd-core`, module `unattended`) ;
- service Windows privilégié et son canal local (`nd-service`) ;
- confinement du transfert de fichiers (`nd-files`, `LocalFs::jailed`) ;
- chaîne de mise à jour signée (TUF, `packaging/update`) ;
- serveurs rendez-vous, relais et comptes (`server/`).

Hors périmètre : déni de service par volumétrie brute, ingénierie sociale, et
vulnérabilités de dépendances déjà publiées (couvertes par l'audit RustSec de
la CI).

## Versions supportées

| Version | Correctifs de sécurité |
|---|---|
| Dernière version 0.x publiée | ✅ |
| Versions antérieures | ❌ |
