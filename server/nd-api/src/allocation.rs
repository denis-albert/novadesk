//! Attribution des ID NovaDesk — uniques, non énumérables, liés à un compte.
//!
//! Un `NovaId` (voir `nd_proto::NovaId`) est un entier à **9 chiffres**
//! (`100 000 000..=999 999 999`), comme les ID AnyDesk. L'attribution suit le
//! plan 11 (FPE/FF1) : un **compteur persistant** passe dans une **permutation
//! pseudo-aléatoire à clé** (réseau de Feistel équilibré sur 30 bits, fonction
//! de ronde SHA-256, *cycle-walking* pour rester dans le domaine décimal),
//! comme le fait FF1 — deux compteurs distincts donnent deux ID distincts,
//! mais la suite émise est indevinable sans la clé de permutation :
//! impossible d'énumérer les ID voisins d'un ID connu.
//!
//! Chaque ID émis est **lié au compte demandeur et à la clé statique du
//! client** ; le magasin s'en souvient ([`AllocateurId::proprietaire`]) et ne
//! réattribue jamais un ID (compteur monotone persisté + registre des émis,
//! qui protège aussi d'un changement accidentel de clé de permutation).
//!
//! À l'allocation, `nd-api` émet le [`crate::auth::JetonEnregistrement`]
//! correspondant, exigé par le serveur de rendez-vous (anti-squatting).

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Plus petit ID NovaDesk attribuable (premier ID à 9 chiffres).
pub const ID_MIN: u64 = 100_000_000;
/// Taille du domaine d'attribution (tous les entiers à 9 chiffres).
pub const TAILLE_DOMAINE: u64 = 900_000_000;

/// Largeur du réseau de Feistel : 2^30 couvre le domaine (900 M < 2^30).
const BITS_FEISTEL: u32 = 30;
/// Largeur d'une moitié de Feistel (réseau équilibré 15 + 15).
const BITS_MOITIE: u32 = BITS_FEISTEL / 2;
/// Masque d'une moitié (15 bits).
const MASQUE_MOITIE: u64 = (1 << BITS_MOITIE) - 1;
/// Nombre de rondes (≥ 4 suffit pour une PRP ; 8 par marge, coût négligeable).
const RONDES: u8 = 8;

/// Erreurs métier de l'attribution d'ID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErreurAllocation {
    /// Compte demandeur vide.
    CompteVide,
    /// Tous les ID du domaine ont été émis (900 millions !).
    DomaineEpuise,
}

impl fmt::Display for ErreurAllocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ErreurAllocation::CompteVide => write!(f, "compte demandeur vide"),
            ErreurAllocation::DomaineEpuise => write!(f, "domaine d'ID épuisé"),
        }
    }
}

impl std::error::Error for ErreurAllocation {}

/// Un ID émis, tel que persisté : à qui il appartient et avec quelle clé
/// statique il a été lié (voir `crate::auth::JetonEnregistrement`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdEmis {
    /// ID NovaDesk attribué (9 chiffres).
    pub id: u64,
    /// Compte propriétaire (dérivé du jeton applicatif à l'allocation).
    pub compte: String,
    /// Clé publique statique (Ed25519) du client, en hexadécimal.
    pub cle_client_hex: String,
}

/// État interne de l'allocateur.
struct AllocInner {
    /// Prochain rang de compteur à permuter (monotone, jamais rembobiné).
    compteur: u64,
    /// Clé de la permutation (32 octets, persistée : la changer changerait
    /// l'ordre d'émission — le registre `emis` protège alors de la collision).
    cle: [u8; 32],
    /// Registre des ID émis : id → propriétaire.
    emis: HashMap<u64, IdEmis>,
}

/// Allocateur d'ID partagé, thread-safe et clonable (les clones partagent
/// le même état, comme les autres magasins de `nd-api`).
#[derive(Clone)]
pub struct AllocateurId(Arc<Mutex<AllocInner>>);

impl AllocateurId {
    /// Allocateur vierge avec une clé de permutation fraîche.
    ///
    /// # Errors
    /// Propage l'échec du générateur aléatoire du système.
    pub fn new() -> io::Result<Self> {
        let mut cle = [0u8; 32];
        getrandom::fill(&mut cle).map_err(io::Error::other)?;
        Ok(Self(Arc::new(Mutex::new(AllocInner {
            compteur: 0,
            cle,
            emis: HashMap::new(),
        }))))
    }

    /// Reconstruit l'allocateur depuis l'état persisté. Une clé absente ou
    /// illisible (premier démarrage, fichier antérieur) est régénérée — le
    /// registre des émis garantit alors qu'aucun ID déjà émis n'est réutilisé.
    ///
    /// # Errors
    /// Propage l'échec du générateur aléatoire du système.
    pub fn from_snapshot(compteur: u64, cle_hex: &str, emis: Vec<IdEmis>) -> io::Result<Self> {
        let cle = match hex::decode(cle_hex).ok().and_then(|o| o.try_into().ok()) {
            Some(cle) => cle,
            None => {
                let mut cle = [0u8; 32];
                getrandom::fill(&mut cle).map_err(io::Error::other)?;
                cle
            }
        };
        Ok(Self(Arc::new(Mutex::new(AllocInner {
            compteur,
            cle,
            emis: emis.into_iter().map(|e| (e.id, e)).collect(),
        }))))
    }

    /// Instantané persistable : (compteur, clé hexadécimale, émis triés par id).
    #[must_use]
    pub fn snapshot(&self) -> (u64, String, Vec<IdEmis>) {
        let inner = self.0.lock().unwrap();
        let mut emis: Vec<IdEmis> = inner.emis.values().cloned().collect();
        emis.sort_by_key(|e| e.id);
        (inner.compteur, hex::encode(inner.cle), emis)
    }

    /// Alloue un nouvel ID pour `compte`, lié à la clé statique `cle_client`.
    ///
    /// L'ID renvoyé est unique (jamais émis auparavant), à 9 chiffres, et non
    /// corrélé aux ID émis avant ou après lui.
    ///
    /// # Errors
    /// `CompteVide` si le compte est vide, `DomaineEpuise` si les 900 millions
    /// d'ID ont été émis.
    pub fn allouer(&self, compte: &str, cle_client: &[u8; 32]) -> Result<u64, ErreurAllocation> {
        if compte.trim().is_empty() {
            return Err(ErreurAllocation::CompteVide);
        }
        let mut inner = self.0.lock().unwrap();
        loop {
            if inner.compteur >= TAILLE_DOMAINE {
                return Err(ErreurAllocation::DomaineEpuise);
            }
            let id = ID_MIN + permuter(&inner.cle, inner.compteur);
            inner.compteur += 1;
            // Un ID déjà au registre ne peut venir que d'un changement de clé
            // de permutation : on avance simplement au rang suivant.
            if let std::collections::hash_map::Entry::Vacant(entree) = inner.emis.entry(id) {
                entree.insert(IdEmis {
                    id,
                    compte: compte.to_string(),
                    cle_client_hex: hex::encode(cle_client),
                });
                return Ok(id);
            }
        }
    }

    /// Compte propriétaire d'un ID émis, s'il existe.
    #[must_use]
    pub fn proprietaire(&self, id: u64) -> Option<String> {
        self.0
            .lock()
            .unwrap()
            .emis
            .get(&id)
            .map(|e| e.compte.clone())
    }

    /// `compte` est-il le propriétaire enregistré de l'ID `id` ?
    /// (`false` si l'ID n'a jamais été émis.)
    #[must_use]
    pub fn est_proprietaire(&self, id: u64, compte: &str) -> bool {
        self.0
            .lock()
            .unwrap()
            .emis
            .get(&id)
            .is_some_and(|e| e.compte == compte)
    }

    /// Nombre d'ID émis depuis l'origine.
    #[must_use]
    pub fn nombre_emis(&self) -> usize {
        self.0.lock().unwrap().emis.len()
    }
}

// ---------------------------------------------------------------------------
// Permutation pseudo-aléatoire du domaine décimal
// ---------------------------------------------------------------------------

/// Permutation à clé de `[0, TAILLE_DOMAINE)` : réseau de Feistel sur 30 bits
/// puis *cycle-walking* (on ré-applique le réseau tant que la sortie déborde
/// du domaine — en suivant ainsi le cycle, on obtient une permutation exacte
/// du domaine, technique standard du chiffrement préservant le format).
fn permuter(cle: &[u8; 32], valeur: u64) -> u64 {
    debug_assert!(valeur < TAILLE_DOMAINE);
    let mut courant = feistel(cle, valeur);
    // Terminaison garantie : `feistel` est une permutation de [0, 2^30), donc
    // le cycle contenant `valeur` (< TAILLE_DOMAINE) repasse forcément par une
    // valeur du domaine ; en moyenne 1,2 itération (2^30 / 900 M).
    while courant >= TAILLE_DOMAINE {
        courant = feistel(cle, courant);
    }
    courant
}

/// Réseau de Feistel équilibré sur 30 bits (moitiés de 15 bits, [`RONDES`]
/// rondes) : bijection de `[0, 2^30)` paramétrée par `cle`.
fn feistel(cle: &[u8; 32], valeur: u64) -> u64 {
    let mut gauche = (valeur >> BITS_MOITIE) & MASQUE_MOITIE;
    let mut droite = valeur & MASQUE_MOITIE;
    for ronde in 0..RONDES {
        let (nouvelle_gauche, nouvelle_droite) = (droite, gauche ^ ronde_prf(cle, ronde, droite));
        gauche = nouvelle_gauche;
        droite = nouvelle_droite;
    }
    (gauche << BITS_MOITIE) | droite
}

/// Fonction de ronde : PRF SHA-256(clé ‖ ronde ‖ moitié), repliée sur 15 bits.
fn ronde_prf(cle: &[u8; 32], ronde: u8, moitie: u64) -> u64 {
    let mut hacheur = Sha256::new();
    hacheur.update(cle);
    hacheur.update([ronde]);
    hacheur.update(moitie.to_be_bytes());
    let empreinte = hacheur.finalize();
    u64::from_be_bytes(empreinte[..8].try_into().expect("SHA-256 fait 32 octets")) & MASQUE_MOITIE
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Allocateur déterministe pour les tests (clé de permutation fixe).
    fn allocateur_test() -> AllocateurId {
        AllocateurId::from_snapshot(0, &hex::encode([42u8; 32]), Vec::new()).expect("allocateur")
    }

    #[test]
    fn feistel_est_une_bijection_inversible_par_cycle() {
        // Injectivité sur un échantillon : pas deux entrées vers la même sortie.
        let cle = [3u8; 32];
        let mut vues = std::collections::HashSet::new();
        for valeur in 0..2_000u64 {
            assert!(vues.insert(feistel(&cle, valeur)), "collision Feistel");
        }
    }

    #[test]
    fn ids_uniques_a_neuf_chiffres_et_non_sequentiels() {
        let alloc = allocateur_test();
        let cle_client = [1u8; 32];
        let mut ids = Vec::new();
        for i in 0..1_000u32 {
            let id = alloc
                .allouer(&format!("compte-{i}"), &cle_client)
                .expect("allocation");
            assert!((ID_MIN..ID_MIN + TAILLE_DOMAINE).contains(&id), "{id}");
            ids.push(id);
        }
        // Unicité stricte.
        let ensemble: std::collections::HashSet<u64> = ids.iter().copied().collect();
        assert_eq!(ensemble.len(), ids.len(), "ID réémis");
        // Non-énumérabilité (garde-fou) : la suite émise n'est pas la suite
        // des entiers consécutifs — un voisin d'ID connu n'apprend rien.
        let consecutifs = ids.windows(2).filter(|p| p[1] == p[0] + 1).count();
        assert!(consecutifs < 5, "suite quasi séquentielle : {consecutifs}");
    }

    #[test]
    fn ids_lies_au_compte_demandeur() {
        let alloc = allocateur_test();
        let id_alice = alloc.allouer("alice", &[1u8; 32]).expect("alice");
        let id_bob = alloc.allouer("bob", &[2u8; 32]).expect("bob");

        assert_eq!(alloc.proprietaire(id_alice), Some("alice".to_string()));
        assert!(alloc.est_proprietaire(id_alice, "alice"));
        assert!(!alloc.est_proprietaire(id_alice, "bob"));
        assert!(alloc.est_proprietaire(id_bob, "bob"));
        // ID jamais émis : aucun propriétaire.
        assert_eq!(alloc.proprietaire(1), None);
        assert!(!alloc.est_proprietaire(1, "alice"));
        // Compte vide : refusé.
        assert_eq!(
            alloc.allouer("  ", &[0u8; 32]),
            Err(ErreurAllocation::CompteVide)
        );
    }

    #[test]
    fn aucune_reattribution_apres_rechargement() {
        let alloc = allocateur_test();
        let premiers: Vec<u64> = (0..50)
            .map(|_| alloc.allouer("alice", &[1u8; 32]).expect("allocation"))
            .collect();

        // Rechargement depuis l'instantané : le compteur et le registre suivent.
        let (compteur, cle_hex, emis) = alloc.snapshot();
        assert_eq!(emis.len(), 50);
        let rejoue = AllocateurId::from_snapshot(compteur, &cle_hex, emis).expect("rechargement");
        assert_eq!(rejoue.nombre_emis(), 50);
        for _ in 0..50 {
            let nouveau = rejoue.allouer("bob", &[2u8; 32]).expect("allocation");
            assert!(!premiers.contains(&nouveau), "ID réattribué : {nouveau}");
        }
        // Les propriétaires d'origine sont conservés.
        assert!(rejoue.est_proprietaire(premiers[0], "alice"));
    }

    #[test]
    fn changement_de_cle_de_permutation_sans_collision() {
        // Simule une clé perdue/changée : le registre des émis fait barrage.
        let alloc = allocateur_test();
        let premiers: Vec<u64> = (0..20)
            .map(|_| alloc.allouer("alice", &[1u8; 32]).expect("allocation"))
            .collect();
        let (_, _, emis) = alloc.snapshot();

        // Compteur REMIS À ZÉRO avec une autre clé : les rangs repassent, mais
        // aucun ID déjà émis ne ressort.
        let autre_cle = AllocateurId::from_snapshot(0, &hex::encode([9u8; 32]), emis)
            .expect("autre clé de permutation");
        for _ in 0..20 {
            let nouveau = autre_cle.allouer("bob", &[2u8; 32]).expect("allocation");
            assert!(!premiers.contains(&nouveau), "collision : {nouveau}");
        }
        // Clé illisible dans l'état persisté : régénérée sans casser le reste.
        let (compteur, _, emis) = autre_cle.snapshot();
        let regenere =
            AllocateurId::from_snapshot(compteur, "pas-une-cle", emis).expect("clé régénérée");
        assert_eq!(regenere.nombre_emis(), 40);
    }
}
