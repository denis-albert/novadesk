//! Sonde « prêt pour la session » (plan 09) : transfert multi-fichiers de bout
//! en bout **en mémoire** via [`nd_files::TransferSession`], puis aller-retour
//! de sérialisation d'un [`nd_files::ClipboardContent`].
//!
//! Aucun réseau, aucun accès au presse-papiers du système : les octets produits
//! par une session sont réinjectés à la main dans l'autre, exactement comme
//! nd-core les ferait circuler sur le canal `Files`. Affiche la progression, les
//! fichiers reçus, un contrôle d'intégrité BLAKE3, puis un verdict.

use std::path::Path;

use nd_files::{chunk_hash, ClipboardContent, TransferEvent, TransferSession};

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
    let base = std::env::temp_dir().join(format!("novadesk_session_probe_{}", std::process::id()));
    let src = base.join("src");
    let dst = base.join("dst");
    std::fs::create_dir_all(&src)?;
    std::fs::create_dir_all(&dst)?;

    let resultat = scenario(&src, &dst);
    let _ = std::fs::remove_dir_all(&base); // nettoyage best-effort
    resultat
}

fn scenario(src: &Path, dst: &Path) -> Res {
    // --- 1. Quelques fichiers de tailles variées (dernier chunk partiel).
    let specs = [
        ("rapport.bin", 250_000usize),
        ("photo.bin", 900_000),
        ("note.txt", 42),
    ];
    let mut chemins = Vec::new();
    let mut total = 0u64;
    for (nom, taille) in specs {
        let p = src.join(nom);
        std::fs::write(&p, motif(taille))?;
        total += taille as u64;
        chemins.push(p);
    }
    println!("file : {} fichiers, {total} octets au total", specs.len());

    // --- 2. Session émettrice + réceptrice, pilotées à la main (comme nd-core
    //        sur un canal fiable) : on draine `poll_outgoing` de chaque côté et
    //        on réinjecte dans `handle_incoming` de l'autre.
    let mut emetteur = TransferSession::send_with_chunk_size(chemins, 32 * 1024)?;
    let mut recepteur = TransferSession::receive(dst.to_path_buf());

    let mut trames = 0u64;
    loop {
        let mut avance = false;
        while let Some(bytes) = emetteur.poll_outgoing()? {
            recepteur.handle_incoming(&bytes)?;
            trames += 1;
            avance = true;
        }
        while let Some(bytes) = recepteur.poll_outgoing()? {
            emetteur.handle_incoming(&bytes)?;
            avance = true;
        }
        for e in recepteur.take_events() {
            match e {
                TransferEvent::FileCompleted { name, size, .. } => {
                    println!("  reçu {name} ({size} octets)");
                }
                TransferEvent::Finished => println!("  file terminée"),
                _ => {}
            }
        }
        if !avance {
            break;
        }
    }

    if !recepteur.is_finished() {
        return Err("la session ne s'est pas terminée".into());
    }
    let p = recepteur.progress();
    println!(
        "progression finale : {}/{} octets ({:.0} %), {trames} trames échangées",
        p.bytes_done,
        p.bytes_total,
        p.percent()
    );

    // --- 3. Intégrité : chaque fichier reçu identique à sa source (BLAKE3).
    for (nom, _) in specs {
        if chunk_hash(&std::fs::read(src.join(nom))?) != chunk_hash(&std::fs::read(dst.join(nom))?)
        {
            return Err(format!("hash BLAKE3 divergent pour {nom}").into());
        }
    }
    println!("intégrité : {} fichiers identiques (BLAKE3)", specs.len());

    // --- 4. Presse-papiers : aller-retour de sérialisation d'un contenu texte.
    let contenu = ClipboardContent::Text("NovaDesk presse-papiers éàü₿".to_string());
    let octets = contenu.to_bytes();
    if ClipboardContent::from_bytes(&octets)? != contenu {
        return Err("contenu de presse-papiers altéré par la sérialisation".into());
    }
    println!(
        "presse-papiers : sérialisation ClipboardContent OK ({} octets)",
        octets.len()
    );
    Ok(())
}

/// Motif déterministe non trivial (évite les longues plages constantes).
fn motif(n: usize) -> Vec<u8> {
    (0..n)
        .map(|i| (i.wrapping_mul(31).wrapping_add(i >> 8) & 0xff) as u8)
        .collect()
}
