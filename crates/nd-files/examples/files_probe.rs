//! Sonde du plan 09 : chunks BLAKE3 + reprise, listing `LocalFs`, presse-papiers.
//!
//! Crée un fichier temporaire de quelques Mo, calcule le plan de chunks et le
//! hash racine BLAKE3, relit le fichier et vérifie l'intégrité de chaque chunk
//! (plan complet, puis plan de reprise), liste le dossier temporaire via
//! `LocalFs`, puis (Windows) effectue un aller-retour texte dans le
//! presse-papiers. Affiche les statistiques et un verdict final.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::time::Instant;

use nd_files::{
    chunk_hash, open_remote_fs, plan_file_chunks_with, verify_chunk, ChunkPlan, DEFAULT_CHUNK_SIZE,
};

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
    // --- Fichier temporaire de quelques Mo, motif déterministe,
    //     avec un dernier chunk volontairement partiel.
    let dir = std::env::temp_dir();
    let path = dir.join(format!("novadesk_files_probe_{}.bin", std::process::id()));
    let taille = 3 * DEFAULT_CHUNK_SIZE as usize + 517;
    let contenu = motif(taille);
    std::fs::write(&path, &contenu)?;
    println!("fichier temporaire : {} ({taille} octets)", path.display());

    let resultat = scenario(&path, &contenu, &dir);
    let _ = std::fs::remove_file(&path); // nettoyage best-effort
    resultat
}

fn scenario(path: &Path, contenu: &[u8], dir: &Path) -> Res {
    // --- 1. Plan de chunks complet + hash racine.
    let debut = Instant::now();
    let plan = plan_file_chunks_with(path, 0, DEFAULT_CHUNK_SIZE)?;
    let duree_plan = debut.elapsed();
    println!(
        "plan de chunks : {} chunks de {} octets max, hash racine {} ({:.1} ms)",
        plan.chunks.len(),
        plan.chunk_size,
        hex(&plan.root_hash),
        duree_plan.as_secs_f64() * 1000.0
    );
    if plan.chunks.len() != 4 {
        return Err(format!("attendu 4 chunks, obtenu {}", plan.chunks.len()).into());
    }
    if plan.root_hash != chunk_hash(contenu) {
        return Err("hash racine != BLAKE3 du contenu complet".into());
    }

    // --- 2. Relecture et vérification d'intégrité de chaque chunk.
    let debut = Instant::now();
    verifier_relecture(path, &plan)?;
    let duree_verif = debut.elapsed();
    let debit = plan.file_len as f64 / duree_verif.as_secs_f64() / (1024.0 * 1024.0);
    println!(
        "relecture : {} chunks vérifiés (BLAKE3) en {:.1} ms ({debit:.0} Mio/s)",
        plan.chunks.len(),
        duree_verif.as_secs_f64() * 1000.0
    );

    // --- 3. Reprise : plan à partir du 2e chunk, cohérent avec le plan complet.
    let reprise = u64::from(DEFAULT_CHUNK_SIZE) * 2;
    let plan_reprise = plan_file_chunks_with(path, reprise, DEFAULT_CHUNK_SIZE)?;
    if plan_reprise.chunks != plan.chunks[2..] {
        return Err("plan de reprise incohérent avec le plan complet".into());
    }
    if plan_reprise.root_hash != chunk_hash(&contenu[reprise as usize..]) {
        return Err("hash racine de reprise != BLAKE3 du suffixe".into());
    }
    verifier_relecture(path, &plan_reprise)?;
    println!(
        "reprise à l'offset {reprise} : {} chunks restants, intégrité OK",
        plan_reprise.chunks.len()
    );

    // --- 4. Listing du dossier temporaire via LocalFs (derrière RemoteFs).
    let mut fs = open_remote_fs()?;
    let entrees = fs.list(dir.to_str().ok_or("chemin temporaire non UTF-8")?)?;
    let nom = path
        .file_name()
        .ok_or("nom de fichier absent")?
        .to_string_lossy();
    let entree = entrees
        .iter()
        .find(|e| e.name == nom)
        .ok_or("fichier temporaire absent du listing LocalFs")?;
    if entree.is_dir || entree.size != contenu.len() as u64 {
        return Err(format!(
            "entrée de listing incohérente : is_dir={}, size={}",
            entree.is_dir, entree.size
        )
        .into());
    }
    println!(
        "LocalFs : {} entrées dans {}, fichier retrouvé ({} octets, modifié={:?})",
        entrees.len(),
        dir.display(),
        entree.size,
        entree.modified_epoch
    );

    // --- 5. Presse-papiers (Windows uniquement).
    essai_presse_papiers()
}

/// Relit le fichier chunk par chunk selon le plan et vérifie chaque hash.
fn verifier_relecture(path: &Path, plan: &ChunkPlan) -> Res {
    let mut fichier = File::open(path)?;
    let mut buf = vec![0u8; plan.chunk_size as usize];
    for c in &plan.chunks {
        fichier.seek(SeekFrom::Start(c.offset))?;
        let data = &mut buf[..c.len as usize];
        fichier.read_exact(data)?;
        if !verify_chunk(c, data) {
            return Err(format!("intégrité BLAKE3 en échec sur le chunk {}", c.index).into());
        }
    }
    Ok(())
}

/// Aller-retour texte dans le presse-papiers Windows, en restaurant ensuite
/// l'éventuel texte précédent par politesse envers l'utilisateur.
#[cfg(windows)]
fn essai_presse_papiers() -> Res {
    let clip = nd_files::open_clipboard()?;
    let precedent = clip.get_text()?;
    let message = format!("NovaDesk files_probe — éàü₿ {}", std::process::id());
    clip.set_text(&message)?;
    let relu = clip.get_text()?;
    if let Some(ancien) = precedent {
        let _ = clip.set_text(&ancien);
    }
    if relu.as_deref() == Some(message.as_str()) {
        println!(
            "presse-papiers : aller-retour set_text/get_text OK ({} caractères, non-ASCII inclus)",
            message.chars().count()
        );
        Ok(())
    } else {
        Err(format!("presse-papiers : texte relu inattendu : {relu:?}").into())
    }
}

#[cfg(not(windows))]
fn essai_presse_papiers() -> Res {
    println!("presse-papiers : ignoré (implémentation Windows uniquement à ce stade)");
    Ok(())
}

/// Motif déterministe non trivial (évite les longues plages constantes).
fn motif(n: usize) -> Vec<u8> {
    (0..n)
        .map(|i| (i.wrapping_mul(31).wrapping_add(i >> 8) & 0xff) as u8)
        .collect()
}

/// Rendu hexadécimal court d'un hash (8 premiers octets).
fn hex(h: &[u8; 32]) -> String {
    h.iter()
        .take(8)
        .map(|o| format!("{o:02x}"))
        .collect::<String>()
        + "…"
}
