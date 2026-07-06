//! Sonde du plan 09 (gestionnaire de fichiers) : opérations d'écriture de
//! `RemoteFs` sur un répertoire temporaire dédié, via `LocalFs::jailed`.
//!
//! Déroulé : dans un sous-dossier temporaire unique, mkdir → create_file →
//! écriture (std) → copy_file (taille vérifiée) → rename → stat → tentatives
//! d'évasion refusées par le confinement → remove_file/remove_dir_all.
//! Chaque étape est affichée, puis un verdict final ; le sous-dossier est
//! nettoyé dans tous les cas.

use std::path::Path;

use nd_files::{LocalFs, RemoteFs};
use nd_proto::NdError;

type Res = std::result::Result<(), Box<dyn std::error::Error>>;

fn main() {
    match run() {
        Ok(()) => println!("\nverdict : OK"),
        Err(e) => {
            eprintln!("\nverdict : ECHEC — {e}");
            std::process::exit(1);
        }
    }
}

fn run() -> Res {
    // Sous-dossier temporaire unique (pid) : racine du confinement.
    let racine = std::env::temp_dir().join(format!("novadesk_fsops_probe_{}", std::process::id()));
    std::fs::create_dir_all(&racine)?;
    println!("racine temporaire : {}", racine.display());

    let resultat = scenario(&racine);
    let _ = std::fs::remove_dir_all(&racine); // nettoyage best-effort
    resultat
}

fn scenario(racine: &Path) -> Res {
    // `jailed` : tous les chemins (relatifs) sont ancrés et bornés sous la racine.
    let mut fs = LocalFs::jailed(racine)?;
    println!("LocalFs::jailed : chemins bornés sous la racine");

    // --- 1. mkdir (avec parents).
    fs.mkdir("dossier/sous")?;
    if !fs.exists("dossier/sous")? {
        return Err("mkdir : le répertoire créé n'existe pas".into());
    }
    println!("mkdir dossier/sous : OK");

    // --- 2. create_file (vide) puis écriture du contenu via std (comme le
    //        ferait le module `transfer` côté récepteur).
    fs.create_file("dossier/source.bin")?;
    let contenu = motif(128 * 1024 + 33);
    std::fs::write(racine.join("dossier").join("source.bin"), &contenu)?;
    println!(
        "create_file + écriture : dossier/source.bin ({} octets)",
        contenu.len()
    );

    // --- 3. copy_file : le nombre d'octets copiés doit être la taille source.
    let copies = fs.copy_file("dossier/source.bin", "dossier/copie.bin")?;
    if copies != contenu.len() as u64 {
        return Err(format!(
            "copy_file : {copies} octets copiés, attendu {}",
            contenu.len()
        )
        .into());
    }
    println!("copy_file → dossier/copie.bin : {copies} octets (taille vérifiée)");

    // --- 4. rename : l'ancienne entrée disparaît.
    fs.rename("dossier/copie.bin", "dossier/finale.bin")?;
    if fs.exists("dossier/copie.bin")? {
        return Err("rename : la source existe encore".into());
    }
    println!("rename copie.bin → finale.bin : OK");

    // --- 5. stat : type, taille et horodatage cohérents.
    let entree = fs
        .stat("dossier/finale.bin")?
        .ok_or("stat : entrée absente après rename")?;
    if entree.is_dir || entree.size != contenu.len() as u64 {
        return Err(format!(
            "stat : entrée incohérente (is_dir={}, size={})",
            entree.is_dir, entree.size
        )
        .into());
    }
    println!(
        "stat finale.bin : {} octets, modifié={:?}",
        entree.size, entree.modified_epoch
    );

    // --- 6. Confinement : `..` et chemin absolu hors racine sont refusés.
    exiger_refus(fs.exists("../evasion.txt").err(), "composant '..'")?;
    let hors = std::env::temp_dir().join("novadesk_fsops_probe_evasion.txt");
    let hors = hors.to_str().ok_or("chemin temporaire non UTF-8")?;
    exiger_refus(fs.create_file(hors).err(), "chemin absolu hors racine")?;

    // --- 7. remove_file + remove_dir_all : plus rien ne subsiste.
    fs.remove_file("dossier/source.bin")?;
    if fs.stat("dossier/source.bin")?.is_some() {
        return Err("remove_file : le fichier existe encore".into());
    }
    fs.remove_dir_all("dossier")?;
    if fs.exists("dossier")? {
        return Err("remove_dir_all : le répertoire existe encore".into());
    }
    println!("remove_file + remove_dir_all : OK");
    Ok(())
}

/// Vérifie qu'une tentative d'évasion a bien été refusée par une erreur de
/// protocole (et non acceptée ou échouée pour une autre raison).
fn exiger_refus(erreur: Option<NdError>, cas: &str) -> Res {
    match erreur {
        Some(NdError::Protocol(_)) => {
            println!("jail : {cas} refusé (attendu)");
            Ok(())
        }
        Some(autre) => Err(format!("jail : {cas} — erreur inattendue : {autre}").into()),
        None => Err(format!("jail : {cas} accepté alors qu'il devait être refusé").into()),
    }
}

/// Motif déterministe non trivial (évite les longues plages constantes).
fn motif(n: usize) -> Vec<u8> {
    (0..n)
        .map(|i| (i.wrapping_mul(31).wrapping_add(i >> 8) & 0xff) as u8)
        .collect()
}
