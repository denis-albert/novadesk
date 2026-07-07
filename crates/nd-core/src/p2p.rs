//! Établissement d'un transport QUIC **par ID NovaDesk** : pont entre le
//! connecteur P2P de `nd-signaling` (rendez-vous, STUN, hole punching) et les
//! points d'entrée de `nd-transport` (QUIC sur socket percée, repli relais).
//!
//! C'est le chemin réel derrière `SessionEndpoint::ByRendezvous` (voir
//! `session.rs`) :
//!
//! * côté **appelant** ([`connecter_par_rendezvous`]) : `establish_p2p` →
//!   [`ConnAttempt::Direct`] → `connect_quic_over_socket` (certificat du pair
//!   épinglé), ou [`ConnAttempt::RelayFallback`] → `connect_quic_via_relay` si
//!   un relais est configuré ;
//! * côté **appelé** ([`accepter_par_rendezvous`]) : `register` + heartbeat →
//!   boucle `await_p2p` → [`P2pIncoming::Direct`] → `accept_quic_over_socket`
//!   avec l'identité TLS dont le certificat a été publié, ou repli relais.
//!
//! Les deux fonctions rendent le **type concret** [`QuicTransport`] : la
//! boucle de session s'accroche à `is_connected`/`on_disconnect` pour la
//! détection de coupure (reconnexion, plan 04) avant de faire suivre le
//! transport au handshake Noise.
//!
//! # Honnêteté sur la couverture
//!
//! Tout ce chemin est exerçable en boucle locale (rendez-vous éphémère, punch
//! loopback) — c'est ce que prouvent les tests et la sonde
//! `examples/session_integree_demo.rs`. La traversée d'un **vrai NAT** dépend
//! du type de NAT et n'est pas testable sur une seule machine (voir
//! `nd-signaling::punch`) ; le repli relais exige un serveur `nd-relay`
//! joignable et, en production, un ticket **signé** émis par le courtier de
//! session (lot 07) — le ticket dérivé ici ([`ticket_relais`]) n'est accepté
//! que par un relais de test sans vérification de signature.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use nd_proto::{NdError, NovaId, Result};
use nd_signaling::{establish_p2p, ConnAttempt, P2pIncoming, RendezvousClient};
use nd_transport::{
    accept_quic_over_socket, accept_quic_via_relay, connect_quic_over_socket,
    connect_quic_via_relay, QuicTransport, ServerIdentity,
};

/// Pause entre deux tentatives d'établissement côté appelant (le pair peut
/// être en train de s'enregistrer ou de publier ses candidats).
const PAUSE_TENTATIVE: Duration = Duration::from_millis(250);

/// Fenêtre d'attente d'une demande de punch par itération de la boucle
/// d'acceptation ([`accepter_par_rendezvous`]) : courte pour vérifier
/// fréquemment le signal d'arrêt et rafraîchir le heartbeat.
const FENETRE_AWAIT: Duration = Duration::from_secs(2);

/// Adresse publiée au `register` : le chemin par ID passe par le hole
/// punching (candidats frais publiés par `await_p2p`), pas par cette adresse.
/// Seul le **certificat** publié compte (épinglage par l'appelant) ; l'adresse
/// est un marqueur documenté « joignable par punch uniquement ».
const ADRESSE_PUNCH_SEULEMENT: &str = "0.0.0.0:0";

/// Ticket de relais **partagé** par les deux pairs d'une session : dérivé de la
/// paire d'IDs (appelant, appelé), donc calculable des deux côtés sans échange
/// supplémentaire.
///
/// Production : le courtier de session (lot 07) émettra un ticket **signé
/// Ed25519** (portée + expiration) que `nd-relay` vérifiera ; ce ticket dérivé
/// ne convient qu'à un relais de test protocole-compatible.
#[must_use]
pub(crate) fn ticket_relais(appelant: NovaId, appele: NovaId) -> Vec<u8> {
    format!("novadesk-p2p:{}:{}", appelant.as_u64(), appele.as_u64()).into_bytes()
}

/// Enregistre `local_id` au rendez-vous avec le certificat de `identite`
/// (l'adresse publiée est le marqueur [`ADRESSE_PUNCH_SEULEMENT`]).
fn enregistrer(rv: &RendezvousClient, local_id: NovaId, identite: &ServerIdentity) -> Result<()> {
    let adresse: SocketAddr = ADRESSE_PUNCH_SEULEMENT
        .parse()
        .expect("adresse de punch valide");
    rv.register(local_id, adresse, identite.cert_der())
}

/// Établit un transport QUIC vers `peer_id` (côté **appelant**, rôle
/// contrôleur) : résolution + punch via le rendez-vous, puis QUIC sur la
/// socket percée — ou tunnel relais si le punch échoue et qu'un `relay` est
/// configuré.
///
/// Réessaie jusqu'à `delai_max` quand le pair n'est pas (encore) résolu ou n'a
/// pas (encore) publié de candidats — l'appelé peut être en train de se mettre
/// en attente. `stop` interrompt la boucle (arrêt de session).
///
/// # Errors
/// Erreur si `delai_max` expire sans chemin établi, si `stop` est levé, ou si
/// l'établissement QUIC échoue sur un chemin pourtant ouvert.
pub(crate) fn connecter_par_rendezvous(
    rv: &RendezvousClient,
    local_id: NovaId,
    peer_id: NovaId,
    stun_servers: &[SocketAddr],
    relay: Option<SocketAddr>,
    delai_max: Duration,
    stop: &Arc<AtomicBool>,
) -> Result<QuicTransport> {
    let echeance = Instant::now() + delai_max;
    let mut derniere_raison = String::from("aucune tentative");
    while Instant::now() < echeance {
        if stop.load(Ordering::Relaxed) {
            return Err(NdError::Protocol(
                "session arrêtée pendant l'établissement par rendez-vous".to_owned(),
            ));
        }
        match establish_p2p(rv, local_id, peer_id, stun_servers) {
            // Punch réussi : QUIC client sur la socket percée, certificat épinglé.
            Ok(ConnAttempt::Direct(chemin)) => {
                return connect_quic_over_socket(
                    chemin.socket,
                    chemin.peer_addr,
                    &chemin.peer_cert_der,
                );
            }
            Ok(ConnAttempt::RelayFallback {
                peer_cert_der,
                reason,
            }) => {
                // « Aucun candidat » = le pair n'est pas (encore) en attente via
                // await_p2p : on réessaie, la fenêtre de simultanéité viendra.
                if reason.contains("aucun candidat") {
                    derniere_raison = reason;
                } else if let Some(relais) = relay {
                    // Punch réellement échoué (NAT symétrique, UDP filtré…) :
                    // l'appelé a vu la même demande échouer et se présente au
                    // relais de son côté — même ticket dérivé de la paire d'IDs.
                    return connect_quic_via_relay(
                        relais,
                        &ticket_relais(local_id, peer_id),
                        &peer_cert_der,
                    );
                } else {
                    derniere_raison = format!("punch échoué sans relais configuré : {reason}");
                }
            }
            // Pair introuvable/hors-ligne ou rendez-vous injoignable : réessayer
            // (le pair peut être en train de s'enregistrer).
            Err(e) => derniere_raison = e.to_string(),
        }
        std::thread::sleep(PAUSE_TENTATIVE);
    }
    Err(NdError::Protocol(format!(
        "établissement par rendez-vous vers {peer_id} sans succès après {delai_max:?} \
         (dernière raison : {derniere_raison})"
    )))
}

/// Filtre d'admission des appelants : consulté **avant** d'accepter QUIC —
/// c'est le point d'ancrage du dialogue d'acceptation de l'UI (accès non
/// surveillé) et du filtre « même pair » de la reconnexion.
pub(crate) type AdmissionPair<'a> = &'a dyn Fn(NovaId) -> bool;

/// Paramètres d'attente d'une connexion entrante par rendez-vous (côté
/// **appelé**, rôle contrôlé) — voir [`accepter_par_rendezvous`].
pub(crate) struct AttenteRendezvous<'a> {
    /// Client du serveur de rendez-vous.
    pub rv: &'a RendezvousClient,
    /// ID NovaDesk local (publié et maintenu en vie par heartbeat).
    pub local_id: NovaId,
    /// Identité TLS dont le **certificat est publié** au rendez-vous : c'est
    /// elle que présentent la socket percée et le repli relais (épinglage).
    pub identite: &'a ServerIdentity,
    /// Serveurs STUN interrogés pour le candidat réflexif (vide = LAN/local).
    pub stun_servers: &'a [SocketAddr],
    /// Relais de repli quand le punch échoue (`None` = pas de repli).
    pub relay: Option<SocketAddr>,
    /// Filtre d'admission : un appelant refusé est ignoré (socket percée
    /// abandonnée, l'attente continue) sans qu'aucun octet applicatif ne
    /// circule.
    pub admission: AdmissionPair<'a>,
}

/// Attend une connexion entrante par rendez-vous : enregistre l'ID (certificat
/// de l'identité), le maintient en vie (heartbeat à chaque itération), boucle
/// sur `await_p2p` puis porte QUIC sur la socket percée (`accept_quic_over_socket`)
/// ou bascule sur le relais. Renvoie le transport **et l'ID de l'appelant**.
///
/// `delai_max` : `None` = attendre indéfiniment (interruption par `stop`
/// seulement) — c'est l'attente nominale d'un poste contrôlé ; `Some(d)` borne
/// l'attente (fenêtre de reconnexion).
///
/// # Errors
/// Erreur si `stop` est levé, si `delai_max` expire, ou si l'enregistrement au
/// rendez-vous est impossible.
pub(crate) fn accepter_par_rendezvous(
    attente: &AttenteRendezvous<'_>,
    delai_max: Option<Duration>,
    stop: &Arc<AtomicBool>,
) -> Result<(QuicTransport, NovaId)> {
    enregistrer(attente.rv, attente.local_id, attente.identite)?;
    let echeance = delai_max.map(|d| Instant::now() + d);
    loop {
        if stop.load(Ordering::Relaxed) {
            return Err(NdError::Protocol(
                "session arrêtée pendant l'attente par rendez-vous".to_owned(),
            ));
        }
        if echeance.is_some_and(|e| Instant::now() >= e) {
            return Err(NdError::Protocol(format!(
                "aucune connexion entrante pour {} avant {delai_max:?}",
                attente.local_id
            )));
        }
        // Présence : un heartbeat par itération (≪ TTL du registre) ; s'il
        // échoue (entrée expirée, serveur redémarré), on se ré-enregistre.
        if attente.rv.heartbeat(attente.local_id).is_err() {
            enregistrer(attente.rv, attente.local_id, attente.identite)?;
        }
        match nd_signaling::await_p2p(
            attente.rv,
            attente.local_id,
            attente.stun_servers,
            FENETRE_AWAIT,
        ) {
            Ok(P2pIncoming::Direct(entrant)) => {
                if !(attente.admission)(entrant.from) {
                    continue;
                }
                let transport = accept_quic_over_socket(entrant.socket, attente.identite)?;
                return Ok((transport, entrant.from));
            }
            Ok(P2pIncoming::RelayFallback { from, reason: _ }) => {
                if !(attente.admission)(from) {
                    continue;
                }
                // L'appelant bascule sur le relais de son côté : on s'y
                // présente avec le même ticket dérivé de la paire d'IDs.
                let Some(relais) = attente.relay else {
                    continue;
                };
                let transport = accept_quic_via_relay(
                    relais,
                    &ticket_relais(from, attente.local_id),
                    attente.identite,
                )?;
                return Ok((transport, from));
            }
            // Aucune demande dans la fenêtre : itération suivante (heartbeat).
            Err(_) => continue,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticket_relais_identique_des_deux_cotes() {
        let appelant = NovaId(111_111_111);
        let appele = NovaId(222_222_222);
        // L'appelant le dérive de (local, pair) ; l'appelé de (from, local) :
        // même paire ordonnée, même ticket.
        assert_eq!(
            ticket_relais(appelant, appele),
            ticket_relais(appelant, appele)
        );
        // L'ordre porte du sens : une session inversée est une autre session.
        assert_ne!(
            ticket_relais(appelant, appele),
            ticket_relais(appele, appelant)
        );
    }

    #[test]
    fn connecter_expire_proprement_sans_pair() {
        // Rendez-vous mort (port fermé) : la boucle réessaie puis expire avec
        // la dernière raison — sans paniquer ni bloquer.
        let rv = RendezvousClient::new("127.0.0.1:9".parse().expect("adresse"));
        let stop = Arc::new(AtomicBool::new(false));
        let resultat = connecter_par_rendezvous(
            &rv,
            NovaId(1),
            NovaId(2),
            &[],
            None,
            Duration::from_millis(300),
            &stop,
        );
        assert!(resultat.is_err());
    }

    #[test]
    fn connecter_repond_au_signal_d_arret() {
        let rv = RendezvousClient::new("127.0.0.1:9".parse().expect("adresse"));
        let stop = Arc::new(AtomicBool::new(true));
        let resultat = connecter_par_rendezvous(
            &rv,
            NovaId(1),
            NovaId(2),
            &[],
            None,
            Duration::from_secs(30),
            &stop,
        );
        assert!(resultat.is_err(), "l'arrêt interrompt l'établissement");
    }
}
