//! Bureau à distance **complet et sécurisé** — l'exemple d'intégration NovaDesk :
//! prise de contrôle **par ID**, **chiffrée de bout en bout**, en loopback.
//!
//! Assemble toutes les briques déjà validées séparément :
//! - rendez-vous local (`nd-signaling`) : l'hôte publie son `NovaId`, le viewer le
//!   résout (comme `connect_by_id`) ;
//! - identités **persistantes** (`IdentityStore::load_or_create`) et handshake Noise XX
//!   sur QUIC via `establish` → [`EncryptedTransport`] (comme `e2e_session`) ;
//! - épinglage TOFU des empreintes (`KnownPeers::verify_or_pin`) + SAS anti-MITM ;
//! - boucle bidirectionnelle **sur le transport chiffré** (comme `control_loop`) :
//!   vidéo hôte→viewer (capture → H.264 → QUIC chiffré) ET entrées viewer→hôte
//!   (QUIC chiffré → injection).
//!
//! Les mouvements souris scriptés sont **relatifs et s'annulent** (le curseur revient à
//! sa place) ; aucune frappe clavier n'est injectée. Le rendez-vous ne voit que des
//! adresses ; le transport ne voit que du ciphertext Noise : un relais intermédiaire ne
//! pourrait rien déchiffrer.
//!
//! Lancer : `cargo run --release --example secure_desktop -p nd-core`

use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use nd_capture::{create_capturer, CaptureConfig, CapturedFrame};
use nd_codec::{
    create_decoder, create_encoder, CodecKind, EncodedChunk, EncoderConfig, VideoDecoder,
};
use nd_core::{apply_input, establish, EncryptedTransport};
use nd_crypto::identity::IdentityStore;
use nd_crypto::pinning::{KnownPeers, PinResult};
use nd_crypto::{HandshakeRole, PeerFingerprint};
use nd_input::create_injector;
use nd_proto::{ChannelKind, InputEvent, MonitorId, NovaId, Reliability};
use nd_signaling::{serve, Registry, RendezvousClient};
use nd_transport::{bind, connect, Transport};

/// Nombre d'images vidéo que l'hôte envoie (N).
const VIDEO_N: usize = 10;

/// Fichier d'identité temporaire à nom unique, supprimé en fin d'exécution (`Drop`).
struct FichierIdentite(PathBuf);

impl FichierIdentite {
    fn nouveau(role: &str) -> Self {
        let nom = format!("novadesk-identite-{role}-{}.txt", std::process::id());
        Self(std::env::temp_dir().join(nom))
    }

    fn chemin(&self) -> &Path {
        &self.0
    }
}

impl Drop for FichierIdentite {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Bilan du côté hôte (poste contrôlé).
struct BilanHote {
    images_envoyees: usize,
    entrees_injectees: usize,
    empreinte_locale: PeerFingerprint,
    empreinte_viewer: PeerFingerprint,
    epinglage: PinResult,
}

/// Bilan du côté viewer (poste qui pilote).
struct BilanViewer {
    images_decodees: usize,
    entrees_envoyees: usize,
    empreinte_locale: PeerFingerprint,
    empreinte_hote: PeerFingerprint,
    epinglage: PinResult,
}

/// Côté **hôte** : identité persistante, publication de l'ID au rendez-vous, accept
/// QUIC, handshake Noise (répondeur), épinglage du viewer, puis boucle bidirectionnelle
/// chiffrée : capture → H.264 → envoi vidéo ET réception → injection des entrées.
fn execute_hote(
    adresse_rv: SocketAddr,
    id: NovaId,
    chemin_identite: PathBuf,
    entrees_attendues: usize,
) -> Result<BilanHote, String> {
    // Identité persistante : créée au premier appel, rechargée ensuite. On vérifie
    // au passage que le rechargement redonne bien les mêmes clés.
    let identite = IdentityStore::load_or_create(&chemin_identite).map_err(|e| e.to_string())?;
    let relue = IdentityStore::load_or_create(&chemin_identite).map_err(|e| e.to_string())?;
    if relue.public != identite.public {
        return Err("identité de l'hôte non persistante entre deux chargements".into());
    }

    // Écouteur QUIC + publication (ID → adresse + certificat) au rendez-vous.
    let ecouteur = bind(
        "127.0.0.1:0"
            .parse()
            .map_err(|e: std::net::AddrParseError| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    let adresse_quic = ecouteur.local_addr();
    let certificat = ecouteur.server_cert_der();
    RendezvousClient::new(adresse_rv)
        .register(id, adresse_quic, &certificat)
        .map_err(|e| e.to_string())?;
    println!("Hôte   : ID {id} publié → {adresse_quic}");

    // Accept, puis handshake Noise XX (répondeur) → transport chiffré de bout en bout.
    let interne = ecouteur.accept().map_err(|e| e.to_string())?;
    let mut enc = establish(interne, HandshakeRole::Responder, &identite.private)
        .map_err(|e| e.to_string())?;
    let empreinte_locale = enc.local_fingerprint();
    let empreinte_viewer = enc
        .remote_fingerprint()
        .ok_or("empreinte du viewer absente après le handshake")?;
    println!(
        "Hôte   : session E2E établie — SAS local {} (à annoncer au viewer), viewer vu {}",
        empreinte_locale.sas(),
        empreinte_viewer.short_hex()
    );

    // Épinglage TOFU du viewer : première rencontre attendue.
    let mut pairs_connus = KnownPeers::new();
    let epinglage = pairs_connus.verify_or_pin("viewer", &empreinte_viewer);

    // Boucle bidirectionnelle SUR le transport chiffré (logique de `control_loop`).
    let mut capteur = create_capturer().map_err(|e| e.to_string())?;
    capteur
        .start(CaptureConfig {
            monitor: MonitorId(0),
            target_fps: 60,
            capture_cursor: false,
        })
        .map_err(|e| e.to_string())?;
    let mut encodeur = create_encoder(CodecKind::H264).map_err(|e| e.to_string())?;
    let injecteur = create_injector().map_err(|e| e.to_string())?;
    let canal_video = enc.open_channel(ChannelKind::Video(MonitorId(0)));

    let mut images_envoyees = 0usize;
    let mut entrees_injectees = 0usize;
    let mut encodeur_configure = false;
    let mut derniere_image: Option<CapturedFrame> = None;
    let mut tentatives = 0usize;
    while (images_envoyees < VIDEO_N || entrees_injectees < entrees_attendues) && tentatives < 5000
    {
        tentatives += 1;

        // Vidéo sortante : capture → encodage H.264 → envoi chiffré. Écran statique :
        // la dernière image disponible est ré-encodée, comme un vrai flux temps réel.
        if images_envoyees < VIDEO_N {
            let image = capteur.next_frame().map_err(|e| e.to_string())?;
            if image.image.is_some() {
                if !encodeur_configure {
                    encodeur
                        .configure(EncoderConfig {
                            kind: CodecKind::H264,
                            width: image.width,
                            height: image.height,
                            target_bitrate_kbps: 8_000,
                            max_fps: 60,
                        })
                        .map_err(|e| e.to_string())?;
                    encodeur_configure = true;
                }
                derniere_image = Some(image);
            }
            if encodeur_configure {
                if let Some(image) = &derniere_image {
                    let morceau = encodeur
                        .encode(image, images_envoyees == 0)
                        .map_err(|e| e.to_string())?;
                    enc.send(canal_video, morceau.data, Reliability::UnreliableFec)
                        .map_err(|e| e.to_string())?;
                    images_envoyees += 1;
                }
            }
        }

        // Entrées entrantes : déchiffrement → désérialisation → injection dans l'OS.
        while let Some((_canal, donnees)) = enc.poll_recv().map_err(|e| e.to_string())? {
            if let Some(evenement) = InputEvent::from_bytes(&donnees) {
                apply_input(injecteur.as_ref(), &evenement).map_err(|e| e.to_string())?;
                entrees_injectees += 1;
            }
        }

        thread::sleep(Duration::from_millis(2));
    }

    // Laisse le temps au viewer de drainer les dernières images avant de fermer.
    thread::sleep(Duration::from_millis(500));

    Ok(BilanHote {
        images_envoyees,
        entrees_injectees,
        empreinte_locale,
        empreinte_viewer,
        epinglage,
    })
}

/// Décode toute la vidéo déjà arrivée sur le transport chiffré.
fn decode_en_attente(
    enc: &mut EncryptedTransport,
    decodeur: &mut dyn VideoDecoder,
    images_decodees: &mut usize,
) -> Result<(), String> {
    while let Some((_canal, donnees)) = enc.poll_recv().map_err(|e| e.to_string())? {
        let morceau = EncodedChunk {
            data: donnees,
            is_keyframe: false,
            monitor: MonitorId(0),
            timestamp_us: 0,
        };
        if decodeur
            .decode(&morceau)
            .map_err(|e| e.to_string())?
            .is_some()
        {
            *images_decodees += 1;
        }
    }
    Ok(())
}

/// Côté **viewer** : identité persistante, résolution de l'ID de l'hôte, connexion
/// QUIC, handshake Noise (initiateur), SAS + épinglage de l'hôte, puis boucle :
/// décodage de la vidéo reçue ET envoi des entrées scriptées.
fn execute_viewer(
    adresse_rv: SocketAddr,
    id_hote: NovaId,
    chemin_identite: &Path,
    script: &[InputEvent],
) -> Result<BilanViewer, String> {
    let identite = IdentityStore::load_or_create(chemin_identite).map_err(|e| e.to_string())?;

    // Résolution de l'ID via le rendez-vous, avec quelques tentatives : l'hôte se
    // publie en parallèle dans son propre thread.
    let rendez_vous = RendezvousClient::new(adresse_rv);
    let mut fiche = None;
    for _ in 0..120 {
        if let Ok(f) = rendez_vous.lookup(id_hote) {
            fiche = Some(f);
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }
    let fiche = fiche.ok_or("ID de l'hôte jamais résolu (non enregistré ?)")?;
    println!(
        "Viewer : ID {id_hote} résolu → {} (certificat {} octets)",
        fiche.addr,
        fiche.cert_der.len()
    );

    // Connexion QUIC (certificat épinglé via le rendez-vous), puis handshake Noise XX
    // (initiateur) → chiffrement de bout en bout.
    let interne = connect(fiche.addr, &fiche.cert_der).map_err(|e| e.to_string())?;
    let mut enc = establish(interne, HandshakeRole::Initiator, &identite.private)
        .map_err(|e| e.to_string())?;
    let empreinte_locale = enc.local_fingerprint();
    let empreinte_hote = enc
        .remote_fingerprint()
        .ok_or("empreinte de l'hôte absente après le handshake")?;

    // SAS : le code court que l'utilisateur compare de visu (ou au téléphone) avec
    // celui annoncé par l'hôte, puis épinglage TOFU : première rencontre attendue.
    println!(
        "Viewer : SAS lu pour l'hôte = {} (empreinte {})",
        empreinte_hote.sas(),
        empreinte_hote.short_hex()
    );
    let mut pairs_connus = KnownPeers::new();
    let epinglage = pairs_connus.verify_or_pin("hôte", &empreinte_hote);

    // Boucle : envoi des entrées scriptées (chiffrées) ET décodage de la vidéo reçue.
    let mut decodeur = create_decoder(CodecKind::H264).map_err(|e| e.to_string())?;
    let canal_entrees = enc.open_channel(ChannelKind::Input);
    let mut images_decodees = 0usize;

    for evenement in script {
        enc.send(canal_entrees, evenement.to_bytes(), Reliability::Reliable)
            .map_err(|e| e.to_string())?;
        thread::sleep(Duration::from_millis(15));
        decode_en_attente(&mut enc, decodeur.as_mut(), &mut images_decodees)?;
    }

    // Draine la vidéo restante jusqu'à la cible (ou inactivité prolongée).
    let mut inactif = 0;
    while images_decodees < VIDEO_N && inactif < 2000 {
        let avant = images_decodees;
        decode_en_attente(&mut enc, decodeur.as_mut(), &mut images_decodees)?;
        if images_decodees == avant {
            inactif += 1;
            thread::sleep(Duration::from_millis(2));
        }
    }

    Ok(BilanViewer {
        images_decodees,
        entrees_envoyees: script.len(),
        empreinte_locale,
        empreinte_hote,
        epinglage,
    })
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("NovaDesk — bureau à distance sécurisé : intégration complète (loopback)");

    // 1. Serveur de rendez-vous local (annuaire ID → adresse + certificat).
    let ecouteur_rv = TcpListener::bind("127.0.0.1:0")?;
    let adresse_rv = ecouteur_rv.local_addr()?;
    thread::spawn(move || {
        let _ = serve(ecouteur_rv, Registry::new());
    });
    println!("Rendez-vous en écoute sur {adresse_rv}");

    // Identités persistantes : un fichier temporaire unique par rôle (supprimés en fin
    // d'exécution ; en production ils vivraient dans le profil de l'utilisateur).
    let identite_hote = FichierIdentite::nouveau("hote");
    let identite_viewer = FichierIdentite::nouveau("viewer");

    // Entrées scriptées : mouvements souris RELATIFS qui s'annulent deux à deux (le
    // curseur revient à sa position initiale). Aucune frappe clavier (M = 8).
    let script: Vec<InputEvent> = vec![
        InputEvent::MouseMoveRel { dx: 20.0, dy: 0.0 },
        InputEvent::MouseMoveRel { dx: 0.0, dy: 20.0 },
        InputEvent::MouseMoveRel { dx: -20.0, dy: 0.0 },
        InputEvent::MouseMoveRel { dx: 0.0, dy: -20.0 },
        InputEvent::MouseMoveRel { dx: 15.0, dy: 15.0 },
        InputEvent::MouseMoveRel {
            dx: -15.0,
            dy: -15.0,
        },
        InputEvent::MouseMoveRel {
            dx: 10.0,
            dy: -10.0,
        },
        InputEvent::MouseMoveRel {
            dx: -10.0,
            dy: 10.0,
        },
    ];
    let entrees_n = script.len();

    let id_hote = NovaId(424_242_424);

    // 2. Hôte (thread) : publie son ID, accepte, chiffre, diffuse l'écran et se laisse
    // piloter.
    let chemin_hote = identite_hote.chemin().to_path_buf();
    let hote = thread::spawn(move || execute_hote(adresse_rv, id_hote, chemin_hote, entrees_n));

    // 3. Viewer (thread principal) : résout l'ID, se connecte, vérifie/épingle, pilote.
    let bilan_viewer = execute_viewer(adresse_rv, id_hote, identite_viewer.chemin(), &script)
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

    let bilan_hote = hote
        .join()
        .expect("thread hôte")
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

    // 4. Rapport et verdict.
    println!();
    println!("— Rapport d'intégration —");
    println!(
        "Hôte   : {}/{VIDEO_N} images envoyées, {}/{entrees_n} entrées injectées, épinglage viewer {:?}",
        bilan_hote.images_envoyees, bilan_hote.entrees_injectees, bilan_hote.epinglage
    );
    println!(
        "Viewer : {}/{VIDEO_N} images décodées, {}/{entrees_n} entrées envoyées, épinglage hôte {:?}",
        bilan_viewer.images_decodees, bilan_viewer.entrees_envoyees, bilan_viewer.epinglage
    );
    println!(
        "SAS    : hôte annonce {} / viewer lit {} — viewer annonce {} / hôte lit {}",
        bilan_hote.empreinte_locale.sas(),
        bilan_viewer.empreinte_hote.sas(),
        bilan_viewer.empreinte_locale.sas(),
        bilan_hote.empreinte_viewer.sas()
    );

    let video_ok =
        bilan_hote.images_envoyees == VIDEO_N && bilan_viewer.images_decodees >= VIDEO_N - 1;
    let entrees_ok =
        bilan_viewer.entrees_envoyees == entrees_n && bilan_hote.entrees_injectees == entrees_n;
    // Empreintes croisées : chacun détient exactement la clé publique de l'autre —
    // c'est ce qui exclut un homme-du-milieu (le SAS n'en est que le résumé lisible).
    let empreintes_ok = bilan_viewer.empreinte_hote == bilan_hote.empreinte_locale
        && bilan_hote.empreinte_viewer == bilan_viewer.empreinte_locale;
    let sas_ok = bilan_viewer.empreinte_hote.sas() == bilan_hote.empreinte_locale.sas()
        && bilan_hote.empreinte_viewer.sas() == bilan_viewer.empreinte_locale.sas();
    let epinglage_ok = bilan_hote.epinglage == PinResult::FirstSeen
        && bilan_viewer.epinglage == PinResult::FirstSeen;

    println!(
        "Vérifs : vidéo {video_ok}, entrées {entrees_ok}, empreintes croisées {empreintes_ok}, SAS {sas_ok}, épinglage TOFU {epinglage_ok}"
    );

    if video_ok && entrees_ok && empreintes_ok && sas_ok && epinglage_ok {
        println!("OK : bureau à distance par ID, chiffré de bout en bout, validé.");
        Ok(())
    } else {
        Err(format!(
            "échec d'intégration — vidéo {video_ok}, entrées {entrees_ok}, empreintes {empreintes_ok}, SAS {sas_ok}, épinglage {epinglage_ok}"
        )
        .into())
    }
}
