//! Serveur de relais NovaDesk (squelette).
//!
//! Achemine le trafic chiffré de bout en bout entre deux pairs quand le P2P échoue
//! (NAT symétrique/CGNAT). Le relais ne voit que du ciphertext (voir plan 05/06).

fn main() {
    println!(
        "nd-relay — NovaDesk (protocole v{}) — squelette, non implémenté.",
        nd_proto::ProtocolVersion::CURRENT
    );
    println!("À implémenter : appariement par ticket signé, relais opaque, quotas (plan 05/11).");
}
