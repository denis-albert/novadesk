//! `h264` — analyse minimale d'un flux H.264 **Annex B** (pur, testable sur l'hôte).
//!
//! Le client reçoit du pair des **unités d'accès** H.264 (une image codée) au format
//! Annex B (préfixes de démarrage `00 00 01` ou `00 00 00 01`). WebCodecs exige que
//! chaque `EncodedVideoChunk` soit étiqueté `key` (image-clé) ou `delta`, et que le
//! décodeur démarre sur une image-clé. Ces fonctions repèrent les NAL IDR pour poser
//! cette étiquette sans dépendre d'un drapeau applicatif.

/// Découpe un tampon Annex B en unités NAL (tranches **sans** le préfixe de démarrage),
/// dans l'ordre du flux. Les préfixes `00 00 01` et `00 00 00 01` sont acceptés.
///
/// Seul le début de chaque NAL importe pour l'analyse de type ; les éventuels octets
/// nuls de bourrage en fin de NAL sont sans incidence sur l'en-tête.
#[must_use]
pub fn unites_nal(flux: &[u8]) -> Vec<&[u8]> {
    let n = flux.len();
    // Positions des préfixes de démarrage `00 00 01` (le 0 supplémentaire d'un préfixe
    // 4 octets est simplement absorbé comme bourrage de fin de la NAL précédente).
    let mut prefixes: Vec<usize> = Vec::new();
    let mut i = 0usize;
    while i + 3 <= n {
        if flux[i] == 0 && flux[i + 1] == 0 && flux[i + 2] == 1 {
            prefixes.push(i);
            i += 3;
        } else {
            i += 1;
        }
    }

    let mut nals = Vec::with_capacity(prefixes.len());
    for (k, &pos) in prefixes.iter().enumerate() {
        let debut = pos + 3; // saute `00 00 01`
        let fin = prefixes.get(k + 1).copied().unwrap_or(n);
        if debut < fin {
            nals.push(&flux[debut..fin]);
        }
    }
    nals
}

/// Type de NAL (5 bits de poids faible du premier octet), ou `None` si vide.
#[must_use]
pub fn type_nal(nal: &[u8]) -> Option<u8> {
    nal.first().map(|octet| octet & 0x1F)
}

/// Vrai si l'unité d'accès contient au moins une NAL **IDR** (type 5) : c'est alors une
/// **image-clé** (point de resynchronisation) au sens WebCodecs.
#[must_use]
pub fn contient_idr(acces_unit: &[u8]) -> bool {
    unites_nal(acces_unit)
        .iter()
        .any(|nal| type_nal(nal) == Some(5))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decoupe_prefixes_3_et_4_octets() {
        // SPS (7) en préfixe 4 octets, puis IDR (5) en préfixe 3 octets.
        let flux = [0, 0, 0, 1, 0x67, 0xAA, 0, 0, 1, 0x65, 0xBB, 0xCC];
        let nals = unites_nal(&flux);
        assert_eq!(nals.len(), 2);
        assert_eq!(type_nal(nals[0]), Some(7)); // SPS
        assert_eq!(type_nal(nals[1]), Some(5)); // IDR
    }

    #[test]
    fn idr_detecte_comme_image_cle() {
        // 0x65 = NAL ref idc 3 + type 5 (IDR).
        let idr = [0, 0, 1, 0x65, 0x88];
        assert!(contient_idr(&idr));
    }

    #[test]
    fn tranche_non_idr_n_est_pas_cle() {
        // 0x41 = type 1 (tranche non-IDR).
        let delta = [0, 0, 1, 0x41, 0x99, 0x00];
        assert!(!contient_idr(&delta));
        assert_eq!(type_nal(unites_nal(&delta)[0]), Some(1));
    }

    #[test]
    fn flux_vide_ou_sans_prefixe() {
        assert!(unites_nal(&[]).is_empty());
        assert!(unites_nal(&[0x65, 0x00]).is_empty()); // aucun préfixe de démarrage
        assert!(!contient_idr(&[]));
    }

    #[test]
    fn type_nal_sur_nal_vide() {
        assert_eq!(type_nal(&[]), None);
    }
}
