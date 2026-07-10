//! Sonde d'intégration du **contrôle d'admission automatique** de l'hôte
//! « accès non surveillé » (blocker B3), sur les briques réelles en boucle
//! locale : rendez-vous éphémère → hole punching → QUIC → **Noise** → décision
//! d'admission **dans le canal chiffré**.
//!
//! Sept cas prouvés, chacun de bout en bout (le contrôleur est le vrai
//! [`SessionEngine`], qui émet sa `DemandeAdmission` juste après
//! l'établissement ; l'hôte est le vrai [`UnattendedHost`]) :
//!
//! 1. **bon mot de passe** → accepté **sans** que le crochet manuel soit
//!    sollicité (l'accès est réellement autonome) ;
//! 2. **mauvais mot de passe** → refusé, et le crochet manuel n'est **pas**
//!    consulté (pas d'usure de l'utilisateur par essais successifs) ;
//! 3. **appareil de confiance** (sans mot de passe) → accepté sans dialogue ;
//! 4. **aucune preuve** → le crochet manuel existant est toujours sollicité et
//!    sa décision honorée (compatibilité avec le flux d'approbation de l'UI) ;
//! 5. **invitation éphémère valide** → admise **avec le profil de l'invitation**
//!    (distinct du défaut du service) et le code est **consommé** (usage
//!    unique : 2ᵉ échange refusé), sans dialogue ;
//! 6. **invitation expirée** → refusée sans ouvrir le dialogue manuel ;
//! 7. **demande enrichie** (nom d'affichage + profil demandé, sans preuve) → ces
//!    infos sont remontées au crochet manuel, dont la décision est honorée.
//!
//! Sécurité vérifiée au passage : le mot de passe ne voyage que **dans** le
//! canal Noise (le sous-type `DemandeAdmission` n'est émis qu'après
//! l'établissement) et l'hôte ne le voit qu'à travers sa closure de
//! vérification — en production, `nd-ffi` y recalcule le hachage **BLAKE3
//! salé** persisté (`etat::verifier_mot_de_passe_non_surveille`), prouvé par
//! les tests de `nd-ffi` ; ici la closure d'égalité prouve le contrat (le
//! moteur ne stocke ni ne journalise jamais le clair).

use std::collections::HashMap;
use std::net::{SocketAddr, TcpListener};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use nd_capture::{
    CaptureConfig, CaptureEvent, CapturedFrame, FrameImage, PixelFormat, ScreenCapturer,
};
use nd_core::{
    DemandeAdmissionManuelle, FabriqueCapteur, FabriqueInjecteur, SecretAdmission, SessionConfig,
    SessionEndpoint, SessionEngine, SessionHandle, SessionOptions, SessionRole, UnattendedHost,
    UnattendedHostHandle,
};
use nd_features::invite::{unix_now, InviteStore, RedeemResult, SessionInvite};
use nd_features::{Capability, PermissionSet, Permissions};
use nd_input::{InputInjector, MouseButton};
use nd_proto::{MonitorId, NovaId};
use nd_signaling::{serve, Registry};
use nd_transport::ServerIdentity;

/// Délai maximal pour qu'une décision d'admission soit observable (punch +
/// QUIC + Noise + fenêtre d'admission, avec marge pour les machines lentes).
const DELAI_DECISION: Duration = Duration::from_secs(60);

/// Une seule sonde à la fois : chaque cas monte un hôte réel (qui peut ouvrir
/// une capture d'écran une fois admis) et son propre rendez-vous — la
/// sérialisation évite toute contention entre les quatre cas.
static UN_SEUL_CAS: Mutex<()> = Mutex::new(());

/// Démarre un serveur de rendez-vous éphémère et rend son adresse.
fn rendezvous_ephemere() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind rendez-vous");
    let addr = listener.local_addr().expect("adresse rendez-vous");
    thread::spawn(move || {
        let _ = serve(listener, Registry::new());
    });
    addr
}

/// Démarre le **vrai contrôleur** ([`SessionEngine`]) vers `hote` par
/// rendez-vous, avec un éventuel mot de passe d'admission dans les options.
fn demarrer_controleur(
    rv: SocketAddr,
    local: NovaId,
    hote: NovaId,
    mot_de_passe: Option<&str>,
) -> SessionHandle {
    let config = SessionConfig {
        role: SessionRole::Controller,
        local_id: local,
        peer_id: Some(hote),
        permissions: Permissions::default(),
    };
    let options = SessionOptions {
        mot_de_passe: mot_de_passe.map(SecretAdmission::new),
        ..SessionOptions::default()
    };
    let endpoint = SessionEndpoint::ByRendezvous {
        server: rv,
        stun_servers: vec![],
        relay: None,
    };
    SessionEngine::start_with_options(config, endpoint, options).expect("démarrage du contrôleur")
}

/// Démarre le **vrai contrôleur** vers `hote` par rendez-vous, en ajustant
/// librement ses [`SessionOptions`] (invitation, nom d'affichage, profil
/// demandé…) via `ajuster`.
fn demarrer_controleur_enrichi(
    rv: SocketAddr,
    local: NovaId,
    hote: NovaId,
    ajuster: impl FnOnce(&mut SessionOptions),
) -> SessionHandle {
    let config = SessionConfig {
        role: SessionRole::Controller,
        local_id: local,
        peer_id: Some(hote),
        permissions: Permissions::default(),
    };
    let mut options = SessionOptions::default();
    ajuster(&mut options);
    let endpoint = SessionEndpoint::ByRendezvous {
        server: rv,
        stun_servers: vec![],
        relay: None,
    };
    SessionEngine::start_with_options(config, endpoint, options).expect("démarrage du contrôleur")
}

/// Crochet manuel **enrichi espion** : consigne chaque [`DemandeAdmissionManuelle`]
/// reçue puis répond `reponse`. Rend la liste observée et la closure à brancher
/// sur [`UnattendedHost::start_with_admission_enrichie`].
fn crochet_espion_enrichi(
    reponse: bool,
) -> (
    Arc<Mutex<Vec<DemandeAdmissionManuelle>>>,
    impl Fn(&DemandeAdmissionManuelle) -> bool + Send + 'static,
) {
    let vus = Arc::new(Mutex::new(Vec::new()));
    let vus_crochet = Arc::clone(&vus);
    let crochet = move |demande: &DemandeAdmissionManuelle| {
        vus_crochet
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(demande.clone());
        reponse
    };
    (vus, crochet)
}

/// Validateur d'invitations adossé à un vrai [`InviteStore`] partagé et à une
/// table code → profil : sur un échange **valide** (non expiré, non déjà
/// consommé), le code est **consommé** et le profil de l'invitation est rendu ;
/// sinon `None`. Rend aussi le dernier profil réellement remis (pour vérifier
/// « admis avec le bon profil »).
type ProfilRemis = Arc<Mutex<Option<PermissionSet>>>;
fn validateur_invitations(
    store: Arc<Mutex<InviteStore>>,
    profils: HashMap<String, PermissionSet>,
) -> (
    ProfilRemis,
    impl Fn(NovaId, &str) -> Option<PermissionSet> + Send + 'static,
) {
    let remis: ProfilRemis = Arc::new(Mutex::new(None));
    let remis_closure = Arc::clone(&remis);
    let valideur = move |_pair: NovaId, code: &str| {
        let echange = store
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .redeem(code, unix_now());
        if echange == RedeemResult::Valid {
            let profil = profils.get(code).copied();
            *remis_closure.lock().unwrap_or_else(PoisonError::into_inner) = profil;
            profil
        } else {
            None
        }
    };
    (remis, valideur)
}

/// Crochet manuel **espion** : consigne chaque pair qui lui est soumis puis
/// répond `reponse`. Rend la liste observée et la closure à brancher.
fn crochet_espion(
    reponse: bool,
) -> (
    Arc<Mutex<Vec<NovaId>>>,
    impl Fn(NovaId) -> bool + Send + 'static,
) {
    let vus = Arc::new(Mutex::new(Vec::new()));
    let vus_crochet = Arc::clone(&vus);
    let crochet = move |pair: NovaId| {
        vus_crochet
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(pair);
        reponse
    };
    (vus, crochet)
}

/// Pairs consignés par le crochet espion (instantané).
fn pairs_vus(vus: &Arc<Mutex<Vec<NovaId>>>) -> Vec<NovaId> {
    vus.lock().unwrap_or_else(PoisonError::into_inner).clone()
}

/// Attend que `lire()` atteigne `au_moins`, au plus [`DELAI_DECISION`].
fn attendre_compteur(lire: impl Fn() -> u64, au_moins: u64) -> bool {
    let echeance = Instant::now() + DELAI_DECISION;
    while Instant::now() < echeance {
        if lire() >= au_moins {
            return true;
        }
        thread::sleep(Duration::from_millis(25));
    }
    false
}

/// Contexte d'erreur lisible en cas d'échec d'attente.
fn contexte(hote: &UnattendedHostHandle) -> String {
    format!(
        "servies={}, refusés={}, dernière erreur hôte : {:?}",
        hote.sessions_served(),
        hote.peers_refused(),
        hote.last_error()
    )
}

/// (1) Le **bon mot de passe** admet la session automatiquement : aucune
/// intervention manuelle — le crochet n'est jamais sollicité.
#[test]
fn bon_mot_de_passe_admis_sans_crochet_manuel() {
    let _cas = UN_SEUL_CAS.lock().unwrap_or_else(PoisonError::into_inner);
    let rv = rendezvous_ephemere();
    let hote_id = NovaId(700_000_001);
    let controleur_id = NovaId(700_000_002);

    let (vus, crochet) = crochet_espion(false);
    let hote = UnattendedHost::start_with_admission(
        hote_id,
        rv,
        vec![],
        ServerIdentity::generate().expect("identité"),
        PermissionSet::view_only(),
        crochet,
        // En production : recalcul du hachage BLAKE3 salé persisté (nd-ffi).
        |mdp| mdp == "sésame-très-secret",
        |_pair| false,
    )
    .expect("start_with_admission");

    let session = demarrer_controleur(rv, controleur_id, hote_id, Some("sésame-très-secret"));
    assert!(
        attendre_compteur(|| hote.sessions_served(), 1),
        "la session doit être admise par mot de passe ({})",
        contexte(&hote)
    );
    assert_eq!(hote.peers_refused(), 0, "aucun refus attendu");
    assert!(
        pairs_vus(&vus).is_empty(),
        "le crochet manuel ne doit pas être sollicité quand le mot de passe prouve"
    );
    session.stop();
    hote.stop();
}

/// (2) Un **mauvais mot de passe** est refusé **immédiatement** : aucune
/// session servie, et le crochet manuel — qui accepterait pourtant — n'est pas
/// consulté (un essai de mot de passe ne doit pas devenir un dialogue).
#[test]
fn mauvais_mot_de_passe_refuse_sans_dialogue() {
    let _cas = UN_SEUL_CAS.lock().unwrap_or_else(PoisonError::into_inner);
    let rv = rendezvous_ephemere();
    let hote_id = NovaId(700_000_011);
    let controleur_id = NovaId(700_000_012);

    // Crochet qui ACCEPTERAIT : s'il était (à tort) consulté, une session
    // serait servie et le test le verrait.
    let (vus, crochet) = crochet_espion(true);
    let hote = UnattendedHost::start_with_admission(
        hote_id,
        rv,
        vec![],
        ServerIdentity::generate().expect("identité"),
        PermissionSet::view_only(),
        crochet,
        |mdp| mdp == "le-vrai-mot-de-passe",
        |_pair| false,
    )
    .expect("start_with_admission");

    let session = demarrer_controleur(rv, controleur_id, hote_id, Some("mot-de-passe-intrus"));
    assert!(
        attendre_compteur(|| hote.peers_refused(), 1),
        "le mauvais mot de passe doit être refusé ({})",
        contexte(&hote)
    );
    assert_eq!(
        hote.sessions_served(),
        0,
        "aucune session ne doit être servie"
    );
    assert!(
        pairs_vus(&vus).is_empty(),
        "un mot de passe invalide refuse sans déranger l'UI"
    );
    assert!(
        hote.is_running(),
        "le service survit au refus et retourne à l'attente"
    );
    session.stop();
    hote.stop();
}

/// (3) Un **appareil de confiance** est admis sans mot de passe ni dialogue.
#[test]
fn appareil_de_confiance_admis_sans_mot_de_passe() {
    let _cas = UN_SEUL_CAS.lock().unwrap_or_else(PoisonError::into_inner);
    let rv = rendezvous_ephemere();
    let hote_id = NovaId(700_000_021);
    let controleur_id = NovaId(700_000_022);

    let (vus, crochet) = crochet_espion(false);
    let hote = UnattendedHost::start_with_admission(
        hote_id,
        rv,
        vec![],
        ServerIdentity::generate().expect("identité"),
        PermissionSet::view_only(),
        crochet,
        |_mdp| false,
        move |pair| pair == controleur_id,
    )
    .expect("start_with_admission");

    // Aucun mot de passe : la confiance seule doit suffire.
    let session = demarrer_controleur(rv, controleur_id, hote_id, None);
    assert!(
        attendre_compteur(|| hote.sessions_served(), 1),
        "l'appareil de confiance doit être admis sans preuve supplémentaire ({})",
        contexte(&hote)
    );
    assert_eq!(hote.peers_refused(), 0, "aucun refus attendu");
    assert!(
        pairs_vus(&vus).is_empty(),
        "le crochet manuel ne doit pas être sollicité pour un appareil de confiance"
    );
    session.stop();
    hote.stop();
}

/// (4) **Aucune preuve** (ni confiance, ni mot de passe) : le crochet manuel
/// existant est toujours sollicité — avec le bon ID — et sa décision honorée
/// (compatibilité du flux d'approbation de l'UI).
#[test]
fn sans_preuve_le_crochet_manuel_tranche_toujours() {
    let _cas = UN_SEUL_CAS.lock().unwrap_or_else(PoisonError::into_inner);
    let rv = rendezvous_ephemere();
    let hote_id = NovaId(700_000_031);
    let controleur_id = NovaId(700_000_032);

    let (vus, crochet) = crochet_espion(true);
    let hote = UnattendedHost::start_with_admission(
        hote_id,
        rv,
        vec![],
        ServerIdentity::generate().expect("identité"),
        PermissionSet::view_only(),
        crochet,
        |_mdp| false,
        |_pair| false,
    )
    .expect("start_with_admission");

    let session = demarrer_controleur(rv, controleur_id, hote_id, None);
    assert!(
        attendre_compteur(|| hote.sessions_served(), 1),
        "sans preuve, la décision (positive) du crochet manuel doit être honorée ({})",
        contexte(&hote)
    );
    assert!(
        pairs_vus(&vus).contains(&controleur_id),
        "le crochet manuel doit avoir vu l'appelant {controleur_id} (vus : {:?})",
        pairs_vus(&vus)
    );
    assert_eq!(hote.peers_refused(), 0, "aucun refus attendu");
    session.stop();
    hote.stop();
}

/// (5) Une **invitation éphémère valide** admet la session **avec le profil de
/// l'invitation** (distinct du profil par défaut du service), sans dialogue, et
/// **consomme** le code (usage unique : un second échange est refusé).
#[test]
fn invitation_valide_admet_avec_profil_et_consomme() {
    let _cas = UN_SEUL_CAS.lock().unwrap_or_else(PoisonError::into_inner);
    let rv = rendezvous_ephemere();
    let hote_id = NovaId(700_000_041);
    let controleur_id = NovaId(700_000_042);

    // Magasin réel : un code à usage unique (300 s), au profil « contrôle total »
    // — différent du profil par défaut du service (vue seule), pour prouver que
    // le profil de l'invitation prime.
    let store = Arc::new(Mutex::new(InviteStore::new()));
    let profil_invite = PermissionSet::full();
    let invite = store
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .issue(300, true);
    let code = invite.code.clone();
    let profils = HashMap::from([(code.clone(), profil_invite)]);
    let (profil_remis, valideur) = validateur_invitations(Arc::clone(&store), profils);

    // Le crochet manuel accepterait : s'il était (à tort) consulté, on le verrait.
    let (vus, crochet) = crochet_espion_enrichi(true);
    let hote = UnattendedHost::start_with_admission_enrichie(
        hote_id,
        rv,
        vec![],
        ServerIdentity::generate().expect("identité"),
        PermissionSet::view_only(),
        crochet,
        |_mdp| false,
        |_pair| false,
        valideur,
        None,
    )
    .expect("start_with_admission_enrichie");

    let session = demarrer_controleur_enrichi(rv, controleur_id, hote_id, |o| {
        o.invitation = Some(code.clone());
    });
    assert!(
        attendre_compteur(|| hote.sessions_served(), 1),
        "une invitation valide doit admettre la session ({})",
        contexte(&hote)
    );
    // Admise avec le **bon profil** (celui de l'invitation).
    assert_eq!(
        *profil_remis.lock().unwrap_or_else(PoisonError::into_inner),
        Some(profil_invite),
        "la session doit être admise avec le profil de l'invitation"
    );
    // L'invitation a tranché : le dialogue manuel n'a pas été sollicité.
    assert!(
        vus.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_empty(),
        "une invitation valide n'ouvre pas le dialogue manuel"
    );
    // **Consommée** (usage unique) : un second échange du même code est refusé.
    assert_eq!(
        store
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .redeem(&code, unix_now()),
        RedeemResult::AlreadyUsed,
        "le code à usage unique doit avoir été consommé par l'admission"
    );
    session.stop();
    hote.stop();
}

/// (6) Une **invitation expirée** est refusée **immédiatement** : aucune session
/// servie, et le dialogue manuel — qui accepterait pourtant — n'est **pas**
/// consulté (une preuve présentée qui échoue ne devient pas un dialogue).
#[test]
fn invitation_expiree_refusee() {
    let _cas = UN_SEUL_CAS.lock().unwrap_or_else(PoisonError::into_inner);
    let rv = rendezvous_ephemere();
    let hote_id = NovaId(700_000_051);
    let controleur_id = NovaId(700_000_052);

    // Invitation enregistrée avec une expiration **dans le passé**.
    let store = Arc::new(Mutex::new(InviteStore::new()));
    let code = "EXP-IRE-DXY".to_owned();
    store
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .register(&SessionInvite {
            code: code.clone(),
            expires_unix: unix_now().saturating_sub(10),
            one_time: true,
        });
    let (_profil_remis, valideur) = validateur_invitations(Arc::clone(&store), HashMap::new());

    let (vus, crochet) = crochet_espion_enrichi(true);
    let hote = UnattendedHost::start_with_admission_enrichie(
        hote_id,
        rv,
        vec![],
        ServerIdentity::generate().expect("identité"),
        PermissionSet::view_only(),
        crochet,
        |_mdp| false,
        |_pair| false,
        valideur,
        None,
    )
    .expect("start_with_admission_enrichie");

    let session = demarrer_controleur_enrichi(rv, controleur_id, hote_id, |o| {
        o.invitation = Some(code.clone());
    });
    assert!(
        attendre_compteur(|| hote.peers_refused(), 1),
        "une invitation expirée doit être refusée ({})",
        contexte(&hote)
    );
    assert_eq!(
        hote.sessions_served(),
        0,
        "aucune session ne doit être servie sur invitation expirée"
    );
    assert!(
        vus.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_empty(),
        "une invitation invalide refuse sans ouvrir le dialogue manuel"
    );
    assert!(hote.is_running(), "le service survit au refus");
    session.stop();
    hote.stop();
}

/// (7) **Demande enrichie** sans preuve (ni mot de passe, ni invitation) : le
/// **nom d'affichage** et le **profil demandé** déclarés par le contrôleur sont
/// remontés au crochet d'approbation manuel, dont la décision est honorée.
#[test]
fn demande_enrichie_remonte_nom_et_profil_au_crochet() {
    let _cas = UN_SEUL_CAS.lock().unwrap_or_else(PoisonError::into_inner);
    let rv = rendezvous_ephemere();
    let hote_id = NovaId(700_000_061);
    let controleur_id = NovaId(700_000_062);

    let profil_demande: PermissionSet = [Capability::ViewScreen, Capability::ControlMouse]
        .into_iter()
        .collect();
    // Aucune invitation dans ce mode.
    let sans_invitation = |_pair: NovaId, _code: &str| -> Option<PermissionSet> { None };
    let (vus, crochet) = crochet_espion_enrichi(true);
    let hote = UnattendedHost::start_with_admission_enrichie(
        hote_id,
        rv,
        vec![],
        ServerIdentity::generate().expect("identité"),
        PermissionSet::view_only(),
        crochet,
        |_mdp| false,
        |_pair| false,
        sans_invitation,
        None,
    )
    .expect("start_with_admission_enrichie");

    // Ni mot de passe ni invitation : seulement l'enrichissement (nom + profil).
    let session = demarrer_controleur_enrichi(rv, controleur_id, hote_id, |o| {
        o.nom_affichage = Some("Alice — support".to_owned());
        o.permissions_demandees = Some(profil_demande);
    });
    assert!(
        attendre_compteur(|| hote.sessions_served(), 1),
        "sans preuve, le crochet manuel enrichi (positif) doit être honoré ({})",
        contexte(&hote)
    );
    // Le crochet a reçu le bon appelant, avec le nom et le profil demandés.
    let recues = vus.lock().unwrap_or_else(PoisonError::into_inner).clone();
    let demande = recues
        .iter()
        .find(|d| d.pair == controleur_id)
        .expect("le crochet doit avoir vu l'appelant enrichi");
    assert_eq!(demande.nom_affichage.as_deref(), Some("Alice — support"));
    assert_eq!(demande.permissions_demandees, Some(profil_demande));
    assert_eq!(hote.peers_refused(), 0, "aucun refus attendu");
    session.stop();
    hote.stop();
}

// ---------------------------------------------------------------------------
// (8) Point d'injection : la fabrique de capteur injectée remplace le capteur
// système dans la boucle hôte (raccord du service accès non surveillé).
// ---------------------------------------------------------------------------

/// Capteur **factice** : rend des trames 64×64 synthétiques et **compte** ses
/// `next_frame` — la preuve que la boucle hôte l'a bien utilisé (et non
/// `create_capturer`). Chaque instance partage le compteur de sa fabrique.
struct CapteurFactice {
    trames: Arc<AtomicU64>,
}

impl ScreenCapturer for CapteurFactice {
    fn start(&mut self, _cfg: CaptureConfig) -> nd_proto::Result<()> {
        Ok(())
    }

    fn next_frame(&mut self) -> nd_proto::Result<CapturedFrame> {
        let seq = self.trames.fetch_add(1, Ordering::Relaxed);
        // Cadence modérée : la boucle hôte ne tourne pas en boucle serrée.
        thread::sleep(Duration::from_millis(10));
        Ok(frame_factice(seq))
    }

    fn poll_event(&mut self) -> Option<CaptureEvent> {
        None
    }

    fn stop(&mut self) {}
}

/// Frame BGRA 64×64 dont le motif dépend de `seq` (contenu non trivial pour
/// l'encodeur), sans capture d'écran réelle.
fn frame_factice(seq: u64) -> CapturedFrame {
    const COTE: u32 = 64;
    let mut data = vec![0u8; (COTE * COTE * 4) as usize];
    for (i, pixel) in data.chunks_exact_mut(4).enumerate() {
        let i = i as u64;
        pixel[0] = ((i + seq * 31) % 256) as u8;
        pixel[1] = ((i / 3 + seq * 7) % 256) as u8;
        pixel[2] = ((seq * 11) % 256) as u8;
        pixel[3] = 255;
    }
    CapturedFrame {
        width: COTE,
        height: COTE,
        monitor: MonitorId(0),
        format: PixelFormat::Bgra8,
        dirty: vec![],
        cursor: None,
        timestamp_us: seq * 16_000,
        image: Some(FrameImage::Cpu {
            data,
            stride: (COTE * 4) as usize,
        }),
    }
}

/// Injecteur **factice** inoffensif : n'atteint aucun périphérique réel (le test
/// n'injecte rien dans l'OS hôte).
struct InjecteurFactice;

impl InputInjector for InjecteurFactice {
    fn mouse_move_abs(&self, _x: f64, _y: f64, _monitor: MonitorId) -> nd_proto::Result<()> {
        Ok(())
    }
    fn mouse_move_rel(&self, _dx: f64, _dy: f64) -> nd_proto::Result<()> {
        Ok(())
    }
    fn mouse_button(&self, _btn: MouseButton, _down: bool) -> nd_proto::Result<()> {
        Ok(())
    }
    fn scroll(&self, _dx: f64, _dy: f64) -> nd_proto::Result<()> {
        Ok(())
    }
    fn key(&self, _scancode: u32, _down: bool) -> nd_proto::Result<()> {
        Ok(())
    }
    fn unicode(&self, _ch: char) -> nd_proto::Result<()> {
        Ok(())
    }
    fn release_all(&self) {}
}

/// (8) La **fabrique de capteur injectée** (via
/// [`UnattendedHost::start_with_admission_enrichie_fabriques`]) est bien appelée
/// par la boucle hôte : un vrai contrôleur (appareil de confiance) est admis, le
/// pipeline hôte démarre et **tire ses trames du capteur factice** — dont le
/// compteur croît. Si `create_capturer` (le défaut système) était utilisé à sa
/// place, ce compteur resterait à zéro.
#[test]
fn fabrique_capteur_injectee_est_utilisee_par_la_boucle_hote() {
    let _cas = UN_SEUL_CAS.lock().unwrap_or_else(PoisonError::into_inner);
    let rv = rendezvous_ephemere();
    let hote_id = NovaId(700_000_081);
    let controleur_id = NovaId(700_000_082);

    // Fabrique de capteur : instance neuve par époque, compteur de trames partagé.
    let trames = Arc::new(AtomicU64::new(0));
    let trames_fabrique = Arc::clone(&trames);
    let fabrique_capteur: FabriqueCapteur = Arc::new(move || {
        Ok(Box::new(CapteurFactice {
            trames: Arc::clone(&trames_fabrique),
        }) as Box<dyn ScreenCapturer>)
    });
    // Injecteur factice (le contrôleur n'émet aucune entrée ; inoffensif).
    let fabrique_injecteur: FabriqueInjecteur =
        Arc::new(|| Ok(Box::new(InjecteurFactice) as Box<dyn InputInjector>));

    // Le crochet manuel refuserait : c'est la **confiance** qui admet la session.
    let (vus, crochet) = crochet_espion_enrichi(false);
    let hote = UnattendedHost::start_with_admission_enrichie_fabriques(
        hote_id,
        rv,
        vec![],
        ServerIdentity::generate().expect("identité"),
        PermissionSet::full(),
        crochet,
        |_mdp| false,
        // Le contrôleur est un appareil de confiance → admis sans preuve.
        move |pair| pair == controleur_id,
        |_pair, _code| None,
        None,
        Some(fabrique_capteur),
        Some(fabrique_injecteur),
    )
    .expect("start_with_admission_enrichie_fabriques");

    // Vrai contrôleur (sans mot de passe : admis par confiance).
    let session = demarrer_controleur(rv, controleur_id, hote_id, None);

    // La fabrique injectée alimente la boucle hôte : preuve qu'elle remplace bien
    // `create_capturer`.
    assert!(
        attendre_compteur(|| trames.load(Ordering::Relaxed), 1),
        "la fabrique de capteur injectée doit alimenter la boucle hôte ({})",
        contexte(&hote)
    );
    // Admission par confiance : le dialogue manuel n'a pas été sollicité.
    assert!(
        vus.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_empty(),
        "l'appareil de confiance admet sans solliciter le crochet manuel"
    );

    session.stop();
    hote.stop();
}
