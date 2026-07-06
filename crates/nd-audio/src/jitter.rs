//! Jitter buffer adaptatif pour la restitution audio (plan 08 §6).
//!
//! Absorbe la gigue réseau, réordonne les paquets par horodatage média
//! (`timestamp_us`) et les restitue à leur échéance de lecture ; un paquet
//! manquant à l'échéance est signalé comme un « trou » ([`SortieJitter::Trou`])
//! que le lecteur comble par dissimulation de perte (PLC, plan 08 §4.5).
//!
//! Entièrement indépendant de l'horloge murale et de l'OS : l'appelant fournit
//! les instants (`arrivee_us` à l'insertion, `maintenant_us` au tirage), ce qui
//! rend le module déterministe et testable sans audio réel.
//!
//! Politique (inspirée de NetEQ, voir plan 08) :
//! - la profondeur cible suit un percentile haut (P95) de la gigue
//!   inter-arrivée sur fenêtre glissante, bornée `[min, max]` : elle **monte
//!   vite** (éviter l'underrun) et **redescend lentement** (éviter les
//!   à-coups) ; avec `min == max`, le délai est figé (utile en test) ;
//! - paquet manquant à l'échéance → trou pour PLC, au plus
//!   [`MAX_TROUS_CONSECUTIFS`] de suite sur tampon vide ; au-delà, gel en
//!   [`SortieJitter::Attente`] jusqu'à réception (le PLC devient inaudible
//!   passé 2–3 trames, plan 08 §4.5) ;
//! - coupure plus longue que la portée du PLC → resynchronisation sur le
//!   premier paquet disponible (comptabilisée) ;
//! - paquet plus vieux que le point de lecture → écarté (comptabilisé).

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, VecDeque};

use crate::codec::TRAME_MS;
use crate::AudioPacket;

/// Profondeur cible minimale par défaut (µs) — une trame de protection.
pub const DELAI_MIN_DEFAUT_US: u64 = 20_000;

/// Profondeur cible maximale par défaut (µs) — plafond WAN dégradé (plan 08).
pub const DELAI_MAX_DEFAUT_US: u64 = 120_000;

/// Trous consécutifs maximum signalés (portée utile du PLC, plan 08 §4.5).
pub const MAX_TROUS_CONSECUTIFS: u32 = 3;

/// Taille de la fenêtre glissante d'échantillons de gigue inter-arrivée.
const FENETRE_GIGUE: usize = 64;

/// Résultat d'un tirage du jitter buffer.
#[derive(Debug, Clone)]
pub enum SortieJitter {
    /// Paquet prêt à être décodé et joué.
    Paquet(AudioPacket),
    /// Trame absente à son échéance : à combler par PLC côté décodeur.
    Trou {
        /// Horodatage média de la trame manquante.
        timestamp_us: u64,
    },
    /// Rien à jouer pour l'instant (échéance non atteinte ou tampon gelé).
    Attente,
}

/// Compteurs d'événements du jitter buffer (diagnostic et tests).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StatsJitter {
    /// Paquets arrivés après leur échéance de lecture, écartés.
    pub en_retard: u64,
    /// Paquets reçus en double, écartés.
    pub doublons: u64,
    /// Trous signalés pour PLC.
    pub trous: u64,
    /// Resynchronisations après coupure plus longue que la portée du PLC.
    pub resynchronisations: u64,
}

/// Jitter buffer adaptatif : réordonne les [`AudioPacket`] et les restitue à
/// l'échéance `arrivée projetée + délai cible`, trous signalés pour PLC.
pub struct JitterBuffer {
    /// Durée nominale d'une trame (µs) — le pas de la grille d'horodatages.
    duree_trame_us: u64,
    /// Paquets en attente, triés par horodatage média.
    paquets: BTreeMap<u64, AudioPacket>,
    /// Prochain horodatage attendu en sortie (point de lecture).
    prochain_ts: Option<u64>,
    /// Référence de projection média → horloge locale :
    /// `(timestamp_us, arrivee_us)` du premier paquet reçu.
    base: Option<(u64, u64)>,
    /// Profondeur cible courante (µs), pilotée par le gestionnaire de délai.
    delai_cible_us: u64,
    delai_min_us: u64,
    delai_max_us: u64,
    /// Temps de transit relatif du paquet précédent (µs, signé).
    transit_precedent: Option<i64>,
    /// Fenêtre glissante des variations de transit (gigue inter-arrivée, µs).
    fenetre_gigue: VecDeque<u64>,
    /// Trous émis d'affilée depuis le dernier paquet restitué.
    trous_consecutifs: u32,
    stats: StatsJitter,
}

impl JitterBuffer {
    /// Jitter buffer pour des trames de [`TRAME_MS`] avec les bornes par défaut.
    #[must_use]
    pub fn new() -> Self {
        Self::avec_parametres(
            u64::from(TRAME_MS) * 1_000,
            DELAI_MIN_DEFAUT_US,
            DELAI_MAX_DEFAUT_US,
        )
    }

    /// Jitter buffer paramétré : durée de trame et bornes du délai cible (µs).
    ///
    /// Le délai démarre bas (une trame, borné) et ne croît que sous gigue
    /// mesurée. `delai_min_us == delai_max_us` fige le délai à cette valeur.
    #[must_use]
    pub fn avec_parametres(duree_trame_us: u64, delai_min_us: u64, delai_max_us: u64) -> Self {
        let duree_trame_us = duree_trame_us.max(1);
        let delai_max_us = delai_max_us.max(delai_min_us);
        JitterBuffer {
            duree_trame_us,
            paquets: BTreeMap::new(),
            prochain_ts: None,
            base: None,
            delai_cible_us: duree_trame_us.clamp(delai_min_us, delai_max_us),
            delai_min_us,
            delai_max_us,
            transit_precedent: None,
            fenetre_gigue: VecDeque::with_capacity(FENETRE_GIGUE),
            trous_consecutifs: 0,
            stats: StatsJitter::default(),
        }
    }

    /// Dépose un paquet reçu du réseau, `arrivee_us` étant l'instant local de
    /// réception (horloge monotone au choix de l'appelant, en µs).
    ///
    /// Les paquets peuvent arriver dans le désordre ; ceux plus vieux que le
    /// point de lecture ou déjà présents sont écartés (voir [`StatsJitter`]).
    pub fn inserer(&mut self, paquet: AudioPacket, arrivee_us: u64) {
        // L'arrivée nourrit toujours l'estimation de gigue, même si le paquet
        // est ensuite écarté : son horodatage de transit reste représentatif.
        self.observer_arrivee(paquet.timestamp_us, arrivee_us);

        if let Some(attendu) = self.prochain_ts {
            if paquet.timestamp_us < attendu {
                // Sa fenêtre de lecture est passée (jouée, trou émis ou resync).
                self.stats.en_retard += 1;
                return;
            }
        }
        match self.paquets.entry(paquet.timestamp_us) {
            Entry::Occupied(_) => self.stats.doublons += 1,
            Entry::Vacant(e) => {
                e.insert(paquet);
            }
        }
    }

    /// Tire ce qui est dû à l'instant `maintenant_us` (même horloge que les
    /// `arrivee_us` passés à [`inserer`](Self::inserer)).
    ///
    /// À appeler à la cadence des trames ; au plus une trame (paquet ou trou)
    /// est restituée par appel, dans l'ordre strict des horodatages.
    #[must_use]
    pub fn suivant(&mut self, maintenant_us: u64) -> SortieJitter {
        loop {
            let Some((base_media, base_locale)) = self.base else {
                return SortieJitter::Attente; // rien reçu depuis la création
            };
            let premier = self.paquets.keys().next().copied();
            let Some(attendu) = self.prochain_ts.or(premier) else {
                return SortieJitter::Attente; // tampon vide, point de lecture inconnu
            };

            // Échéance locale de la trame attendue : instant d'arrivée projeté
            // (translation média → local via la référence) + profondeur cible.
            let echeance = base_locale as i64
                + (attendu as i64 - base_media as i64)
                + self.delai_cible_us as i64;
            if (maintenant_us as i64) < echeance {
                return SortieJitter::Attente;
            }

            if let Some(ts_min) = premier {
                // Tolérance d'une demi-trame : absorbe un léger désalignement
                // de la grille d'horodatages sans déclasser le paquet.
                if ts_min <= attendu.saturating_add(self.duree_trame_us / 2) {
                    let (ts, paquet) = self.paquets.pop_first().expect("tampon non vide");
                    self.prochain_ts = Some(ts + self.duree_trame_us);
                    self.trous_consecutifs = 0;
                    return SortieJitter::Paquet(paquet);
                }
                if ts_min - attendu > u64::from(MAX_TROUS_CONSECUTIFS) * self.duree_trame_us {
                    // Coupure au-delà de la portée du PLC : on saute au premier
                    // paquet disponible plutôt que d'émettre une rafale de trous.
                    self.stats.resynchronisations += 1;
                    self.trous_consecutifs = 0;
                    self.prochain_ts = Some(ts_min);
                    continue; // réévalue l'échéance du nouveau point de lecture
                }
                // Trame perdue mais suite proche : trou de liaison pour PLC.
                return self.emettre_trou(attendu);
            }

            // Tampon vide alors qu'une trame est due (underrun) : PLC borné,
            // puis gel sans avancer le point de lecture jusqu'à réception.
            if self.trous_consecutifs >= MAX_TROUS_CONSECUTIFS {
                return SortieJitter::Attente;
            }
            return self.emettre_trou(attendu);
        }
    }

    /// Profondeur cible courante du tampon (µs).
    #[must_use]
    pub fn delai_cible_us(&self) -> u64 {
        self.delai_cible_us
    }

    /// Nombre de paquets actuellement en attente.
    #[must_use]
    pub fn longueur(&self) -> usize {
        self.paquets.len()
    }

    /// Compteurs d'événements depuis la création.
    #[must_use]
    pub fn stats(&self) -> StatsJitter {
        self.stats
    }

    /// Signale la trame `attendu` comme manquante et avance le point de lecture.
    fn emettre_trou(&mut self, attendu: u64) -> SortieJitter {
        self.trous_consecutifs += 1;
        self.stats.trous += 1;
        self.prochain_ts = Some(attendu + self.duree_trame_us);
        SortieJitter::Trou {
            timestamp_us: attendu,
        }
    }

    /// Gestionnaire de délai : mesure la variation du temps de transit entre
    /// paquets successifs (gigue inter-arrivée) et ajuste la profondeur cible.
    fn observer_arrivee(&mut self, timestamp_us: u64, arrivee_us: u64) {
        let (base_media, base_locale) = *self.base.get_or_insert((timestamp_us, arrivee_us));
        // Transit relatif au premier paquet : constant si le réseau est stable,
        // fluctuant sous gigue (seules les variations nous intéressent).
        let transit =
            (arrivee_us as i64 - base_locale as i64) - (timestamp_us as i64 - base_media as i64);
        if let Some(precedent) = self.transit_precedent {
            if self.fenetre_gigue.len() == FENETRE_GIGUE {
                self.fenetre_gigue.pop_front();
            }
            self.fenetre_gigue.push_back(transit.abs_diff(precedent));
            self.ajuster_delai_cible();
        }
        self.transit_precedent = Some(transit);
    }

    /// Cible = une trame + 2 × P95 de la gigue, bornée `[min, max]` ; montée
    /// immédiate, descente amortie (une fraction de trame par paquet reçu).
    fn ajuster_delai_cible(&mut self) {
        let mut tri: Vec<u64> = self.fenetre_gigue.iter().copied().collect();
        tri.sort_unstable();
        let p95 = tri[(tri.len() - 1) * 95 / 100];
        let cible = (self.duree_trame_us + 2 * p95).clamp(self.delai_min_us, self.delai_max_us);
        if cible > self.delai_cible_us {
            self.delai_cible_us = cible; // monte vite : l'underrun coûte cher
        } else {
            let pas = (self.duree_trame_us / 8).max(1);
            self.delai_cible_us = self.delai_cible_us.saturating_sub(pas).max(cible);
        }
    }
}

impl Default for JitterBuffer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Une trame de 20 ms en µs.
    const TRAME: u64 = 20_000;

    /// Paquet factice : la charge encode l'horodatage pour tracer l'ordre.
    fn p(timestamp_us: u64) -> AudioPacket {
        AudioPacket {
            data: timestamp_us.to_le_bytes().to_vec(),
            timestamp_us,
        }
    }

    /// Jitter buffer à délai figé (min == max) pour des tests déterministes.
    fn tampon_fixe(delai_us: u64) -> JitterBuffer {
        JitterBuffer::avec_parametres(TRAME, delai_us, delai_us)
    }

    /// Extrait l'horodatage d'un tirage attendu comme `Paquet`.
    fn ts_du_paquet(sortie: SortieJitter) -> u64 {
        match sortie {
            SortieJitter::Paquet(p) => p.timestamp_us,
            autre => panic!("paquet attendu, obtenu {autre:?}"),
        }
    }

    #[test]
    fn vide_au_depart() {
        let mut jb = JitterBuffer::new();
        assert!(matches!(jb.suivant(1_000_000), SortieJitter::Attente));
        assert_eq!(jb.longueur(), 0);
    }

    #[test]
    fn attente_avant_echeance_puis_restitution() {
        let mut jb = tampon_fixe(40_000);
        jb.inserer(p(0), 0);
        jb.inserer(p(TRAME), TRAME);

        // Rien n'est dû avant `arrivée projetée + délai cible`.
        assert!(matches!(jb.suivant(0), SortieJitter::Attente));
        assert!(matches!(jb.suivant(39_999), SortieJitter::Attente));
        // Échéances : 0 + 40 ms puis 20 ms + 40 ms.
        assert_eq!(ts_du_paquet(jb.suivant(40_000)), 0);
        assert!(matches!(jb.suivant(40_001), SortieJitter::Attente));
        assert_eq!(ts_du_paquet(jb.suivant(60_000)), TRAME);
    }

    #[test]
    fn reordonne_les_paquets_arrives_dans_le_desordre() {
        let mut jb = tampon_fixe(40_000);
        jb.inserer(p(2 * TRAME), 5_000);
        jb.inserer(p(0), 6_000);
        jb.inserer(p(TRAME), 7_000);

        // Tirés bien après toutes les échéances : l'ordre média est rétabli.
        assert_eq!(ts_du_paquet(jb.suivant(200_000)), 0);
        assert_eq!(ts_du_paquet(jb.suivant(200_000)), TRAME);
        assert_eq!(ts_du_paquet(jb.suivant(200_000)), 2 * TRAME);
        assert_eq!(jb.stats().trous, 0);
    }

    #[test]
    fn paquet_manquant_signale_comme_trou_pour_plc() {
        let mut jb = tampon_fixe(40_000);
        jb.inserer(p(0), 0);
        jb.inserer(p(2 * TRAME), 2 * TRAME); // la trame à 20 ms est perdue

        assert_eq!(ts_du_paquet(jb.suivant(40_000)), 0);
        match jb.suivant(60_000) {
            SortieJitter::Trou { timestamp_us } => assert_eq!(timestamp_us, TRAME),
            autre => panic!("trou attendu, obtenu {autre:?}"),
        }
        // La lecture reprend sur la trame suivante, à son échéance.
        assert_eq!(ts_du_paquet(jb.suivant(80_000)), 2 * TRAME);
        assert_eq!(jb.stats().trous, 1);
    }

    #[test]
    fn paquet_en_retard_ecarte_apres_son_trou() {
        let mut jb = tampon_fixe(40_000);
        jb.inserer(p(0), 0);
        jb.inserer(p(2 * TRAME), 2 * TRAME);

        assert_eq!(ts_du_paquet(jb.suivant(40_000)), 0);
        assert!(matches!(jb.suivant(60_000), SortieJitter::Trou { .. }));

        // La trame à 20 ms arrive après son échéance : trop tard, écartée.
        jb.inserer(p(TRAME), 61_000);
        assert_eq!(jb.stats().en_retard, 1);
        assert_eq!(ts_du_paquet(jb.suivant(80_000)), 2 * TRAME);
    }

    #[test]
    fn doublon_ecarte() {
        let mut jb = tampon_fixe(40_000);
        jb.inserer(p(0), 0);
        jb.inserer(p(0), 1_000);
        assert_eq!(jb.stats().doublons, 1);
        assert_eq!(jb.longueur(), 1);
    }

    #[test]
    fn gel_apres_la_portee_du_plc_sur_tampon_vide() {
        let mut jb = tampon_fixe(40_000);
        jb.inserer(p(0), 0);

        assert_eq!(ts_du_paquet(jb.suivant(40_000)), 0);
        // Flux interrompu : au plus MAX_TROUS_CONSECUTIFS trous, puis gel.
        assert!(matches!(jb.suivant(60_000), SortieJitter::Trou { .. }));
        assert!(matches!(jb.suivant(80_000), SortieJitter::Trou { .. }));
        assert!(matches!(jb.suivant(100_000), SortieJitter::Trou { .. }));
        assert!(matches!(jb.suivant(120_000), SortieJitter::Attente));
        assert!(matches!(jb.suivant(10_000_000), SortieJitter::Attente));
        assert_eq!(jb.stats().trous, u64::from(MAX_TROUS_CONSECUTIFS));
    }

    #[test]
    fn resynchronisation_apres_longue_coupure() {
        let mut jb = tampon_fixe(40_000);
        jb.inserer(p(0), 0);
        assert_eq!(ts_du_paquet(jb.suivant(40_000)), 0);
        for t in [60_000, 80_000, 100_000] {
            assert!(matches!(jb.suivant(t), SortieJitter::Trou { .. }));
        }
        assert!(matches!(jb.suivant(120_000), SortieJitter::Attente));

        // Le flux reprend 200 ms plus loin : resynchronisation sans rafale de
        // trous, puis lecture à l'échéance du nouveau point (200 ms + 40 ms).
        jb.inserer(p(200_000), 200_000);
        assert!(matches!(jb.suivant(200_000), SortieJitter::Attente));
        assert_eq!(ts_du_paquet(jb.suivant(240_000)), 200_000);
        assert_eq!(jb.stats().resynchronisations, 1);
        assert_eq!(jb.stats().trous, u64::from(MAX_TROUS_CONSECUTIFS));
    }

    #[test]
    fn delai_adaptatif_monte_sous_gigue() {
        let mut jb = JitterBuffer::avec_parametres(TRAME, 20_000, 120_000);
        assert_eq!(jb.delai_cible_us(), 20_000); // démarre bas

        // Arrivées en dents de scie : ±15 ms de gigue inter-arrivée.
        for i in 0..50u64 {
            let ts = i * TRAME;
            let gigue = if i % 2 == 1 { 15_000 } else { 0 };
            jb.inserer(p(ts), ts + gigue);
        }
        // Cible = une trame + 2 × P95(15 ms) = 50 ms, montée immédiate.
        assert_eq!(jb.delai_cible_us(), 50_000);
    }

    #[test]
    fn delai_adaptatif_borne_par_le_maximum() {
        let mut jb = JitterBuffer::avec_parametres(TRAME, 20_000, 120_000);
        for i in 0..20u64 {
            let ts = i * TRAME;
            let gigue = if i % 2 == 1 { 500_000 } else { 0 };
            jb.inserer(p(ts), ts + gigue);
        }
        assert_eq!(jb.delai_cible_us(), 120_000);
    }

    #[test]
    fn delai_adaptatif_redescend_lentement_apres_accalmie() {
        let mut jb = JitterBuffer::avec_parametres(TRAME, 20_000, 120_000);
        for i in 0..50u64 {
            let ts = i * TRAME;
            let gigue = if i % 2 == 1 { 15_000 } else { 0 };
            jb.inserer(p(ts), ts + gigue);
        }
        let pic = jb.delai_cible_us();
        assert_eq!(pic, 50_000);

        // Réseau redevenu stable : la cible redescend pas à pas jusqu'au
        // minimum une fois la fenêtre de gigue purgée.
        for i in 50..200u64 {
            let ts = i * TRAME;
            jb.inserer(p(ts), ts);
        }
        assert!(jb.delai_cible_us() < pic);
        assert_eq!(jb.delai_cible_us(), 20_000);
    }

    #[test]
    fn delai_fige_quand_min_egale_max() {
        let mut jb = tampon_fixe(40_000);
        for i in 0..30u64 {
            let ts = i * TRAME;
            let gigue = if i % 2 == 1 { 90_000 } else { 0 };
            jb.inserer(p(ts), ts + gigue);
        }
        assert_eq!(jb.delai_cible_us(), 40_000);
    }
}
