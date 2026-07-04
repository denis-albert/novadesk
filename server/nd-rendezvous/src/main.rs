//! Serveur de rendez-vous / signalisation NovaDesk (squelette).
//!
//! Enregistrera les clients, maintiendra la présence et mettra les pairs en relation
//! (échange de candidats, déclenchement du hole punching) sans jamais déchiffrer le
//! média. Voir `../../plan-technique/05-connectivite-nat.md` et `11-backend-infrastructure.md`.

fn main() {
    println!(
        "nd-rendezvous — NovaDesk (protocole v{}) — squelette, non implémenté.",
        nd_proto::ProtocolVersion::CURRENT
    );
    println!("À implémenter : enregistrement d'ID, présence, échange de candidats (plan 05/11).");
}
