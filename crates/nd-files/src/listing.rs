//! Listing de répertoire distant (plan 09) : la requête/réponse qui alimente
//! le navigateur de fichiers distant de l'UI (aujourd'hui une maquette).
//!
//! Le module est autonome et indépendant du réseau : côté contrôleur, l'UI
//! construit une [`RequeteListe`] et la sérialise ([`RequeteListe::to_bytes`]) ;
//! côté hôte, [`lister_repertoire`] (ou [`traiter_requete_liste`] directement
//! sur les octets du canal) produit la [`ReponseListe`] à renvoyer.
//! L'acheminement sur un canal de session sera branché par `nd-core` (plan 16).
//!
//! # Format binaire (compact, little-endian)
//!
//! Un tampon = exactement **un** message — le canal apporte son propre cadrage,
//! comme pour [`ClipboardContent`](crate::ClipboardContent) ; tout octet
//! excédentaire est refusé. Les tags de la requête et de la réponse sont
//! distincts pour lever toute ambiguïté de sens sur un canal partagé.
//!
//! ```text
//! requête (tag 1) : [tag : u8][long. chemin : u32 LE][chemin UTF-8]
//! réponse (tag 2) : [tag : u8][long. chemin : u32 LE][chemin UTF-8]
//!                   [drapeau erreur : u8]{si 1 : [long. u32 LE][message UTF-8]}
//!                   [nombre d'entrées : u32 LE]{ entrée… }
//! entrée          : [long. nom : u32 LE][nom UTF-8][taille : u64 LE]
//!                   [est_dossier : u8][drapeau mtime : u8]{si 1 : [mtime : u64 LE]}
//! ```
//!
//! Le décodage est robuste aux entrées malformées : troncature, tag inconnu,
//! drapeau hors `{0, 1}`, texte non UTF-8 ou octets excédentaires produisent
//! une [`NdError::Protocol`], jamais une panique ; un nombre d'entrées annoncé
//! délirant ne pré-alloue jamais plus que ce que le tampon peut contenir.
//!
//! # Sécurité
//!
//! Le chemin de la requête est utilisé **tel quel** côté hôte, sans
//! confinement : ce module ne fait que du **listing en lecture seule**
//! (`read_dir`/`metadata`) — aucune écriture, création ni suppression n'est
//! effectuée ici. Le contrôle d'accès réel (consentement, permissions de
//! session, confinement éventuel façon [`LocalFs::jailed`](crate::LocalFs))
//! relève du routage qui sera ajouté par `nd-core`.

use std::io::ErrorKind;

use nd_proto::{NdError, Result};

use crate::epoch_modification;

/// Tag binaire de la requête de listing (0 est évité pour détecter les
/// tampons nuls, comme dans [`crate::transfer`]).
const TAG_REQUETE: u8 = 1;
/// Tag binaire de la réponse de listing.
const TAG_REPONSE: u8 = 2;

/// Taille minimale d'une entrée encodée : nom vide (4 octets de longueur),
/// taille (8), `est_dossier` (1), drapeau mtime (1). Sert à borner la
/// pré-allocation au décodage.
const ENTREE_MIN_OCTETS: usize = 4 + 8 + 1 + 1;

/// Entrée d'un listing de répertoire distant (un fichier ou un dossier).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntreeFs {
    /// Nom de l'entrée (dernier composant, sans le chemin parent).
    pub nom: String,
    /// Taille en octets (0 pour les dossiers et les racines).
    pub taille: u64,
    /// `true` pour un dossier (navigable), `false` pour un fichier.
    pub est_dossier: bool,
    /// Horodatage de modification (secondes epoch), si disponible.
    pub modifie_le: Option<u64>,
}

/// Requête de listing envoyée par le contrôleur : le chemin du répertoire à
/// lister côté hôte. Chemin **vide** = demande des racines (lettres de lecteur
/// Windows, `/` ailleurs) pour amorcer le navigateur.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequeteListe {
    /// Chemin du répertoire à lister, tel que l'hôte le comprend.
    pub chemin: String,
}

/// Réponse de listing renvoyée par l'hôte : soit les entrées du répertoire,
/// soit une erreur lisible — jamais les deux à la fois.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReponseListe {
    /// Chemin demandé, renvoyé tel quel (corrélation côté contrôleur).
    pub chemin: String,
    /// Entrées du répertoire : dossiers d'abord, puis fichiers, chaque groupe
    /// trié par nom. Vide quand `erreur` est renseignée.
    pub entrees: Vec<EntreeFs>,
    /// Erreur lisible (dossier inexistant, accès refusé…) quand le listing a
    /// échoué ; `None` en cas de succès.
    pub erreur: Option<String>,
}

impl RequeteListe {
    /// Sérialise la requête en octets autonomes (voir le format en tête de
    /// module), prêts pour le canal de session.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(1 + 4 + self.chemin.len());
        out.push(TAG_REQUETE);
        ecrire_chaine(&mut out, &self.chemin);
        out
    }

    /// Désérialise une requête depuis `buf`, qui doit contenir exactement un
    /// message (aucun octet excédentaire). [`NdError::Protocol`] sinon.
    pub fn from_bytes(buf: &[u8]) -> Result<Self> {
        let mut reste = exiger_tag(buf, TAG_REQUETE, "requête de listing")?;
        let chemin = lire_chaine(&mut reste)?;
        exiger_vide(reste)?;
        Ok(Self { chemin })
    }
}

impl ReponseListe {
    /// Réponse d'échec : aucune entrée, `erreur` renseignée (aide partagée
    /// entre le handler et le traitement des requêtes malformées).
    fn en_erreur(chemin: &str, message: String) -> Self {
        Self {
            chemin: chemin.to_string(),
            entrees: Vec::new(),
            erreur: Some(message),
        }
    }

    /// Sérialise la réponse en octets autonomes (voir le format en tête de
    /// module), prêts pour le canal de session.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.push(TAG_REPONSE);
        ecrire_chaine(&mut out, &self.chemin);
        match &self.erreur {
            Some(message) => {
                out.push(1);
                ecrire_chaine(&mut out, message);
            }
            None => out.push(0),
        }
        out.extend_from_slice(&(self.entrees.len() as u32).to_le_bytes());
        for entree in &self.entrees {
            ecrire_chaine(&mut out, &entree.nom);
            out.extend_from_slice(&entree.taille.to_le_bytes());
            out.push(u8::from(entree.est_dossier));
            match entree.modifie_le {
                Some(mtime) => {
                    out.push(1);
                    out.extend_from_slice(&mtime.to_le_bytes());
                }
                None => out.push(0),
            }
        }
        out
    }

    /// Désérialise une réponse depuis `buf`, qui doit contenir exactement un
    /// message (aucun octet excédentaire). [`NdError::Protocol`] sinon.
    pub fn from_bytes(buf: &[u8]) -> Result<Self> {
        let mut reste = exiger_tag(buf, TAG_REPONSE, "réponse de listing")?;
        let chemin = lire_chaine(&mut reste)?;
        let erreur = if lire_drapeau(&mut reste)? {
            Some(lire_chaine(&mut reste)?)
        } else {
            None
        };
        let nombre = lire_u32(&mut reste)? as usize;
        // Pré-allocation bornée par ce que le tampon peut réellement contenir
        // (au moins ENTREE_MIN_OCTETS par entrée) : un nombre annoncé délirant
        // échoue en troncature sans jamais allouer des gigaoctets.
        let mut entrees = Vec::with_capacity(nombre.min(reste.len() / ENTREE_MIN_OCTETS));
        for _ in 0..nombre {
            let nom = lire_chaine(&mut reste)?;
            let taille = lire_u64(&mut reste)?;
            let est_dossier = lire_drapeau(&mut reste)?;
            let modifie_le = if lire_drapeau(&mut reste)? {
                Some(lire_u64(&mut reste)?)
            } else {
                None
            };
            entrees.push(EntreeFs {
                nom,
                taille,
                est_dossier,
                modifie_le,
            });
        }
        exiger_vide(reste)?;
        Ok(Self {
            chemin,
            entrees,
            erreur,
        })
    }
}

// ---------------------------------------------------------------------------
// Handler côté hôte
// ---------------------------------------------------------------------------

/// Handler hôte du listing : liste le contenu réel du répertoire `chemin` via
/// `std::fs` et produit la réponse à renvoyer au contrôleur.
///
/// * `chemin` **vide** → entrées « racines » pour amorcer le navigateur :
///   lettres de lecteur présentes (`C:\`, `D:\`…) sous Windows, `/` ailleurs.
/// * Les dossiers viennent d'abord, puis les fichiers ; chaque groupe est trié
///   par nom (ordre binaire, déterministe pour l'affichage et les tests).
/// * Toute erreur d'ouverture (dossier inexistant, accès refusé, chemin qui
///   n'est pas un dossier…) est renvoyée dans [`ReponseListe::erreur`] : cette
///   fonction ne panique jamais et renvoie toujours une réponse.
/// * Une entrée qui disparaît ou devient illisible en cours de parcours
///   (répertoire vivant) n'interrompt pas le listing : elle est omise, ou
///   renseignée au mieux (sans taille ni mtime) si seul `metadata` échoue.
///
/// Sécurité : **lecture seule**, chemin utilisé tel quel — voir la doc du
/// module ; le contrôle d'accès relève du routage `nd-core`.
pub fn lister_repertoire(chemin: &str) -> ReponseListe {
    if chemin.is_empty() {
        return ReponseListe {
            chemin: String::new(),
            entrees: racines(),
            erreur: None,
        };
    }
    let lecteur = match std::fs::read_dir(chemin) {
        Ok(lecteur) => lecteur,
        Err(e) => return ReponseListe::en_erreur(chemin, message_erreur(&e)),
    };
    let mut entrees = Vec::new();
    for element in lecteur {
        // Élément illisible en cours de parcours : listing au mieux, jamais
        // d'échec global pour une entrée isolée.
        let Ok(element) = element else { continue };
        let nom = element.file_name().to_string_lossy().into_owned();
        match element.metadata() {
            Ok(meta) => entrees.push(EntreeFs {
                nom,
                taille: meta.len(),
                est_dossier: meta.is_dir(),
                modifie_le: epoch_modification(&meta),
            }),
            // L'entrée a disparu entre `read_dir` et `metadata` : omise.
            Err(e) if e.kind() == ErrorKind::NotFound => continue,
            // Métadonnées inaccessibles (droits…) : entrée au mieux, le type
            // venant du parcours lui-même quand il est connu.
            Err(_) => entrees.push(EntreeFs {
                nom,
                taille: 0,
                est_dossier: element.file_type().is_ok_and(|t| t.is_dir()),
                modifie_le: None,
            }),
        }
    }
    // Dossiers d'abord, puis fichiers ; chaque groupe trié par nom.
    entrees.sort_by(|a, b| {
        b.est_dossier
            .cmp(&a.est_dossier)
            .then_with(|| a.nom.cmp(&b.nom))
    });
    ReponseListe {
        chemin: chemin.to_string(),
        entrees,
        erreur: None,
    }
}

/// Traite côté hôte une requête de listing reçue **en octets** et renvoie la
/// réponse encodée, prête à repartir sur le canal. Ne panique jamais et répond
/// toujours : une requête malformée produit une réponse dont `erreur` est
/// renseignée (et dont le chemin est vide, faute de chemin décodable).
pub fn traiter_requete_liste(octets: &[u8]) -> Vec<u8> {
    let reponse = match RequeteListe::from_bytes(octets) {
        Ok(requete) => lister_repertoire(&requete.chemin),
        Err(e) => ReponseListe::en_erreur("", format!("requête de listing invalide : {e}")),
    };
    reponse.to_bytes()
}

/// Entrées « racines » renvoyées pour un chemin vide, afin d'amorcer le
/// navigateur : chaque nom est directement utilisable comme chemin de la
/// requête suivante.
///
/// Sous Windows, les lettres `A:`–`Z:` sont sondées via `std::fs` (pas de
/// FFI) sous leur forme `X:\` — `X:` seul serait relatif au répertoire courant
/// du lecteur ; un lecteur absent ou vide (optique sans disque) échoue à la
/// sonde et est simplement omis.
#[cfg(windows)]
fn racines() -> Vec<EntreeFs> {
    (b'A'..=b'Z')
        .map(|lettre| format!("{}:\\", lettre as char))
        .filter(|racine| std::path::Path::new(racine).is_dir())
        .map(|nom| EntreeFs {
            nom,
            taille: 0,
            est_dossier: true,
            modifie_le: None,
        })
        .collect()
}

/// Entrées « racines » pour un chemin vide : la racine unique `/` des
/// systèmes de type Unix.
#[cfg(not(windows))]
fn racines() -> Vec<EntreeFs> {
    vec![EntreeFs {
        nom: "/".to_string(),
        taille: 0,
        est_dossier: true,
        modifie_le: None,
    }]
}

/// Message d'erreur en français pour la réponse : cause stable pour les cas
/// courants, détail système conservé entre parenthèses pour le diagnostic.
fn message_erreur(e: &std::io::Error) -> String {
    let cause = match e.kind() {
        ErrorKind::NotFound => "dossier inexistant",
        ErrorKind::PermissionDenied => "accès refusé",
        ErrorKind::NotADirectory => "pas un dossier",
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
            "octets excédentaires après le message de listing".into(),
        ))
    }
}

/// Prélève `n` octets en tête de `charge` (avance le curseur).
fn lire_octets<'a>(charge: &mut &'a [u8], n: usize) -> Result<&'a [u8]> {
    if charge.len() < n {
        return Err(NdError::Protocol(format!(
            "message de listing tronqué : {n} octets attendus, {} restants",
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
        .map_err(|_| NdError::Protocol("texte de listing non UTF-8".into()))
}

/// Lit un drapeau binaire strict — `0` ou `1` uniquement, pour garder le
/// format symétrique ; toute autre valeur est un octet corrompu.
fn lire_drapeau(charge: &mut &[u8]) -> Result<bool> {
    match lire_octets(charge, 1)?[0] {
        0 => Ok(false),
        1 => Ok(true),
        v => Err(NdError::Protocol(format!(
            "drapeau de listing invalide : {v}"
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
        std::env::temp_dir().join(format!("nd_files_listing_{}_{nom}", std::process::id()))
    }

    #[test]
    fn round_trip_requete() {
        // Chemin vide (racines), accents, chemin Windows typique.
        for chemin in ["", "C:\\Users\\Café\\Éléments — récents", "/home/été/docs"] {
            let requete = RequeteListe {
                chemin: chemin.to_string(),
            };
            let octets = requete.to_bytes();
            assert_eq!(RequeteListe::from_bytes(&octets).unwrap(), requete);
        }
        // Un octet excédentaire après un message complet est refusé.
        let mut trop = RequeteListe {
            chemin: "C:\\".to_string(),
        }
        .to_bytes();
        trop.push(0);
        assert!(RequeteListe::from_bytes(&trop).is_err());
        // Une réponse n'est pas décodable comme requête (tags distincts).
        let reponse = lister_repertoire("").to_bytes();
        assert!(RequeteListe::from_bytes(&reponse).is_err());
    }

    #[test]
    fn round_trip_reponse() {
        let reponses = [
            // Succès sans entrée (dossier vide).
            ReponseListe {
                chemin: "C:\\vide".to_string(),
                entrees: Vec::new(),
                erreur: None,
            },
            // Succès : accents, grandes tailles, mtime présent et absent.
            ReponseListe {
                chemin: "D:\\Données\\Été".to_string(),
                entrees: vec![
                    EntreeFs {
                        nom: "Dossier — été".to_string(),
                        taille: 0,
                        est_dossier: true,
                        modifie_le: Some(1_752_000_000),
                    },
                    EntreeFs {
                        nom: "énorme.bin".to_string(),
                        taille: u64::MAX,
                        est_dossier: false,
                        modifie_le: None,
                    },
                    EntreeFs {
                        nom: String::new(), // nom vide toléré par le format
                        taille: 1,
                        est_dossier: false,
                        modifie_le: Some(u64::MAX),
                    },
                ],
                erreur: None,
            },
            // Échec : erreur renseignée, aucune entrée.
            ReponseListe {
                chemin: "E:\\perdu".to_string(),
                entrees: Vec::new(),
                erreur: Some("dossier inexistant (détail système)".to_string()),
            },
        ];
        for reponse in &reponses {
            let octets = reponse.to_bytes();
            assert_eq!(&ReponseListe::from_bytes(&octets).unwrap(), reponse);
        }
        // Un octet excédentaire après un message complet est refusé.
        let mut trop = reponses[0].to_bytes();
        trop.push(0);
        assert!(ReponseListe::from_bytes(&trop).is_err());
    }

    #[test]
    fn decodage_malforme_rejete_sans_panique() {
        // Tampons vides et tags inconnus.
        assert!(RequeteListe::from_bytes(&[]).is_err());
        assert!(ReponseListe::from_bytes(&[]).is_err());
        assert!(RequeteListe::from_bytes(&[99]).is_err());
        assert!(ReponseListe::from_bytes(&[99]).is_err());
        // Chaîne tronquée : 5 octets annoncés, 1 fourni.
        assert!(RequeteListe::from_bytes(&[TAG_REQUETE, 5, 0, 0, 0, b'a']).is_err());
        // Chemin non UTF-8.
        assert!(RequeteListe::from_bytes(&[TAG_REQUETE, 2, 0, 0, 0, 0xFF, 0xFF]).is_err());
        // Drapeau d'erreur hors {0, 1} : octet 5 (tag + longueur d'un chemin vide).
        let mut drapeau = lister_repertoire("").to_bytes();
        drapeau[5] = 7;
        assert!(ReponseListe::from_bytes(&drapeau).is_err());
        // Nombre d'entrées délirant (u32::MAX annoncé, rien derrière) : erreur
        // de troncature immédiate, sans allocation démesurée ni panique.
        let mut delirant = vec![TAG_REPONSE];
        delirant.extend_from_slice(&0u32.to_le_bytes()); // chemin vide
        delirant.push(0); // pas d'erreur
        delirant.extend_from_slice(&u32::MAX.to_le_bytes());
        assert!(ReponseListe::from_bytes(&delirant).is_err());
        // Réponse amputée de son dernier octet : troncature détectée.
        let octets = ReponseListe {
            chemin: "C:\\x".to_string(),
            entrees: vec![EntreeFs {
                nom: "a".to_string(),
                taille: 3,
                est_dossier: false,
                modifie_le: Some(42),
            }],
            erreur: None,
        }
        .to_bytes();
        assert!(ReponseListe::from_bytes(&octets[..octets.len() - 1]).is_err());
    }

    #[test]
    fn lister_repertoire_dossier_peuple() {
        // Dossier temporaire peuplé : 2 sous-dossiers + 2 fichiers, noms
        // choisis pour vérifier le tri « dossiers d'abord, puis par nom ».
        let dir = chemin_temp("peuple");
        std::fs::create_dir_all(dir.join("z_dossier")).unwrap();
        std::fs::create_dir_all(dir.join("a_dossier")).unwrap();
        std::fs::write(dir.join("zz.bin"), b"12345").unwrap();
        std::fs::write(dir.join("aa.txt"), "né".as_bytes()).unwrap(); // 3 octets UTF-8

        let reponse = lister_repertoire(dir.to_str().unwrap());
        assert!(reponse.erreur.is_none());
        assert_eq!(reponse.chemin, dir.to_str().unwrap());
        assert_eq!(reponse.entrees.len(), 4);
        let noms: Vec<&str> = reponse.entrees.iter().map(|e| e.nom.as_str()).collect();
        assert_eq!(noms, ["a_dossier", "z_dossier", "aa.txt", "zz.bin"]);
        assert!(reponse.entrees[0].est_dossier && reponse.entrees[1].est_dossier);
        assert!(!reponse.entrees[2].est_dossier && !reponse.entrees[3].est_dossier);
        assert_eq!(reponse.entrees[2].taille, 3);
        assert_eq!(reponse.entrees[3].taille, 5);
        assert!(reponse.entrees.iter().all(|e| e.modifie_le.is_some()));

        // La réponse réelle survit à l'aller-retour binaire.
        let relue = ReponseListe::from_bytes(&reponse.to_bytes()).unwrap();
        assert_eq!(relue, reponse);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lister_repertoire_erreurs_sans_panique() {
        // Dossier inexistant : erreur renseignée, aucune entrée, chemin échoyé.
        let absent = chemin_temp("inexistant");
        let _ = std::fs::remove_dir_all(&absent);
        let reponse = lister_repertoire(absent.to_str().unwrap());
        assert_eq!(reponse.chemin, absent.to_str().unwrap());
        assert!(reponse.entrees.is_empty());
        let message = reponse
            .erreur
            .expect("erreur attendue (dossier inexistant)");
        assert!(message.contains("dossier inexistant"), "{message}");

        // Chemin pointant un fichier (pas un dossier) : erreur, pas de panique.
        let fichier = chemin_temp("fichier.bin");
        std::fs::write(&fichier, b"nd").unwrap();
        let reponse = lister_repertoire(fichier.to_str().unwrap());
        assert!(reponse.entrees.is_empty());
        assert!(reponse.erreur.is_some());
        let _ = std::fs::remove_file(&fichier);

        // La réponse d'erreur survit elle aussi à l'aller-retour binaire.
        let reponse = lister_repertoire(absent.to_str().unwrap());
        assert_eq!(
            ReponseListe::from_bytes(&reponse.to_bytes()).unwrap(),
            reponse
        );
    }

    #[test]
    fn lister_repertoire_chemin_vide_racines() {
        let reponse = lister_repertoire("");
        assert!(reponse.erreur.is_none());
        assert!(reponse.chemin.is_empty());
        assert!(!reponse.entrees.is_empty(), "au moins une racine attendue");
        assert!(reponse
            .entrees
            .iter()
            .all(|e| e.est_dossier && e.taille == 0 && e.modifie_le.is_none()));
        #[cfg(windows)]
        {
            // Chaque racine est de la forme « X:\ », directement listable.
            assert!(reponse
                .entrees
                .iter()
                .all(|e| e.nom.len() == 3 && e.nom.ends_with(":\\")));
            // Le lecteur qui héberge le répertoire temporaire est forcément
            // présent (quand le TEMP est bien sur un lecteur à lettre).
            let racine_temp = std::env::temp_dir()
                .ancestors()
                .last()
                .unwrap()
                .to_string_lossy()
                .into_owned();
            if racine_temp.len() == 3 && racine_temp.as_bytes()[1] == b':' {
                assert!(reponse
                    .entrees
                    .iter()
                    .any(|e| e.nom.eq_ignore_ascii_case(&racine_temp)));
            }
        }
        #[cfg(not(windows))]
        {
            assert_eq!(reponse.entrees.len(), 1);
            assert_eq!(reponse.entrees[0].nom, "/");
        }
    }

    #[test]
    fn traiter_requete_liste_octets_de_bout_en_bout() {
        let dir = chemin_temp("bout_en_bout");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("f.bin"), b"abc").unwrap();

        // Requête valide : la réponse encodée se décode et liste le dossier.
        let requete = RequeteListe {
            chemin: dir.to_string_lossy().into_owned(),
        };
        let octets = traiter_requete_liste(&requete.to_bytes());
        let reponse = ReponseListe::from_bytes(&octets).unwrap();
        assert!(reponse.erreur.is_none());
        assert_eq!(reponse.entrees.len(), 1);
        assert_eq!(reponse.entrees[0].nom, "f.bin");
        assert_eq!(reponse.entrees[0].taille, 3);

        // Requête malformée : toujours une réponse décodable, avec `erreur`.
        let reponse = ReponseListe::from_bytes(&traiter_requete_liste(&[0xFF, 1, 2])).unwrap();
        assert!(reponse.entrees.is_empty());
        assert!(reponse.erreur.is_some());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
