//! Contenu réel du presse-papiers « fichiers » (plan 09) : de quoi
//! **matérialiser** localement, à la demande, les fichiers annoncés par la
//! synchro presse-papiers — au lieu d'y coller des chemins distants qui
//! n'existent pas sur la machine réceptrice.
//!
//! # Le problème corrigé
//!
//! [`ClipboardContent::Files`](crate::ClipboardContent::Files) transporte des
//! **chemins** (`CF_HDROP`). Appliqués tels quels par
//! [`Clipboard::set_files`](crate::Clipboard::set_files), ces chemins pointent
//! vers le disque de l'émetteur : sur le récepteur, ils sont **inexistants**, et
//! coller ne produit rien d'utile. Le texte et l'image du presse-papiers, eux,
//! sont autoporteurs et restent gérés tels quels par
//! [`ClipboardSync`](crate::ClipboardSync) — ce module ne les touche pas.
//!
//! # Flux de matérialisation (piloté par `nd-core`)
//!
//! 1. **Émetteur** : à la copie de fichiers, il annonce un [`ManifesteFichiers`]
//!    ([`manifeste_fichiers`]) — pour chaque fichier, son chemin source, son nom
//!    et sa taille. Énumération de l'UI : nom + taille.
//! 2. **Récepteur** : il choisit un **dossier temporaire local** et calcule les
//!    chemins de destination sûrs ([`chemins_locaux`]), puis télécharge le
//!    contenu de chaque fichier **par tranches** via la primitive du module
//!    [`download`](crate::download) (`RequeteFichier`/`ReponseFichier`), en
//!    écrivant chaque tranche reçue ([`ecrire_reponse_locale`]).
//! 3. Une fois le contenu écrit, le récepteur place dans le presse-papiers les
//!    **chemins locaux** (ceux du dossier temporaire) via
//!    [`Clipboard::set_files`](crate::Clipboard::set_files) — jamais les chemins
//!    distants. Coller produit alors de vrais fichiers locaux.
//!
//! Le module reste **indépendant du réseau** : `nd-core` fait circuler les
//! octets ([`ManifesteFichiers::to_bytes`], puis les tranches de `download`) et
//! sert la lecture derrière sa permission (côté source, en lecture seule).
//!
//! # Format binaire du manifeste (compact, little-endian)
//!
//! ```text
//! manifeste (tag 1) : [tag : u8][nombre : u32 LE]{ fichier… }
//! fichier           : [long. chemin : u32 LE][chemin UTF-8]
//!                     [long. nom : u32 LE][nom UTF-8][taille : u64 LE]
//! ```
//!
//! Décodage robuste aux malformés (troncature, tag inconnu, texte non UTF-8,
//! octets excédentaires → [`NdError::Protocol`], jamais de panique) ; un nombre
//! d'entrées délirant ne pré-alloue jamais plus que ce que le tampon peut
//! contenir.
//!
//! # Sécurité
//!
//! * Côté **source**, la lecture des tranches est en lecture seule et le chemin
//!   est utilisé tel quel (confinement/permission = `nd-core`, voir
//!   [`download`](crate::download)).
//! * Côté **récepteur**, les noms annoncés sont réduits à un **composant de
//!   base** ([`chemin_local`]) avant d'être joints au dossier temporaire :
//!   aucun `..` ni chemin absolu distant ne peut faire écrire hors du dossier.

use std::collections::HashSet;
use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use nd_proto::{NdError, Result};

use crate::download::ReponseFichier;

/// Tag binaire du manifeste (0 est évité pour détecter les tampons nuls, comme
/// dans les autres modules du crate).
const TAG_MANIFESTE: u8 = 1;

/// Taille minimale d'un fichier encodé : chemin vide (4) + nom vide (4) +
/// taille (8). Sert à borner la pré-allocation au décodage.
const FICHIER_MIN_OCTETS: usize = 4 + 4 + 8;

/// Un fichier annoncé par le presse-papiers distant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FichierPressePapiers {
    /// Chemin **source** (côté émetteur) : la clé des
    /// [`RequeteFichier`](crate::download::RequeteFichier) qui en téléchargent
    /// le contenu. Non utilisé tel quel côté récepteur (voir [`chemin_local`]).
    pub chemin: String,
    /// Nom de base, pour l'affichage (énumération) et le nom du fichier
    /// temporaire local.
    pub nom: String,
    /// Taille en octets, pour l'énumération et la progression.
    pub taille: u64,
}

/// Manifeste des fichiers annoncés : ce que le récepteur énumère (nom + taille)
/// avant de matérialiser le contenu par tranches.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ManifesteFichiers {
    /// Fichiers annoncés, dans l'ordre de la copie.
    pub fichiers: Vec<FichierPressePapiers>,
}

impl ManifesteFichiers {
    /// Sérialise le manifeste en octets autonomes (voir le format en tête de
    /// module), prêts pour le canal de session.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.push(TAG_MANIFESTE);
        out.extend_from_slice(&(self.fichiers.len() as u32).to_le_bytes());
        for f in &self.fichiers {
            ecrire_chaine(&mut out, &f.chemin);
            ecrire_chaine(&mut out, &f.nom);
            out.extend_from_slice(&f.taille.to_le_bytes());
        }
        out
    }

    /// Désérialise un manifeste depuis `buf`, qui doit contenir exactement un
    /// message (aucun octet excédentaire). [`NdError::Protocol`] sinon.
    pub fn from_bytes(buf: &[u8]) -> Result<Self> {
        let mut reste = exiger_tag(buf, TAG_MANIFESTE, "manifeste de fichiers")?;
        let nombre = lire_u32(&mut reste)? as usize;
        // Pré-allocation bornée par ce que le tampon peut réellement contenir :
        // un nombre annoncé délirant échoue en troncature sans allouer.
        let mut fichiers = Vec::with_capacity(nombre.min(reste.len() / FICHIER_MIN_OCTETS));
        for _ in 0..nombre {
            let chemin = lire_chaine(&mut reste)?;
            let nom = lire_chaine(&mut reste)?;
            let taille = lire_u64(&mut reste)?;
            fichiers.push(FichierPressePapiers {
                chemin,
                nom,
                taille,
            });
        }
        exiger_vide(reste)?;
        Ok(Self { fichiers })
    }
}

// ---------------------------------------------------------------------------
// Côté émetteur : annonce
// ---------------------------------------------------------------------------

/// Construit le manifeste à annoncer à partir des chemins **sources** (ceux que
/// [`Clipboard::get_files`](crate::Clipboard::get_files) a rapportés).
///
/// Chaque chemin est `stat`é en **lecture seule** : seuls les **fichiers
/// réguliers** lisibles sont retenus (nom de base + taille). Les dossiers, les
/// entrées spéciales et les chemins illisibles sont **omis** — la
/// matérialisation ne concerne que du contenu de fichier (l'expansion récursive
/// de dossiers est hors périmètre). Ne panique jamais.
pub fn manifeste_fichiers(chemins: &[PathBuf]) -> ManifesteFichiers {
    let mut fichiers = Vec::new();
    for chemin in chemins {
        let Ok(meta) = std::fs::metadata(chemin) else {
            continue; // illisible : omis
        };
        if !meta.is_file() {
            continue; // dossier ou entrée spéciale : non matérialisable ici
        }
        let Some(nom) = chemin.file_name().map(|n| n.to_string_lossy().into_owned()) else {
            continue; // sans nom de base (racine…) : omis
        };
        fichiers.push(FichierPressePapiers {
            // Chemin source rendu en UTF-8 (perte possible pour un chemin
            // non-UTF-8, cas rare, cohérent avec `ClipboardContent::Files`).
            chemin: chemin.to_string_lossy().into_owned(),
            nom,
            taille: meta.len(),
        });
    }
    ManifesteFichiers { fichiers }
}

// ---------------------------------------------------------------------------
// Côté récepteur : matérialisation locale
// ---------------------------------------------------------------------------

/// Chemin **local** (dans `dossier`) où matérialiser un fichier annoncé `nom`,
/// après réduction de `nom` à un composant de base sûr (anti-traversée : un
/// `..`, un chemin absolu ou un séparateur ne peut pas faire sortir de
/// `dossier`). [`NdError::Protocol`] si `nom` n'a pas de composant de base
/// exploitable (`.`, `..`, vide…).
pub fn chemin_local(dossier: &Path, nom: &str) -> Result<PathBuf> {
    Ok(dossier.join(base_sure(nom)?))
}

/// Chemins **locaux** de destination pour tout un [`ManifesteFichiers`], dans
/// `dossier`, alignés par index sur `manifeste.fichiers`.
///
/// Les noms de base sont assainis ([`chemin_local`]) puis **dédupliqués** : deux
/// fichiers annoncés au même nom de base reçoivent des cibles locales distinctes
/// (`nom (2).ext`, `nom (3).ext`…), pour qu'aucune écriture n'en écrase une
/// autre et que [`Clipboard::set_files`](crate::Clipboard::set_files) reçoive des
/// chemins tous différents. [`NdError::Protocol`] si un nom est inexploitable.
pub fn chemins_locaux(dossier: &Path, manifeste: &ManifesteFichiers) -> Result<Vec<PathBuf>> {
    let mut utilises: HashSet<String> = HashSet::new();
    let mut sorties = Vec::with_capacity(manifeste.fichiers.len());
    for f in &manifeste.fichiers {
        let base = base_sure(&f.nom)?;
        let unique = rendre_unique(&base, &mut utilises);
        sorties.push(dossier.join(unique));
    }
    Ok(sorties)
}

/// Écrit une tranche reçue (octets à partir de `offset`) dans le fichier
/// **local** `dest`, en la positionnant à `offset` (le fichier est créé au
/// besoin). Quand `fin` est vrai, la longueur du fichier est fixée à
/// `offset + donnees.len()` : toute queue résiduelle d'un fichier plus ancien du
/// même nom est ainsi coupée, et la taille finale est exacte.
///
/// Reconstitution attendue : appels successifs à `offset` croissant (la boucle
/// de tranches du récepteur), le dernier avec `fin = true`.
pub fn ecrire_tranche_locale(dest: &Path, offset: u64, donnees: &[u8], fin: bool) -> Result<()> {
    let mut fichier = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(dest)?;
    fichier.seek(SeekFrom::Start(offset))?;
    fichier.write_all(donnees)?;
    if fin {
        fichier.set_len(offset + donnees.len() as u64)?;
    }
    Ok(())
}

/// Écrit dans `dest` la tranche portée par une [`ReponseFichier`] reçue du pair
/// (primitive du module [`download`](crate::download)) et renvoie `true` si
/// c'était la **dernière** (`fin`) — le récepteur sait alors passer au fichier
/// suivant.
///
/// [`NdError::Protocol`] si la réponse porte une `erreur` (échec côté source) :
/// rien n'est écrit dans ce cas.
pub fn ecrire_reponse_locale(dest: &Path, reponse: &ReponseFichier) -> Result<bool> {
    if let Some(message) = &reponse.erreur {
        return Err(NdError::Protocol(format!(
            "tranche en erreur côté source : {message}"
        )));
    }
    ecrire_tranche_locale(dest, reponse.offset, &reponse.donnees, reponse.fin)?;
    Ok(reponse.fin)
}

/// Réduit `nom` à son dernier composant de chemin (protection basique
/// anti-traversée) et refuse les noms vides ou `.`/`..` (même esprit que
/// l'assainissement des noms reçus dans [`crate::session`]).
fn base_sure(nom: &str) -> Result<String> {
    match Path::new(nom).file_name().and_then(|n| n.to_str()) {
        Some(base) if !base.is_empty() && base != "." && base != ".." => Ok(base.to_string()),
        _ => Err(NdError::Protocol(format!(
            "nom de fichier annoncé invalide : {nom}"
        ))),
    }
}

/// Rend `base` unique parmi `utilises` : renvoie `base` s'il est libre, sinon
/// insère un compteur avant l'extension (`tige (2).ext`, `tige (3).ext`…).
fn rendre_unique(base: &str, utilises: &mut HashSet<String>) -> String {
    if utilises.insert(base.to_string()) {
        return base.to_string();
    }
    // Sépare tige/extension pour insérer le compteur au bon endroit ; une tige
    // vide (fichier « .cachefile ») garde le nom entier comme tige.
    let (tige, ext) = match base.rsplit_once('.') {
        Some((t, e)) if !t.is_empty() => (t, format!(".{e}")),
        _ => (base, String::new()),
    };
    let mut n = 2u32;
    loop {
        let candidat = format!("{tige} ({n}){ext}");
        if utilises.insert(candidat.clone()) {
            return candidat;
        }
        n += 1;
    }
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
            "octets excédentaires après le manifeste de fichiers".into(),
        ))
    }
}

/// Prélève `n` octets en tête de `charge` (avance le curseur).
fn lire_octets<'a>(charge: &mut &'a [u8], n: usize) -> Result<&'a [u8]> {
    if charge.len() < n {
        return Err(NdError::Protocol(format!(
            "manifeste de fichiers tronqué : {n} octets attendus, {} restants",
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
        .map_err(|_| NdError::Protocol("texte de manifeste non UTF-8".into()))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::download::lire_tranche;

    /// Répertoire temporaire unique pour un test (isolé entre exécutions).
    fn dir_temp(nom: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "nd_files_clipfiles_{}_{nom}_{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Motif déterministe non trivial.
    fn motif(n: usize) -> Vec<u8> {
        (0..n)
            .map(|i| (i.wrapping_mul(31).wrapping_add(i >> 8) & 0xff) as u8)
            .collect()
    }

    #[test]
    fn round_trip_manifeste() {
        let manifestes = [
            // Vide.
            ManifesteFichiers::default(),
            // Accents, grandes tailles, nom/chemin vides tolérés par le format.
            ManifesteFichiers {
                fichiers: vec![
                    FichierPressePapiers {
                        chemin: "C:\\Users\\Café\\rapport — été.pdf".to_string(),
                        nom: "rapport — été.pdf".to_string(),
                        taille: 1_234_567,
                    },
                    FichierPressePapiers {
                        chemin: "/home/été/énorme.bin".to_string(),
                        nom: "énorme.bin".to_string(),
                        taille: u64::MAX,
                    },
                    FichierPressePapiers {
                        chemin: String::new(),
                        nom: String::new(),
                        taille: 0,
                    },
                ],
            },
        ];
        for manifeste in &manifestes {
            let octets = manifeste.to_bytes();
            assert_eq!(&ManifesteFichiers::from_bytes(&octets).unwrap(), manifeste);
        }
        // Un octet excédentaire après un message complet est refusé.
        let mut trop = manifestes[0].to_bytes();
        trop.push(0);
        assert!(ManifesteFichiers::from_bytes(&trop).is_err());
    }

    #[test]
    fn decodage_malforme_rejete_sans_panique() {
        assert!(ManifesteFichiers::from_bytes(&[]).is_err()); // vide
        assert!(ManifesteFichiers::from_bytes(&[99]).is_err()); // tag inconnu
                                                                // Nombre délirant (u32::MAX annoncé, rien derrière) : troncature, pas
                                                                // d'allocation démesurée ni de panique.
        let mut delirant = vec![TAG_MANIFESTE];
        delirant.extend_from_slice(&u32::MAX.to_le_bytes());
        assert!(ManifesteFichiers::from_bytes(&delirant).is_err());
        // Chemin non UTF-8.
        let mut non_utf8 = vec![TAG_MANIFESTE];
        non_utf8.extend_from_slice(&1u32.to_le_bytes()); // 1 fichier
        non_utf8.extend_from_slice(&2u32.to_le_bytes()); // long. chemin
        non_utf8.extend_from_slice(&[0xFF, 0xFF]); // chemin invalide
        assert!(ManifesteFichiers::from_bytes(&non_utf8).is_err());
        // Manifeste amputé de son dernier octet : troncature détectée.
        let octets = ManifesteFichiers {
            fichiers: vec![FichierPressePapiers {
                chemin: "a".to_string(),
                nom: "a".to_string(),
                taille: 7,
            }],
        }
        .to_bytes();
        assert!(ManifesteFichiers::from_bytes(&octets[..octets.len() - 1]).is_err());
    }

    #[test]
    fn manifeste_depuis_fichiers_reels() {
        let dir = dir_temp("annonce");
        std::fs::write(dir.join("a.bin"), b"novadesk").unwrap(); // 8 octets
        std::fs::write(dir.join("b.txt"), "né".as_bytes()).unwrap(); // 3 octets
        std::fs::create_dir_all(dir.join("sous_dossier")).unwrap();
        let absent = dir.join("inexistant.bin");

        let chemins = vec![
            dir.join("a.bin"),
            dir.join("b.txt"),
            dir.join("sous_dossier"), // dossier : omis
            absent,                   // inexistant : omis
        ];
        let manifeste = manifeste_fichiers(&chemins);
        // Seuls les deux fichiers réguliers sont retenus.
        assert_eq!(manifeste.fichiers.len(), 2);
        let a = &manifeste.fichiers[0];
        assert_eq!(a.nom, "a.bin");
        assert_eq!(a.taille, 8);
        assert!(a.chemin.ends_with("a.bin"));
        assert_eq!(manifeste.fichiers[1].nom, "b.txt");
        assert_eq!(manifeste.fichiers[1].taille, 3);

        // Le manifeste réel survit à l'aller-retour binaire.
        assert_eq!(
            ManifesteFichiers::from_bytes(&manifeste.to_bytes()).unwrap(),
            manifeste
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn chemin_local_anti_traversee() {
        let dossier = PathBuf::from("C:\\temp\\session");
        // Nom simple : joint tel quel.
        assert_eq!(
            chemin_local(&dossier, "rapport.pdf").unwrap(),
            dossier.join("rapport.pdf")
        );
        // Traversée et chemin absolu : réduits au composant de base, sous le dossier.
        assert_eq!(
            chemin_local(&dossier, "../../evasion.txt").unwrap(),
            dossier.join("evasion.txt")
        );
        assert_eq!(
            chemin_local(&dossier, "C:\\Windows\\System32\\cmd.exe").unwrap(),
            dossier.join("cmd.exe")
        );
        assert_eq!(
            chemin_local(&dossier, "a/b/c.bin").unwrap(),
            dossier.join("c.bin")
        );
        // Noms sans composant exploitable : refusés.
        assert!(chemin_local(&dossier, "").is_err());
        assert!(chemin_local(&dossier, "..").is_err());
        assert!(chemin_local(&dossier, ".").is_err());
    }

    #[test]
    fn chemins_locaux_deduplique() {
        let dossier = PathBuf::from("/tmp/session");
        let manifeste = ManifesteFichiers {
            fichiers: vec![
                FichierPressePapiers {
                    chemin: "/a/rapport.pdf".to_string(),
                    nom: "rapport.pdf".to_string(),
                    taille: 1,
                },
                FichierPressePapiers {
                    chemin: "/b/rapport.pdf".to_string(), // même nom de base
                    nom: "rapport.pdf".to_string(),
                    taille: 2,
                },
                FichierPressePapiers {
                    chemin: "/c/rapport.pdf".to_string(), // encore
                    nom: "rapport.pdf".to_string(),
                    taille: 3,
                },
            ],
        };
        let locaux = chemins_locaux(&dossier, &manifeste).unwrap();
        assert_eq!(locaux[0], dossier.join("rapport.pdf"));
        assert_eq!(locaux[1], dossier.join("rapport (2).pdf"));
        assert_eq!(locaux[2], dossier.join("rapport (3).pdf"));
        // Tous distincts (aucune écrasement lors de l'écriture / set_files).
        assert_ne!(locaux[0], locaux[1]);
        assert_ne!(locaux[1], locaux[2]);
    }

    #[test]
    fn materialisation_de_bout_en_bout() {
        // Simule tout le flux récepteur : source « distante » lue par tranches
        // (§1), écrite dans un dossier temporaire local, avec chemins locaux.
        let source = dir_temp("mat_source");
        let local = dir_temp("mat_local");
        let contenu_a = motif(3 * 4096 + 17);
        let contenu_b = motif(100);
        std::fs::write(source.join("gros.bin"), &contenu_a).unwrap();
        std::fs::write(source.join("petit.bin"), &contenu_b).unwrap();

        // 1. Annonce (émetteur).
        let manifeste = manifeste_fichiers(&[source.join("gros.bin"), source.join("petit.bin")]);
        assert_eq!(manifeste.fichiers.len(), 2);

        // 2. Destinations locales (récepteur).
        let destinations = chemins_locaux(&local, &manifeste).unwrap();
        assert!(destinations.iter().all(|d| d.starts_with(&local)));

        // 3. Téléchargement par tranches + écriture locale.
        for (fichier, dest) in manifeste.fichiers.iter().zip(&destinations) {
            let mut offset = 0u64;
            loop {
                // La source lit la tranche (comme le ferait le handler hôte §1).
                let reponse = lire_tranche(&fichier.chemin, offset, 4096);
                offset += reponse.donnees.len() as u64;
                let fin = ecrire_reponse_locale(dest, &reponse).unwrap();
                if fin {
                    break;
                }
            }
        }

        // Les fichiers LOCAUX ont exactement le contenu de la source.
        assert_eq!(std::fs::read(&destinations[0]).unwrap(), contenu_a);
        assert_eq!(std::fs::read(&destinations[1]).unwrap(), contenu_b);
        // Ce sont bien des chemins locaux (dans le dossier temporaire), pas les
        // chemins sources distants.
        assert!(destinations[0].starts_with(&local));
        assert_ne!(
            destinations[0].to_string_lossy(),
            manifeste.fichiers[0].chemin
        );

        let _ = std::fs::remove_dir_all(&source);
        let _ = std::fs::remove_dir_all(&local);
    }

    #[test]
    fn ecriture_finale_coupe_la_queue_residuelle() {
        // Un fichier local plus ancien et plus long doit être ramené à la
        // taille exacte de la nouvelle matérialisation (drapeau `fin`).
        let dir = dir_temp("queue");
        let dest = dir.join("f.bin");
        std::fs::write(&dest, motif(10_000)).unwrap(); // ancien contenu, plus long

        ecrire_tranche_locale(&dest, 0, &motif(100), false).unwrap();
        ecrire_tranche_locale(&dest, 100, &motif(50), true).unwrap();
        let relu = std::fs::read(&dest).unwrap();
        assert_eq!(relu.len(), 150); // queue de l'ancien fichier coupée
        assert_eq!(&relu[..100], &motif(100)[..]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reponse_en_erreur_refusee() {
        let dir = dir_temp("erreur");
        let dest = dir.join("f.bin");
        let reponse = ReponseFichier {
            chemin: "distant.bin".to_string(),
            offset: 0,
            donnees: Vec::new(),
            fin: false,
            erreur: Some("fichier inexistant".to_string()),
        };
        assert!(ecrire_reponse_locale(&dest, &reponse).is_err());
        // Rien n'a été créé pour une réponse en erreur.
        assert!(!dest.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
