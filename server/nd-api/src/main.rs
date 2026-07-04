//! API applicative NovaDesk (squelette).
//!
//! Exposera le carnet d'adresses/équipes (RBAC), les licences et le service de mises à
//! jour. Voir `../../plan-technique/11-backend-infrastructure.md` et `15-deploiement-mise-a-jour.md`.

fn main() {
    println!(
        "nd-api — NovaDesk (protocole v{}) — squelette, non implémenté.",
        nd_proto::ProtocolVersion::CURRENT
    );
    println!("À implémenter : carnet d'adresses, RBAC, licences, MAJ (plan 11/15).");
}
