//! Sonde micro : capture ~1 s du microphone (périphérique de capture par
//! défaut, WASAPI en mode partagé), encode en Opus profil voix (~28 kbps,
//! DTX), affiche le nombre de trames et leur taille moyenne, puis redécode
//! pour vérifier le pipeline de bout en bout.
//!
//! Note honnête : sans micro branché ou en silence complet, les paquets
//! restent valides (trames Opus de silence, minuscules grâce au DTX) — seul
//! le pic |PCM| retombe à zéro.

#[cfg(windows)]
fn main() -> nd_proto::Result<()> {
    use nd_audio::{create_microphone_capturer, echantillons_par_trame, DecodeurOpus, TRAME_MS};

    let mut capteur = create_microphone_capturer()?;
    let format = capteur.format();
    println!(
        "micro WASAPI ouvert : {} Hz, {} canal(aux), trames Opus voix de {TRAME_MS} ms",
        format.sample_rate, format.channels
    );

    let mut decodeur = DecodeurOpus::new(format)?;
    let nb_trames = 50; // 50 × 20 ms ≈ 1 s
    let mut total_octets = 0usize;
    let mut echantillons_decodes = 0usize;
    let mut pic = 0f32;

    for _ in 0..nb_trames {
        let paquet = capteur.next_packet()?;
        total_octets += paquet.data.len();

        // Vérification : chaque paquet doit se redécoder en une trame pleine.
        let pcm = decodeur.decoder(&paquet.data)?;
        echantillons_decodes += pcm.len() / usize::from(format.channels);
        pic = pcm.iter().fold(pic, |m, &v| m.max(v.abs()));
    }

    println!(
        "{nb_trames} trames Opus capturées, taille moyenne {} octets",
        total_octets / nb_trames
    );
    println!(
        "redécodage : {echantillons_decodes} échantillons/canal (attendu {}), pic |PCM| = {pic:.4}",
        nb_trames * echantillons_par_trame(format)
    );
    if pic == 0.0 {
        println!("(pic nul : micro muet ou absent — silence, c'est normal)");
    }
    Ok(())
}

#[cfg(not(windows))]
fn main() {
    eprintln!("mic_probe : exemple Windows uniquement (capture WASAPI, voir plan 08/16).");
}
