//! Tests d'intégration du tunnel TCP (plan 13) : les octets traversent
//! `pipe_bidirectional` et `LocalForwarder` intacts, dans les deux sens,
//! et les fins de flux se propagent en cascade.

use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::thread;

use nd_features::tunnel::{pipe_bidirectional, LocalForwarder};

/// Crée une paire de flux TCP connectés l'un à l'autre via l'interface locale.
fn paire_tcp() -> (TcpStream, TcpStream) {
    let ecouteur = TcpListener::bind("127.0.0.1:0").unwrap();
    let adresse = ecouteur.local_addr().unwrap();
    let client = TcpStream::connect(adresse).unwrap();
    let (serveur, _) = ecouteur.accept().unwrap();
    (client, serveur)
}

#[test]
fn pipe_bidirectional_octets_intacts_dans_les_deux_sens() {
    // Topologie : gauche <-> (interne_a =pont= interne_b) <-> droite.
    let (mut gauche, interne_a) = paire_tcp();
    let (interne_b, mut droite) = paire_tcp();
    let pont = thread::spawn(move || pipe_bidirectional(interne_a, interne_b));

    // Charges utiles distinctes par sens, assez grosses pour dépasser les
    // tampons des sockets (d'où les écritures sur des threads dédiés).
    let aller: Vec<u8> = (0..96_000u32).map(|i| (i % 251) as u8).collect();
    let retour: Vec<u8> = (0..96_000u32).map(|i| (i % 241) as u8).collect();

    let ecrivain_gauche = {
        let mut flux = gauche.try_clone().unwrap();
        let donnees = aller.clone();
        thread::spawn(move || {
            flux.write_all(&donnees).unwrap();
            // Fin d'écriture : doit se propager jusqu'à `droite` via le pont.
            flux.shutdown(Shutdown::Write).unwrap();
        })
    };
    let ecrivain_droite = {
        let mut flux = droite.try_clone().unwrap();
        let donnees = retour.clone();
        thread::spawn(move || {
            flux.write_all(&donnees).unwrap();
            flux.shutdown(Shutdown::Write).unwrap();
        })
    };

    // `read_to_end` ne rend la main que si le shutdown a bien cascadé.
    let mut recu_droite = Vec::new();
    droite.read_to_end(&mut recu_droite).unwrap();
    let mut recu_gauche = Vec::new();
    gauche.read_to_end(&mut recu_gauche).unwrap();

    assert_eq!(recu_droite, aller);
    assert_eq!(recu_gauche, retour);

    ecrivain_gauche.join().unwrap();
    ecrivain_droite.join().unwrap();
    pont.join().unwrap().unwrap();
}

#[test]
fn local_forwarder_relaie_une_connexion() {
    // « Service distant » : renvoie la requête en majuscules ASCII.
    let distant = TcpListener::bind("127.0.0.1:0").unwrap();
    let adresse_distante = distant.local_addr().unwrap();
    let serveur = thread::spawn(move || {
        let (mut flux, _) = distant.accept().unwrap();
        let mut requete = Vec::new();
        flux.read_to_end(&mut requete).unwrap();
        flux.write_all(&requete.to_ascii_uppercase()).unwrap();
        // La chute du flux propage la fin jusqu'au client via le tunnel.
    });

    let forwarder = LocalForwarder::bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let adresse_locale = forwarder.local_addr().unwrap();
    let relais =
        thread::spawn(move || forwarder.forward_one(|_| TcpStream::connect(adresse_distante)));

    let mut client = TcpStream::connect(adresse_locale).unwrap();
    client.write_all(b"tunnel novadesk").unwrap();
    client.shutdown(Shutdown::Write).unwrap();

    let mut reponse = Vec::new();
    client.read_to_end(&mut reponse).unwrap();
    assert_eq!(reponse, b"TUNNEL NOVADESK");

    serveur.join().unwrap();
    relais.join().unwrap().unwrap();
}
