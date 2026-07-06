//! Chemin datagrammes non fiables + FEC (plan 04) : fragmentation côté émission,
//! réassemblage côté réception.
//!
//! L'émetteur découpe chaque charge média (trame vidéo, lot audio) en fragments qui
//! tiennent dans un datagramme QUIC, ajoute la parité Reed-Solomon ([`crate::fec`])
//! dimensionnée d'après la perte observée ([`FecParams::adapt`]), et coiffe chaque
//! fragment d'un en-tête compact. Le récepteur regroupe les fragments par trame et
//! reconstruit la charge dès que `k` fragments distincts sont arrivés — sans jamais
//! retransmettre : au-delà de la parité, la trame est simplement perdue (le codec
//! vidéo demandera une image clé, voir plan 03).
//!
//! Format d'un datagramme (grand-boutiste, en-tête de [`HEADER_LEN`] octets) :
//!
//! ```text
//! [tag u8][moniteur u32][trame u32][index u8][k u8][m u8][longueur u32][fragment…]
//! ```
//!
//! * `tag` / `moniteur` : canal logique (mêmes valeurs que le flux fiable) ;
//! * `trame` : identifiant croissant de la charge d'origine (modulo 2³²) ;
//! * `index` : position du fragment dans le lot FEC (`0..k` données, `k..k+m` parité) ;
//! * `k` / `m` : paramètres du lot, portés par chaque fragment — le lot est
//!   auto-descriptif, aucune signalisation hors bande n'est nécessaire ;
//! * `longueur` : taille du fragment, recoupée avec la taille du datagramme
//!   (les datagrammes QUIC préservent les frontières, c'est une ceinture de sécurité).

use std::collections::{HashMap, VecDeque};

use bytes::Bytes;
use nd_proto::{ChannelKind, Result};

use crate::fec::{FecDecoder, FecEncoder, FecParams, FecShard, LEN_PREFIX};
use crate::quic::{kind_tag, tag_kind};

/// Taille de l'en-tête d'un fragment datagramme.
pub(crate) const HEADER_LEN: usize = 16;

/// Nombre maximal de fragments de données (`k`) par trame sur le chemin datagrammes.
///
/// Au-delà, la trame repart sur le flux fiable : garder `k ≤ 192` réserve au moins
/// `256 − 192 = 64` positions de parité dans GF(2⁸), soit ≥ 33 % de surcoût possible
/// même pour les plus grosses trames.
const MAX_K: usize = 192;

/// Nombre maximal de trames en cours de réassemblage. À saturation, la plus ancienne
/// est abandonnée : sémantique non fiable, une trame incomplète finit perdue.
const MAX_TRAMES_EN_VOL: usize = 64;

/// Nombre maximal de décodeurs mis en cache (paires `(k, m)` distinctes) ; au-delà,
/// le cache est vidé (les matrices se reconstruisent, c'est juste moins rapide).
const MAX_DECODEURS: usize = 32;

/// En-tête d'un fragment datagramme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EnTete {
    /// Tag du canal logique (mêmes valeurs que le flux fiable).
    tag: u8,
    /// Moniteur pour les canaux vidéo, 0 sinon.
    moniteur: u32,
    /// Identifiant de la trame d'origine (croissant, modulo 2³²).
    trame: u32,
    /// Position du fragment dans le lot FEC.
    index: u8,
    /// Nombre de fragments de données du lot.
    k: u8,
    /// Nombre de fragments de parité du lot.
    m: u8,
    /// Longueur du fragment (hors en-tête).
    longueur: u32,
}

impl EnTete {
    /// Sérialise l'en-tête suivi de `fragment` en un datagramme prêt à émettre.
    fn en_datagramme(&self, fragment: &[u8]) -> Bytes {
        let mut out = Vec::with_capacity(HEADER_LEN + fragment.len());
        out.push(self.tag);
        out.extend_from_slice(&self.moniteur.to_be_bytes());
        out.extend_from_slice(&self.trame.to_be_bytes());
        out.push(self.index);
        out.push(self.k);
        out.push(self.m);
        out.extend_from_slice(&self.longueur.to_be_bytes());
        out.extend_from_slice(fragment);
        Bytes::from(out)
    }

    /// Découpe un datagramme reçu en en-tête + fragment ; `None` s'il est trop court.
    fn lire(datagramme: &[u8]) -> Option<(Self, &[u8])> {
        if datagramme.len() < HEADER_LEN {
            return None;
        }
        let (hdr, fragment) = datagramme.split_at(HEADER_LEN);
        Some((
            Self {
                tag: hdr[0],
                moniteur: u32::from_be_bytes([hdr[1], hdr[2], hdr[3], hdr[4]]),
                trame: u32::from_be_bytes([hdr[5], hdr[6], hdr[7], hdr[8]]),
                index: hdr[9],
                k: hdr[10],
                m: hdr[11],
                longueur: u32::from_be_bytes([hdr[12], hdr[13], hdr[14], hdr[15]]),
            },
            fragment,
        ))
    }
}

/// Fragmenteur côté émission : numérote les trames et réutilise l'encodeur FEC tant
/// que les paramètres `(k, m)` ne changent pas (construire les matrices coûte).
#[derive(Default)]
pub(crate) struct Fragmenteur {
    /// Identifiant attribué à la prochaine trame.
    prochaine_trame: u32,
    /// Dernier encodeur utilisé, réutilisé si les paramètres n'ont pas bougé.
    encodeur: Option<FecEncoder>,
}

impl Fragmenteur {
    /// Découpe `charge` en datagrammes (en-tête + fragment FEC) prêts à émettre.
    ///
    /// `max_datagramme` est le MTU datagramme courant de la connexion
    /// (`Connection::max_datagram_size`) ; la parité est dimensionnée par
    /// [`FecParams::adapt`] d'après `perte` (taux de perte lissé du chemin).
    ///
    /// Renvoie `Ok(None)` quand le chemin datagrammes ne convient pas — MTU plus
    /// petit que l'en-tête, ou trame exigeant plus de [`MAX_K`] fragments — auquel
    /// cas l'appelant replie sur le flux fiable.
    pub(crate) fn fragmenter(
        &mut self,
        kind: ChannelKind,
        charge: &[u8],
        max_datagramme: usize,
        perte: f32,
    ) -> Result<Option<Vec<Bytes>>> {
        let utile = max_datagramme.saturating_sub(HEADER_LEN);
        if utile == 0 {
            return Ok(None);
        }
        // k minimal tel que ⌈(LEN_PREFIX + longueur) / k⌉ ≤ utile : chaque fragment
        // (préfixe de longueur FEC compris) tient dans un datagramme.
        let k = (LEN_PREFIX + charge.len()).div_ceil(utile);
        if k > MAX_K {
            return Ok(None);
        }
        let params = FecParams::adapt(k, perte)?;
        if self.encodeur.as_ref().map(|e| e.params()) != Some(params) {
            self.encodeur = Some(FecEncoder::new(params)?);
        }
        let encodeur = self
            .encodeur
            .as_ref()
            .expect("encodeur initialisé ci-dessus");

        let fragments = encodeur.encode(charge)?;
        let trame = self.prochaine_trame;
        self.prochaine_trame = self.prochaine_trame.wrapping_add(1);
        let (tag, moniteur) = kind_tag(kind);
        // `k + m ≤ 256` et `k ≤ MAX_K` (validés par `FecParams`) : tout tient sur un octet.
        let (k, m) = (params.data_shards as u8, params.parity_shards as u8);
        Ok(Some(
            fragments
                .into_iter()
                .map(|s| {
                    EnTete {
                        tag,
                        moniteur,
                        trame,
                        index: s.index,
                        k,
                        m,
                        longueur: s.data.len() as u32,
                    }
                    .en_datagramme(&s.data)
                })
                .collect(),
        ))
    }
}

/// Clé d'une trame en cours de réassemblage : `(tag, moniteur, id de trame)`.
type CleTrame = (u8, u32, u32);

/// Trame en cours de réassemblage.
struct TrameEnCours {
    /// Paramètres FEC annoncés par le premier fragment reçu.
    params: FecParams,
    /// Taille commune des fragments du lot.
    taille_fragment: usize,
    /// Fragments distincts reçus (au plus `k` : on décode dès le seuil atteint).
    fragments: Vec<FecShard>,
    /// Bitmap des index déjà vus (256 bits), pour écarter les doublons.
    vus: [u64; 4],
    /// Trame déjà livrée : les fragments tardifs sont ignorés jusqu'à l'éviction.
    livree: bool,
}

impl TrameEnCours {
    fn new(params: FecParams, taille_fragment: usize) -> Self {
        Self {
            params,
            taille_fragment,
            fragments: Vec::with_capacity(params.data_shards),
            vus: [0; 4],
            livree: false,
        }
    }

    /// Marque `index` comme vu ; renvoie `false` si c'était un doublon.
    fn marquer(&mut self, index: u8) -> bool {
        let mot = usize::from(index) / 64;
        let masque = 1u64 << (u64::from(index) % 64);
        if self.vus[mot] & masque != 0 {
            return false;
        }
        self.vus[mot] |= masque;
        true
    }
}

/// Réassembleur côté réception : regroupe les fragments par trame et reconstruit la
/// charge via FEC dès que `k` fragments distincts sont présents.
#[derive(Default)]
pub(crate) struct Reassembleur {
    /// Trames en cours, indexées par canal + identifiant de trame.
    trames: HashMap<CleTrame, TrameEnCours>,
    /// Ordre d'arrivée des trames, pour évincer la plus ancienne à saturation.
    ordre: VecDeque<CleTrame>,
    /// Décodeurs réutilisés par paramètres `(k, m)`.
    decodeurs: HashMap<(u8, u8), FecDecoder>,
}

impl Reassembleur {
    /// Absorbe un datagramme reçu ; renvoie la charge complète si sa trame vient de
    /// se compléter (exactement une livraison par trame).
    ///
    /// Les datagrammes malformés ou incohérents sont ignorés en silence : sur un
    /// chemin non fiable, jeter vaut mieux que fermer la connexion.
    pub(crate) fn absorber(&mut self, datagramme: &[u8]) -> Option<(ChannelKind, Vec<u8>)> {
        let (entete, fragment) = EnTete::lire(datagramme)?;
        let kind = tag_kind(entete.tag, entete.moniteur)?;
        let params = FecParams::new(usize::from(entete.k), usize::from(entete.m)).ok()?;
        if usize::from(entete.index) >= params.total_shards()
            || fragment.is_empty()
            || fragment.len() != entete.longueur as usize
        {
            return None;
        }

        let cle = (entete.tag, entete.moniteur, entete.trame);
        if !self.trames.contains_key(&cle) {
            if self.trames.len() >= MAX_TRAMES_EN_VOL {
                if let Some(ancienne) = self.ordre.pop_front() {
                    self.trames.remove(&ancienne);
                }
            }
            self.ordre.push_back(cle);
            self.trames
                .insert(cle, TrameEnCours::new(params, fragment.len()));
        }
        let trame = self
            .trames
            .get_mut(&cle)
            .expect("trame présente ou insérée");

        // Trame déjà livrée, fragment incohérent avec le lot, ou doublon : ignorés.
        if trame.livree || trame.params != params || trame.taille_fragment != fragment.len() {
            return None;
        }
        if !trame.marquer(entete.index) {
            return None;
        }
        trame.fragments.push(FecShard {
            index: entete.index,
            data: fragment.to_vec(),
        });
        if trame.fragments.len() < params.data_shards {
            return None;
        }

        // Seuil atteint : k fragments distincts suffisent à reconstruire la charge.
        let fragments = std::mem::take(&mut trame.fragments);
        trame.livree = true;
        let decodeur = self.decodeur(params)?;
        let charge = decodeur.decode(&fragments).ok()?;
        Some((kind, charge))
    }

    /// Décodeur (mis en cache) pour les paramètres donnés.
    fn decodeur(&mut self, params: FecParams) -> Option<&FecDecoder> {
        let cle = (params.data_shards as u8, params.parity_shards as u8);
        if !self.decodeurs.contains_key(&cle) {
            if self.decodeurs.len() >= MAX_DECODEURS {
                self.decodeurs.clear();
            }
            self.decodeurs.insert(cle, FecDecoder::new(params).ok()?);
        }
        self.decodeurs.get(&cle)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nd_proto::MonitorId;

    /// Charge de test pseudo-aléatoire mais déterministe.
    fn charge(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i.wrapping_mul(37) % 256) as u8).collect()
    }

    #[test]
    fn fragmentation_puis_reassemblage_sans_perte() {
        let mut fragmenteur = Fragmenteur::default();
        let kind = ChannelKind::Video(MonitorId(3));
        let payload = charge(50_000);
        let datagrammes = fragmenteur
            .fragmenter(kind, &payload, 1200, 0.0)
            .expect("fragmentation")
            .expect("chemin datagrammes retenu");
        assert!(datagrammes.iter().all(|d| d.len() <= 1200));

        let mut reassembleur = Reassembleur::default();
        let mut livraisons = 0;
        let mut recue = Vec::new();
        for d in &datagrammes {
            if let Some((k, c)) = reassembleur.absorber(d) {
                livraisons += 1;
                assert_eq!(k, kind);
                recue = c;
            }
        }
        assert_eq!(livraisons, 1, "livrée exactement une fois");
        assert_eq!(recue, payload);
    }

    #[test]
    fn reconstruction_malgre_fragments_perdus_et_desordre() {
        let mut fragmenteur = Fragmenteur::default();
        let kind = ChannelKind::Audio;
        let payload = charge(20_000);
        // 20 % de pertes annoncées : adapt provisionne le double (marge rafales).
        let datagrammes = fragmenteur
            .fragmenter(kind, &payload, 1200, 0.20)
            .expect("fragmentation")
            .expect("chemin datagrammes retenu");
        // Perd un datagramme sur cinq puis mélange l'ordre des restants.
        let mut restants: Vec<Bytes> = datagrammes
            .iter()
            .enumerate()
            .filter(|(i, _)| i % 5 != 0)
            .map(|(_, d)| d.clone())
            .collect();
        restants.reverse();
        restants.rotate_left(3);

        let mut reassembleur = Reassembleur::default();
        let mut recue = None;
        for d in &restants {
            if let Some((k, c)) = reassembleur.absorber(d) {
                assert_eq!(k, kind);
                recue = Some(c);
            }
        }
        assert_eq!(recue.expect("trame reconstruite"), payload);
    }

    #[test]
    fn trames_entrelacees_reassemblees_independamment() {
        let mut fragmenteur = Fragmenteur::default();
        let kind = ChannelKind::Video(MonitorId(0));
        let a = charge(9_000);
        let b: Vec<u8> = charge(7_000).iter().map(|x| x ^ 0xFF).collect();
        let da = fragmenteur
            .fragmenter(kind, &a, 1200, 0.0)
            .expect("fragmentation")
            .expect("datagrammes");
        let db = fragmenteur
            .fragmenter(kind, &b, 1200, 0.0)
            .expect("fragmentation")
            .expect("datagrammes");

        // Entrelace les deux trames.
        let mut reassembleur = Reassembleur::default();
        let mut recues = Vec::new();
        let mut ia = da.iter();
        let mut ib = db.iter();
        loop {
            let (na, nb) = (ia.next(), ib.next());
            for d in [na, nb].into_iter().flatten() {
                if let Some((_, c)) = reassembleur.absorber(d) {
                    recues.push(c);
                }
            }
            if na.is_none() && nb.is_none() {
                break;
            }
        }
        assert_eq!(recues.len(), 2);
        assert!(recues.contains(&a));
        assert!(recues.contains(&b));
    }

    #[test]
    fn doublons_et_fragments_tardifs_ignores() {
        let mut fragmenteur = Fragmenteur::default();
        let kind = ChannelKind::Audio;
        let payload = charge(5_000);
        let datagrammes = fragmenteur
            .fragmenter(kind, &payload, 1200, 0.10)
            .expect("fragmentation")
            .expect("datagrammes");

        let mut reassembleur = Reassembleur::default();
        let mut livraisons = 0;
        // Chaque datagramme passé deux fois : doublons puis fragments tardifs.
        for d in datagrammes.iter().chain(datagrammes.iter()) {
            if reassembleur.absorber(d).is_some() {
                livraisons += 1;
            }
        }
        assert_eq!(livraisons, 1, "une seule livraison malgré les doublons");
    }

    #[test]
    fn datagrammes_malformes_ignores() {
        let mut reassembleur = Reassembleur::default();
        // Trop court.
        assert!(reassembleur.absorber(&[]).is_none());
        assert!(reassembleur.absorber(&[0u8; HEADER_LEN - 1]).is_none());
        let brut = [1u8, 2, 3, 4];
        // Tag de canal inconnu.
        let entete = EnTete {
            tag: 9,
            moniteur: 0,
            trame: 0,
            index: 0,
            k: 2,
            m: 1,
            longueur: 4,
        };
        assert!(reassembleur
            .absorber(&entete.en_datagramme(&brut))
            .is_none());
        // Index hors du lot.
        let entete = EnTete {
            tag: 1,
            index: 5,
            ..entete
        };
        assert!(reassembleur
            .absorber(&entete.en_datagramme(&brut))
            .is_none());
        // Longueur incohérente avec la taille du datagramme.
        let entete = EnTete {
            index: 0,
            longueur: 99,
            ..entete
        };
        assert!(reassembleur
            .absorber(&entete.en_datagramme(&brut))
            .is_none());
        // Paramètres FEC invalides (k = 0).
        let entete = EnTete {
            k: 0,
            longueur: 4,
            ..entete
        };
        assert!(reassembleur
            .absorber(&entete.en_datagramme(&brut))
            .is_none());
        // Fragment vide.
        let entete = EnTete {
            k: 2,
            longueur: 0,
            ..entete
        };
        assert!(reassembleur.absorber(&entete.en_datagramme(&[])).is_none());
    }

    #[test]
    fn repli_quand_hors_gabarit() {
        let mut fragmenteur = Fragmenteur::default();
        // MTU qui ne laisse aucune place au fragment : pas de chemin datagrammes.
        assert!(fragmenteur
            .fragmenter(ChannelKind::Audio, &[1, 2, 3], HEADER_LEN, 0.0)
            .expect("fragmentation")
            .is_none());
        // Trame exigeant plus de MAX_K fragments : repli également.
        let enorme = vec![0u8; (MAX_K + 1) * 8];
        assert!(fragmenteur
            .fragmenter(ChannelKind::Audio, &enorme, HEADER_LEN + 8, 0.0)
            .expect("fragmentation")
            .is_none());
    }

    #[test]
    fn eviction_des_trames_les_plus_anciennes() {
        let mut fragmenteur = Fragmenteur::default();
        let kind = ChannelKind::Video(MonitorId(0));
        let mut reassembleur = Reassembleur::default();
        // Sature le réassembleur de trames incomplètes (k ≥ 2 : un seul fragment).
        for _ in 0..(MAX_TRAMES_EN_VOL + 8) {
            let datagrammes = fragmenteur
                .fragmenter(kind, &charge(4_000), 1200, 0.0)
                .expect("fragmentation")
                .expect("datagrammes");
            assert!(reassembleur.absorber(&datagrammes[0]).is_none());
        }
        assert!(reassembleur.trames.len() <= MAX_TRAMES_EN_VOL);
        // Une trame fraîche complète doit toujours passer.
        let payload = charge(4_000);
        let datagrammes = fragmenteur
            .fragmenter(kind, &payload, 1200, 0.0)
            .expect("fragmentation")
            .expect("datagrammes");
        let mut recue = None;
        for d in &datagrammes {
            if let Some((_, c)) = reassembleur.absorber(d) {
                recue = Some(c);
            }
        }
        assert_eq!(recue.expect("trame fraîche livrée"), payload);
    }
}
