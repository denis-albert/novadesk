//! Récupération de fichier distant à la demande (plan 09) : la requête/réponse
//! qui complète le [`listing`](crate::listing) — une fois un fichier repéré
//! dans le navigateur, le contrôleur en télécharge le contenu **par tranches**.
//!
//! Le module est autonome et indépendant du réseau, dans le même style que le
//! listing : côté contrôleur, l'UI construit une [`RequeteFichier`] et la
//! sérialise ([`RequeteFichier::to_bytes`]) ; côté hôte, [`lire_tranche`] (ou
//! [`traiter_requete_fichier`] directement sur les octets du canal) produit la
//! [`ReponseFichier`] à renvoyer. L'acheminement sur un canal de session sera
//! branché par `nd-core` (plan 16, permission côté hôte ajoutée là-bas).
//!
//! # Lecture par tranches
//!
//! Un fichier volumineux se lit en plusieurs allers-retours : le contrôleur
//! demande `[offset, offset + taille_max)`, l'hôte renvoie les octets réellement
//! lus et le drapeau [`ReponseFichier::fin`] (vrai quand la tranche atteint la
//! fin du fichier). Le contrôleur avance `offset` de `donnees.len()` jusqu'à
//! `fin`. Chaque tranche est **bornée** à [`TAILLE_TRANCHE_MAX`] côté source
//! (quelle que soit la valeur demandée), pour qu'une requête ne fasse jamais
//! exploser la mémoire de l'hôte.
//!
//! # Format binaire (compact, little-endian)
//!
//! Un tampon = exactement **un** message — le canal apporte son propre cadrage,
//! comme pour [`RequeteListe`](crate::listing::RequeteListe) ; tout octet
//! excédentaire est refusé. Les tags de la requête et de la réponse sont
//! distincts pour lever toute ambiguïté de sens sur un canal partagé.
//!
//! ```text
//! requête (tag 1) : [tag : u8][long. chemin : u32 LE][chemin UTF-8]
//!                   [offset : u64 LE][taille_max : u32 LE]
//! réponse (tag 2) : [tag : u8][long. chemin : u32 LE][chemin UTF-8][offset : u64 LE]
//!                   [drapeau erreur : u8]{si 1 : [long. u32 LE][message UTF-8]}
//!                   [fin : u8][long. données : u32 LE][données]
//! ```
//!
//! Le décodage est robuste aux entrées malformées : troncature, tag inconnu,
//! drapeau hors `{0, 1}`, texte non UTF-8 ou octets excédentaires produisent une
//! [`NdError::Protocol`], jamais une panique ; une longueur de données annoncée
//! délirante ne lit jamais au-delà de ce que le tampon contient réellement.
//!
//! # Sécurité
//!
//! Le chemin de la requête est utilisé **tel quel** côté hôte, sans
//! confinement : ce module ne fait que de la **lecture seule** (`File::open`
//! puis lecture bornée) — aucune écriture, création ni suppression. Le contrôle
//! d'accès réel (consentement, permission de session type
//! `FileDownload`, confinement éventuel façon
//! [`LocalFs::jailed`](crate::LocalFs)) relève du routage `nd-core` (plan 16).

use std::fs::File;
use std::io::{ErrorKind, Read, Seek, SeekFrom};

use nd_proto::{NdError, Result};

/// Tag binaire de la requête de fichier (0 est évité pour détecter les tampons
/// nuls, comme dans [`crate::listing`] et [`crate::transfer`]).
const TAG_REQUETE: u8 = 1;
/// Tag binaire de la réponse de fichier.
const TAG_REPONSE: u8 = 2;

/// Borne supérieure d'une tranche lue en une fois (1 MiB) : la source ne lit
/// jamais plus, quelle que soit la `taille_max` demandée, pour éviter toute
/// explosion mémoire. Le contrôleur récupère simplement le fichier en davantage
/// d'allers-retours si sa demande dépasse cette borne.
pub const TAILLE_TRANCHE_MAX: u32 = 1024 * 1024;

/// Requête de tranche envoyée par le contrôleur : le chemin du fichier à lire
/// côté hôte, l'`offset` de départ et le nombre maximal d'octets voulus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequeteFichier {
    /// Chemin du fichier à lire, tel que l'hôte le comprend.
    pub chemin: String,
    /// Offset (octets depuis le début du fichier) du premier octet demandé.
    pub offset: u64,
    /// Nombre maximal d'octets voulus dans cette tranche (borné à
    /// [`TAILLE_TRANCHE_MAX`] côté hôte).
    pub taille_max: u32,
}

/// Réponse de tranche renvoyée par l'hôte : soit les octets lus (avec le drapeau
/// de fin), soit une erreur lisible — jamais un contenu utile *et* une erreur.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReponseFichier {
    /// Chemin demandé, renvoyé tel quel (corrélation côté contrôleur).
    pub chemin: String,
    /// Offset de départ de la tranche (celui de la requête).
    pub offset: u64,
    /// Octets lus à partir de `offset` (vide en cas d'erreur ou de fin exacte).
    pub donnees: Vec<u8>,
    /// `true` si cette tranche atteint la fin du fichier (plus rien à demander).
    pub fin: bool,
    /// Erreur lisible (fichier inexistant, accès refusé, offset au-delà de la
    /// fin…) quand la lecture a échoué ; `None` en cas de succès.
    pub erreur: Option<String>,
}

impl RequeteFichier {
    /// Sérialise la requête en octets autonomes (voir le format en tête de
    /// module), prêts pour le canal de session.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(1 + 4 + self.chemin.len() + 8 + 4);
        out.push(TAG_REQUETE);
        ecrire_chaine(&mut out, &self.chemin);
        out.extend_from_slice(&self.offset.to_le_bytes());
        out.extend_from_slice(&self.taille_max.to_le_bytes());
        out
    }

    /// Désérialise une requête depuis `buf`, qui doit contenir exactement un
    /// message (aucun octet excédentaire). [`NdError::Protocol`] sinon.
    pub fn from_bytes(buf: &[u8]) -> Result<Self> {
        let mut reste = exiger_tag(buf, TAG_REQUETE, "requête de fichier")?;
        let chemin = lire_chaine(&mut reste)?;
        let offset = lire_u64(&mut reste)?;
        let taille_max = lire_u32(&mut reste)?;
        exiger_vide(reste)?;
        Ok(Self {
            chemin,
            offset,
            taille_max,
        })
    }
}

impl ReponseFichier {
    /// Réponse d'échec : aucune donnée, `erreur` renseignée (aide partagée entre
    /// le handler et le traitement des requêtes malformées).
    fn en_erreur(chemin: &str, offset: u64, message: String) -> Self {
        Self {
            chemin: chemin.to_string(),
            offset,
            donnees: Vec::new(),
            fin: false,
            erreur: Some(message),
        }
    }

    /// Sérialise la réponse en octets autonomes (voir le format en tête de
    /// module), prêts pour le canal de session.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out =
            Vec::with_capacity(1 + 4 + self.chemin.len() + 8 + 2 + 4 + self.donnees.len());
        out.push(TAG_REPONSE);
        ecrire_chaine(&mut out, &self.chemin);
        out.extend_from_slice(&self.offset.to_le_bytes());
        match &self.erreur {
            Some(message) => {
                out.push(1);
                ecrire_chaine(&mut out, message);
            }
            None => out.push(0),
        }
        out.push(u8::from(self.fin));
        out.extend_from_slice(&(self.donnees.len() as u32).to_le_bytes());
        out.extend_from_slice(&self.donnees);
        out
    }

    /// Désérialise une réponse depuis `buf`, qui doit contenir exactement un
    /// message (aucun octet excédentaire). [`NdError::Protocol`] sinon.
    pub fn from_bytes(buf: &[u8]) -> Result<Self> {
        let mut reste = exiger_tag(buf, TAG_REPONSE, "réponse de fichier")?;
        let chemin = lire_chaine(&mut reste)?;
        let offset = lire_u64(&mut reste)?;
        let erreur = if lire_drapeau(&mut reste)? {
            Some(lire_chaine(&mut reste)?)
        } else {
            None
        };
        let fin = lire_drapeau(&mut reste)?;
        let long = lire_u32(&mut reste)? as usize;
        // `lire_octets` borne `long` à ce que le tampon contient réellement :
        // une longueur annoncée délirante échoue en troncature, sans allouer.
        let donnees = lire_octets(&mut reste, long)?.to_vec();
        exiger_vide(reste)?;
        Ok(Self {
            chemin,
            offset,
            donnees,
            fin,
            erreur,
        })
    }
}

// ---------------------------------------------------------------------------
// Handler côté hôte
// ---------------------------------------------------------------------------

/// Handler hôte de récupération : lit la tranche `[offset, offset + taille_max)`
/// du fichier `chemin` via `std::fs` et produit la réponse à renvoyer.
///
/// * Lecture **seule** ; le chemin est utilisé tel quel (voir la doc du module —
///   le contrôle d'accès relève de `nd-core`).
/// * La taille effectivement lue est bornée à [`TAILLE_TRANCHE_MAX`], puis à ce
///   qui reste réellement dans le fichier : jamais d'allocation démesurée.
/// * [`ReponseFichier::fin`] vaut `true` dès que la tranche atteint la fin du
///   fichier (le contrôleur sait alors qu'il n'y a plus rien à demander). Un
///   `offset` égal à la taille (fichier vide inclus) renvoie une tranche vide
///   avec `fin = true`.
/// * Toute erreur (fichier inexistant, chemin non-fichier, accès refusé, offset
///   au-delà de la fin, `taille_max` nulle…) est renvoyée dans
///   [`ReponseFichier::erreur`] : cette fonction **ne panique jamais** et
///   renvoie toujours une réponse.
pub fn lire_tranche(chemin: &str, offset: u64, taille_max: u32) -> ReponseFichier {
    match tranche_ou_erreur(chemin, offset, taille_max) {
        Ok((donnees, fin)) => ReponseFichier {
            chemin: chemin.to_string(),
            offset,
            donnees,
            fin,
            erreur: None,
        },
        Err(message) => ReponseFichier::en_erreur(chemin, offset, message),
    }
}

/// Cœur de [`lire_tranche`] : renvoie `(donnees, fin)` ou un message d'erreur
/// lisible (jamais de panique — toute erreur d'E/S est transformée en message).
fn tranche_ou_erreur(
    chemin: &str,
    offset: u64,
    taille_max: u32,
) -> std::result::Result<(Vec<u8>, bool), String> {
    let mut fichier = File::open(chemin).map_err(|e| message_erreur(&e))?;
    let meta = fichier.metadata().map_err(|e| message_erreur(&e))?;
    if !meta.is_file() {
        return Err("le chemin n'est pas un fichier".to_string());
    }
    let taille = meta.len();
    if offset > taille {
        return Err(format!(
            "offset {offset} au-delà de la fin du fichier ({taille} octets)"
        ));
    }
    // Fin exacte (fichier vide compris) : tranche vide terminale, valable quelle
    // que soit `taille_max` — le contrôleur voit `fin` et s'arrête.
    if offset == taille {
        return Ok((Vec::new(), true));
    }
    if taille_max == 0 {
        return Err("taille de tranche demandée nulle".to_string());
    }
    let plafond = taille_max.min(TAILLE_TRANCHE_MAX);
    let a_lire = (taille - offset).min(u64::from(plafond)) as usize;
    fichier
        .seek(SeekFrom::Start(offset))
        .map_err(|e| message_erreur(&e))?;
    let mut donnees = vec![0u8; a_lire];
    fichier
        .read_exact(&mut donnees)
        .map_err(|e| message_erreur(&e))?;
    let fin = offset + a_lire as u64 >= taille;
    Ok((donnees, fin))
}

/// Traite côté hôte une requête de fichier reçue **en octets** et renvoie la
/// réponse encodée, prête à repartir sur le canal. Ne panique jamais et répond
/// toujours : une requête malformée produit une réponse dont `erreur` est
/// renseignée (chemin vide, faute de chemin décodable).
pub fn traiter_requete_fichier(octets: &[u8]) -> Vec<u8> {
    let reponse = match RequeteFichier::from_bytes(octets) {
        Ok(requete) => lire_tranche(&requete.chemin, requete.offset, requete.taille_max),
        Err(e) => ReponseFichier::en_erreur("", 0, format!("requête de fichier invalide : {e}")),
    };
    reponse.to_bytes()
}

/// Message d'erreur en français pour la réponse : cause stable pour les cas
/// courants, détail système conservé entre parenthèses pour le diagnostic
/// (même convention que [`crate::listing`]).
fn message_erreur(e: &std::io::Error) -> String {
    let cause = match e.kind() {
        ErrorKind::NotFound => "fichier inexistant",
        ErrorKind::PermissionDenied => "accès refusé",
        _ => "erreur d'entrée/sortie",
    };
    format!("{cause} ({e})")
}

// ---------------------------------------------------------------------------
// Aides d'encodage/décodage (style commun aux modules du crate)
// ---------------------------------------------------------------------------

/// Ajoute une chaîne UTF-8 préfixée de sa longueur (`u32` LE).
fn ecrire_chaine(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.len() as u32).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
}

/// Vérifie le tag en tête de `buf` et renvoie la charge utile qui le suit.
fn exiger_tag<'a>(buf: &'a [u8], attendu: u8, quoi: &str) -> Result<&'a [u8]> {
    let (&tag, reste) = buf
        .split_first()
        .ok_or_else(|| NdError::Protocol(format!("{quoi} vide (tag manquant)")))?;
    if tag != attendu {
        return Err(NdError::Protocol(format!(
            "tag de {quoi} inattendu : {tag} (attendu : {attendu})"
        )));
    }
    Ok(reste)
}

/// Exige que `reste` soit vide (aucun octet excédentaire).
fn exiger_vide(reste: &[u8]) -> Result<()> {
    if reste.is_empty() {
        Ok(())
    } else {
        Err(NdError::Protocol(
            "octets excédentaires après le message de fichier".into(),
        ))
    }
}

/// Prélève `n` octets en tête de `charge` (avance le curseur).
fn lire_octets<'a>(charge: &mut &'a [u8], n: usize) -> Result<&'a [u8]> {
    if charge.len() < n {
        return Err(NdError::Protocol(format!(
            "message de fichier tronqué : {n} octets attendus, {} restants",
            charge.len()
        )));
    }
    let (tete, reste) = charge.split_at(n);
    *charge = reste;
    Ok(tete)
}

/// Lit un `u32` little-endian en tête de `charge`.
fn lire_u32(charge: &mut &[u8]) -> Result<u32> {
    let o = lire_octets(charge, 4)?;
    Ok(u32::from_le_bytes([o[0], o[1], o[2], o[3]]))
}

/// Lit un `u64` little-endian en tête de `charge`.
fn lire_u64(charge: &mut &[u8]) -> Result<u64> {
    let o = lire_octets(charge, 8)?;
    Ok(u64::from_le_bytes([
        o[0], o[1], o[2], o[3], o[4], o[5], o[6], o[7],
    ]))
}

/// Lit une chaîne UTF-8 préfixée de sa longueur (`u32` LE).
fn lire_chaine(charge: &mut &[u8]) -> Result<String> {
    let n = lire_u32(charge)? as usize;
    String::from_utf8(lire_octets(charge, n)?.to_vec())
        .map_err(|_| NdError::Protocol("texte de fichier non UTF-8".into()))
}

/// Lit un drapeau binaire strict — `0` ou `1` uniquement, pour garder le format
/// symétrique ; toute autre valeur est un octet corrompu.
fn lire_drapeau(charge: &mut &[u8]) -> Result<bool> {
    match lire_octets(charge, 1)?[0] {
        0 => Ok(false),
        1 => Ok(true),
        v => Err(NdError::Protocol(format!(
            "drapeau de fichier invalide : {v}"
        ))),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Chemin temporaire unique pour un test (évite les collisions entre tests
    /// parallèles et entre exécutions).
    fn chemin_temp(nom: &str) -> PathBuf {
        std::env::temp_dir().join(format!("nd_files_download_{}_{nom}", std::process::id()))
    }

    /// Motif déterministe non trivial (chaque offset produit un octet distinct
    /// de ses voisins, sans période courte évidente).
    fn motif(n: usize) -> Vec<u8> {
        (0..n)
            .map(|i| (i.wrapping_mul(31).wrapping_add(i >> 8) & 0xff) as u8)
            .collect()
    }

    #[test]
    fn round_trip_requete() {
        for (chemin, offset, taille_max) in [
            ("", 0u64, 0u32),
            ("C:\\Users\\Café\\gros — fichier.bin", 1_234_567_890, 65_536),
            ("/home/été/data.bin", u64::MAX, u32::MAX),
        ] {
            let requete = RequeteFichier {
                chemin: chemin.to_string(),
                offset,
                taille_max,
            };
            let octets = requete.to_bytes();
            assert_eq!(RequeteFichier::from_bytes(&octets).unwrap(), requete);
        }
        // Un octet excédentaire après un message complet est refusé.
        let mut trop = RequeteFichier {
            chemin: "C:\\x".to_string(),
            offset: 0,
            taille_max: 1,
        }
        .to_bytes();
        trop.push(0);
        assert!(RequeteFichier::from_bytes(&trop).is_err());
        // Une réponse n'est pas décodable comme requête (mêmes tags mais champs
        // différents → soit tag ok mais structure incompatible, soit troncature).
        let reponse = lire_tranche("", 0, 0).to_bytes();
        assert!(RequeteFichier::from_bytes(&reponse).is_err());
    }

    #[test]
    fn round_trip_reponse() {
        let reponses = [
            // Succès, tranche non finale avec données.
            ReponseFichier {
                chemin: "C:\\données\\a.bin".to_string(),
                offset: 0,
                donnees: motif(4096),
                fin: false,
                erreur: None,
            },
            // Succès, tranche finale (dernier bloc du fichier).
            ReponseFichier {
                chemin: "D:\\Été\\énorme.bin".to_string(),
                offset: u64::MAX - 10,
                donnees: motif(10),
                fin: true,
                erreur: None,
            },
            // Fin exacte : tranche vide terminale.
            ReponseFichier {
                chemin: "vide.bin".to_string(),
                offset: 0,
                donnees: Vec::new(),
                fin: true,
                erreur: None,
            },
            // Échec : erreur renseignée, aucune donnée.
            ReponseFichier {
                chemin: "E:\\perdu.bin".to_string(),
                offset: 42,
                donnees: Vec::new(),
                fin: false,
                erreur: Some("fichier inexistant (détail système)".to_string()),
            },
        ];
        for reponse in &reponses {
            let octets = reponse.to_bytes();
            assert_eq!(&ReponseFichier::from_bytes(&octets).unwrap(), reponse);
        }
        // Un octet excédentaire après un message complet est refusé.
        let mut trop = reponses[0].to_bytes();
        trop.push(0);
        assert!(ReponseFichier::from_bytes(&trop).is_err());
    }

    #[test]
    fn decodage_malforme_rejete_sans_panique() {
        // Tampons vides et tags inconnus.
        assert!(RequeteFichier::from_bytes(&[]).is_err());
        assert!(ReponseFichier::from_bytes(&[]).is_err());
        assert!(RequeteFichier::from_bytes(&[99]).is_err());
        assert!(ReponseFichier::from_bytes(&[99]).is_err());
        // Chaîne tronquée : 5 octets annoncés, 1 fourni.
        assert!(RequeteFichier::from_bytes(&[TAG_REQUETE, 5, 0, 0, 0, b'a']).is_err());
        // Chemin non UTF-8.
        assert!(RequeteFichier::from_bytes(&[TAG_REQUETE, 2, 0, 0, 0, 0xFF, 0xFF]).is_err());
        // Requête tronquée juste avant l'offset (chemin vide mais pas d'offset).
        assert!(RequeteFichier::from_bytes(&[TAG_REQUETE, 0, 0, 0, 0]).is_err());
        // Drapeau d'erreur hors {0, 1} dans une réponse.
        let mut drapeau = ReponseFichier {
            chemin: String::new(),
            offset: 0,
            donnees: Vec::new(),
            fin: false,
            erreur: None,
        }
        .to_bytes();
        // Après [tag][long. chemin = 0 (4 o)][offset = 0 (8 o)] vient le drapeau.
        drapeau[1 + 4 + 8] = 7;
        assert!(ReponseFichier::from_bytes(&drapeau).is_err());
        // Longueur de données délirante (u32::MAX annoncé, rien derrière) :
        // erreur de troncature immédiate, sans allocation démesurée.
        let mut delirant = vec![TAG_REPONSE];
        delirant.extend_from_slice(&0u32.to_le_bytes()); // chemin vide
        delirant.extend_from_slice(&0u64.to_le_bytes()); // offset
        delirant.push(0); // pas d'erreur
        delirant.push(0); // fin = false
        delirant.extend_from_slice(&u32::MAX.to_le_bytes()); // long. données
        assert!(ReponseFichier::from_bytes(&delirant).is_err());
        // Réponse amputée de son dernier octet de données : troncature détectée.
        let octets = ReponseFichier {
            chemin: "C:\\x".to_string(),
            offset: 7,
            donnees: motif(16),
            fin: false,
            erreur: None,
        }
        .to_bytes();
        assert!(ReponseFichier::from_bytes(&octets[..octets.len() - 1]).is_err());
    }

    #[test]
    fn lire_tranche_contenu_et_fin() {
        let contenu = motif(5000);
        let path = chemin_temp("tranche.bin");
        std::fs::write(&path, &contenu).unwrap();
        let s = path.to_str().unwrap();

        // Première tranche partielle : contenu exact, pas encore la fin.
        let r = lire_tranche(s, 0, 2000);
        assert!(r.erreur.is_none());
        assert_eq!(r.offset, 0);
        assert_eq!(r.donnees, &contenu[..2000]);
        assert!(!r.fin);

        // Tranche du milieu.
        let r = lire_tranche(s, 2000, 2000);
        assert_eq!(r.donnees, &contenu[2000..4000]);
        assert!(!r.fin);

        // Dernière tranche : moins d'octets que demandé, drapeau de fin levé.
        let r = lire_tranche(s, 4000, 2000);
        assert_eq!(r.donnees, &contenu[4000..]);
        assert_eq!(r.donnees.len(), 1000);
        assert!(r.fin);

        // Offset exactement à la fin : tranche vide terminale.
        let r = lire_tranche(s, contenu.len() as u64, 2000);
        assert!(r.erreur.is_none());
        assert!(r.donnees.is_empty());
        assert!(r.fin);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn lire_tranche_erreurs() {
        // Fichier inexistant → erreur, jamais de panique.
        let absent = chemin_temp("inexistant.bin");
        let _ = std::fs::remove_file(&absent);
        let r = lire_tranche(absent.to_str().unwrap(), 0, 1024);
        assert!(r.donnees.is_empty());
        assert!(r.erreur.unwrap().contains("fichier inexistant"));

        // Offset au-delà de la fin → erreur.
        let path = chemin_temp("court.bin");
        std::fs::write(&path, b"novadesk").unwrap(); // 8 octets
        let r = lire_tranche(path.to_str().unwrap(), 100, 1024);
        assert!(r.donnees.is_empty());
        assert!(r.erreur.unwrap().contains("au-delà de la fin"));

        // taille_max nulle sur un fichier non vide → erreur (pas de progrès).
        let r = lire_tranche(path.to_str().unwrap(), 0, 0);
        assert!(r.erreur.is_some());

        // Un répertoire n'est pas un fichier → erreur.
        let dir = chemin_temp("un_dossier");
        std::fs::create_dir_all(&dir).unwrap();
        let r = lire_tranche(dir.to_str().unwrap(), 0, 1024);
        assert!(r.erreur.is_some());

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fichier_vide_une_tranche_finale() {
        let path = chemin_temp("vide.bin");
        std::fs::write(&path, b"").unwrap();
        let r = lire_tranche(path.to_str().unwrap(), 0, 4096);
        assert!(r.erreur.is_none());
        assert!(r.donnees.is_empty());
        assert!(r.fin);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn taille_bornee_a_la_limite() {
        // Fichier plus grand que la borne : une demande démesurée est ramenée à
        // TAILLE_TRANCHE_MAX, sans allouer davantage, et n'est pas finale.
        let contenu = motif(TAILLE_TRANCHE_MAX as usize + 4096);
        let path = chemin_temp("gros.bin");
        std::fs::write(&path, &contenu).unwrap();

        let r = lire_tranche(path.to_str().unwrap(), 0, u32::MAX);
        assert!(r.erreur.is_none());
        assert_eq!(r.donnees.len(), TAILLE_TRANCHE_MAX as usize);
        assert_eq!(r.donnees, &contenu[..TAILLE_TRANCHE_MAX as usize]);
        assert!(!r.fin);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn reconstruction_multi_tranches() {
        // Reconstitue un fichier entier via des tranches successives, comme le
        // ferait le contrôleur (boucle jusqu'à `fin`), à travers l'encodage.
        let contenu = motif(3 * 4096 + 517);
        let path = chemin_temp("reconstruction.bin");
        std::fs::write(&path, &contenu).unwrap();
        let s = path.to_string_lossy().into_owned();

        let mut reconstitue = Vec::new();
        let mut offset = 0u64;
        loop {
            let requete = RequeteFichier {
                chemin: s.clone(),
                offset,
                taille_max: 4096,
            };
            // Passe par le handler « octets » de bout en bout (comme sur le fil).
            let octets = traiter_requete_fichier(&requete.to_bytes());
            let reponse = ReponseFichier::from_bytes(&octets).unwrap();
            assert!(reponse.erreur.is_none());
            assert_eq!(reponse.offset, offset);
            reconstitue.extend_from_slice(&reponse.donnees);
            offset += reponse.donnees.len() as u64;
            if reponse.fin {
                break;
            }
        }
        assert_eq!(reconstitue, contenu);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn traiter_requete_fichier_malformee() {
        // Une requête illisible produit toujours une réponse décodable, en erreur.
        let reponse = ReponseFichier::from_bytes(&traiter_requete_fichier(&[0xFF, 1, 2])).unwrap();
        assert!(reponse.donnees.is_empty());
        assert!(reponse.erreur.is_some());
    }
}
