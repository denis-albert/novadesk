//! Câblage des briques « prêtes pour la session » (lot 05) dans la vraie boucle
//! média de [`crate::SessionEngine`] : audio système, transfert de fichiers,
//! presse-papiers, chat et bascule multi-écran — chacune gardée par une
//! [`Capability`](nd_features::Capability).
//!
//! # Canaux logiques et **ordre des nonces Noise**
//!
//! Le transport chiffré de session ([`crate::EncryptedTransport`]) porte **une
//! seule** session Noise par direction : les charges y sont chiffrées avec un
//! compteur de nonce qui **doit** être déchiffré dans le même ordre à l'autre
//! bout (snow `TransportState` ne tolère ni perte ni réordonnancement). Or le
//! transport QUIC fusionne deux files à la réception (flux fiable **vs**
//! datagrammes) : mélanger, **dans une même direction**, des envois fiables et
//! des datagrammes désynchronise le nonce (vérifié empiriquement — le premier
//! message arrivé dans le désordre casse le déchiffrement).
//!
//! Conséquence directe sur l'architecture : **chaque direction n'emploie qu'un
//! seul domaine d'ordonnancement**.
//!
//! * `contrôleur → hôte` : déjà **fiable** (canal `Input`). On y ajoute
//!   Fichiers, Presse-papiers, Chat et Bascule-moniteur, tous **fiables**,
//!   multiplexés (canal `Files` pour le transfert ; canal `Control` avec un
//!   petit en-tête de sous-type pour presse-papiers/chat/moniteur).
//! * `hôte → contrôleur` : la vidéo occupe historiquement le domaine
//!   **datagrammes**. Comme le plan de contrôle (réponses chat, presse-papiers,
//!   trames de contrôle du transfert) doit remonter **de façon fiable**, la
//!   direction `hôte → contrôleur` passe **intégralement en fiable** dès que les
//!   fonctions étendues sont actives : vidéo, audio et plan de contrôle sur le
//!   flux fiable ordonné. Le nonce reste synchronisé (un seul domaine). Les
//!   sessions **sans** fonction étendue gardent la vidéo en datagrammes+FEC
//!   (comportement historique inchangé).
//!
//! Le choix « audio non fiable » suggéré par le lot est donc arbitré en
//! **fiable** ici : c'est le prix de l'intégrité du nonce avec une session Noise
//! unique. En boucle locale (sans perte) le comportement est identique ; la voie
//! « média non fiable + plan de contrôle chiffré séparément » (deux sessions
//! Noise démultiplexées) est notée comme évolution production.

use std::path::PathBuf;

use nd_audio::{AudioPacket, SourceEmission};
use nd_capture::Rect;
use nd_codec::ContentProfile;
use nd_features::PermissionSet;
use nd_files::RequeteFichier;
use nd_proto::{ChannelKind, MonitorId, NovaId};

use crate::{PeerInfo, RemoteMonitor};

/// Nombre de moniteurs distincts pré-cartographiés pour la réception vidéo
/// (couvre la bascule multi-écran sans reconstruire la carte des canaux).
pub(crate) const MONITEURS_MAX: u32 = 8;

/// Message de chat livré au consommateur via [`crate::SessionHandle::chat_rx`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatMessage {
    /// `true` si le message vient du **pair distant**, `false` s'il s'agit de
    /// l'écho local d'un message que ce poste vient d'émettre.
    pub from_remote: bool,
    /// Texte du message (UTF-8).
    pub text: String,
}

/// Catégorie d'un message reçu, déduite du canal logique — pilote le
/// démultiplexage du récepteur unique de chaque côté.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Categorie {
    /// Flux vidéo d'un moniteur (décodage → frames).
    Video(MonitorId),
    /// Flux audio (dépaquetage → lecture).
    Audio,
    /// Transfert de fichiers (trames [`nd_files::TransferSession`]).
    Fichiers,
    /// Entrées clavier/souris (injection côté hôte).
    Input,
    /// Plan de contrôle annexe (presse-papiers, chat, bascule moniteur).
    Controle,
}

impl Categorie {
    /// Catégorie d'un [`ChannelKind`].
    pub(crate) fn depuis_kind(kind: ChannelKind) -> Categorie {
        match kind {
            ChannelKind::Video(m) => Categorie::Video(m),
            ChannelKind::Audio => Categorie::Audio,
            ChannelKind::Files => Categorie::Fichiers,
            ChannelKind::Input => Categorie::Input,
            ChannelKind::Control => Categorie::Controle,
        }
    }
}

/// Sous-type d'un message multiplexé sur le canal `Control` **après** le
/// handshake (le handshake Noise, lui, a déjà libéré le canal).
///
/// Les valeurs sont sérialisées (octet de tête) : ne jamais les renuméroter,
/// seulement en ajouter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SousTypeControle {
    /// Message de chat (payload = texte UTF-8).
    Chat = 1,
    /// Synchro presse-papiers (payload = octets [`nd_files::ClipboardSync`]).
    PressePapiers = 2,
    /// Demande de bascule moniteur (payload = index u32 big-endian).
    BasculeMoniteur = 3,
    /// **Demande** de mode confidentialité, contrôleur → hôte (payload = 1 octet
    /// 0/1). L'hôte, s'il détient [`nd_features::Capability::PrivacyMode`], cesse
    /// alors d'émettre l'écran réel et diffuse un cadre noir.
    Confidentialite = 4,
    /// **État** du mode confidentialité, hôte → contrôleur (payload = 1 octet
    /// 0/1) : le drapeau que le contrôleur affiche (indicateur « rideau actif »).
    ConfidentialiteEtat = 5,
    /// Couche d'annotation / tableau blanc, dans les deux sens (payload =
    /// [`nd_features::AnnotationLayer::to_bytes`]).
    Annotation = 6,
    /// **Demande** de région / cadre d'écran, contrôleur → hôte (payload vide =
    /// plein écran, sinon `x`,`y`,`w`,`h` en u32 big-endian — voir
    /// [`encoder_region`]).
    Region = 7,
    /// Trame d'un tunnel TCP de session, dans les deux sens (payload =
    /// `[id u32 BE][genre u8][données]`, voir [`crate::tunnel`]).
    Tunnel = 8,
    /// **Renégociation des permissions à chaud**, contrôleur → hôte (payload =
    /// bits `u16` big-endian d'un [`PermissionSet`], voir [`encoder_permissions`]).
    /// L'hôte remplace son ensemble vivant, lu par le filtre d'injection.
    MajPermissions = 9,
    /// **Préréglage de qualité**, contrôleur → hôte (payload = `[profil u8]` +
    /// `[plafond_kbps u32 BE]`, voir [`encoder_qualite`]). L'hôte reconfigure
    /// l'encodeur et l'échelle ABR sous le plafond.
    MajQualite = 10,
    /// **Liste des moniteurs** de l'hôte, hôte → contrôleur (payload =
    /// `[n u16 BE]` puis `n` entrées, voir [`encoder_moniteurs`]).
    Moniteurs = 11,
    /// **Infos système du pair** (nom d'hôte + OS), hôte → contrôleur (payload =
    /// `[len_hote u16 BE][hôte utf8][os utf8]`, voir [`encoder_infos_pair`]).
    InfosPair = 12,
    /// **Demande d'admission** d'un hôte non surveillé, contrôleur → hôte,
    /// émise juste après l'établissement — donc **dans le canal déjà chiffré par
    /// Noise**. Charge **rétro-compatible** : `[peer_id u64 BE][présence u8]` +
    /// mot de passe hérité, ou format étendu additif (invitation, nom
    /// d'affichage, profil demandé) — voir [`encoder_demande_admission`]. L'hôte
    /// à admission automatique la consomme **avant** ses boucles média ; partout
    /// ailleurs (session ordinaire, hôte déjà admis), elle est ignorée.
    DemandeAdmission = 13,
    /// **Requête de listing de répertoire distant**, contrôleur → hôte
    /// (payload = [`nd_files::RequeteListe::to_bytes`], chemin vide = racines).
    /// L'hôte répond **derrière la permission**
    /// [`nd_features::Capability::FileDownload`] : refus ⇒ réponse dont
    /// `erreur` vaut « accès refusé », jamais de listing sans droit.
    RequeteFs = 14,
    /// **Réponse de listing de répertoire distant**, hôte → contrôleur
    /// (payload = [`nd_files::ReponseListe::to_bytes`]). Le chemin échoyé sert
    /// de corrélation côté contrôleur
    /// ([`crate::SessionHandle::list_remote_dir`]).
    ReponseFs = 15,
    /// **Requête de récupération d'une tranche de fichier distant**, contrôleur
    /// → hôte (payload = [`nd_files::RequeteFichier::to_bytes`] : chemin, offset,
    /// taille max). L'hôte la sert **derrière la même permission**
    /// [`nd_features::Capability::FileDownload`] que le listing : refus ⇒ réponse
    /// dont `erreur` vaut « accès refusé », **jamais** de lecture du disque sans
    /// droit. Complète [`RequeteFs`](Self::RequeteFs) (repérer) par le contenu.
    RequeteFichierDistant = 16,
    /// **Réponse d'une tranche de fichier distant**, hôte → contrôleur (payload =
    /// [`nd_files::ReponseFichier::to_bytes`]). Corrélée par chemin **et** offset
    /// côté contrôleur ([`crate::SessionHandle::download_remote_file`]).
    ReponseFichierDistant = 17,
    /// **Source d'émission audio de l'hôte**, contrôleur → hôte (payload =
    /// `[mode u8]`, voir [`encoder_source_audio`]). L'hôte applique
    /// [`nd_audio::AudioSession::definir_source_emission`] (système / micro /
    /// mixé) à sa session audio ; micro absent ⇒ repli système (géré par nd-audio).
    MajSourceAudio = 18,
}

impl SousTypeControle {
    /// Reconstruit le sous-type depuis l'octet de tête.
    pub(crate) fn depuis_octet(octet: u8) -> Option<SousTypeControle> {
        match octet {
            1 => Some(SousTypeControle::Chat),
            2 => Some(SousTypeControle::PressePapiers),
            3 => Some(SousTypeControle::BasculeMoniteur),
            4 => Some(SousTypeControle::Confidentialite),
            5 => Some(SousTypeControle::ConfidentialiteEtat),
            6 => Some(SousTypeControle::Annotation),
            7 => Some(SousTypeControle::Region),
            8 => Some(SousTypeControle::Tunnel),
            9 => Some(SousTypeControle::MajPermissions),
            10 => Some(SousTypeControle::MajQualite),
            11 => Some(SousTypeControle::Moniteurs),
            12 => Some(SousTypeControle::InfosPair),
            13 => Some(SousTypeControle::DemandeAdmission),
            14 => Some(SousTypeControle::RequeteFs),
            15 => Some(SousTypeControle::ReponseFs),
            16 => Some(SousTypeControle::RequeteFichierDistant),
            17 => Some(SousTypeControle::ReponseFichierDistant),
            18 => Some(SousTypeControle::MajSourceAudio),
            _ => None,
        }
    }
}

/// Encadre un message de plan de contrôle : `[sous-type u8] ++ payload`.
pub(crate) fn encoder_controle(sous_type: SousTypeControle, payload: &[u8]) -> Vec<u8> {
    let mut trame = Vec::with_capacity(1 + payload.len());
    trame.push(sous_type as u8);
    trame.extend_from_slice(payload);
    trame
}

/// Décode un message de plan de contrôle en `(sous-type, payload)`.
pub(crate) fn decoder_controle(trame: &[u8]) -> Option<(SousTypeControle, &[u8])> {
    let (&tete, reste) = trame.split_first()?;
    Some((SousTypeControle::depuis_octet(tete)?, reste))
}

/// Encadre un paquet audio pour le canal `Audio` : `[timestamp_us u64 BE] ++ data`.
pub(crate) fn encoder_audio(paquet: &AudioPacket) -> Vec<u8> {
    let mut trame = Vec::with_capacity(8 + paquet.data.len());
    trame.extend_from_slice(&paquet.timestamp_us.to_be_bytes());
    trame.extend_from_slice(&paquet.data);
    trame
}

/// Décode un paquet audio reçu sur le canal `Audio`.
pub(crate) fn decoder_audio(trame: &[u8]) -> Option<AudioPacket> {
    let entete = trame.get(0..8)?;
    let timestamp_us = u64::from_be_bytes(entete.try_into().ok()?);
    Some(AudioPacket {
        timestamp_us,
        data: trame.get(8..)?.to_vec(),
    })
}

/// Commande adressée aux threads média d'une session vivante via
/// [`crate::SessionHandle`] (fichiers à envoyer, bascule moniteur, audio,
/// confidentialité, région d'écran).
#[derive(Debug, Clone)]
pub(crate) enum CommandeMedia {
    /// Démarrer l'envoi d'une file de fichiers vers le pair.
    EnvoyerFichiers(Vec<PathBuf>),
    /// Demander au pair (hôte) de diffuser le moniteur d'index donné.
    BasculerMoniteur(u32),
    /// Activer/désactiver l'émission (hôte) ou la lecture (contrôleur) audio.
    AudioActif(bool),
    /// Activer/désactiver le mode confidentialité (contrôleur → demande à
    /// l'hôte ; hôte → s'applique à lui-même).
    Confidentialite(bool),
    /// Restreindre (ou rétablir en plein écran avec `None`) la région d'écran
    /// partagée — le « cadre d'écran ».
    DefinirRegion(Option<Rect>),
    /// Renégocier les permissions à chaud (contrôleur → demande à l'hôte ; hôte
    /// → applique directement à son ensemble vivant).
    MajPermissions(PermissionSet),
    /// Appliquer un préréglage de qualité : profil ABR + plafond de débit
    /// (kbit/s, `0` = aucun plafond). Contrôleur → demande à l'hôte ; hôte →
    /// applique à sa boucle de diffusion.
    MajQualite(ContentProfile, u32),
    /// Démarrer (avec un chemin) ou arrêter (`None`) l'enregistrement local de
    /// l'hôte **en cours de session**. Sans effet côté contrôleur (l'hôte seul
    /// encode et muxe).
    DefinirEnregistrement(Option<PathBuf>),
    /// Demander à l'hôte le **listing du répertoire distant** donné (chemin
    /// vide = racines). Rôle contrôleur uniquement : la requête part sur le
    /// canal `Control` ([`SousTypeControle::RequeteFs`]) et la réponse revient
    /// par [`SousTypeControle::ReponseFs`] — attendue, corrélée par chemin,
    /// dans [`crate::SessionHandle::list_remote_dir`]. Sans effet côté hôte
    /// (le poste liste ses propres dossiers sans passer par la session).
    ListerRepertoireDistant(String),
    /// Demander à l'hôte une **tranche de fichier distant** (rôle contrôleur) :
    /// la requête part sur le canal `Control`
    /// ([`SousTypeControle::RequeteFichierDistant`]) et la réponse revient par
    /// [`SousTypeControle::ReponseFichierDistant`] — attendue, corrélée par
    /// chemin + offset, dans la boucle de
    /// [`crate::SessionHandle::download_remote_file`]. Sans effet côté hôte.
    TelechargerFichierDistant(RequeteFichier),
    /// Piloter la **source d'émission audio** de l'hôte : contrôleur → demande à
    /// l'hôte ([`SousTypeControle::MajSourceAudio`]) ; hôte → applique directement
    /// [`nd_audio::AudioSession::definir_source_emission`] à sa session audio.
    MajSourceAudio(SourceEmission),
}

/// Encode une région / cadre d'écran pour le canal `Control` : payload **vide**
/// pour le plein écran (`None`), sinon `x`,`y`,`w`,`h` en u32 big-endian.
pub(crate) fn encoder_region(region: Option<Rect>) -> Vec<u8> {
    match region {
        None => Vec::new(),
        Some(r) => {
            let mut trame = Vec::with_capacity(16);
            trame.extend_from_slice(&r.x.to_be_bytes());
            trame.extend_from_slice(&r.y.to_be_bytes());
            trame.extend_from_slice(&r.w.to_be_bytes());
            trame.extend_from_slice(&r.h.to_be_bytes());
            trame
        }
    }
}

/// Décode une région / cadre d'écran (inverse d'[`encoder_region`]). Payload
/// vide **ou** malformé ⇒ `None` (plein écran) : jamais de panique sur entrée
/// hostile.
pub(crate) fn decoder_region(payload: &[u8]) -> Option<Rect> {
    let octets: &[u8; 16] = payload.try_into().ok()?;
    let lire = |debut: usize| {
        u32::from_be_bytes(
            octets[debut..debut + 4]
                .try_into()
                .expect("tranche de 4 octets"),
        )
    };
    Some(Rect {
        x: lire(0),
        y: lire(4),
        w: lire(8),
        h: lire(12),
    })
}

// ---------------------------------------------------------------------------
// Plan de contrôle de session : permissions à chaud, qualité, moniteurs, pair
// ---------------------------------------------------------------------------

/// Encode un [`PermissionSet`] pour le canal `Control` : ses bits en `u16`
/// big-endian (2 octets). Sous-type [`SousTypeControle::MajPermissions`].
pub(crate) fn encoder_permissions(permissions: PermissionSet) -> Vec<u8> {
    permissions.to_bits().to_be_bytes().to_vec()
}

/// Décode un [`PermissionSet`] (inverse d'[`encoder_permissions`]). Longueur
/// invalide ⇒ `None` : jamais de panique sur entrée hostile.
pub(crate) fn decoder_permissions(payload: &[u8]) -> Option<PermissionSet> {
    let octets: [u8; 2] = payload.try_into().ok()?;
    Some(PermissionSet::from_bits(u16::from_be_bytes(octets)))
}

/// Encode un préréglage de qualité pour le canal `Control` : `[profil u8]`
/// (`0` = [`ContentProfile::Text`], `1` = [`ContentProfile::Video`]) suivi du
/// `[plafond_kbps u32 BE]`. Sous-type [`SousTypeControle::MajQualite`].
pub(crate) fn encoder_qualite(profil: ContentProfile, plafond_kbps: u32) -> Vec<u8> {
    let mut trame = Vec::with_capacity(5);
    trame.push(match profil {
        ContentProfile::Text => 0,
        ContentProfile::Video => 1,
    });
    trame.extend_from_slice(&plafond_kbps.to_be_bytes());
    trame
}

/// Décode un préréglage de qualité (inverse d'[`encoder_qualite`]). Longueur ou
/// octet de profil invalide ⇒ `None` (jamais de panique sur entrée hostile).
pub(crate) fn decoder_qualite(payload: &[u8]) -> Option<(ContentProfile, u32)> {
    let octets: [u8; 5] = payload.try_into().ok()?;
    let profil = match octets[0] {
        0 => ContentProfile::Text,
        1 => ContentProfile::Video,
        _ => return None,
    };
    let plafond = u32::from_be_bytes([octets[1], octets[2], octets[3], octets[4]]);
    Some((profil, plafond))
}

/// Encode la **source d'émission audio** pour le canal `Control` : un octet de
/// mode (`0` = [`SourceEmission::SystemeSeul`], `1` = [`SourceEmission::MicroSeul`],
/// `2` = [`SourceEmission::SystemeEtMicro`]). Sous-type
/// [`SousTypeControle::MajSourceAudio`].
pub(crate) fn encoder_source_audio(source: SourceEmission) -> Vec<u8> {
    let mode = match source {
        SourceEmission::SystemeSeul => 0u8,
        SourceEmission::MicroSeul => 1,
        SourceEmission::SystemeEtMicro => 2,
    };
    vec![mode]
}

/// Décode une **source d'émission audio** (inverse d'[`encoder_source_audio`]).
/// Longueur ou octet de mode invalide ⇒ `None` (jamais de panique sur entrée
/// hostile).
pub(crate) fn decoder_source_audio(payload: &[u8]) -> Option<SourceEmission> {
    match payload {
        [0] => Some(SourceEmission::SystemeSeul),
        [1] => Some(SourceEmission::MicroSeul),
        [2] => Some(SourceEmission::SystemeEtMicro),
        _ => None,
    }
}

/// Taille d'une entrée moniteur sérialisée : `index`,`w`,`h` (u32 BE) + `principal` (u8).
const TAILLE_MONITEUR: usize = 13;

/// Encode la liste des moniteurs de l'hôte pour le canal `Control` : `[n u16 BE]`
/// puis, pour chacun, `[index u32 BE][largeur u32 BE][hauteur u32 BE][principal u8]`.
/// Sous-type [`SousTypeControle::Moniteurs`].
pub(crate) fn encoder_moniteurs(moniteurs: &[RemoteMonitor]) -> Vec<u8> {
    let nombre = u16::try_from(moniteurs.len()).unwrap_or(u16::MAX);
    let mut trame = Vec::with_capacity(2 + moniteurs.len() * TAILLE_MONITEUR);
    trame.extend_from_slice(&nombre.to_be_bytes());
    for m in moniteurs.iter().take(usize::from(nombre)) {
        trame.extend_from_slice(&m.index.to_be_bytes());
        trame.extend_from_slice(&m.width.to_be_bytes());
        trame.extend_from_slice(&m.height.to_be_bytes());
        trame.push(u8::from(m.primary));
    }
    trame
}

/// Décode la liste des moniteurs (inverse d'[`encoder_moniteurs`]). Tolérant :
/// une trame tronquée rend les entrées **complètes** lues jusque-là (jamais de
/// panique). Une trame trop courte pour l'en-tête rend une liste vide.
pub(crate) fn decoder_moniteurs(payload: &[u8]) -> Vec<RemoteMonitor> {
    let Some(entete) = payload.get(0..2) else {
        return Vec::new();
    };
    let annonce = usize::from(u16::from_be_bytes([entete[0], entete[1]]));
    let corps = &payload[2..];
    let lire_u32 = |bloc: &[u8], debut: usize| {
        u32::from_be_bytes([
            bloc[debut],
            bloc[debut + 1],
            bloc[debut + 2],
            bloc[debut + 3],
        ])
    };
    let disponibles = corps.len() / TAILLE_MONITEUR;
    let mut moniteurs = Vec::with_capacity(annonce.min(disponibles));
    for i in 0..annonce.min(disponibles) {
        let bloc = &corps[i * TAILLE_MONITEUR..(i + 1) * TAILLE_MONITEUR];
        moniteurs.push(RemoteMonitor {
            index: lire_u32(bloc, 0),
            width: lire_u32(bloc, 4),
            height: lire_u32(bloc, 8),
            primary: bloc[12] != 0,
        });
    }
    moniteurs
}

/// Encode les infos système du pair pour le canal `Control` :
/// `[len_hote u16 BE][hôte utf8][os utf8]`. Sous-type [`SousTypeControle::InfosPair`].
pub(crate) fn encoder_infos_pair(infos: &PeerInfo) -> Vec<u8> {
    let hote = infos.host.as_bytes();
    let len_hote = u16::try_from(hote.len()).unwrap_or(u16::MAX);
    let mut trame = Vec::with_capacity(2 + hote.len() + infos.os.len());
    trame.extend_from_slice(&len_hote.to_be_bytes());
    trame.extend_from_slice(&hote[..usize::from(len_hote)]);
    trame.extend_from_slice(infos.os.as_bytes());
    trame
}

/// Décode les infos système du pair (inverse d'[`encoder_infos_pair`]). Trame
/// tronquée ou non-UTF-8 ⇒ `None` (jamais de panique sur entrée hostile).
pub(crate) fn decoder_infos_pair(payload: &[u8]) -> Option<PeerInfo> {
    let entete = payload.get(0..2)?;
    let len_hote = usize::from(u16::from_be_bytes([entete[0], entete[1]]));
    let hote = payload.get(2..2 + len_hote)?;
    let os = payload.get(2 + len_hote..)?;
    Some(PeerInfo {
        host: String::from_utf8(hote.to_vec()).ok()?,
        os: String::from_utf8(os.to_vec()).ok()?,
    })
}

// ---------------------------------------------------------------------------
// Admission de l'accès non surveillé (contrôle d'admission dans le canal Noise)
// ---------------------------------------------------------------------------

/// Champs d'une **demande d'admission** émise par le contrôleur (rôle
/// contrôleur → hôte), tels qu'ils partent sur le fil. Tout ce qui suit
/// `peer_id` est **additif** : un contrôleur hérité ne portait que le mot de
/// passe (voir [`encoder_demande_admission`] pour le format et la rétro-compat).
#[derive(Debug, Clone)]
pub(crate) struct DemandeAdmissionSortante<'a> {
    /// ID que le contrôleur déclare (recoupé côté hôte avec le pair du punch).
    pub peer_id: NovaId,
    /// Mot de passe d'admission permanent (clair, canal Noise uniquement).
    pub mot_de_passe: Option<&'a str>,
    /// Code d'invitation éphémère présenté (usage unique / TTL / profil).
    pub invitation: Option<&'a str>,
    /// Nom d'affichage / alias choisi par le contrôleur (dialogue hôte).
    pub nom_affichage: Option<&'a str>,
    /// Profil de permissions demandé par le contrôleur (dialogue hôte).
    pub permissions_demandees: Option<PermissionSet>,
}

/// Contenu **décodé** d'une demande d'admission (inverse de
/// [`encoder_demande_admission`]). Les champs additifs valent `None` pour un
/// message hérité plus court (rétro-compatibilité).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DemandeAdmissionRecue {
    /// ID déclaré par le contrôleur.
    pub declare: NovaId,
    /// Mot de passe reçu (clair, canal Noise), s'il en porte un.
    pub mot_de_passe: Option<String>,
    /// Code d'invitation éphémère présenté, s'il en porte un.
    pub invitation: Option<String>,
    /// Nom d'affichage déclaré, s'il en porte un.
    pub nom_affichage: Option<String>,
    /// Profil de permissions demandé, s'il en porte un.
    pub permissions_demandees: Option<PermissionSet>,
}

impl DemandeAdmissionRecue {
    /// Demande « simple » (format hérité) : seul un mot de passe éventuel.
    fn simple(declare: NovaId, mot_de_passe: Option<String>) -> Self {
        DemandeAdmissionRecue {
            declare,
            mot_de_passe,
            invitation: None,
            nom_affichage: None,
            permissions_demandees: None,
        }
    }
}

/// Octet de présence/format (offset 8) d'une demande d'admission, sérialisé :
/// `0` = héritée sans mot de passe, `1` = héritée avec mot de passe (le mot de
/// passe est le reste de la trame). Ne jamais renuméroter, seulement en ajouter.
const DEMANDE_HERITEE_SANS_MDP: u8 = 0;
const DEMANDE_HERITEE_AVEC_MDP: u8 = 1;
/// Format **étendu** : `[drapeaux u8]` puis les champs présents (voir
/// [`encoder_demande_admission`]). Additif — un décodeur hérité, qui ne connaît
/// que 0/1, l'ignore proprement (présence inconnue ⇒ aucune preuve exploitable).
const DEMANDE_ETENDUE: u8 = 2;

/// Drapeaux du format étendu : présence de chaque champ additif.
const DRAPEAU_MDP: u8 = 0b0001;
const DRAPEAU_INVITATION: u8 = 0b0010;
const DRAPEAU_NOM: u8 = 0b0100;
const DRAPEAU_PERMISSIONS: u8 = 0b1000;

/// Encode une **demande d'admission** pour le canal `Control` (sous-type
/// [`SousTypeControle::DemandeAdmission`]).
///
/// Rétro-compatibilité : **sans aucun champ additif** (invitation, nom, profil
/// demandé), la trame émise est **octet pour octet** celle des contrôleurs
/// antérieurs — `[peer_id u64 BE][présence u8]` puis, si `présence == 1`, le mot
/// de passe UTF-8 (le reste). Dès qu'un champ additif est présent, la trame
/// bascule en **format étendu** (`présence == 2`) : `[peer_id u64 BE][2]
/// [drapeaux u8]` puis, pour chaque champ **présent et dans l'ordre**, le mot de
/// passe, l'invitation et le nom (chacun `[len u16 BE][utf8]`), enfin le profil
/// demandé (`[bits u16 BE]`).
///
/// Sécurité : cette trame n'est émise **que sur le canal de session déjà chiffré
/// et authentifié par Noise** — le clair du mot de passe comme le code
/// d'invitation ne circulent jamais hors de ce canal, ni journalisés ni
/// persistés (l'hôte les valide via des closures, voir
/// [`crate::UnattendedHost::start_with_admission`]).
pub(crate) fn encoder_demande_admission(demande: &DemandeAdmissionSortante<'_>) -> Vec<u8> {
    let etendue = demande.invitation.is_some()
        || demande.nom_affichage.is_some()
        || demande.permissions_demandees.is_some();
    let mut trame = Vec::new();
    trame.extend_from_slice(&demande.peer_id.as_u64().to_be_bytes());
    if !etendue {
        // Format hérité (inchangé octet pour octet).
        match demande.mot_de_passe {
            Some(mdp) => {
                trame.push(DEMANDE_HERITEE_AVEC_MDP);
                trame.extend_from_slice(mdp.as_bytes());
            }
            None => trame.push(DEMANDE_HERITEE_SANS_MDP),
        }
        return trame;
    }
    // Format étendu : drapeaux puis champs présents, longueur-préfixés.
    trame.push(DEMANDE_ETENDUE);
    let mut drapeaux = 0u8;
    if demande.mot_de_passe.is_some() {
        drapeaux |= DRAPEAU_MDP;
    }
    if demande.invitation.is_some() {
        drapeaux |= DRAPEAU_INVITATION;
    }
    if demande.nom_affichage.is_some() {
        drapeaux |= DRAPEAU_NOM;
    }
    if demande.permissions_demandees.is_some() {
        drapeaux |= DRAPEAU_PERMISSIONS;
    }
    trame.push(drapeaux);
    for valeur in [
        demande.mot_de_passe,
        demande.invitation,
        demande.nom_affichage,
    ]
    .into_iter()
    .flatten()
    {
        pousser_chaine(&mut trame, valeur);
    }
    if let Some(permissions) = demande.permissions_demandees {
        trame.extend_from_slice(&permissions.to_bits().to_be_bytes());
    }
    trame
}

/// Ajoute une chaîne longueur-préfixée `[len u16 BE][utf8]` à `trame` (longueur
/// plafonnée à `u16::MAX`, comme [`encoder_infos_pair`]).
fn pousser_chaine(trame: &mut Vec<u8>, valeur: &str) {
    let octets = valeur.as_bytes();
    let len = u16::try_from(octets.len()).unwrap_or(u16::MAX);
    trame.extend_from_slice(&len.to_be_bytes());
    trame.extend_from_slice(&octets[..usize::from(len)]);
}

/// Décode une demande d'admission (inverse d'[`encoder_demande_admission`]).
/// Tolérant : un **message hérité plus court** (présence 0/1) se décode avec les
/// champs additifs à `None` ; toute trame tronquée, présence inconnue, octets
/// orphelins après un « sans mot de passe » hérité, ou UTF-8 invalide ⇒ `None`
/// (jamais de panique sur entrée hostile). Des octets résiduels après le format
/// étendu sont tolérés (champs d'une version ultérieure).
pub(crate) fn decoder_demande_admission(payload: &[u8]) -> Option<DemandeAdmissionRecue> {
    let id = NovaId(u64::from_be_bytes(payload.get(0..8)?.try_into().ok()?));
    let (&presence, reste) = payload.get(8..)?.split_first()?;
    match presence {
        DEMANDE_HERITEE_SANS_MDP if reste.is_empty() => {
            Some(DemandeAdmissionRecue::simple(id, None))
        }
        DEMANDE_HERITEE_AVEC_MDP => Some(DemandeAdmissionRecue::simple(
            id,
            Some(String::from_utf8(reste.to_vec()).ok()?),
        )),
        DEMANDE_ETENDUE => decoder_demande_etendue(id, reste),
        _ => None,
    }
}

/// Décode le corps d'une demande au **format étendu** (après `[id u64][2]`).
fn decoder_demande_etendue(declare: NovaId, corps: &[u8]) -> Option<DemandeAdmissionRecue> {
    let (&drapeaux, mut reste) = corps.split_first()?;
    let mot_de_passe = lire_chaine_optionnelle(&mut reste, drapeaux & DRAPEAU_MDP != 0)?;
    let invitation = lire_chaine_optionnelle(&mut reste, drapeaux & DRAPEAU_INVITATION != 0)?;
    let nom_affichage = lire_chaine_optionnelle(&mut reste, drapeaux & DRAPEAU_NOM != 0)?;
    let permissions_demandees = if drapeaux & DRAPEAU_PERMISSIONS != 0 {
        let bits = reste.get(0..2)?;
        Some(PermissionSet::from_bits(u16::from_be_bytes([
            bits[0], bits[1],
        ])))
    } else {
        None
    };
    Some(DemandeAdmissionRecue {
        declare,
        mot_de_passe,
        invitation,
        nom_affichage,
        permissions_demandees,
    })
}

/// Lit une chaîne longueur-préfixée `[len u16 BE][utf8]` si `present`, en
/// avançant `reste`. `None` (l'`Option` externe) = trame tronquée ou UTF-8
/// invalide ; `Some(None)` = champ absent (rien lu).
fn lire_chaine_optionnelle(reste: &mut &[u8], present: bool) -> Option<Option<String>> {
    if !present {
        return Some(None);
    }
    let entete = reste.get(0..2)?;
    let len = usize::from(u16::from_be_bytes([entete[0], entete[1]]));
    let valeur = String::from_utf8(reste.get(2..2 + len)?.to_vec()).ok()?;
    *reste = reste.get(2 + len..)?;
    Some(Some(valeur))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_controle() {
        for st in [
            SousTypeControle::Chat,
            SousTypeControle::PressePapiers,
            SousTypeControle::BasculeMoniteur,
        ] {
            let trame = encoder_controle(st, b"charge utile");
            let (dec, payload) = decoder_controle(&trame).expect("décodage");
            assert_eq!(dec, st);
            assert_eq!(payload, b"charge utile");
        }
        assert!(decoder_controle(&[]).is_none());
        assert!(decoder_controle(&[99]).is_none());
    }

    #[test]
    fn roundtrip_audio() {
        let p = AudioPacket {
            data: vec![1, 2, 3, 4, 5],
            timestamp_us: 123_456_789,
        };
        let trame = encoder_audio(&p);
        let dec = decoder_audio(&trame).expect("décodage");
        assert_eq!(dec.timestamp_us, p.timestamp_us);
        assert_eq!(dec.data, p.data);
        assert!(decoder_audio(&[0, 1, 2]).is_none());
    }

    #[test]
    fn roundtrip_region() {
        let region = Some(Rect {
            x: 10,
            y: 20,
            w: 640,
            h: 480,
        });
        assert_eq!(decoder_region(&encoder_region(region)), region);
        // Plein écran : payload vide, aller-retour sur `None`.
        assert!(encoder_region(None).is_empty());
        assert_eq!(decoder_region(&encoder_region(None)), None);
        // Entrée malformée (mauvaise longueur) ⇒ None sans panique.
        assert_eq!(decoder_region(&[1, 2, 3]), None);
    }

    #[test]
    fn sous_types_avances_aller_retour() {
        for st in [
            SousTypeControle::Confidentialite,
            SousTypeControle::ConfidentialiteEtat,
            SousTypeControle::Annotation,
            SousTypeControle::Region,
            SousTypeControle::Tunnel,
            SousTypeControle::MajPermissions,
            SousTypeControle::MajQualite,
            SousTypeControle::Moniteurs,
            SousTypeControle::InfosPair,
            SousTypeControle::DemandeAdmission,
            SousTypeControle::RequeteFs,
            SousTypeControle::ReponseFs,
            SousTypeControle::RequeteFichierDistant,
            SousTypeControle::ReponseFichierDistant,
            SousTypeControle::MajSourceAudio,
        ] {
            assert_eq!(SousTypeControle::depuis_octet(st as u8), Some(st));
        }
    }

    #[test]
    fn roundtrip_source_audio() {
        for source in [
            SourceEmission::SystemeSeul,
            SourceEmission::MicroSeul,
            SourceEmission::SystemeEtMicro,
        ] {
            assert_eq!(
                decoder_source_audio(&encoder_source_audio(source)),
                Some(source)
            );
        }
        // Longueur ou octet de mode invalide ⇒ None sans panique.
        assert_eq!(decoder_source_audio(&[]), None);
        assert_eq!(decoder_source_audio(&[3]), None);
        assert_eq!(decoder_source_audio(&[0, 0]), None);
    }

    /// Demande sortante « simple » (seul un mot de passe éventuel) — pour les
    /// cas hérités.
    fn sortante_simple(
        peer_id: NovaId,
        mot_de_passe: Option<&str>,
    ) -> DemandeAdmissionSortante<'_> {
        DemandeAdmissionSortante {
            peer_id,
            mot_de_passe,
            invitation: None,
            nom_affichage: None,
            permissions_demandees: None,
        }
    }

    #[test]
    fn roundtrip_demande_admission_heritee() {
        let pair = NovaId(123_456_789);
        // Avec mot de passe (y compris vide : la présence est explicite).
        for mdp in ["sésame-ouvre-toi", ""] {
            assert_eq!(
                decoder_demande_admission(&encoder_demande_admission(&sortante_simple(
                    pair,
                    Some(mdp)
                ))),
                Some(DemandeAdmissionRecue::simple(pair, Some(mdp.to_owned())))
            );
        }
        // Sans mot de passe.
        assert_eq!(
            decoder_demande_admission(&encoder_demande_admission(&sortante_simple(pair, None))),
            Some(DemandeAdmissionRecue::simple(pair, None))
        );
        // Sans champ additif, la trame reste au format hérité, octet pour octet :
        // `[id u64 BE][présence u8]` (+ mot de passe si présent).
        assert_eq!(
            encoder_demande_admission(&sortante_simple(pair, None)).len(),
            9
        );
        assert_eq!(
            encoder_demande_admission(&sortante_simple(pair, Some("abc")))[8],
            DEMANDE_HERITEE_AVEC_MDP
        );
    }

    #[test]
    fn roundtrip_demande_admission_enrichie() {
        let pair = NovaId(555_000_111);
        let profil: PermissionSet = [
            nd_features::Capability::ViewScreen,
            nd_features::Capability::ControlMouse,
        ]
        .into_iter()
        .collect();
        // Tous les champs additifs présents : format étendu, aller-retour exact.
        let demande = DemandeAdmissionSortante {
            peer_id: pair,
            mot_de_passe: Some("mdp"),
            invitation: Some("ABC-DEF-GHJ"),
            nom_affichage: Some("Alice — poste d'été"),
            permissions_demandees: Some(profil),
        };
        let trame = encoder_demande_admission(&demande);
        assert_eq!(
            trame[8], DEMANDE_ETENDUE,
            "un champ additif ⇒ format étendu"
        );
        assert_eq!(
            decoder_demande_admission(&trame),
            Some(DemandeAdmissionRecue {
                declare: pair,
                mot_de_passe: Some("mdp".to_owned()),
                invitation: Some("ABC-DEF-GHJ".to_owned()),
                nom_affichage: Some("Alice — poste d'été".to_owned()),
                permissions_demandees: Some(profil),
            })
        );
        // Un seul champ additif à la fois (les autres restent None).
        let invit_seule = DemandeAdmissionSortante {
            invitation: Some("ZZZ-ZZZ-ZZZ"),
            ..sortante_simple(pair, None)
        };
        let recue = decoder_demande_admission(&encoder_demande_admission(&invit_seule)).unwrap();
        assert_eq!(recue.invitation.as_deref(), Some("ZZZ-ZZZ-ZZZ"));
        assert_eq!(recue.mot_de_passe, None);
        assert_eq!(recue.nom_affichage, None);
        assert_eq!(recue.permissions_demandees, None);
    }

    #[test]
    fn decodeur_tolere_les_messages_herites_et_hostiles() {
        let pair = NovaId(42);
        // Rétro-compat : une trame héritée (présence 0/1) se décode avec tous
        // les champs additifs à None.
        let heritee_sans = encoder_demande_admission(&sortante_simple(pair, None));
        assert_eq!(
            decoder_demande_admission(&heritee_sans),
            Some(DemandeAdmissionRecue::simple(pair, None))
        );
        // Tolérance en avant : des octets résiduels après le format étendu (champs
        // d'une version future) sont ignorés, pas rejetés.
        let mut future = encoder_demande_admission(&DemandeAdmissionSortante {
            invitation: Some("AAA-BBB-CCC"),
            ..sortante_simple(pair, None)
        });
        future.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
        assert_eq!(
            decoder_demande_admission(&future).and_then(|d| d.invitation),
            Some("AAA-BBB-CCC".to_owned())
        );
        // Entrées hostiles : tronquée, présence inconnue, octets orphelins après
        // « absent » hérité, UTF-8 invalide, chaîne étendue tronquée ⇒ None.
        assert_eq!(decoder_demande_admission(&[1, 2, 3]), None);
        let mut presence_inconnue = encoder_demande_admission(&sortante_simple(pair, None));
        presence_inconnue[8] = 9;
        assert_eq!(decoder_demande_admission(&presence_inconnue), None);
        let mut orphelins = encoder_demande_admission(&sortante_simple(pair, None));
        orphelins.push(b'x');
        assert_eq!(decoder_demande_admission(&orphelins), None);
        let mut utf8_invalide = encoder_demande_admission(&sortante_simple(pair, Some("a")));
        utf8_invalide[9] = 0xFF;
        assert_eq!(decoder_demande_admission(&utf8_invalide), None);
        // Format étendu annonçant une invitation mais tronqué avant la longueur.
        assert_eq!(
            decoder_demande_admission(&[
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                42,
                DEMANDE_ETENDUE,
                DRAPEAU_INVITATION
            ]),
            None
        );
    }

    #[test]
    fn roundtrip_permissions() {
        let permissions: PermissionSet = [
            nd_features::Capability::ViewScreen,
            nd_features::Capability::ControlKeyboard,
            nd_features::Capability::TcpTunnel,
        ]
        .into_iter()
        .collect();
        assert_eq!(
            decoder_permissions(&encoder_permissions(permissions)),
            Some(permissions)
        );
        // Longueur invalide ⇒ None sans panique.
        assert_eq!(decoder_permissions(&[0x01]), None);
    }

    #[test]
    fn roundtrip_qualite() {
        for profil in [ContentProfile::Text, ContentProfile::Video] {
            for plafond in [0u32, 4_000, u32::MAX] {
                let trame = encoder_qualite(profil, plafond);
                assert_eq!(decoder_qualite(&trame), Some((profil, plafond)));
            }
        }
        // Longueur invalide ou octet de profil inconnu ⇒ None sans panique.
        assert_eq!(decoder_qualite(&[9, 0, 0, 0, 0]), None);
        assert_eq!(decoder_qualite(&[0, 0, 0]), None);
    }

    #[test]
    fn roundtrip_moniteurs() {
        let moniteurs = vec![
            RemoteMonitor {
                index: 0,
                width: 1920,
                height: 1080,
                primary: true,
            },
            RemoteMonitor {
                index: 1,
                width: 2560,
                height: 1440,
                primary: false,
            },
        ];
        assert_eq!(decoder_moniteurs(&encoder_moniteurs(&moniteurs)), moniteurs);
        // Liste vide : aller-retour sur un vecteur vide, en-tête tronqué ⇒ vide.
        assert!(decoder_moniteurs(&encoder_moniteurs(&[])).is_empty());
        assert!(decoder_moniteurs(&[0x00]).is_empty());
    }

    #[test]
    fn roundtrip_infos_pair() {
        let infos = PeerInfo {
            host: "poste-été".to_owned(),
            os: "windows (x86_64)".to_owned(),
        };
        assert_eq!(decoder_infos_pair(&encoder_infos_pair(&infos)), Some(infos));
        // Trame tronquée ⇒ None sans panique.
        assert_eq!(decoder_infos_pair(&[0x00]), None);
        assert_eq!(decoder_infos_pair(&[0xFF, 0xFF, 0x41]), None);
    }

    #[test]
    fn categorie_depuis_kind() {
        assert_eq!(Categorie::depuis_kind(ChannelKind::Audio), Categorie::Audio);
        assert_eq!(
            Categorie::depuis_kind(ChannelKind::Video(MonitorId(3))),
            Categorie::Video(MonitorId(3))
        );
        assert_eq!(
            Categorie::depuis_kind(ChannelKind::Control),
            Categorie::Controle
        );
    }
}
