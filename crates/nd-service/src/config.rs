//! Configuration **machine** du service NovaDesk, lue sous `C:\ProgramData\NovaDesk`.
//!
//! Le service tourne en **LocalSystem** (session 0) : il ne peut donc pas lire la
//! configuration **par utilisateur** de l'application (`%APPDATA%\NovaDesk`, gérée
//! par `nd-ffi::etat`). Il lit à la place un dossier **machine**, partagé, que
//! l'UI (ou un administrateur) renseigne pour le mode service.
//!
//! # Emplacement
//!
//! * **Surcharge (tests / multi-instance)** : `NOVADESK_SERVICE_DIR`, si définie et
//!   non vide, **remplace** tout le reste (chemin pris tel quel).
//! * Windows : `%ProgramData%\NovaDesk` (par défaut `C:\ProgramData\NovaDesk`).
//! * Repli hors Windows : `NOVADESK_SERVICE_DIR` sinon `./novadesk-service`
//!   (le service n'est fonctionnel que sous Windows ; ce repli sert aux tests).
//!
//! # Schéma (`config.json`)
//!
//! Miroir **des champs** du module `etat` de `nd-ffi` (accès non surveillé), à une
//! divergence **assumée** près : les secrets y sont en **portée machine**, pas
//! chiffrés au repos par DPAPI utilisateur — un service LocalSystem **ne peut pas**
//! déchiffrer un blob DPAPI écrit par l'utilisateur (contextes distincts). Le
//! mot de passe n'est donc **jamais en clair** non plus : on conserve son **haché
//! salé BLAKE3** (même schéma que `etat`), lisible par LocalSystem. Le durcissement
//! par DPAPI *à portée machine* (`CRYPTPROTECT_LOCAL_MACHINE`) est une étape
//! ultérieure documentée.
//!
//! ```json
//! {
//!   "id": 123456789,
//!   "serveur_rendezvous": "203.0.113.10:9000",
//!   "serveurs_stun": ["stun.l.google.com:19302"],
//!   "permissions_bits": 4095,
//!   "mot_de_passe": { "sel": "…hex…", "empreinte": "…hex…" },
//!   "appareils_de_confiance": [222222222],
//!   "admission_autorisee": [333333333]
//! }
//! ```
//!
//! L'identité **TLS** (certificat épinglable publié au rendez-vous) est persistée
//! à part, en DER brut (`identite_tls.cert.der` / `identite_tls.key.der`), et le
//! `NovaId` en est dérivé de façon **stable** au premier démarrage puis persisté
//! dans `config.json`.

use std::fs;
use std::io::ErrorKind;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use nd_features::PermissionSet;
use nd_proto::NovaId;
use nd_transport::ServerIdentity;

/// Nom du fichier de configuration machine (JSON).
const FICHIER_CONFIG: &str = "config.json";
/// Certificat TLS auto-signé (DER) publié au rendez-vous.
const FICHIER_CERT: &str = "identite_tls.cert.der";
/// Clé privée TLS (PKCS#8 DER) de l'identité publiée.
const FICHIER_CLE: &str = "identite_tls.key.der";

/// Longueur du sel du haché de mot de passe (octets).
const LONGUEUR_SEL: usize = 16;

// ---------------------------------------------------------------------------
// Répertoire de données machine
// ---------------------------------------------------------------------------

/// Résout le répertoire de configuration **machine** du service (voir le module).
#[must_use]
pub fn repertoire_service() -> PathBuf {
    let non_vide = |var: &str| std::env::var(var).ok().filter(|v| !v.is_empty());
    if let Some(dir) = non_vide("NOVADESK_SERVICE_DIR") {
        return PathBuf::from(dir);
    }
    #[cfg(windows)]
    {
        non_vide("ProgramData").map_or_else(
            || PathBuf::from(r"C:\ProgramData\NovaDesk"),
            |pd| PathBuf::from(pd).join("NovaDesk"),
        )
    }
    #[cfg(not(windows))]
    {
        PathBuf::from("./novadesk-service")
    }
}

// ---------------------------------------------------------------------------
// Configuration résolue (prête à l'emploi par l'hôte)
// ---------------------------------------------------------------------------

/// Configuration du service **résolue** : valeurs analysées et identité chargée,
/// prêtes à alimenter [`crate::hote::demarrer`].
pub struct ConfigService {
    /// Identifiant à 9 chiffres publié au rendez-vous.
    pub id: NovaId,
    /// Serveur de rendez-vous (obligatoire).
    pub rendezvous: SocketAddr,
    /// Serveurs STUN pour la traversée NAT (éventuellement vide).
    pub stun: Vec<SocketAddr>,
    /// Permissions accordées aux sessions servies.
    pub permissions: PermissionSet,
    /// Haché du mot de passe non surveillé (`None` = aucun mot de passe).
    pub mot_de_passe: Option<MotDePasseHache>,
    /// Appareils de confiance (admis sans mot de passe).
    pub appareils_de_confiance: Vec<u64>,
    /// Liste blanche d'admission (admis sans mot de passe), en union avec la
    /// confiance ci-dessus.
    pub admission_autorisee: Vec<u64>,
    /// Identité TLS persistée (certificat épinglable).
    pub identite: ServerIdentity,
}

/// Forme **sur disque** de `config.json` (découplée de [`ConfigService`]).
#[derive(Default, Serialize, Deserialize)]
struct ConfigDisque {
    /// `NovaId` à 9 chiffres ; `0` (ou absent) = dérivé du certificat au premier
    /// démarrage puis persisté.
    #[serde(default)]
    id: u64,
    #[serde(default)]
    serveur_rendezvous: String,
    #[serde(default)]
    serveurs_stun: Vec<String>,
    /// Bits d'un [`PermissionSet`] ; absent = toutes les capacités ([`PermissionSet::full`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    permissions_bits: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mot_de_passe: Option<MotDePasseHache>,
    #[serde(default)]
    appareils_de_confiance: Vec<u64>,
    #[serde(default)]
    admission_autorisee: Vec<u64>,
}

// ---------------------------------------------------------------------------
// Chargement / préparation
// ---------------------------------------------------------------------------

/// Charge la configuration résolue depuis `repertoire`, en créant l'identité TLS
/// et en dérivant/persistant le `NovaId` au besoin.
///
/// # Errors
/// Erreur si `config.json` est illisible (JSON invalide), si le serveur de
/// rendez-vous est absent ou mal formé, si un serveur STUN est mal formé, ou si
/// l'identité TLS ne peut être ni chargée ni générée.
pub fn charger(repertoire: PathBuf) -> Result<ConfigService, String> {
    let mut disque = lire_config(&repertoire)?;
    let identite = charger_ou_creer_identite(&repertoire)?;

    // NovaId stable : valeur persistée si valide, sinon dérivée du certificat et
    // écrite pour les démarrages suivants.
    let id = if disque.id >= 100_000_000 {
        disque.id
    } else {
        let derive = nova_id_depuis_cert(identite.cert_der());
        disque.id = derive;
        ecrire_config(&repertoire, &disque)?;
        derive
    };

    let rendezvous = disque.serveur_rendezvous.trim();
    if rendezvous.is_empty() {
        return Err(format!(
            "serveur de rendez-vous non configuré : renseignez « serveur_rendezvous » dans {}",
            repertoire.join(FICHIER_CONFIG).display()
        ));
    }
    let rendezvous: SocketAddr = rendezvous.parse().map_err(|e| {
        format!("adresse du serveur de rendez-vous « {rendezvous} » invalide (attendu « ip:port ») : {e}")
    })?;

    let stun = analyser_stun(&disque.serveurs_stun)?;
    let permissions = disque
        .permissions_bits
        .map_or_else(PermissionSet::full, PermissionSet::from_bits);

    Ok(ConfigService {
        id: NovaId(id),
        rendezvous,
        stun,
        permissions,
        mot_de_passe: disque.mot_de_passe.clone(),
        appareils_de_confiance: disque.appareils_de_confiance.clone(),
        admission_autorisee: disque.admission_autorisee.clone(),
        identite,
    })
}

/// Prépare la configuration initiale (appelée à l'installation) : crée le
/// répertoire et l'identité TLS, dérive et persiste le `NovaId` si nécessaire, et
/// garantit l'existence de `config.json` (serveur de rendez-vous à renseigner).
/// Renvoie l'identifiant machine à afficher à l'administrateur.
///
/// # Errors
/// Erreur d'entrée/sortie (création du répertoire, écriture) ou de génération de
/// l'identité TLS.
pub fn preparer_config_initiale(repertoire: &Path) -> Result<u64, String> {
    fs::create_dir_all(repertoire).map_err(|e| {
        format!(
            "création du répertoire machine « {} » impossible : {e}",
            repertoire.display()
        )
    })?;
    let identite = charger_ou_creer_identite(repertoire)?;
    let mut disque = lire_config(repertoire)?;
    if disque.id < 100_000_000 {
        disque.id = nova_id_depuis_cert(identite.cert_der());
    }
    ecrire_config(repertoire, &disque)?;
    Ok(disque.id)
}

/// Écrit (ou remplace) le haché du mot de passe non surveillé dans `config.json`.
/// Un mot de passe **vide** l'efface. Pratique pour configurer le mode service
/// sans l'UI (`novadesk-svc set-password …`).
///
/// # Errors
/// Erreur de lecture/écriture de `config.json`.
pub fn definir_mot_de_passe(repertoire: &Path, clair: &str) -> Result<(), String> {
    let mut disque = lire_config(repertoire)?;
    disque.mot_de_passe = if clair.is_empty() {
        None
    } else {
        Some(MotDePasseHache::depuis_clair(clair))
    };
    ecrire_config(repertoire, &disque)
}

// ---------------------------------------------------------------------------
// Primitives fichier
// ---------------------------------------------------------------------------

/// Lit `config.json` ; fichier absent = configuration vide, JSON invalide = erreur.
fn lire_config(repertoire: &Path) -> Result<ConfigDisque, String> {
    match fs::read(repertoire.join(FICHIER_CONFIG)) {
        Ok(octets) => serde_json::from_slice(&octets)
            .map_err(|e| format!("« {FICHIER_CONFIG} » illisible (JSON invalide) : {e}")),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(ConfigDisque::default()),
        Err(e) => Err(format!("lecture de « {FICHIER_CONFIG} » impossible : {e}")),
    }
}

/// Écrit `config.json` **atomiquement** (fichier temporaire renommé sur la cible).
fn ecrire_config(repertoire: &Path, disque: &ConfigDisque) -> Result<(), String> {
    let contenu = serde_json::to_vec_pretty(disque)
        .map_err(|e| format!("sérialisation de « {FICHIER_CONFIG} » impossible : {e}"))?;
    ecrire_atomique(repertoire, FICHIER_CONFIG, &contenu)
}

/// Charge l'identité TLS persistée (DER) ou en génère une neuve, persistée pour
/// les démarrages suivants (certificat épinglable stable).
fn charger_ou_creer_identite(repertoire: &Path) -> Result<ServerIdentity, String> {
    let cert = fs::read(repertoire.join(FICHIER_CERT));
    let cle = fs::read(repertoire.join(FICHIER_CLE));
    if let (Ok(cert), Ok(cle)) = (&cert, &cle) {
        if !cert.is_empty() && !cle.is_empty() {
            return Ok(ServerIdentity::from_der_parts(cert.clone(), cle.clone()));
        }
    }
    let identite = ServerIdentity::generate()
        .map_err(|e| format!("génération de l'identité TLS impossible : {e}"))?;
    ecrire_atomique(repertoire, FICHIER_CERT, identite.cert_der())?;
    ecrire_atomique(repertoire, FICHIER_CLE, identite.key_pkcs8_der())?;
    Ok(identite)
}

/// Écrit `contenu` **atomiquement** sous `repertoire/fichier`.
fn ecrire_atomique(repertoire: &Path, fichier: &str, contenu: &[u8]) -> Result<(), String> {
    fs::create_dir_all(repertoire).map_err(|e| {
        format!(
            "création du répertoire machine « {} » impossible : {e}",
            repertoire.display()
        )
    })?;
    static COMPTEUR_TMP: AtomicU64 = AtomicU64::new(0);
    let unique = COMPTEUR_TMP.fetch_add(1, Ordering::Relaxed);
    let tmp = repertoire.join(format!("{fichier}.tmp-{}-{unique}", std::process::id()));
    fs::write(&tmp, contenu).map_err(|e| format!("écriture de « {fichier} » impossible : {e}"))?;
    fs::rename(&tmp, repertoire.join(fichier)).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("remplacement atomique de « {fichier} » impossible : {e}")
    })
}

/// Analyse une liste de serveurs STUN (« ip:port ») ; l'entrée fautive est située.
fn analyser_stun(entrees: &[String]) -> Result<Vec<SocketAddr>, String> {
    entrees
        .iter()
        .enumerate()
        .map(|(i, s)| {
            s.trim().parse::<SocketAddr>().map_err(|e| {
                format!(
                    "serveur STUN n°{} « {s} » invalide (attendu « ip:port ») : {e}",
                    i + 1
                )
            })
        })
        .collect()
}

/// Dérive un `NovaId` à 9 chiffres (`100 000 000`–`999 999 999`) des 8 premiers
/// octets de l'empreinte BLAKE3 du certificat (identité stable → ID stable).
fn nova_id_depuis_cert(cert_der: &[u8]) -> u64 {
    let empreinte = blake3::hash(cert_der);
    let octets = empreinte.as_bytes();
    let mut tampon = [0u8; 8];
    tampon.copy_from_slice(&octets[..8]);
    100_000_000 + u64::from_be_bytes(tampon) % 900_000_000
}

// ---------------------------------------------------------------------------
// Mot de passe haché (schéma identique à `nd-ffi::etat`)
// ---------------------------------------------------------------------------

/// Mot de passe permanent haché : sel aléatoire + `BLAKE3(sel || mot de passe)`,
/// tous deux en hexadécimal. Le clair n'est **jamais** stocké.
#[derive(Clone, Serialize, Deserialize)]
pub struct MotDePasseHache {
    sel: String,
    empreinte: String,
}

impl MotDePasseHache {
    /// Hache `pwd` avec un sel aléatoire neuf.
    #[must_use]
    pub fn depuis_clair(pwd: &str) -> Self {
        let sel = octets_aleatoires(LONGUEUR_SEL);
        MotDePasseHache {
            empreinte: empreinte_mot_de_passe(&sel, pwd),
            sel: hex_minuscule(&sel),
        }
    }

    /// Vérifie un mot de passe candidat contre ce haché.
    #[must_use]
    pub fn verifier(&self, pwd: &str) -> bool {
        match decoder_hex(&self.sel) {
            Some(sel) => empreinte_mot_de_passe(&sel, pwd) == self.empreinte,
            None => false,
        }
    }
}

/// Empreinte hexadécimale `BLAKE3(sel || mot de passe)`.
fn empreinte_mot_de_passe(sel: &[u8], pwd: &str) -> String {
    let mut donnees = sel.to_vec();
    donnees.extend_from_slice(pwd.as_bytes());
    blake3::hash(&donnees).to_hex().to_string()
}

/// Renvoie `n` octets aléatoires. Source primaire : le CSPRNG du workspace (via
/// `nd_crypto::generate_static_keypair`) ; repli déterministe (horloge + PID +
/// compteur diffusé) si la génération de clés échouait — même stratégie que
/// `nd-ffi::etat`.
fn octets_aleatoires(n: usize) -> Vec<u8> {
    let mut pool = match nd_crypto::generate_static_keypair() {
        Ok(paire) => {
            let mut octets = paire.private;
            octets.extend_from_slice(&paire.public);
            octets
        }
        Err(_) => Vec::new(),
    };
    while pool.len() < n {
        pool.extend_from_slice(&graine_temporelle().to_le_bytes());
    }
    pool.truncate(n);
    pool
}

/// Graine de repli : horloge nanoseconde ⊕ PID ⊕ compteur, diffusée (splitmix64).
fn graine_temporelle() -> u64 {
    static COMPTEUR: AtomicU64 = AtomicU64::new(0);
    let compte = COMPTEUR.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    splitmix64(nanos ^ compte.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ u64::from(std::process::id()))
}

/// Diffuseur de bits splitmix64 (qualité suffisante pour un repli d'aléa).
fn splitmix64(graine: u64) -> u64 {
    let mut z = graine.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Encode des octets en hexadécimal minuscule.
fn hex_minuscule(octets: &[u8]) -> String {
    octets.iter().map(|b| format!("{b:02x}")).collect()
}

/// Décode une chaîne hexadécimale ; `None` si mal formée.
fn decoder_hex(texte: &str) -> Option<Vec<u8>> {
    if !texte.is_ascii() || !texte.len().is_multiple_of(2) {
        return None;
    }
    (0..texte.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&texte[i..i + 2], 16).ok())
        .collect()
}

// ---------------------------------------------------------------------------
// Tests : round-trips disque et haché de mot de passe
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn mot_de_passe_hache_verifie_le_bon_clair() {
        let hache = MotDePasseHache::depuis_clair("Séquoia-42");
        assert!(hache.verifier("Séquoia-42"));
        assert!(!hache.verifier("mauvais"));
        assert!(!hache.verifier(""));
        // Sel aléatoire : deux hachés du même clair diffèrent (pas de table arc-en-ciel).
        let autre = MotDePasseHache::depuis_clair("Séquoia-42");
        assert_ne!(hache.sel, autre.sel, "sel aléatoire par haché");
    }

    #[test]
    fn preparer_puis_charger_donne_un_id_stable() {
        let dir = TempDir::new().expect("répertoire temporaire");
        let id1 = preparer_config_initiale(dir.path()).expect("préparation");
        assert!(
            (100_000_000..=999_999_999).contains(&id1),
            "ID à 9 chiffres"
        );

        // Sans rendez-vous, `charger` échoue proprement (config incomplète).
        let sans_rv = charger(dir.path().to_path_buf());
        assert!(sans_rv.is_err(), "rendez-vous manquant → erreur");

        // Renseigne le rendez-vous et un mot de passe, puis charge : ID inchangé.
        definir_mot_de_passe(dir.path(), "secret").expect("mot de passe");
        renseigner_rendezvous(dir.path(), "127.0.0.1:9000");
        let cfg = charger(dir.path().to_path_buf()).expect("chargement");
        assert_eq!(
            cfg.id.as_u64(),
            id1,
            "ID stable entre préparation et chargement"
        );
        assert!(cfg.mot_de_passe.expect("mdp").verifier("secret"));
        assert_eq!(cfg.rendezvous.port(), 9000);
        // Permissions par défaut = toutes les capacités.
        assert!(!cfg.permissions.is_empty());
    }

    #[test]
    fn identite_tls_est_persistee_entre_chargements() {
        let dir = TempDir::new().expect("répertoire temporaire");
        preparer_config_initiale(dir.path()).expect("préparation");
        renseigner_rendezvous(dir.path(), "127.0.0.1:9000");
        let a = charger(dir.path().to_path_buf()).expect("charge a");
        let b = charger(dir.path().to_path_buf()).expect("charge b");
        assert_eq!(
            a.identite.cert_der(),
            b.identite.cert_der(),
            "certificat rechargé, pas régénéré"
        );
    }

    /// Écrit un serveur de rendez-vous dans `config.json` (aide de test).
    fn renseigner_rendezvous(repertoire: &Path, adresse: &str) {
        let mut disque = lire_config(repertoire).expect("lecture config");
        disque.serveur_rendezvous = adresse.to_owned();
        ecrire_config(repertoire, &disque).expect("écriture config");
    }
}
