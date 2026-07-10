//! `nd-ffi` — surface d'API exposée à l'interface Flutter.
//!
//! La façade orientée UI (types plats « DTO » + fonctions synchrones renvoyant
//! `Result<_, String>`, flux via `StreamSink`) vit dans le module [`api`],
//! ré-exporté à la racine. Voir `../../plan-technique/10-interface-client.md`.
//! Depuis le lot « session live », elle pilote le moteur réel
//! ([`nd_core::SessionEngine`]) par identifiant de session opaque : démarrage
//! ([`api::start_session`]), flux d'états et de frames vidéo
//! ([`api::session_state_stream`], [`api::session_video_stream`]), entrées
//! ([`api::send_input`]), statistiques ([`api::session_stats`]) et arrêt
//! ([`api::stop_session`]).
//!
//! # Régénération du pont Dart (orchestrateur, hors de ce poste)
//!
//! Le binding Rust (`src/frb_generated.rs`) et le Dart (`ui/lib/bridge/generated`)
//! se régénèrent avec `flutter` et `flutter_rust_bridge_codegen` **2.12.0** (même
//! version que la crate `flutter_rust_bridge` de `Cargo.toml` et que le paquet Dart
//! de `ui/pubspec.lock`) dans le PATH :
//!
//! ```text
//! # 1. Retirer l'échafaudage de compilation pré-régénération :
//! #    supprimer crates/nd-ffi/src/pont_provisoire5.rs
//! #    et la ligne `mod pont_provisoire5;` de lib.rs.
//! #    (Oubli = conflit d'impl E0119 à la compilation, impossible à rater.)
//! # 2. Depuis novadesk/ui (lit flutter_rust_bridge.yaml : rust_input crate::api) :
//! flutter_rust_bridge_codegen generate
//! ```
//!
//! **Stopgap `frb_generated.rs` (lot §2)** : l'enrichissement de `SessionStatsDto`
//! (nouveaux champs) a cassé le seul `impl SseDecode` généré qui construisait la
//! structure par littéral exhaustif (type de **sortie**, décodeur jamais appelé à
//! l'exécution). Comme la régénération est impossible depuis ce poste, ce littéral
//! a été complété sur place dans `frb_generated.rs` (marqué « lot §2 ») pour
//! garder la crate compilable ; la régénération l'écrasera à l'identique.
//!
//! **Stopgap `frb_generated.rs` (lot session media)** : idem pour
//! `SessionOptionsDto`, qui gagne trois champs (`extended_features`,
//! `transfer_dir`, `transport_reconnect`) ; ses `impl SseDecode`/`SseEncode`
//! générés (littéral exhaustif / accès par champ) ont été complétés sur place
//! (marqués « lot session media »). La régénération les réécrira à l'identique.
//!
//! **Lot « admission non surveillée » (blocker B3)** : l'accès non surveillé
//! devient réellement autonome — `start_unattended_host` câble désormais le
//! **contrôle d'admission automatique** du moteur
//! (`nd_core::UnattendedHost::start_with_admission`) : appareil de confiance ou
//! mot de passe permanent prouvé (vérifié contre le hachage salé du module
//! [`etat`]) ⇒ accepté sans dialogue ; sinon repli sur le flux
//! `unattended_incoming_stream`/`approve_incoming` existant. Côté contrôleur,
//! `SessionOptionsDto` gagne le champ **additif** `mot_de_passe:
//! Option<String>` (défaut `None`), transmis à l'hôte **dans le canal Noise**.
//! Stopgap `frb_generated.rs` : seul l'`impl SseDecode` (littéral exhaustif) a
//! été complété sur place (`mot_de_passe: None`, marqué « admission ») — le
//! Dart actuel ne transmet pas encore ce champ ; la régénération l'exposera.
//!
//! Fonctions exposées au Dart après régénération — **lot 03** : `start_session`,
//! `session_listen_info`, `session_state_stream` (→ `Stream<SessionStateDto>`),
//! `session_video_stream` (→ `Stream<VideoFrameDto>`), `wait_session_state`,
//! `collect_video_frames`, `session_stats`, `session_last_error`, `send_input`,
//! `stop_session`. **Lot §2** : `start_session_with_options`,
//! `start_unattended_host`, `unattended_incoming_stream`
//! (→ `Stream<IncomingRequestDto>`), `approve_incoming`, `unattended_stats`,
//! `stop_unattended_host`. **Lot session media (nouvelles)** :
//! `session_chat_stream` (→ `Stream<ChatMessageDto>`), `send_chat`,
//! `session_transfer_stream` (→ `Stream<TransferEventDto>`), `send_files`,
//! `set_audio_enabled`, `switch_monitor`. DTO : `VideoFrameDto`,
//! `SessionStatsDto` (enrichi), `SessionEndpointDto` (variante `ByRendezvous`),
//! `SessionOptionsDto` (enrichi : `extended_features`, `transfer_dir`,
//! `transport_reconnect`), `IncomingRequestDto`, `ListenInfoDto`,
//! `ChatMessageDto` (nouveau), `TransferEventDto` (nouveau).
//!
//! **Lot « état persistant » (nouvelles)** — état applicatif réel et durable
//! (module privé [`etat`], stockage JSON atomique) : `local_identity`,
//! `generate_ephemeral_password`, `list_contacts`, `add_contact`,
//! `update_contact`, `remove_contact`, `set_favorite`, `list_groups`,
//! `add_group`, `get_settings`, `get_setting`, `set_setting`, `record_session`,
//! `recent_sessions`, `list_recordings`, `unattended_config`,
//! `set_unattended_password`, `verify_unattended_password`, `add_trusted_device`,
//! `remove_trusted_device`, `record_access`, `access_log`. DTO :
//! `LocalIdentityDto`, `AddressBookEntryDto`, `SettingDto`, `RecentSessionDto`,
//! `RecordingDto`, `UnattendedConfigDto`, `AccessLogEntryDto`. Toutes ces
//! fonctions sont **synchrones** (aucun `StreamSink`) : elles n'exigent aucun
//! `impl SseEncode` écrit à la main, donc **aucun `pont_provisoire4.rs` n'est
//! nécessaire** pour ce lot (le codegen produira leurs `SseEncode`/`SseDecode`
//! à la régénération, à l'identique).
//!
//! **Lot « multi-instance & Wake-on-LAN » (nouvelle)** : `send_wol(mac,
//! broadcast)` émet le paquet magique Wake-on-LAN (`nd_features::send_wol`),
//! **synchrone** à DTO plats (`String`, `Option<String>`) — donc aucun
//! échafaudage de pont requis, le codegen produira son `SseEncode`/`SseDecode` à
//! la régénération. Par ailleurs, le répertoire de données du module [`etat`]
//! est désormais **surchargé** par la variable d'environnement
//! `NOVADESK_DATA_DIR` si elle est définie (plusieurs instances = plusieurs
//! identités/ID persistants) ; à défaut, comportement inchangé.
//!
//! **Lot « capacités moteur exposées » (nouvelles)** — met à portée du Dart des
//! capacités déjà implémentées dans le moteur mais jusqu'ici inatteignables :
//! confidentialité (`set_privacy`, `privacy_active`), cadre d'écran
//! (`set_session_region`, `session_requested_region` ; DTO `RegionDto`), tunnel
//! TCP (`open_tunnel`, `close_tunnels` ; DTO `TunnelOuvertDto`), annotations /
//! tableau blanc (`send_annotation`, `session_annotation_stream` →
//! `Stream<AnnotationDto>` ; DTO `AnnotationDto`) et **relecture
//! d'enregistrement** (`open_recording`, `recording_next_frame`,
//! `recording_seek`, `close_recording` ; DTO `RecordingInfoDto`, réutilise
//! `VideoFrameDto`). La relecture vit dans le module privé [`lecture`].
//!
//! **Lot « extras session & relecture » (nouvelles)** — contrôles de session
//! sous les noms `session_*` attendus par l'UI (mêmes chemins `flux` que le lot
//! précédent, qui reste intact) et relecture en **flux poussé** :
//! `session_set_privacy`, `session_set_region`, `session_send_annotation`,
//! `session_open_tunnel` (hôte et port distants séparés), `recording_info`
//! (métadonnées seules, sans lecteur à fermer) et `recording_frame_stream`
//! (→ `Stream<VideoFrameDto>` : `RecordingPlayer` de `nd-features` + décodeur
//! H.264 de `nd-codec`, une image RGBA poussée par échantillon). **Aucun DTO ni
//! `StreamSink` neuf** : les quatre contrôles sont synchrones à DTO plats déjà
//! bridgés (`RegionDto`, `AnnotationDto`), `recording_info` réutilise
//! `RecordingInfoDto` et `recording_frame_stream` réutilise le
//! `StreamSink<VideoFrameDto>` de `session_video_stream` (`SseEncode` déjà
//! généré) — donc **aucun `pont_provisoire` n'est requis** pour ce lot ; la
//! régénération ne fera qu'ajouter le câblage des nouvelles fonctions.
//!
//! **Lot « plan de contrôle de session » (nouvelles)** — cinq capacités que l'UI
//! ne pouvait pas encore piloter, chacune additive sur le canal `Control`
//! existant (ou locale à l'hôte pour l'enregistrement), gardées par les
//! permissions le cas échéant :
//! 1. `session_set_permission(session_id, capacite, autorise)` — **permissions à
//!    chaud** (le contrôleur renégocie ; l'hôte l'applique au filtre d'injection
//!    au vol) ;
//! 2. `session_set_quality(session_id, preset)` — **préréglage de qualité**
//!    (`auto`/`fluide`/`equilibre`/`netteté` → profil ABR + plafond de débit
//!    appliqués à l'encodeur hôte ; l'ABR continue sous le plafond) ;
//! 3. `session_set_recording(session_id, chemin)` — **enregistrement à chaud**
//!    (démarre une nouvelle époque MP4 / arrête proprement, côté hôte) ;
//! 4. `session_monitors(session_id) -> Vec<MonitorInfoDto>` — **liste des
//!    moniteurs** réels publiée par l'hôte (remplace l'« Écran 1/2 » codé en
//!    dur ; l'index alimente `switch_monitor`) ;
//! 5. `session_peer_info(session_id) -> PeerInfoDto` — **infos système du pair**
//!    (nom d'hôte + OS).
//!
//! DTO **neufs** (que la régénération ajoutera) : `MonitorInfoDto`
//! (`index`, `largeur`, `hauteur`, `principal`) et `PeerInfoDto` (`hote`, `os`).
//! Les permissions passent par une clé plate (`capacite: String` +
//! `autorise: bool`) : **aucun DTO de permissions neuf**. Toutes ces fonctions
//! sont **synchrones à DTO plats** (aucun `StreamSink`) — donc **aucun
//! `pont_provisoire` n'est requis** ; le codegen produira leurs
//! `SseEncode`/`SseDecode` à la régénération.
//!
//! **Lot « listing distant & découverte LAN » (nouvelles)** — deux briques déjà
//! livrées, mises à portée du Dart :
//! 1. `session_list_remote_dir(session_id, chemin) -> Vec<EntreeFsDto>` —
//!    **listing de répertoire distant** (`nd_files`, plan 09) routé **dans la
//!    session** par `nd-core` (sous-types `Control` additifs
//!    `RequeteFs`/`ReponseFs`, réponse corrélée par chemin, délai borné) :
//!    servi par l'hôte **derrière la permission** fichiers/réception
//!    (`fichiers_reception`) — refus ⇒ `Err("accès refusé …")`, jamais de
//!    listing sans droit. Chemin vide = racines du poste hôte. L'erreur de
//!    l'hôte (`ReponseListe::erreur`) est propagée en `Err(String)`.
//! 2. `discovery_start(nom, port)` / `discovery_peers() ->
//!    Vec<DiscoveredPeerDto>` / `discovery_stop()` — **découverte LAN**
//!    (`nd_features::decouverte`) : annonceur de présence (identité locale
//!    persistante + nom donné ; `port == 0` → port par défaut du parc) et
//!    écouteur des voisins (id local exclu), **une seule instance vivante par
//!    processus** (démarrage idempotent), instantané dédupliqué/expiré.
//!
//! DTO **neufs** (que la régénération ajoutera) : `EntreeFsDto` (`nom`,
//! `taille`, `est_dossier`, `modifie_le: Option<u64>`) et `DiscoveredPeerDto`
//! (`id`, `id_formate` groupé par 3, `nom`, `adresse`). Toutes ces fonctions
//! sont **synchrones à DTO plats** (aucun `StreamSink`) — donc **aucun
//! `pont_provisoire` n'est requis** ; le codegen produira leurs
//! `SseEncode`/`SseDecode` à la régénération.
//!
//! **Lot « durcissement du stockage local » (nouvelles)** — trois réglages
//! réellement effectifs, **sans droits administrateur** (module privé
//! [`plateforme`], spécifique Windows avec repli documenté hors Windows) :
//! 1. **Secrets au repos chiffrés (DPAPI)** — la clé privée d'identité
//!    (`identite.cle`) et le haché du mot de passe d'accès non surveillé sont
//!    désormais **chiffrés au repos** via `CryptProtectData`/`CryptUnprotectData`
//!    (portée utilisateur). **Migration transparente** : un ancien fichier en
//!    clair est déchiffré puis **ré-écrit chiffré** à la première lecture (aucune
//!    identité ni configuration existante n'est cassée). Interne à [`etat`] —
//!    aucune nouvelle fonction de façade.
//! 2. **Démarrage avec le système** — `apply_autostart(actif)` ajoute/retire la
//!    valeur `NovaDesk` de `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`
//!    (chemin de l'exécutable) ; `set_setting("demarrer_avec_systeme", …)`
//!    l'applique aussi automatiquement quand le réglage change.
//! 3. **Liste blanche d'admission (ACL)** — `list_admission_allowlist`,
//!    `add_admission_allowed(id)`, `remove_admission_allowed(id)` persistent une
//!    liste d'ID admis **sans mot de passe** en accès non surveillé, **branchée
//!    dans le vérificateur d'admission** (`start_unattended_host`) : la confiance
//!    à l'admission vaut **liste blanche ∪ appareils de confiance**. Toutes ces
//!    fonctions sont **synchrones à DTO plats** — donc **aucun `pont_provisoire`
//!    n'est requis** ; le codegen produira leurs `SseEncode`/`SseDecode` à la
//!    régénération.

// Binding généré par `flutter_rust_bridge_codegen generate` (config dans
// `ui/flutter_rust_bridge.yaml`). `unsafe` toléré : code FFI généré, non écrit
// à la main ; ne pas éditer (régénéré à chaque `generate`).
#[allow(unsafe_code)]
mod frb_generated;

pub mod api;

/// Gestion interne des sessions live et des hôtes non surveillés (tables
/// statiques + threads de drainage + file d'approbation). Hors du périmètre
/// scanné par le codegen (`rust_input: crate::api`).
mod flux;

/// État applicatif **persistant** (identité locale, carnet d'adresses, réglages,
/// historique, enregistrements, accès non surveillé) : stockage JSON atomique
/// sous le répertoire de données de l'application. Hors du périmètre scanné par
/// le codegen ; la façade [`api`] l'enveloppe en fonctions plates.
mod etat;

/// Relecture d'enregistrement (`.mp4`/`.ndr`) par identifiant opaque : table
/// statique de lecteurs (`nd_features::RecordingPlayer` + décodeur `nd_codec`),
/// décodage RGBA image par image, recherche par horodatage — plus deux accès
/// sans identifiant : métadonnées seules et relecture en flux poussé. Hors du
/// périmètre scanné par le codegen ; la façade [`api`] l'enveloppe en fonctions
/// plates.
mod lecture;

/// Intégration plateforme **Windows sans droits administrateur** : chiffrement
/// des secrets au repos (DPAPI) consommé par [`etat`], et démarrage automatique
/// via la clé de registre `Run` de l'utilisateur. Repli documenté hors Windows
/// (`#[cfg]` : secrets en clair, auto-démarrage inerte). Hors du périmètre scanné
/// par le codegen.
mod plateforme;

pub use api::*;

// `StreamSink` (défini par le pont généré) apparaît dans les signatures publiques de
// `api` : ce ré-export le rend publiquement visible (sinon lint `private_interfaces`).
pub use frb_generated::StreamSink;

/// Version du moteur, exposée à l'UI (ex. écran « À propos »).
#[must_use]
pub fn engine_version_string() -> String {
    nd_core::engine_version().to_string()
}

/// Formate un ID NovaDesk pour affichage (groupé par 3).
#[must_use]
pub fn format_id(id: u64) -> String {
    nd_proto::NovaId(id).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_non_vide() {
        assert!(!engine_version_string().is_empty());
    }

    #[test]
    fn format_id_groupe() {
        assert_eq!(format_id(123_456_789), "123 456 789");
    }
}
