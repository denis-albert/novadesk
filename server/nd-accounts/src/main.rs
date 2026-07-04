//! Service comptes / authentification NovaDesk (squelette).
//!
//! Gérera les comptes, OAuth2/OIDC, JWT, 2FA (TOTP) et le SSO entreprise.
//! Voir `../../plan-technique/11-backend-infrastructure.md`.

fn main() {
    println!(
        "nd-accounts — NovaDesk (protocole v{}) — squelette, non implémenté.",
        nd_proto::ProtocolVersion::CURRENT
    );
    println!("À implémenter : comptes, OAuth2/OIDC, JWT, 2FA, SSO (plan 11).");
}
