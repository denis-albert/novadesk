//! Sonde audio : capture ~1 s de l'audio système (loopback WASAPI), encode en
//! Opus, affiche le nombre de trames et leur taille moyenne, puis redécode
//! pour vérifier le pipeline de bout en bout.
//!
//! Note honnête : si rien n'est en cours de lecture sur la machine, le
//! loopback renvoie du silence — les paquets restent valides (trames Opus de
//! silence, très compactes de l'ordre de quelques octets).

#[cfg(windows)]
fn main() -> nd_proto::Result<()> {
    use nd_audio::{create_system_capturer, echantillons_par_trame, DecodeurOpus, TRAME_MS};

    let mut capteur = create_system_capturer()?;
    let format = capteur.format();
    println!(
        "loopback WASAPI ouvert : {} Hz, {} canaux, trames Opus de {TRAME_MS} ms",
        format.sample_rate, format.channels
    );

    let mut decodeur = DecodeurOpus::new(format)?;
    let nb_trames = 50; // 50 × 20 ms ≈ 1 s
    let mut total_octets = 0usize;
    let mut echantillons_decodes = 0usize;
    let mut credit_max = 0f32;

    for _ in 0..nb_trames {
        let paquet = capteur.next_packet()?;
        total_octets += paquet.data.len();

        // Vérification : chaque paquet doit se redécoder en une trame pleine.
        let pcm = decodeur.decoder(&paquet.data)?;
        echantillons_decodes += pcm.len() / usize::from(format.channels);
        credit_max = pcm.iter().fold(credit_max, |m, &v| m.max(v.abs()));
    }

    println!(
        "{nb_trames} trames Opus capturées, taille moyenne {} octets",
        total_octets / nb_trames
    );
    println!(
        "redécodage : {echantillons_decodes} échantillons/canal (attendu {}), pic |PCM| = {credit_max:.4}",
        nb_trames * echantillons_par_trame(format)
    );
    if credit_max == 0.0 {
        println!("(pic nul : rien n'était en cours de lecture — silence, c'est normal)");
    }
    Ok(())
}

#[cfg(not(windows))]
fn main() {
    eprintln!("audio_probe : exemple Windows uniquement (loopback WASAPI, voir plan 08/16).");
}
