//! FEC par effacement (Reed-Solomon sur GF(2^8)) pour la vidéo/audio.
//!
//! Sur les datagrammes non fiables (voir module [`quic`](crate::quic) et plan 04),
//! retransmettre coûte un aller-retour entier : inacceptable pour le média temps réel.
//! À la place, l'émetteur découpe chaque charge (trame vidéo, lot audio) en `k`
//! fragments de données et y ajoute `m` fragments de parité ; le récepteur reconstruit
//! la charge d'origine dès que **`k` fragments quelconques** parmi les `k + m` arrivent.
//!
//! Le module est volontairement indépendant du chemin réseau : il transforme des
//! octets en fragments ([`FecShard`]) et inversement, en mémoire. Le branchement sur
//! les datagrammes QUIC (en-tête de lot, numérotation) viendra dans un second temps.
//!
//! Padding : la longueur d'origine est mémorisée dans un préfixe de 4 octets
//! (u32 petit-boutiste) encodé **avec** la charge, si bien que le décodage n'a besoin
//! que des fragments et des paramètres `(k, m)` pour restituer exactement les octets
//! d'entrée, même quand la taille n'est pas un multiple de `k`.

use nd_proto::{NdError, Result};
use reed_solomon_erasure::galois_8::ReedSolomon;

/// Nombre maximal de fragments par lot (données + parité) : ordre du corps GF(2^8).
pub const MAX_SHARDS: usize = 256;

/// Taille du préfixe mémorisant la longueur d'origine (u32 petit-boutiste).
const LEN_PREFIX: usize = 4;

/// Taux de perte au-delà duquel on cesse d'augmenter la parité : à plus de 50 % de
/// pertes le lien est de toute façon inutilisable, inonder de parité n'aiderait pas.
const MAX_LOSS: f32 = 0.5;

/// Marge de sécurité du taux adaptatif : on provisionne le double du taux de perte
/// observé pour survivre aux rafales (les pertes réseau ne sont pas uniformes).
const LOSS_HEADROOM: f32 = 2.0;

/// Fragment FEC prêt à partir dans un datagramme.
///
/// Tous les fragments d'un même lot ont la même longueur. Les index `0..k` portent
/// les données, les index `k..k+m` la parité.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FecShard {
    /// Position du fragment dans le lot (tient sur un octet : `MAX_SHARDS` = 256).
    pub index: u8,
    /// Octets du fragment.
    pub data: Vec<u8>,
}

/// Paramètres d'un lot FEC : `k` fragments de données + `m` fragments de parité.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FecParams {
    /// `k` — nombre de fragments de données.
    pub data_shards: usize,
    /// `m` — nombre de fragments de parité (autant de pertes tolérées).
    pub parity_shards: usize,
}

impl FecParams {
    /// Valide et construit des paramètres FEC.
    ///
    /// Contraintes : `k ≥ 1`, `m ≥ 1`, `k + m ≤ MAX_SHARDS` (ordre de GF(2^8)).
    pub fn new(data_shards: usize, parity_shards: usize) -> Result<Self> {
        if data_shards == 0 {
            return Err(NdError::Transport(
                "FEC : il faut au moins 1 fragment de données (k ≥ 1)".into(),
            ));
        }
        if parity_shards == 0 {
            return Err(NdError::Transport(
                "FEC : il faut au moins 1 fragment de parité (m ≥ 1)".into(),
            ));
        }
        if data_shards + parity_shards > MAX_SHARDS {
            return Err(NdError::Transport(format!(
                "FEC : k + m = {} dépasse le maximum {MAX_SHARDS} (GF(2^8))",
                data_shards + parity_shards
            )));
        }
        Ok(Self {
            data_shards,
            parity_shards,
        })
    }

    /// Paramétrage adaptatif du surcoût selon le taux de perte observé
    /// (typiquement [`PathEstimate::loss_ratio`](crate::PathEstimate)).
    ///
    /// La parité vaut `⌈k × perte × marge⌉`, bornée à `[1, k]` et au plafond GF(2^8) :
    /// au minimum un fragment de parité (même sans perte mesurée, un datagramme peut
    /// toujours disparaître), au maximum 100 % de surcoût.
    pub fn adapt(data_shards: usize, loss_ratio: f32) -> Result<Self> {
        if data_shards == 0 || data_shards >= MAX_SHARDS {
            return Err(NdError::Transport(format!(
                "FEC : k = {data_shards} hors de l'intervalle [1, {}]",
                MAX_SHARDS - 1
            )));
        }
        // Mesure invalide (NaN/inf) : on retombe sur le surcoût minimal.
        let loss = if loss_ratio.is_finite() {
            loss_ratio.clamp(0.0, MAX_LOSS)
        } else {
            0.0
        };
        #[allow(clippy::cast_precision_loss, clippy::cast_sign_loss)]
        let brut = ((data_shards as f32) * loss * LOSS_HEADROOM).ceil() as usize;
        let plafond = data_shards.min(MAX_SHARDS - data_shards);
        let parity = brut.clamp(1, plafond);
        Self::new(data_shards, parity)
    }

    /// Nombre total de fragments d'un lot (`k + m`).
    #[must_use]
    pub fn total_shards(&self) -> usize {
        self.data_shards + self.parity_shards
    }
}

/// Traduit une erreur du codec Reed-Solomon en erreur transport.
fn erreur_codec(e: reed_solomon_erasure::Error) -> NdError {
    NdError::Transport(format!("codec Reed-Solomon : {e}"))
}

/// Construit le codec Reed-Solomon correspondant aux paramètres.
fn codec(params: FecParams) -> Result<ReedSolomon> {
    ReedSolomon::new(params.data_shards, params.parity_shards).map_err(erreur_codec)
}

/// Encodeur FEC : découpe une charge en `k` fragments de données + `m` de parité.
///
/// Le codec (matrices de Vandermonde) est construit une fois pour toutes : à réutiliser
/// tant que les paramètres ne changent pas, plutôt que d'appeler [`encode`] en boucle.
pub struct FecEncoder {
    params: FecParams,
    rs: ReedSolomon,
}

impl FecEncoder {
    /// Construit un encodeur pour les paramètres donnés.
    pub fn new(params: FecParams) -> Result<Self> {
        Ok(Self {
            params,
            rs: codec(params)?,
        })
    }

    /// Paramètres du lot produit par cet encodeur.
    #[must_use]
    pub fn params(&self) -> FecParams {
        self.params
    }

    /// Découpe `payload` en `k + m` fragments de même longueur.
    ///
    /// La longueur d'origine est préfixée (4 octets) puis l'ensemble est complété de
    /// zéros jusqu'à un multiple de `k` : le padding est donc transparent au décodage.
    pub fn encode(&self, payload: &[u8]) -> Result<Vec<FecShard>> {
        let k = self.params.data_shards;
        let m = self.params.parity_shards;
        let longueur = u32::try_from(payload.len()).map_err(|_| {
            NdError::Transport(format!(
                "FEC : charge de {} octets, maximum {} (préfixe u32)",
                payload.len(),
                u32::MAX
            ))
        })?;

        // Préfixe de longueur + charge, complétés de zéros jusqu'à k × taille_fragment.
        let taille_fragment = (LEN_PREFIX + payload.len()).div_ceil(k);
        let mut source = Vec::with_capacity(k * taille_fragment);
        source.extend_from_slice(&longueur.to_le_bytes());
        source.extend_from_slice(payload);
        source.resize(k * taille_fragment, 0);

        // k fragments de données suivis de m fragments de parité (remplis par le codec).
        let mut fragments: Vec<Vec<u8>> = Vec::with_capacity(k + m);
        fragments.extend(source.chunks(taille_fragment).map(<[u8]>::to_vec));
        fragments.extend(std::iter::repeat_with(|| vec![0u8; taille_fragment]).take(m));
        self.rs.encode(&mut fragments).map_err(erreur_codec)?;

        Ok(fragments
            .into_iter()
            .enumerate()
            // k + m ≤ 256 est garanti par `FecParams::new`, l'index tient sur un octet.
            .map(|(i, data)| FecShard {
                index: i as u8,
                data,
            })
            .collect())
    }
}

/// Décodeur FEC : reconstruit la charge d'origine à partir d'un sous-ensemble de
/// fragments, dès qu'au moins `k` sur `k + m` sont présents (dans n'importe quel ordre).
pub struct FecDecoder {
    params: FecParams,
    rs: ReedSolomon,
}

impl FecDecoder {
    /// Construit un décodeur pour les paramètres donnés (identiques à l'encodeur).
    pub fn new(params: FecParams) -> Result<Self> {
        Ok(Self {
            params,
            rs: codec(params)?,
        })
    }

    /// Paramètres du lot attendu par ce décodeur.
    #[must_use]
    pub fn params(&self) -> FecParams {
        self.params
    }

    /// Reconstruit la charge d'origine à partir des fragments reçus.
    ///
    /// L'ordre des fragments est libre et les doublons d'index sont ignorés (premier
    /// arrivé conservé). Échoue proprement si moins de `k` fragments distincts sont
    /// présents, si un index sort du lot ou si les longueurs sont incohérentes.
    pub fn decode(&self, shards: &[FecShard]) -> Result<Vec<u8>> {
        let k = self.params.data_shards;
        let total = self.params.total_shards();

        // Range chaque fragment reçu à sa place ; None = fragment perdu.
        let mut cases: Vec<Option<Vec<u8>>> = vec![None; total];
        let mut presents = 0usize;
        for shard in shards {
            let idx = usize::from(shard.index);
            if idx >= total {
                return Err(NdError::Transport(format!(
                    "FEC : index de fragment {idx} hors du lot (k + m = {total})"
                )));
            }
            if cases[idx].is_none() {
                cases[idx] = Some(shard.data.clone());
                presents += 1;
            }
        }
        if presents < k {
            return Err(NdError::Transport(format!(
                "FEC : fragments insuffisants ({presents} présents, {k} requis sur {total})"
            )));
        }

        // Reconstruit uniquement les fragments de données manquants.
        self.rs.reconstruct_data(&mut cases).map_err(erreur_codec)?;

        // Recolle les k fragments de données puis retire préfixe et padding.
        let mut donnees = Vec::new();
        for case in cases.iter().take(k) {
            donnees.extend_from_slice(case.as_deref().expect("fragment de données reconstruit"));
        }
        if donnees.len() < LEN_PREFIX {
            return Err(NdError::Transport(
                "FEC : lot trop court pour contenir le préfixe de longueur".into(),
            ));
        }
        let prefixe: [u8; LEN_PREFIX] = donnees[..LEN_PREFIX]
            .try_into()
            .expect("préfixe de longueur de 4 octets");
        let longueur = u32::from_le_bytes(prefixe) as usize;
        if longueur > donnees.len() - LEN_PREFIX {
            return Err(NdError::Transport(format!(
                "FEC : longueur d'origine incohérente ({longueur} octets annoncés, {} disponibles) — paramètres k/m erronés ?",
                donnees.len() - LEN_PREFIX
            )));
        }
        donnees.drain(..LEN_PREFIX);
        donnees.truncate(longueur);
        Ok(donnees)
    }
}

/// Encode `payload` en `k + m` fragments en une passe.
///
/// Construit un codec jetable : pour un flux continu, préférer [`FecEncoder`].
pub fn encode(payload: &[u8], data_shards: usize, parity_shards: usize) -> Result<Vec<FecShard>> {
    FecEncoder::new(FecParams::new(data_shards, parity_shards)?)?.encode(payload)
}

/// Reconstruit la charge d'origine à partir d'au moins `k` fragments sur `k + m`.
///
/// Construit un codec jetable : pour un flux continu, préférer [`FecDecoder`].
pub fn decode(shards: &[FecShard], data_shards: usize, parity_shards: usize) -> Result<Vec<u8>> {
    FecDecoder::new(FecParams::new(data_shards, parity_shards)?)?.decode(shards)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Charge de test pseudo-aléatoire mais déterministe.
    fn charge(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i.wrapping_mul(31) % 251) as u8).collect()
    }

    /// Retire les fragments dont l'index figure dans `perdus`.
    fn sans(fragments: &[FecShard], perdus: &[u8]) -> Vec<FecShard> {
        fragments
            .iter()
            .filter(|s| !perdus.contains(&s.index))
            .cloned()
            .collect()
    }

    #[test]
    fn round_trip_sans_perte() {
        let payload = charge(1000);
        let fragments = encode(&payload, 4, 2).expect("encodage");
        assert_eq!(fragments.len(), 6);
        // Tous les fragments d'un lot ont la même longueur.
        assert!(fragments
            .iter()
            .all(|s| s.data.len() == fragments[0].data.len()));
        let decode = decode(&fragments, 4, 2).expect("décodage");
        assert_eq!(decode, payload);
    }

    #[test]
    fn reconstruction_en_perdant_jusqu_a_m_fragments() {
        let (k, m) = (4usize, 2usize);
        let payload = charge(777);
        let fragments = encode(&payload, k, m).expect("encodage");
        let decodeur = FecDecoder::new(FecParams::new(k, m).expect("paramètres")).expect("codec");

        // Toutes les paires de pertes possibles parmi les k + m fragments.
        let total = (k + m) as u8;
        for a in 0..total {
            for b in (a + 1)..total {
                let restants = sans(&fragments, &[a, b]);
                let reconstruit = decodeur
                    .decode(&restants)
                    .unwrap_or_else(|e| panic!("pertes ({a}, {b}) : {e}"));
                assert_eq!(reconstruit, payload, "pertes ({a}, {b})");
            }
        }
    }

    #[test]
    fn reconstruction_independante_de_l_ordre() {
        let payload = charge(321);
        let fragments = encode(&payload, 5, 3).expect("encodage");
        // Perd 3 fragments (dont des données) puis mélange l'ordre des restants.
        let mut restants = sans(&fragments, &[0, 2, 6]);
        restants.reverse();
        restants.rotate_left(2);
        assert_eq!(decode(&restants, 5, 3).expect("décodage"), payload);
    }

    #[test]
    fn echec_propre_si_plus_de_m_fragments_manquent() {
        let payload = charge(500);
        let fragments = encode(&payload, 4, 2).expect("encodage");
        // 3 pertes pour m = 2 : reconstruction impossible, erreur propre attendue.
        let restants = sans(&fragments, &[0, 1, 4]);
        let erreur = decode(&restants, 4, 2).expect_err("doit échouer");
        assert!(
            erreur.to_string().contains("insuffisants"),
            "message inattendu : {erreur}"
        );
    }

    #[test]
    fn index_hors_lot_rejete() {
        let fragments = vec![FecShard {
            index: 9,
            data: vec![0u8; 8],
        }];
        assert!(decode(&fragments, 4, 2).is_err());
    }

    #[test]
    fn doublons_d_index_ignores() {
        let payload = charge(64);
        let mut fragments = encode(&payload, 3, 2).expect("encodage");
        // Duplique un fragment (datagramme reçu deux fois) et perd deux autres.
        let double = fragments[1].clone();
        fragments.push(double);
        let restants = sans(&fragments, &[0, 3]);
        assert_eq!(decode(&restants, 3, 2).expect("décodage"), payload);
    }

    #[test]
    fn tailles_variees_y_compris_non_multiples_de_k() {
        let (k, m) = (5usize, 3usize);
        // Tailles pathologiques : vide, 1 octet, autour de k, non multiples de k, grandes.
        for taille in [0, 1, 3, 4, 5, 6, 17, 100, 1499, 4096, 65537] {
            let payload = charge(taille);
            let fragments = encode(&payload, k, m).expect("encodage");
            assert_eq!(fragments.len(), k + m, "taille {taille}");
            // Sans perte.
            assert_eq!(decode(&fragments, k, m).expect("décodage"), payload);
            // Avec m pertes (les premiers fragments de données, cas le plus dur).
            let restants = sans(&fragments, &[0, 1, 2]);
            assert_eq!(
                decode(&restants, k, m).expect("décodage avec pertes"),
                payload,
                "taille {taille}"
            );
        }
    }

    #[test]
    fn parametres_invalides_rejetes() {
        assert!(FecParams::new(0, 1).is_err());
        assert!(FecParams::new(1, 0).is_err());
        assert!(FecParams::new(200, 100).is_err()); // 300 > 256
        assert!(FecParams::new(200, 56).is_ok()); // 256 = maximum accepté
    }

    #[test]
    fn taux_adaptatif_selon_les_pertes() {
        // Sans perte : surcoût minimal (1 fragment de parité).
        assert_eq!(FecParams::adapt(10, 0.0).expect("adapt").parity_shards, 1);
        // 10 % de pertes, marge ×2 : ⌈10 × 0,1 × 2⌉ = 2.
        assert_eq!(FecParams::adapt(10, 0.10).expect("adapt").parity_shards, 2);
        // 30 % de pertes : ⌈10 × 0,3 × 2⌉ = 6.
        assert_eq!(FecParams::adapt(10, 0.30).expect("adapt").parity_shards, 6);
        // Pertes extrêmes : plafonné à 100 % de surcoût (m = k).
        assert_eq!(FecParams::adapt(10, 0.95).expect("adapt").parity_shards, 10);
        // Mesure invalide : retombe sur le minimum.
        assert_eq!(
            FecParams::adapt(10, f32::NAN).expect("adapt").parity_shards,
            1
        );
        // Plafond GF(2^8) respecté même pour de grands k.
        let p = FecParams::adapt(200, 0.5).expect("adapt");
        assert_eq!(p.parity_shards, 56);
        assert!(p.total_shards() <= MAX_SHARDS);
        // k invalide.
        assert!(FecParams::adapt(0, 0.1).is_err());
        assert!(FecParams::adapt(256, 0.1).is_err());
    }

    #[test]
    fn round_trip_adaptatif_survit_au_taux_cible() {
        // Le paramétrage adaptatif pour 20 % de pertes doit survivre à 20 % de pertes.
        let params = FecParams::adapt(10, 0.20).expect("adapt");
        let payload = charge(2048);
        let encodeur = FecEncoder::new(params).expect("encodeur");
        let decodeur = FecDecoder::new(params).expect("décodeur");
        let fragments = encodeur.encode(&payload).expect("encodage");
        // 20 % du lot perdus (arrondi à l'entier inférieur, ≤ m par construction).
        let pertes: Vec<u8> = (0..params.total_shards() / 5)
            .map(|i| (i * 3) as u8)
            .collect();
        assert!(pertes.len() <= params.parity_shards);
        let restants = sans(&fragments, &pertes);
        assert_eq!(decodeur.decode(&restants).expect("décodage"), payload);
    }
}
