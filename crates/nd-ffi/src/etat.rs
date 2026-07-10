//! État applicatif **persistant** de la façade FFI (voir [`crate::api`]).
//!
//! `flutter_rust_bridge` travaillant par fonctions plates, l'UI n'affichait
//! jusqu'ici que des données fictives (ID en dur, carnet en dur, réglages sans
//! effet). Ce module fournit l'**état réel et durable** consommé par la façade :
//! identité locale stable, carnet d'adresses, réglages, historique de sessions,
//! enregistrements et configuration d'accès non surveillé.
//!
//! # Stockage retenu
//!
//! **JSON atomique, pur Rust** (`serde`/`serde_json`) : chaque domaine a son
//! fichier sous le répertoire de données de l'application. Une écriture passe par
//! un fichier temporaire renommé sur la cible (`fs::rename` remplace l'ancien de
//! façon atomique) : jamais de fichier à moitié écrit, jamais de perte sur
//! coupure. Un fichier absent vaut « état vide » (premier lancement) ; un fichier
//! présent mais illisible (JSON invalide) remonte une erreur — on ne réinitialise
//! jamais silencieusement des données de l'utilisateur.
//!
//! # Emplacement
//!
//! * **Surcharge (multi-instance)** : si la variable d'environnement
//!   `NOVADESK_DATA_DIR` est définie et non vide, son chemin **remplace** tout le
//!   reste et sert de répertoire de données **tel quel** (aucun sous-dossier
//!   « NovaDesk » ajouté). Deux instances lancées avec deux `NOVADESK_DATA_DIR`
//!   distincts obtiennent ainsi deux identités — donc deux ID — séparées et
//!   persistantes.
//! * Windows : `%APPDATA%\NovaDesk` (via `std::env::var("APPDATA")`).
//! * Repli Unix : `$XDG_DATA_HOME/NovaDesk` puis `$HOME/.local/share/NovaDesk`.
//! * Dernier repli : `std::env::temp_dir()/NovaDesk`.
//!
//! # Sécurité du mot de passe d'accès non surveillé
//!
//! Le mot de passe permanent n'est **jamais** stocké en clair : on conserve un
//! **hachage BLAKE3 salé** (sel aléatoire de 16 octets + `BLAKE3(sel || mot de
//! passe)`), vérifié par recalcul. NOTE (comme `nd_crypto::identity`) : BLAKE3
//! est rapide, ce n'est pas une KDF lente ; le durcissement par KDF (Argon2)
//! viendra plus tard. Le hachage sert à ne jamais écrire le secret en clair, pas
//! à résister à une attaque hors-ligne massive.
//!
//! # Secrets chiffrés au repos (DPAPI, sans administrateur)
//!
//! Deux secrets sensibles sont en outre **chiffrés au repos** via le coffre-fort
//! de l'OS à portée utilisateur (DPAPI sous Windows, [`crate::plateforme`]) : la
//! **clé privée d'identité** (`identite.cle`) et le **haché du mot de passe**
//! d'accès non surveillé. La **migration est transparente** : un ancien fichier
//! en clair est déchiffré puis **ré-écrit chiffré** à la première lecture, sans
//! jamais casser une identité ni une configuration existante. Hors Windows, repli
//! documenté (stockage en clair comme historiquement, `#[cfg]` dans
//! [`crate::plateforme`]).
//!
//! Ce module est **privé** : il n'est pas scanné par le codegen
//! (`rust_input: crate::api`) et ne fait pas partie du contrat FFI ; la façade
//! [`crate::api`] l'enveloppe dans des fonctions plates.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use nd_crypto::{IdentityStore, PeerFingerprint, StaticKeypair};
use nd_features::Mp4Reader;
use nd_proto::NovaId;

use crate::api::{
    AccessLogEntryDto, AddressBookEntryDto, LocalIdentityDto, RecentSessionDto, RecordingDto,
    SettingDto, UnattendedConfigDto,
};

/// Borne de l'historique de sessions récentes (les plus récentes conservées).
const MAX_HISTORIQUE: usize = 50;

/// Borne du journal des accès non surveillés (les plus récents conservés).
const MAX_JOURNAL_ACCES: usize = 200;

/// Longueur d'un mot de passe éphémère généré.
const LONGUEUR_MDP_EPHEMERE: usize = 10;

// Noms des fichiers de persistance (sous le répertoire de données).
const FICHIER_IDENTITE_CLE: &str = "identite.cle";
const FICHIER_IDENTITE_ID: &str = "identite.json";
const FICHIER_CARNET: &str = "carnet.json";
const FICHIER_REGLAGES: &str = "reglages.json";
const FICHIER_HISTORIQUE: &str = "historique.json";
const FICHIER_NON_SURVEILLE: &str = "non_surveille.json";

/// Préfixe (ASCII) d'un fichier de **clé d'identité chiffrée au repos** (DPAPI
/// sous Windows), suivi immédiatement du blob binaire protégé. Un fichier sans ce
/// préfixe est l'ancien format en clair (`novadesk-identite v1` d'`IdentityStore`),
/// migré à la lecture (déchiffré puis ré-écrit chiffré).
const ENTETE_CLE_CHIFFREE: &[u8] = b"novadesk-cle-chiffree v1\n";

/// Clé de réglage du dossier d'enregistrement (résolu par [`Magasin::lister_enregistrements`]).
const CLE_DOSSIER_ENREGISTREMENT: &str = "dossier_enregistrement";

/// Clé de réglage du **démarrage automatique avec le système**. Rendue réellement
/// effective via la clé de registre `Run` de l'utilisateur (voir
/// [`crate::plateforme::appliquer_demarrage_auto`], appliquée par la façade quand
/// le réglage change).
pub(crate) const CLE_DEMARRER_AVEC_SYSTEME: &str = "demarrer_avec_systeme";

// ---------------------------------------------------------------------------
// Répertoire de données de l'application
// ---------------------------------------------------------------------------

/// Résout le répertoire de données de l'application (créé à la demande).
///
/// La variable d'environnement **`NOVADESK_DATA_DIR`**, si elle est présente et
/// non vide, **remplace** tout le reste : son chemin est utilisé **tel quel**
/// (aucun sous-dossier « NovaDesk » ajouté). Cela permet de lancer plusieurs
/// instances sur une même machine avec des répertoires — donc des identités et
/// des ID — distincts. À défaut, ordre historique : `%APPDATA%\NovaDesk`
/// (Windows), puis `$XDG_DATA_HOME/NovaDesk` et `$HOME/.local/share/NovaDesk`
/// (Unix), enfin `temp_dir()/NovaDesk`.
fn repertoire_donnees() -> PathBuf {
    let non_vide = |var: &str| std::env::var(var).ok().filter(|v| !v.is_empty());
    // Surcharge explicite (multi-instance) : le chemin est pris tel quel.
    if let Some(dir) = non_vide("NOVADESK_DATA_DIR") {
        return PathBuf::from(dir);
    }
    if let Some(appdata) = non_vide("APPDATA") {
        return PathBuf::from(appdata).join("NovaDesk");
    }
    if let Some(xdg) = non_vide("XDG_DATA_HOME") {
        return PathBuf::from(xdg).join("NovaDesk");
    }
    if let Some(home) = non_vide("HOME") {
        return PathBuf::from(home).join(".local/share/NovaDesk");
    }
    std::env::temp_dir().join("NovaDesk")
}

/// Magasin par défaut du processus (répertoire de données réel).
static MAGASIN: OnceLock<Magasin> = OnceLock::new();

/// Renvoie le magasin par défaut, initialisé au premier appel.
pub(crate) fn magasin() -> &'static Magasin {
    MAGASIN.get_or_init(|| Magasin::nouveau(repertoire_donnees()))
}

// ---------------------------------------------------------------------------
// Magasin
// ---------------------------------------------------------------------------

/// Magasin d'état persistant enraciné dans un répertoire.
///
/// Le magasin par défaut ([`magasin`]) pointe sur le répertoire de données de
/// l'application ; les tests en construisent d'autres sur des répertoires
/// temporaires uniques ([`Magasin::nouveau`]) pour vérifier les allers-retours
/// disque. Le [`Mutex`] interne sérialise les séquences lecture-modification-
/// écriture (deux appels FFI concurrents ne peuvent pas se marcher dessus).
pub(crate) struct Magasin {
    racine: PathBuf,
    verrou: Mutex<()>,
}

impl Magasin {
    /// Construit un magasin enraciné dans `racine` (le répertoire est créé à la
    /// première écriture).
    pub(crate) fn nouveau(racine: PathBuf) -> Self {
        Magasin {
            racine,
            verrou: Mutex::new(()),
        }
    }

    // --- primitives fichier ------------------------------------------------

    fn chemin(&self, fichier: &str) -> PathBuf {
        self.racine.join(fichier)
    }

    fn assurer_repertoire(&self) -> Result<(), String> {
        fs::create_dir_all(&self.racine).map_err(|e| {
            format!(
                "création du répertoire de données « {} » impossible : {e}",
                self.racine.display()
            )
        })
    }

    /// Lit et désérialise un fichier JSON ; un fichier absent vaut la valeur par
    /// défaut de `T` (premier lancement), un JSON invalide est une erreur.
    fn lire_json<T: DeserializeOwned + Default>(&self, fichier: &str) -> Result<T, String> {
        match fs::read(self.chemin(fichier)) {
            Ok(octets) => serde_json::from_slice(&octets)
                .map_err(|e| format!("fichier « {fichier} » illisible (JSON invalide) : {e}")),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(T::default()),
            Err(e) => Err(format!("lecture de « {fichier} » impossible : {e}")),
        }
    }

    /// Sérialise `valeur` en JSON et l'écrit **atomiquement** (fichier temporaire
    /// renommé sur la cible).
    fn ecrire_json<T: Serialize>(&self, fichier: &str, valeur: &T) -> Result<(), String> {
        let contenu = serde_json::to_vec_pretty(valeur)
            .map_err(|e| format!("sérialisation de « {fichier} » impossible : {e}"))?;
        self.ecrire_octets_atomique(fichier, &contenu)
    }

    /// Écrit `contenu` **atomiquement** (fichier temporaire renommé sur la cible :
    /// jamais de fichier à moitié écrit). Base commune de [`Magasin::ecrire_json`]
    /// et de l'écriture de la clé d'identité chiffrée (blob binaire, hors JSON).
    fn ecrire_octets_atomique(&self, fichier: &str, contenu: &[u8]) -> Result<(), String> {
        self.assurer_repertoire()?;
        static COMPTEUR_TMP: AtomicU64 = AtomicU64::new(0);
        let unique = COMPTEUR_TMP.fetch_add(1, Ordering::Relaxed);
        let tmp = self.chemin(&format!("{fichier}.tmp-{}-{unique}", std::process::id()));

        fs::write(&tmp, contenu)
            .map_err(|e| format!("écriture de « {fichier} » impossible : {e}"))?;
        fs::rename(&tmp, self.chemin(fichier)).map_err(|e| {
            let _ = fs::remove_file(&tmp);
            format!("remplacement atomique de « {fichier} » impossible : {e}")
        })
    }

    fn verrouiller(&self) -> std::sync::MutexGuard<'_, ()> {
        self.verrou.lock().unwrap_or_else(PoisonError::into_inner)
    }

    // --- 1. Identité locale ------------------------------------------------

    /// Identité locale stable : au premier lancement, génère et persiste une paire
    /// de clés statiques ([`IdentityStore::load_or_create`]) et en dérive un
    /// `NovaId` à 9 chiffres, lui aussi persisté ; les lancements suivants
    /// rechargent exactement les mêmes valeurs.
    pub(crate) fn identite_locale(&self) -> Result<LocalIdentityDto, String> {
        let _garde = self.verrouiller();
        self.assurer_repertoire()?;

        // Paire de clés statiques (identité cryptographique stable, TOFU), chiffrée
        // au repos (DPAPI) avec migration transparente de l'ancien format en clair.
        let paire = self.charger_ou_creer_cle_identite()?;
        let empreinte = PeerFingerprint::from_public_key(&paire.public);

        // NovaId à 9 chiffres : dérivé de l'empreinte au premier lancement, puis
        // persisté tel quel (stable même si la dérivation évoluait).
        let mut ident: IdentiteStockee = self.lire_json(FICHIER_IDENTITE_ID)?;
        if ident.id == 0 {
            ident.id = nova_id_depuis_empreinte(&empreinte.0);
            self.ecrire_json(FICHIER_IDENTITE_ID, &ident)?;
        }

        Ok(LocalIdentityDto {
            id: ident.id,
            id_formate: NovaId(ident.id).to_string(),
            empreinte: hex_minuscule(&empreinte.0),
        })
    }

    /// Charge la paire de clés d'identité en la **chiffrant au repos** (DPAPI) et
    /// en **migrant** l'ancien format en clair de façon transparente :
    ///
    /// * fichier **absent** (premier lancement) → [`IdentityStore::load_or_create`]
    ///   génère la paire (fichier clair), aussitôt chiffré au repos ;
    /// * fichier au **nouveau format chiffré** (préfixe [`ENTETE_CLE_CHIFFREE`]) →
    ///   déchiffré en mémoire puis parsé (le contrôle d'intégrité d'`IdentityStore`
    ///   est réutilisé via un fichier clair temporaire supprimé aussitôt) ;
    /// * fichier à l'**ancien format en clair** → chargé tel quel, puis **ré-écrit
    ///   chiffré** (migration). Les clés sont conservées à l'identique — aucune
    ///   identité existante n'est cassée.
    fn charger_ou_creer_cle_identite(&self) -> Result<StaticKeypair, String> {
        let chemin = self.chemin(FICHIER_IDENTITE_CLE);
        match fs::read(&chemin) {
            // Premier lancement : génère (en clair) puis chiffre au repos.
            Err(e) if e.kind() == ErrorKind::NotFound => {
                let paire = IdentityStore::load_or_create(&chemin)
                    .map_err(|e| format!("identité cryptographique locale indisponible : {e}"))?;
                self.chiffrer_cle_au_repos(&chemin)?;
                Ok(paire)
            }
            Err(e) => Err(format!(
                "lecture de « {FICHIER_IDENTITE_CLE} » impossible : {e}"
            )),
            Ok(octets) => match octets.strip_prefix(ENTETE_CLE_CHIFFREE) {
                // Nouveau format : déprotège puis parse via un fichier clair éphémère.
                Some(corps) => {
                    let clair = crate::plateforme::deproteger(corps)?;
                    self.paire_depuis_clair(&clair)
                }
                // Ancien format en clair : charge, puis migre (chiffre au repos).
                None => {
                    let paire = IdentityStore::load_or_create(&chemin).map_err(|e| {
                        format!("identité cryptographique locale indisponible : {e}")
                    })?;
                    self.chiffrer_cle_au_repos(&chemin)?;
                    Ok(paire)
                }
            },
        }
    }

    /// Chiffre au repos le fichier de clé d'identité **présent en clair** : lit son
    /// contenu (les trois lignes texte d'`IdentityStore`), le protège (DPAPI) et le
    /// ré-écrit atomiquement, préfixé par [`ENTETE_CLE_CHIFFREE`]. Idempotent : un
    /// fichier déjà chiffré est laissé tel quel.
    fn chiffrer_cle_au_repos(&self, chemin: &Path) -> Result<(), String> {
        let clair = fs::read(chemin)
            .map_err(|e| format!("lecture de la clé d'identité impossible : {e}"))?;
        if clair.starts_with(ENTETE_CLE_CHIFFREE) {
            return Ok(());
        }
        let protege = crate::plateforme::proteger(&clair)?;
        let mut contenu = ENTETE_CLE_CHIFFREE.to_vec();
        contenu.extend_from_slice(&protege);
        self.ecrire_octets_atomique(FICHIER_IDENTITE_CLE, &contenu)
    }

    /// Reconstruit la paire de clés depuis le **contenu clair** (trois lignes) en
    /// réutilisant le parseur et le **contrôle d'intégrité** d'[`IdentityStore`] :
    /// écrit un fichier clair temporaire à nom unique, le charge, puis le supprime
    /// aussitôt (exposition en clair minimale, sous le verrou du magasin).
    fn paire_depuis_clair(&self, clair: &[u8]) -> Result<StaticKeypair, String> {
        static COMPTEUR_TMP: AtomicU64 = AtomicU64::new(0);
        let unique = COMPTEUR_TMP.fetch_add(1, Ordering::Relaxed);
        let tmp = self.chemin(&format!(
            "{FICHIER_IDENTITE_CLE}.clair-{}-{unique}",
            std::process::id()
        ));
        fs::write(&tmp, clair)
            .map_err(|e| format!("écriture temporaire de la clé impossible : {e}"))?;
        let paire = IdentityStore::load_or_create(&tmp)
            .map_err(|e| format!("identité cryptographique locale indisponible : {e}"));
        let _ = fs::remove_file(&tmp);
        paire
    }

    // --- 2. Carnet d'adresses ----------------------------------------------

    pub(crate) fn lister_contacts(&self) -> Result<Vec<AddressBookEntryDto>, String> {
        let _garde = self.verrouiller();
        Ok(self.lire_json::<CarnetStocke>(FICHIER_CARNET)?.contacts)
    }

    pub(crate) fn ajouter_contact(
        &self,
        alias: String,
        id: u64,
        groupe: String,
        etiquettes: Vec<String>,
    ) -> Result<AddressBookEntryDto, String> {
        let _garde = self.verrouiller();
        let mut carnet: CarnetStocke = self.lire_json(FICHIER_CARNET)?;
        if carnet.contacts.iter().any(|c| c.id == id) {
            return Err(format!("un contact avec l'ID {} existe déjà", NovaId(id)));
        }
        let contact = AddressBookEntryDto {
            id,
            alias,
            groupe: groupe.clone(),
            etiquettes,
            favori: false,
            derniere_connexion: None,
        };
        carnet.contacts.push(contact.clone());
        carnet.enregistrer_groupe(&groupe);
        self.ecrire_json(FICHIER_CARNET, &carnet)?;
        Ok(contact)
    }

    pub(crate) fn modifier_contact(
        &self,
        id: u64,
        alias: String,
        groupe: String,
        etiquettes: Vec<String>,
    ) -> Result<(), String> {
        let _garde = self.verrouiller();
        let mut carnet: CarnetStocke = self.lire_json(FICHIER_CARNET)?;
        let Some(contact) = carnet.contacts.iter_mut().find(|c| c.id == id) else {
            return Err(format!("aucun contact avec l'ID {}", NovaId(id)));
        };
        contact.alias = alias;
        contact.groupe = groupe.clone();
        contact.etiquettes = etiquettes;
        carnet.enregistrer_groupe(&groupe);
        self.ecrire_json(FICHIER_CARNET, &carnet)
    }

    pub(crate) fn supprimer_contact(&self, id: u64) -> Result<(), String> {
        let _garde = self.verrouiller();
        let mut carnet: CarnetStocke = self.lire_json(FICHIER_CARNET)?;
        let avant = carnet.contacts.len();
        carnet.contacts.retain(|c| c.id != id);
        if carnet.contacts.len() == avant {
            return Err(format!("aucun contact avec l'ID {}", NovaId(id)));
        }
        self.ecrire_json(FICHIER_CARNET, &carnet)
    }

    pub(crate) fn definir_favori(&self, id: u64, favori: bool) -> Result<(), String> {
        let _garde = self.verrouiller();
        let mut carnet: CarnetStocke = self.lire_json(FICHIER_CARNET)?;
        let Some(contact) = carnet.contacts.iter_mut().find(|c| c.id == id) else {
            return Err(format!("aucun contact avec l'ID {}", NovaId(id)));
        };
        contact.favori = favori;
        self.ecrire_json(FICHIER_CARNET, &carnet)
    }

    pub(crate) fn lister_groupes(&self) -> Result<Vec<String>, String> {
        let _garde = self.verrouiller();
        Ok(self.lire_json::<CarnetStocke>(FICHIER_CARNET)?.groupes)
    }

    pub(crate) fn ajouter_groupe(&self, nom: String) -> Result<(), String> {
        if nom.trim().is_empty() {
            return Err("nom de groupe vide".to_owned());
        }
        let _garde = self.verrouiller();
        let mut carnet: CarnetStocke = self.lire_json(FICHIER_CARNET)?;
        if carnet.groupes.iter().any(|g| g == &nom) {
            return Err(format!("le groupe « {nom} » existe déjà"));
        }
        carnet.groupes.push(nom);
        self.ecrire_json(FICHIER_CARNET, &carnet)
    }

    // --- 3. Réglages -------------------------------------------------------

    /// Réglages effectifs : valeurs par défaut fusionnées avec les surcharges
    /// persistées, triées par clé (ordre stable pour l'UI).
    fn fusion_reglages(&self) -> Result<BTreeMap<String, String>, String> {
        let mut fusion: BTreeMap<String, String> = reglages_par_defaut(&self.racine);
        let stockes: BTreeMap<String, String> = self.lire_json(FICHIER_REGLAGES)?;
        fusion.extend(stockes);
        Ok(fusion)
    }

    pub(crate) fn get_reglages(&self) -> Result<Vec<SettingDto>, String> {
        let _garde = self.verrouiller();
        Ok(self
            .fusion_reglages()?
            .into_iter()
            .map(|(cle, valeur)| SettingDto { cle, valeur })
            .collect())
    }

    /// Valeur effective d'un réglage (surcharge persistée sinon défaut, sinon
    /// `None` si la clé est inconnue).
    pub(crate) fn reglage(&self, cle: &str) -> Result<Option<String>, String> {
        let _garde = self.verrouiller();
        Ok(self.fusion_reglages()?.remove(cle))
    }

    pub(crate) fn definir_reglage(&self, cle: String, valeur: String) -> Result<(), String> {
        if cle.trim().is_empty() {
            return Err("clé de réglage vide".to_owned());
        }
        let _garde = self.verrouiller();
        let mut stockes: BTreeMap<String, String> = self.lire_json(FICHIER_REGLAGES)?;
        stockes.insert(cle, valeur);
        self.ecrire_json(FICHIER_REGLAGES, &stockes)
    }

    // --- 4. Historique de sessions -----------------------------------------

    /// Journalise le démarrage d'une session : entrée dédupliquée par ID et
    /// remontée en tête, historique borné à [`MAX_HISTORIQUE`]. Met à jour la
    /// `derniere_connexion` du contact correspondant, s'il existe.
    pub(crate) fn enregistrer_session(&self, id: u64, alias: String) -> Result<(), String> {
        let _garde = self.verrouiller();
        let horodatage = maintenant_unix();

        let mut historique: Vec<RecentSessionDto> = self.lire_json(FICHIER_HISTORIQUE)?;
        historique.retain(|s| s.id != id);
        historique.insert(
            0,
            RecentSessionDto {
                id,
                alias,
                timestamp: horodatage,
            },
        );
        historique.truncate(MAX_HISTORIQUE);
        self.ecrire_json(FICHIER_HISTORIQUE, &historique)?;

        // Répercute la dernière connexion sur le contact, le cas échéant.
        let mut carnet: CarnetStocke = self.lire_json(FICHIER_CARNET)?;
        if let Some(contact) = carnet.contacts.iter_mut().find(|c| c.id == id) {
            contact.derniere_connexion = Some(horodatage);
            self.ecrire_json(FICHIER_CARNET, &carnet)?;
        }
        Ok(())
    }

    pub(crate) fn sessions_recentes(&self) -> Result<Vec<RecentSessionDto>, String> {
        let _garde = self.verrouiller();
        self.lire_json(FICHIER_HISTORIQUE)
    }

    // --- 5. Enregistrements ------------------------------------------------

    /// Scanne le dossier d'enregistrement (`dir` s'il est fourni et non vide,
    /// sinon le réglage `dossier_enregistrement`, sinon `<données>/enregistrements`)
    /// et décrit chaque `.mp4`/`.ndr`. La durée d'un `.mp4` est lue via
    /// [`Mp4Reader`] ; à défaut (fichier non-MP4 ou illisible), elle vaut 0 et
    /// seules la taille et la date fichier sont renseignées. Dossier absent =
    /// liste vide.
    pub(crate) fn lister_enregistrements(
        &self,
        dir: Option<String>,
    ) -> Result<Vec<RecordingDto>, String> {
        let dossier = match dir {
            Some(d) if !d.trim().is_empty() => PathBuf::from(d),
            _ => self.dossier_enregistrement_effectif()?,
        };

        let lecture = match fs::read_dir(&dossier) {
            Ok(l) => l,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => {
                return Err(format!(
                    "lecture du dossier d'enregistrement « {} » impossible : {e}",
                    dossier.display()
                ))
            }
        };

        let mut sortie = Vec::new();
        for entree in lecture.flatten() {
            let chemin = entree.path();
            let extension = chemin
                .extension()
                .and_then(|e| e.to_str())
                .map(str::to_ascii_lowercase)
                .unwrap_or_default();
            if extension != "mp4" && extension != "ndr" {
                continue;
            }
            let Ok(meta) = fs::metadata(&chemin) else {
                continue;
            };
            if !meta.is_file() {
                continue;
            }
            let date = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_secs() as i64);
            sortie.push(RecordingDto {
                nom: chemin
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or_default()
                    .to_owned(),
                duree_s: duree_enregistrement(&chemin, &extension),
                taille_octets: meta.len(),
                date,
                chemin: chemin.to_string_lossy().into_owned(),
            });
        }
        // Les plus récents d'abord.
        sortie.sort_by(|a, b| b.date.cmp(&a.date));
        Ok(sortie)
    }

    /// Dossier d'enregistrement effectif : réglage `dossier_enregistrement` s'il
    /// est renseigné, sinon `<répertoire de données>/enregistrements`.
    fn dossier_enregistrement_effectif(&self) -> Result<PathBuf, String> {
        let _garde = self.verrouiller();
        let dossier = self
            .fusion_reglages()?
            .remove(CLE_DOSSIER_ENREGISTREMENT)
            .filter(|d| !d.trim().is_empty())
            .map_or_else(|| self.racine.join("enregistrements"), PathBuf::from);
        Ok(dossier)
    }

    // --- 6. Accès non surveillé -------------------------------------------

    /// Lit l'état non surveillé en **migrant de façon transparente** un éventuel
    /// mot de passe encore en clair (ancien format `mot_de_passe`) vers sa forme
    /// **protégée au repos** (DPAPI) : dès qu'un tel champ est détecté, l'état est
    /// ré-écrit chiffré. Appelée sous le verrou du magasin (les appelants le
    /// détiennent déjà).
    fn lire_non_surveille(&self) -> Result<NonSurveilleStocke, String> {
        let mut etat: NonSurveilleStocke = self.lire_json(FICHIER_NON_SURVEILLE)?;
        if etat.mot_de_passe.is_some() {
            let ancien = etat.mot_de_passe.take();
            etat.definir_mot_de_passe(ancien)?;
            self.ecrire_json(FICHIER_NON_SURVEILLE, &etat)?;
        }
        Ok(etat)
    }

    pub(crate) fn config_non_surveille(&self) -> Result<UnattendedConfigDto, String> {
        let _garde = self.verrouiller();
        let etat = self.lire_non_surveille()?;
        Ok(UnattendedConfigDto {
            a_mot_de_passe: etat.a_mot_de_passe(),
            appareils_de_confiance: etat.appareils,
        })
    }

    /// Définit (ou efface, si `pwd` est vide) le mot de passe permanent d'accès
    /// non surveillé. Seul un **hachage salé**, en outre **chiffré au repos**
    /// (DPAPI), est persisté — jamais le clair.
    pub(crate) fn definir_mot_de_passe_non_surveille(&self, pwd: String) -> Result<(), String> {
        let _garde = self.verrouiller();
        let mut etat = self.lire_non_surveille()?;
        let hache = if pwd.is_empty() {
            None
        } else {
            Some(MotDePasseHache::depuis_clair(&pwd))
        };
        etat.definir_mot_de_passe(hache)?;
        self.ecrire_json(FICHIER_NON_SURVEILLE, &etat)
    }

    /// Vérifie un mot de passe candidat contre le hachage persisté (déchiffré au
    /// vol) — `false` si aucun mot de passe n'est configuré.
    pub(crate) fn verifier_mot_de_passe_non_surveille(&self, pwd: String) -> Result<bool, String> {
        let _garde = self.verrouiller();
        let etat = self.lire_non_surveille()?;
        Ok(etat.mot_de_passe_hache()?.is_some_and(|h| h.verifier(&pwd)))
    }

    /// L'ID fait-il partie des **appareils de confiance** de l'accès non
    /// surveillé ? Consulté par le contrôle d'admission automatique
    /// ([`crate::flux::demarrer_hote_non_surveille`]) en **union** avec la liste
    /// blanche d'admission ([`Magasin::admission_contient`]) : toute erreur de
    /// lecture y vaut refus (fermé par défaut).
    pub(crate) fn appareil_de_confiance(&self, id: u64) -> Result<bool, String> {
        let _garde = self.verrouiller();
        let etat = self.lire_non_surveille()?;
        Ok(etat.appareils.contains(&id))
    }

    pub(crate) fn ajouter_appareil_confiance(&self, id: u64) -> Result<(), String> {
        let _garde = self.verrouiller();
        let mut etat = self.lire_non_surveille()?;
        if !etat.appareils.contains(&id) {
            etat.appareils.push(id);
        }
        self.ecrire_json(FICHIER_NON_SURVEILLE, &etat)
    }

    pub(crate) fn retirer_appareil_confiance(&self, id: u64) -> Result<(), String> {
        let _garde = self.verrouiller();
        let mut etat = self.lire_non_surveille()?;
        let avant = etat.appareils.len();
        etat.appareils.retain(|d| *d != id);
        if etat.appareils.len() == avant {
            return Err(format!(
                "l'appareil {} n'est pas dans la liste de confiance",
                NovaId(id)
            ));
        }
        self.ecrire_json(FICHIER_NON_SURVEILLE, &etat)
    }

    // --- 6bis. Liste blanche d'admission (ACL) -----------------------------
    //
    // Liste d'ID admis **sans mot de passe** en accès non surveillé, persistée
    // dans le même fichier que l'accès non surveillé. Elle est consultée par le
    // vérificateur d'admission en **union** avec les appareils de confiance
    // (`appareil_de_confiance`) : liste blanche ∪ appareils de confiance.

    /// Liste blanche d'admission (ordre d'insertion).
    pub(crate) fn admission_liste(&self) -> Result<Vec<u64>, String> {
        let _garde = self.verrouiller();
        Ok(self.lire_non_surveille()?.admission_autorisee)
    }

    /// Ajoute un ID à la liste blanche d'admission (sans effet s'il y figure déjà).
    pub(crate) fn admission_ajouter(&self, id: u64) -> Result<(), String> {
        let _garde = self.verrouiller();
        let mut etat = self.lire_non_surveille()?;
        if !etat.admission_autorisee.contains(&id) {
            etat.admission_autorisee.push(id);
        }
        self.ecrire_json(FICHIER_NON_SURVEILLE, &etat)
    }

    /// Retire un ID de la liste blanche d'admission. Erreur s'il n'y figure pas.
    pub(crate) fn admission_retirer(&self, id: u64) -> Result<(), String> {
        let _garde = self.verrouiller();
        let mut etat = self.lire_non_surveille()?;
        let avant = etat.admission_autorisee.len();
        etat.admission_autorisee.retain(|x| *x != id);
        if etat.admission_autorisee.len() == avant {
            return Err(format!(
                "l'appareil {} n'est pas dans la liste blanche d'admission",
                NovaId(id)
            ));
        }
        self.ecrire_json(FICHIER_NON_SURVEILLE, &etat)
    }

    /// L'ID figure-t-il dans la **liste blanche d'admission** ? Consulté par le
    /// vérificateur d'admission ([`crate::flux::demarrer_hote_non_surveille`]) en
    /// union avec les appareils de confiance : un ID de l'une **ou** l'autre liste
    /// est admis sans mot de passe. Toute erreur de lecture y vaut refus.
    pub(crate) fn admission_contient(&self, id: u64) -> Result<bool, String> {
        let _garde = self.verrouiller();
        Ok(self.lire_non_surveille()?.admission_autorisee.contains(&id))
    }

    /// Ajoute une entrée au **journal des accès** (append), borné à
    /// [`MAX_JOURNAL_ACCES`].
    pub(crate) fn enregistrer_acces(&self, peer_id: u64, accepte: bool) -> Result<(), String> {
        let _garde = self.verrouiller();
        let mut etat = self.lire_non_surveille()?;
        etat.journal.push(EntreeJournal {
            peer_id,
            timestamp: maintenant_unix(),
            accepte,
        });
        let surplus = etat.journal.len().saturating_sub(MAX_JOURNAL_ACCES);
        if surplus > 0 {
            etat.journal.drain(0..surplus);
        }
        self.ecrire_json(FICHIER_NON_SURVEILLE, &etat)
    }

    /// Journal des accès, du plus récent au plus ancien.
    pub(crate) fn journal_acces(&self) -> Result<Vec<AccessLogEntryDto>, String> {
        let _garde = self.verrouiller();
        let etat = self.lire_non_surveille()?;
        Ok(etat
            .journal
            .into_iter()
            .rev()
            .map(|e| AccessLogEntryDto {
                peer_id: e.peer_id,
                peer_id_formate: NovaId(e.peer_id).to_string(),
                timestamp: e.timestamp,
                accepte: e.accepte,
            })
            .collect())
    }
}

// ---------------------------------------------------------------------------
// Structures persistées (format sur disque, découplé des DTO d'affichage)
// ---------------------------------------------------------------------------

/// Identité locale persistée : le `NovaId` à 9 chiffres (la paire de clés vit
/// dans un fichier texte à part, géré par [`IdentityStore`]).
#[derive(Default, Serialize, Deserialize)]
struct IdentiteStockee {
    #[serde(default)]
    id: u64,
}

/// Carnet d'adresses persisté : contacts + groupes déclarés.
#[derive(Default, Serialize, Deserialize)]
struct CarnetStocke {
    #[serde(default)]
    contacts: Vec<AddressBookEntryDto>,
    #[serde(default)]
    groupes: Vec<String>,
}

impl CarnetStocke {
    /// Enregistre un groupe non vide encore inconnu (idempotent).
    fn enregistrer_groupe(&mut self, groupe: &str) {
        if !groupe.trim().is_empty() && !self.groupes.iter().any(|g| g == groupe) {
            self.groupes.push(groupe.to_owned());
        }
    }
}

/// Configuration d'accès non surveillé persistée.
#[derive(Default, Serialize, Deserialize)]
struct NonSurveilleStocke {
    /// **Ancien** champ : haché du mot de passe écrit en clair sur disque.
    /// Conservé en lecture pour la **migration transparente** vers
    /// `mot_de_passe_protege` ; jamais ré-écrit (voir [`Magasin::lire_non_surveille`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mot_de_passe: Option<MotDePasseHache>,
    /// Haché du mot de passe **protégé au repos** (DPAPI sous Windows) :
    /// hexadécimal du blob chiffré enveloppant le JSON de [`MotDePasseHache`].
    /// Format courant — le clair du hachage n'est plus écrit tel quel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mot_de_passe_protege: Option<String>,
    #[serde(default)]
    appareils: Vec<u64>,
    #[serde(default)]
    journal: Vec<EntreeJournal>,
    /// Liste blanche d'admission (ACL) : ID admis **sans mot de passe** en accès
    /// non surveillé, au même titre que les appareils de confiance (union à
    /// l'admission — voir [`crate::flux::demarrer_hote_non_surveille`]).
    #[serde(default)]
    admission_autorisee: Vec<u64>,
}

impl NonSurveilleStocke {
    /// Un mot de passe permanent est-il configuré (forme protégée ou ancien clair) ?
    fn a_mot_de_passe(&self) -> bool {
        self.mot_de_passe_protege.is_some() || self.mot_de_passe.is_some()
    }

    /// Définit (ou efface avec `None`) le haché du mot de passe en le **protégeant
    /// au repos** (DPAPI). Vide toujours l'ancien champ en clair : un secret non
    /// protégé ne subsiste jamais.
    fn definir_mot_de_passe(&mut self, hache: Option<MotDePasseHache>) -> Result<(), String> {
        self.mot_de_passe = None;
        self.mot_de_passe_protege = match hache {
            Some(h) => {
                let clair = serde_json::to_vec(&h)
                    .map_err(|e| format!("sérialisation du mot de passe impossible : {e}"))?;
                let protege = crate::plateforme::proteger(&clair)?;
                Some(hex_minuscule(&protege))
            }
            None => None,
        };
        Ok(())
    }

    /// Renvoie le haché du mot de passe : depuis le champ **protégé** (déchiffré à
    /// la volée) en priorité, sinon depuis l'ancien champ en clair (avant
    /// migration), sinon `None`.
    fn mot_de_passe_hache(&self) -> Result<Option<MotDePasseHache>, String> {
        if let Some(hex) = &self.mot_de_passe_protege {
            let protege = decoder_hex(hex).ok_or_else(|| {
                "mot de passe protégé illisible (hexadécimal mal formé)".to_owned()
            })?;
            let clair = crate::plateforme::deproteger(&protege)?;
            let hache = serde_json::from_slice(&clair)
                .map_err(|e| format!("mot de passe protégé illisible : {e}"))?;
            Ok(Some(hache))
        } else {
            Ok(self.mot_de_passe.clone())
        }
    }
}

/// Mot de passe permanent haché : sel aléatoire + `BLAKE3(sel || mot de passe)`,
/// tout deux en hexadécimal. Le clair n'est jamais stocké ; ce hachage est en
/// outre chiffré au repos (DPAPI) via [`NonSurveilleStocke::definir_mot_de_passe`].
#[derive(Clone, Serialize, Deserialize)]
struct MotDePasseHache {
    sel: String,
    empreinte: String,
}

impl MotDePasseHache {
    fn depuis_clair(pwd: &str) -> Self {
        let sel = octets_aleatoires(16);
        MotDePasseHache {
            empreinte: empreinte_mot_de_passe(&sel, pwd),
            sel: hex_minuscule(&sel),
        }
    }

    fn verifier(&self, pwd: &str) -> bool {
        match decoder_hex(&self.sel) {
            Some(sel) => empreinte_mot_de_passe(&sel, pwd) == self.empreinte,
            None => false,
        }
    }
}

/// Une entrée du journal des accès (format disque : sans champ d'affichage).
#[derive(Serialize, Deserialize)]
struct EntreeJournal {
    peer_id: u64,
    timestamp: i64,
    accepte: bool,
}

// ---------------------------------------------------------------------------
// Réglages par défaut
// ---------------------------------------------------------------------------

/// Valeurs par défaut raisonnables des réglages (surchargées par le fichier).
fn reglages_par_defaut(racine: &Path) -> BTreeMap<String, String> {
    let dossier_enregistrement = racine
        .join("enregistrements")
        .to_string_lossy()
        .into_owned();
    [
        ("serveur_rendezvous", String::new()),
        ("serveur_relais", String::new()),
        ("serveurs_stun", "stun.l.google.com:19302".to_owned()),
        ("prereglage_qualite", "auto".to_owned()),
        (CLE_DOSSIER_ENREGISTREMENT, dossier_enregistrement),
        ("theme", "systeme".to_owned()),
        ("langue", "fr".to_owned()),
        (CLE_DEMARRER_AVEC_SYSTEME, "false".to_owned()),
    ]
    .into_iter()
    .map(|(cle, valeur)| (cle.to_owned(), valeur))
    .collect()
}

// ---------------------------------------------------------------------------
// Mot de passe éphémère
// ---------------------------------------------------------------------------

/// Génère un mot de passe éphémère lisible (10 caractères d'un alphabet sans
/// symboles ambigus). Entropie tirée du CSPRNG via [`octets_aleatoires`].
pub(crate) fn generer_mot_de_passe_ephemere() -> String {
    // Alphabet sans caractères prêtant à confusion (pas de 0/O/o, 1/l/I).
    const ALPHABET: &[u8] = b"abcdefghijkmnpqrstuvwxyzACDEFGHJKLMNPQRSTUVWXYZ23456789";
    octets_aleatoires(LONGUEUR_MDP_EPHEMERE)
        .into_iter()
        .map(|octet| ALPHABET[usize::from(octet) % ALPHABET.len()] as char)
        .collect()
}

// ---------------------------------------------------------------------------
// Aides : aléa, hachage, hexadécimal, horodatage, dérivation d'ID
// ---------------------------------------------------------------------------

/// Renvoie `n` octets aléatoires. Source primaire : le CSPRNG de `snow` (via
/// `nd_crypto::generate_static_keypair`) ; repli déterministe (horloge + PID +
/// compteur, diffusé par splitmix64) si la génération de clés échouait.
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

/// Graine de repli : horloge nanoseconde ⊕ PID ⊕ compteur, diffusée.
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

/// Empreinte hexadécimale `BLAKE3(sel || mot de passe)`.
fn empreinte_mot_de_passe(sel: &[u8], pwd: &str) -> String {
    let mut donnees = sel.to_vec();
    donnees.extend_from_slice(pwd.as_bytes());
    blake3::hash(&donnees).to_hex().to_string()
}

/// Dérive un `NovaId` à 9 chiffres (`100 000 000`–`999 999 999`) des 8 premiers
/// octets de l'empreinte de la clé publique.
fn nova_id_depuis_empreinte(empreinte: &[u8; 32]) -> u64 {
    let mut tampon = [0u8; 8];
    tampon.copy_from_slice(&empreinte[..8]);
    100_000_000 + u64::from_be_bytes(tampon) % 900_000_000
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

/// Horodatage Unix courant en secondes (0 si l'horloge précède l'époque).
fn maintenant_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// Durée d'un enregistrement en secondes : lue via [`Mp4Reader`] pour un `.mp4`,
/// `0.0` sinon (fichier non-MP4, ou MP4 illisible).
fn duree_enregistrement(chemin: &Path, extension: &str) -> f64 {
    if extension != "mp4" {
        return 0.0;
    }
    let Ok(fichier) = File::open(chemin) else {
        return 0.0;
    };
    Mp4Reader::new(fichier).map_or(0.0, |lecteur| lecteur.duration_us() as f64 / 1_000_000.0)
}

// ---------------------------------------------------------------------------
// Tests : allers-retours disque, réouverture depuis fichier temporaire unique
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Sérialise les tests qui modifient l'environnement global du processus
    /// (`std::env::set_var`), afin qu'ils ne se marchent pas dessus.
    static VERROU_ENV: Mutex<()> = Mutex::new(());

    /// Magasin neuf enraciné dans un répertoire temporaire unique + sa poignée
    /// (le répertoire est supprimé quand la poignée est lâchée).
    fn magasin_temporaire() -> (Magasin, TempDir) {
        let dir = tempfile::tempdir().expect("répertoire temporaire");
        (Magasin::nouveau(dir.path().to_path_buf()), dir)
    }

    #[test]
    fn identite_stable_et_persistante_apres_reouverture() {
        let (mag, dir) = magasin_temporaire();
        let a = mag.identite_locale().expect("identité");
        let b = mag.identite_locale().expect("identité rechargée");
        assert_eq!(a, b, "l'identité doit être stable dans le même magasin");
        assert_eq!(a.id_formate, NovaId(a.id).to_string());
        // 9 chiffres exactement (100 000 000–999 999 999).
        assert!((100_000_000..=999_999_999).contains(&a.id));
        assert_eq!(a.empreinte.len(), 64, "empreinte = 32 octets en hexa");

        // Réouverture depuis le disque : mêmes valeurs (rechargées, pas régénérées).
        let mag2 = Magasin::nouveau(dir.path().to_path_buf());
        assert_eq!(mag2.identite_locale().expect("réouverture"), a);
    }

    #[test]
    fn carnet_allers_retours_et_persistance() {
        let (mag, dir) = magasin_temporaire();
        assert!(mag.lister_contacts().expect("vide").is_empty());

        let contact = mag
            .ajouter_contact(
                "Bureau".to_owned(),
                123_456_789,
                "Travail".to_owned(),
                vec!["prod".to_owned()],
            )
            .expect("ajout");
        assert_eq!(contact.id, 123_456_789);
        assert!(!contact.favori);
        assert_eq!(contact.derniere_connexion, None);

        // Doublon refusé, groupe auto-enregistré.
        assert!(mag
            .ajouter_contact("X".to_owned(), 123_456_789, "Travail".to_owned(), vec![])
            .is_err());
        assert_eq!(mag.lister_groupes().expect("groupes"), vec!["Travail"]);

        mag.definir_favori(123_456_789, true).expect("favori");
        mag.modifier_contact(
            123_456_789,
            "Bureau principal".to_owned(),
            "Maison".to_owned(),
            vec!["a".to_owned(), "b".to_owned()],
        )
        .expect("modif");

        // Réouverture depuis le disque : modifications persistées.
        let mag2 = Magasin::nouveau(dir.path().to_path_buf());
        let contacts = mag2.lister_contacts().expect("relecture");
        assert_eq!(contacts.len(), 1);
        assert!(contacts[0].favori);
        assert_eq!(contacts[0].alias, "Bureau principal");
        assert_eq!(contacts[0].groupe, "Maison");
        assert_eq!(contacts[0].etiquettes, vec!["a", "b"]);
        // Les deux groupes vus (Travail à l'ajout, Maison à la modif) subsistent.
        assert!(mag2
            .lister_groupes()
            .expect("groupes")
            .contains(&"Maison".to_owned()));

        mag2.supprimer_contact(123_456_789).expect("suppression");
        assert!(mag2.lister_contacts().expect("vide").is_empty());
        assert!(mag2.supprimer_contact(123_456_789).is_err());
    }

    #[test]
    fn groupes_explicites_et_doublon_refuse() {
        let (mag, _dir) = magasin_temporaire();
        mag.ajouter_groupe("Clients".to_owned()).expect("ajout");
        assert!(mag.ajouter_groupe("Clients".to_owned()).is_err());
        assert!(mag.ajouter_groupe("   ".to_owned()).is_err());
        assert_eq!(mag.lister_groupes().expect("groupes"), vec!["Clients"]);
    }

    #[test]
    fn reglages_defauts_surcharge_et_persistance() {
        let (mag, dir) = magasin_temporaire();
        let defauts = mag.get_reglages().expect("défauts");
        assert!(defauts
            .iter()
            .any(|s| s.cle == "theme" && s.valeur == "systeme"));
        assert!(defauts
            .iter()
            .any(|s| s.cle == "langue" && s.valeur == "fr"));
        // Triés par clé (ordre stable).
        let cles: Vec<&str> = defauts.iter().map(|s| s.cle.as_str()).collect();
        let mut triees = cles.clone();
        triees.sort_unstable();
        assert_eq!(cles, triees);

        mag.definir_reglage("theme".to_owned(), "sombre".to_owned())
            .expect("surcharge");
        mag.definir_reglage("nouvelle_cle".to_owned(), "42".to_owned())
            .expect("clé neuve");
        assert!(mag.definir_reglage(String::new(), "x".to_owned()).is_err());

        let mag2 = Magasin::nouveau(dir.path().to_path_buf());
        assert_eq!(
            mag2.reglage("theme").expect("lu"),
            Some("sombre".to_owned())
        );
        assert_eq!(
            mag2.reglage("nouvelle_cle").expect("lu"),
            Some("42".to_owned())
        );
        assert_eq!(mag2.reglage("inconnue").expect("lu"), None);
        // Un défaut non surchargé reste présent.
        assert_eq!(mag2.reglage("langue").expect("lu"), Some("fr".to_owned()));
    }

    #[test]
    fn historique_dedup_borne_et_maj_contact() {
        let (mag, _dir) = magasin_temporaire();
        mag.ajouter_contact("Poste".to_owned(), 7, "G".to_owned(), vec![])
            .expect("contact");

        mag.enregistrer_session(7, "Poste".to_owned()).expect("s1");
        mag.enregistrer_session(8, "Autre".to_owned()).expect("s2");
        mag.enregistrer_session(7, "Poste".to_owned())
            .expect("s1 bis");

        let recentes = mag.sessions_recentes().expect("récentes");
        // Dédup par ID : 7 (le plus récent) puis 8.
        assert_eq!(recentes.len(), 2);
        assert_eq!(recentes[0].id, 7);
        assert_eq!(recentes[1].id, 8);

        // La dernière connexion du contact 7 a été renseignée.
        let contact = mag
            .lister_contacts()
            .expect("contacts")
            .into_iter()
            .find(|c| c.id == 7)
            .expect("contact 7");
        assert!(contact.derniere_connexion.is_some());

        // Borne : au-delà de MAX_HISTORIQUE entrées distinctes.
        for id in 0..(MAX_HISTORIQUE as u64 + 10) {
            mag.enregistrer_session(1_000 + id, "x".to_owned())
                .expect("masse");
        }
        assert_eq!(
            mag.sessions_recentes().expect("bornée").len(),
            MAX_HISTORIQUE
        );
    }

    #[test]
    fn enregistrements_scan_extensions_taille_et_tri() {
        let (mag, _dir) = magasin_temporaire();
        let dossier = tempfile::tempdir().expect("dossier enregistrements");
        std::fs::write(dossier.path().join("a.mp4"), b"pas-un-vrai-mp4").expect("a");
        std::fs::write(dossier.path().join("b.ndr"), b"12345").expect("b");
        std::fs::write(dossier.path().join("notes.txt"), b"ignore").expect("txt");

        let liste = mag
            .lister_enregistrements(Some(dossier.path().to_string_lossy().into_owned()))
            .expect("scan");
        // Seuls .mp4 et .ndr sont retenus.
        assert_eq!(liste.len(), 2);
        let noms: Vec<&str> = liste.iter().map(|r| r.nom.as_str()).collect();
        assert!(noms.contains(&"a.mp4") && noms.contains(&"b.ndr"));
        // MP4 factice illisible : durée 0, mais taille fichier renseignée.
        let mp4 = liste.iter().find(|r| r.nom == "a.mp4").expect("mp4");
        assert_eq!(mp4.duree_s, 0.0);
        assert_eq!(mp4.taille_octets, "pas-un-vrai-mp4".len() as u64);

        // Dossier inexistant : liste vide, pas d'erreur.
        assert!(mag
            .lister_enregistrements(Some(
                dossier.path().join("absent").to_string_lossy().into_owned()
            ))
            .expect("absent")
            .is_empty());
    }

    #[test]
    fn non_surveille_mot_de_passe_hache_et_verifie() {
        let (mag, dir) = magasin_temporaire();
        assert!(!mag.config_non_surveille().expect("config").a_mot_de_passe);

        mag.definir_mot_de_passe_non_surveille("s3cr3t-perm".to_owned())
            .expect("mdp");
        assert!(mag.config_non_surveille().expect("config").a_mot_de_passe);
        assert!(mag
            .verifier_mot_de_passe_non_surveille("s3cr3t-perm".to_owned())
            .expect("bon"));
        assert!(!mag
            .verifier_mot_de_passe_non_surveille("mauvais".to_owned())
            .expect("mauvais"));

        // Le clair n'apparaît nulle part sur le disque.
        let brut = std::fs::read_to_string(dir.path().join(FICHIER_NON_SURVEILLE)).expect("json");
        assert!(
            !brut.contains("s3cr3t-perm"),
            "mot de passe stocké en clair !"
        );

        // Persistance de la vérification après réouverture.
        let mag2 = Magasin::nouveau(dir.path().to_path_buf());
        assert!(mag2
            .verifier_mot_de_passe_non_surveille("s3cr3t-perm".to_owned())
            .expect("réouv"));

        // Effacement (mot de passe vide).
        mag2.definir_mot_de_passe_non_surveille(String::new())
            .expect("effacement");
        assert!(!mag2.config_non_surveille().expect("config").a_mot_de_passe);
    }

    #[test]
    fn identite_chiffree_au_repos_des_le_premier_lancement() {
        let (mag, dir) = magasin_temporaire();
        let ident = mag.identite_locale().expect("identité");

        // Le fichier de clé est chiffré au repos (préfixe dédié), pas en clair.
        let brut = std::fs::read(dir.path().join(FICHIER_IDENTITE_CLE)).expect("clé");
        assert!(
            brut.starts_with(ENTETE_CLE_CHIFFREE),
            "la clé d'identité doit être chiffrée au repos dès la création"
        );

        // Réouverture depuis le fichier chiffré : identité stable (déchiffrée).
        let mag2 = Magasin::nouveau(dir.path().to_path_buf());
        assert_eq!(mag2.identite_locale().expect("réouverture chiffrée"), ident);
    }

    #[test]
    fn identite_migre_ancien_fichier_clair_vers_chiffre() {
        let (mag, dir) = magasin_temporaire();
        std::fs::create_dir_all(dir.path()).expect("dir");

        // Écrit un ANCIEN fichier d'identité EN CLAIR (3 lignes d'`IdentityStore`).
        let paire = nd_crypto::generate_static_keypair().expect("paire");
        let priv_hex = hex_minuscule(&paire.private);
        let contenu = format!(
            "novadesk-identite v1\n{priv_hex}\n{}\n",
            hex_minuscule(&paire.public)
        );
        let chemin = dir.path().join(FICHIER_IDENTITE_CLE);
        std::fs::write(&chemin, contenu.as_bytes()).expect("écrit clair");

        // Première lecture : migre (chiffre au repos) SANS changer l'identité.
        let ident1 = mag.identite_locale().expect("identité migrée");

        // Le fichier n'est plus l'ancien clair : préfixe chiffré présent.
        let brut = std::fs::read(&chemin).expect("relecture");
        assert!(
            brut.starts_with(ENTETE_CLE_CHIFFREE),
            "l'ancien fichier en clair doit avoir été migré (chiffré au repos)"
        );
        // Sous Windows (DPAPI réel), le hex de la clé privée n'apparaît plus en
        // clair. Hors Windows, le repli identité le conserve (documenté) : pas
        // d'assertion.
        #[cfg(windows)]
        {
            let aiguille = priv_hex.as_bytes();
            let present = brut.windows(aiguille.len()).any(|f| f == aiguille);
            assert!(!present, "la clé privée ne doit plus figurer en clair");
        }

        // Réouverture depuis le disque (désormais chiffré) : identité inchangée.
        let mag2 = Magasin::nouveau(dir.path().to_path_buf());
        let ident2 = mag2.identite_locale().expect("réouverture");
        assert_eq!(
            ident1, ident2,
            "la migration ne doit pas changer l'identité"
        );
    }

    #[test]
    fn mot_de_passe_non_surveille_migre_ancien_clair_vers_protege() {
        let (mag, dir) = magasin_temporaire();
        std::fs::create_dir_all(dir.path()).expect("dir");

        // ANCIEN format : haché du mot de passe écrit EN CLAIR dans le JSON.
        let ancien = NonSurveilleStocke {
            mot_de_passe: Some(MotDePasseHache::depuis_clair("motdepasse-perm")),
            ..NonSurveilleStocke::default()
        };
        std::fs::write(
            dir.path().join(FICHIER_NON_SURVEILLE),
            serde_json::to_vec_pretty(&ancien).expect("json ancien"),
        )
        .expect("écrit ancien");

        // Lecture : migre vers le champ protégé et vérifie toujours le mot de passe.
        assert!(mag
            .verifier_mot_de_passe_non_surveille("motdepasse-perm".to_owned())
            .expect("vérif après migration"));

        // Le fichier a été ré-écrit au format protégé (plus d'ancien champ clair).
        let brut =
            std::fs::read_to_string(dir.path().join(FICHIER_NON_SURVEILLE)).expect("relecture");
        assert!(
            brut.contains("mot_de_passe_protege"),
            "le mot de passe doit être migré vers le champ protégé"
        );
        assert!(
            !brut.contains("\"mot_de_passe\":"),
            "l'ancien champ en clair ne doit plus subsister après migration"
        );

        // Réouverture : toujours vérifiable, configuration cohérente.
        let mag2 = Magasin::nouveau(dir.path().to_path_buf());
        assert!(mag2.config_non_surveille().expect("config").a_mot_de_passe);
        assert!(mag2
            .verifier_mot_de_passe_non_surveille("motdepasse-perm".to_owned())
            .expect("vérif après réouverture"));
    }

    #[test]
    fn liste_blanche_admission_persistee_et_consultee() {
        let (mag, dir) = magasin_temporaire();
        assert!(mag.admission_liste().expect("vide").is_empty());
        assert!(!mag.admission_contient(42).expect("absent"));

        mag.admission_ajouter(42).expect("ajout");
        mag.admission_ajouter(42).expect("doublon ignoré");
        mag.admission_ajouter(43).expect("ajout2");
        assert_eq!(mag.admission_liste().expect("liste"), vec![42, 43]);
        assert!(mag.admission_contient(42).expect("membre"));
        assert!(!mag.admission_contient(999).expect("non membre"));

        // Persistée après réouverture depuis le disque.
        let mag2 = Magasin::nouveau(dir.path().to_path_buf());
        assert!(mag2.admission_contient(43).expect("réouverture"));
        mag2.admission_retirer(42).expect("retrait");
        assert!(mag2.admission_retirer(999).is_err());
        assert_eq!(mag2.admission_liste().expect("liste"), vec![43]);
    }

    #[test]
    fn admission_liste_blanche_distincte_des_appareils_de_confiance() {
        // Liste blanche d'admission et appareils de confiance : deux listes
        // distinctes que l'admission réunit (l'union est faite côté `flux`). Ici,
        // on vérifie leur indépendance de stockage et de consultation.
        let (mag, _dir) = magasin_temporaire();
        mag.admission_ajouter(7).expect("liste blanche");
        mag.ajouter_appareil_confiance(9).expect("confiance");

        assert_eq!(mag.admission_liste().expect("liste blanche"), vec![7]);
        assert_eq!(
            mag.config_non_surveille()
                .expect("config")
                .appareils_de_confiance,
            vec![9]
        );
        assert!(mag.admission_contient(7).expect("membre liste blanche"));
        assert!(!mag
            .admission_contient(9)
            .expect("confiance ∉ liste blanche"));
        assert!(mag.appareil_de_confiance(9).expect("membre confiance"));
        assert!(!mag
            .appareil_de_confiance(7)
            .expect("liste blanche ∉ confiance"));
    }

    #[test]
    fn appareils_de_confiance_et_journal_acces() {
        let (mag, _dir) = magasin_temporaire();
        // Liste vide (premier lancement) : personne n'est de confiance.
        assert!(!mag.appareil_de_confiance(111).expect("liste vide"));
        mag.ajouter_appareil_confiance(111).expect("ajout");
        mag.ajouter_appareil_confiance(111).expect("doublon ignoré");
        mag.ajouter_appareil_confiance(222).expect("ajout");
        assert_eq!(
            mag.config_non_surveille()
                .expect("config")
                .appareils_de_confiance,
            vec![111, 222]
        );
        // Test d'appartenance : celui du contrôle d'admission automatique.
        assert!(mag.appareil_de_confiance(111).expect("membre"));
        assert!(!mag.appareil_de_confiance(999).expect("non membre"));
        mag.retirer_appareil_confiance(111).expect("retrait");
        assert!(mag.retirer_appareil_confiance(999).is_err());
        assert!(!mag.appareil_de_confiance(111).expect("retiré"));
        assert_eq!(
            mag.config_non_surveille()
                .expect("config")
                .appareils_de_confiance,
            vec![222]
        );

        mag.enregistrer_acces(111, true).expect("acces 1");
        mag.enregistrer_acces(222, false).expect("acces 2");
        let journal = mag.journal_acces().expect("journal");
        // Plus récent d'abord.
        assert_eq!(journal.len(), 2);
        assert_eq!(journal[0].peer_id, 222);
        assert!(!journal[0].accepte);
        assert_eq!(journal[0].peer_id_formate, NovaId(222).to_string());
        assert_eq!(journal[1].peer_id, 111);
        assert!(journal[1].accepte);
    }

    #[test]
    fn journal_acces_borne() {
        let (mag, _dir) = magasin_temporaire();
        for i in 0..(MAX_JOURNAL_ACCES as u64 + 25) {
            mag.enregistrer_acces(i, true).expect("acces");
        }
        assert_eq!(
            mag.journal_acces().expect("journal").len(),
            MAX_JOURNAL_ACCES
        );
    }

    #[test]
    fn json_corrompu_remonte_une_erreur() {
        let (mag, dir) = magasin_temporaire();
        std::fs::create_dir_all(dir.path()).expect("dir");
        std::fs::write(dir.path().join(FICHIER_CARNET), b"{ ceci n'est pas du json")
            .expect("écrit");
        assert!(mag.lister_contacts().is_err());
    }

    #[test]
    fn mot_de_passe_ephemere_lisible_et_variable() {
        let mdp = generer_mot_de_passe_ephemere();
        assert_eq!(mdp.len(), LONGUEUR_MDP_EPHEMERE);
        assert!(mdp.chars().all(|c| c.is_ascii_alphanumeric()));
        // Sans caractères ambigus.
        assert!(!mdp.contains(['0', 'O', 'o', '1', 'l', 'I']));
        // Deux générations successives diffèrent (collision quasi impossible).
        assert_ne!(mdp, generer_mot_de_passe_ephemere());
    }

    #[test]
    fn hex_aller_retour() {
        let octets = [0x00, 0x0f, 0xa5, 0xff];
        assert_eq!(hex_minuscule(&octets), "000fa5ff");
        assert_eq!(decoder_hex("000fa5ff"), Some(octets.to_vec()));
        assert_eq!(decoder_hex("zz"), None);
        assert_eq!(decoder_hex("abc"), None);
    }

    #[test]
    fn data_dir_surcharge_par_variable_environnement() {
        // `std::env::set_var` est un état global du processus : on sérialise les
        // tests qui y touchent (ici, le seul).
        let _verrou = VERROU_ENV.lock().unwrap_or_else(PoisonError::into_inner);

        let dir_a = tempfile::tempdir().expect("répertoire A");
        let dir_b = tempfile::tempdir().expect("répertoire B");

        // Résout le répertoire de données avec NOVADESK_DATA_DIR positionnée puis
        // efface aussitôt la variable (fenêtre d'exposition globale minimale).
        let resoudre_avec = |valeur: &Path| -> PathBuf {
            std::env::set_var("NOVADESK_DATA_DIR", valeur);
            let racine = repertoire_donnees();
            std::env::remove_var("NOVADESK_DATA_DIR");
            racine
        };

        // La variable remplace tout : le chemin est pris tel quel.
        let racine_a = resoudre_avec(dir_a.path());
        assert_eq!(racine_a.as_path(), dir_a.path());
        let racine_b = resoudre_avec(dir_b.path());
        assert_eq!(racine_b.as_path(), dir_b.path());

        // Chaque répertoire porte sa propre identité, stable et persistante
        // (rechargée à l'identique par un second magasin sur la même racine).
        let identite_a = Magasin::nouveau(racine_a.clone())
            .identite_locale()
            .expect("identité A");
        let identite_a_relue = Magasin::nouveau(racine_a)
            .identite_locale()
            .expect("identité A relue");
        assert_eq!(identite_a, identite_a_relue, "identité A persistante");

        let identite_b = Magasin::nouveau(racine_b.clone())
            .identite_locale()
            .expect("identité B");
        let identite_b_relue = Magasin::nouveau(racine_b)
            .identite_locale()
            .expect("identité B relue");
        assert_eq!(identite_b, identite_b_relue, "identité B persistante");

        // Deux NOVADESK_DATA_DIR distincts ⇒ deux ID distincts (identités séparées).
        assert_ne!(
            identite_a.id, identite_b.id,
            "deux répertoires distincts doivent donner deux ID distincts"
        );
    }
}
