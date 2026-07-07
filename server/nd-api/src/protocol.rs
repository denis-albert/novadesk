//! Protocole TCP de l'API applicative — trames à préfixe de longueur `u32` BE.
//!
//! Même format de trame que `nd-signaling` : longueur `u32` gros-boutiste puis
//! charge utile ; chaque connexion porte **une** requête et **une** réponse
//! (voir [`crate::services`]). La charge utile commence par un octet de tag
//! (voir [`Request`] et [`Response`]), suivi des champs dans l'ordre de
//! déclaration. Encodages élémentaires :
//!
//! - entiers : gros-boutiste (`to_be_bytes`) ;
//! - booléens : un octet (0 = faux, 1 = vrai) ;
//! - chaînes : longueur `u32` BE + octets UTF-8 ;
//! - listes : compteur `u32` BE + éléments ;
//! - options : fanion `u8` (0 = absente, 1 = présente + valeur) ;
//! - rôles, permissions, canaux : un octet de code (voir `code_role` & co).

use std::io::{Read, Write};

use crate::groups::Group;
use crate::rbac::{Permission, Role};
use crate::sharing::Beneficiaire;
use crate::update::{ReleaseChannel, UpdateDecision, UpdateManifest, Version};
use crate::Contact;

/// Taille maximale d'une trame acceptée (protège d'une longueur hostile).
const TRAME_MAX: usize = 1 << 16;

// ---------------------------------------------------------------------------
// Trames
// ---------------------------------------------------------------------------

/// Écrit une trame : préfixe de longueur (`u32` BE) + charge utile.
///
/// # Errors
/// Propage les erreurs d'écriture du flux.
pub fn write_frame<W: Write>(w: &mut W, payload: &[u8]) -> std::io::Result<()> {
    w.write_all(&(payload.len() as u32).to_be_bytes())?;
    w.write_all(payload)
}

/// Lit une trame : préfixe de longueur (`u32` BE) + charge utile.
///
/// # Errors
/// `InvalidData` si la trame annoncée dépasse [`TRAME_MAX`], sinon propage les
/// erreurs de lecture du flux.
pub fn read_frame<R: Read>(r: &mut R) -> std::io::Result<Vec<u8>> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let len = u32::from_be_bytes(len) as usize;
    if len > TRAME_MAX {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "trame trop grande",
        ));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

// ---------------------------------------------------------------------------
// Encodage / décodage élémentaires
// ---------------------------------------------------------------------------

fn put_bytes(out: &mut Vec<u8>, b: &[u8]) {
    out.extend_from_slice(&(b.len() as u32).to_be_bytes());
    out.extend_from_slice(b);
}

fn read_u8(d: &[u8], p: &mut usize) -> Option<u8> {
    let v = *d.get(*p)?;
    *p += 1;
    Some(v)
}

fn read_u32(d: &[u8], p: &mut usize) -> Option<u32> {
    let v = u32::from_be_bytes(d.get(*p..*p + 4)?.try_into().ok()?);
    *p += 4;
    Some(v)
}

fn read_u64(d: &[u8], p: &mut usize) -> Option<u64> {
    let v = u64::from_be_bytes(d.get(*p..*p + 8)?.try_into().ok()?);
    *p += 8;
    Some(v)
}

fn read_string(d: &[u8], p: &mut usize) -> Option<String> {
    let len = read_u32(d, p)? as usize;
    let s = String::from_utf8(d.get(*p..*p + len)?.to_vec()).ok()?;
    *p += len;
    Some(s)
}

fn read_bool(d: &[u8], p: &mut usize) -> Option<bool> {
    match read_u8(d, p)? {
        0 => Some(false),
        1 => Some(true),
        _ => None,
    }
}

/// Code filaire d'un [`Role`] (stable : ne jamais renuméroter).
const fn code_role(role: Role) -> u8 {
    match role {
        Role::Viewer => 0,
        Role::Operator => 1,
        Role::Admin => 2,
    }
}

fn role_depuis_code(code: u8) -> Option<Role> {
    match code {
        0 => Some(Role::Viewer),
        1 => Some(Role::Operator),
        2 => Some(Role::Admin),
        _ => None,
    }
}

/// Code filaire d'une [`Permission`] (stable : ne jamais renuméroter).
const fn code_permission(permission: Permission) -> u8 {
    match permission {
        Permission::ViewScreen => 0,
        Permission::ControlInput => 1,
        Permission::TransferFiles => 2,
        Permission::ManageDevices => 3,
        Permission::ManageMembers => 4,
    }
}

fn permission_depuis_code(code: u8) -> Option<Permission> {
    match code {
        0 => Some(Permission::ViewScreen),
        1 => Some(Permission::ControlInput),
        2 => Some(Permission::TransferFiles),
        3 => Some(Permission::ManageDevices),
        4 => Some(Permission::ManageMembers),
        _ => None,
    }
}

/// Code filaire d'un [`ReleaseChannel`] (stable : ne jamais renuméroter).
const fn code_canal(canal: ReleaseChannel) -> u8 {
    match canal {
        ReleaseChannel::Stable => 0,
        ReleaseChannel::Beta => 1,
        ReleaseChannel::Canary => 2,
        ReleaseChannel::Lts => 3,
    }
}

fn canal_depuis_code(code: u8) -> Option<ReleaseChannel> {
    match code {
        0 => Some(ReleaseChannel::Stable),
        1 => Some(ReleaseChannel::Beta),
        2 => Some(ReleaseChannel::Canary),
        3 => Some(ReleaseChannel::Lts),
        _ => None,
    }
}

fn put_version(out: &mut Vec<u8>, version: Version) {
    out.extend_from_slice(&version.major.to_be_bytes());
    out.extend_from_slice(&version.minor.to_be_bytes());
    out.extend_from_slice(&version.patch.to_be_bytes());
}

fn read_version(d: &[u8], p: &mut usize) -> Option<Version> {
    Some(Version::new(
        read_u32(d, p)?,
        read_u32(d, p)?,
        read_u32(d, p)?,
    ))
}

/// Bénéficiaire : tag 0 = compte (chaîne), tag 1 = groupe (id `u64`).
fn put_beneficiaire(out: &mut Vec<u8>, beneficiaire: &Beneficiaire) {
    match beneficiaire {
        Beneficiaire::Compte(compte) => {
            out.push(0);
            put_bytes(out, compte.as_bytes());
        }
        Beneficiaire::Groupe(id) => {
            out.push(1);
            out.extend_from_slice(&id.to_be_bytes());
        }
    }
}

fn read_beneficiaire(d: &[u8], p: &mut usize) -> Option<Beneficiaire> {
    match read_u8(d, p)? {
        0 => Some(Beneficiaire::Compte(read_string(d, p)?)),
        1 => Some(Beneficiaire::Groupe(read_u64(d, p)?)),
        _ => None,
    }
}

fn put_groupe(out: &mut Vec<u8>, groupe: &Group) {
    out.extend_from_slice(&groupe.id.to_be_bytes());
    put_bytes(out, groupe.name.as_bytes());
    out.extend_from_slice(&(groupe.members.len() as u32).to_be_bytes());
    for membre in &groupe.members {
        put_bytes(out, membre.as_bytes());
    }
}

fn read_groupe(d: &[u8], p: &mut usize) -> Option<Group> {
    let id = read_u64(d, p)?;
    let name = read_string(d, p)?;
    let n = read_u32(d, p)?;
    let mut members = Vec::new();
    for _ in 0..n {
        members.push(read_string(d, p)?);
    }
    Some(Group { id, name, members })
}

fn put_manifeste(out: &mut Vec<u8>, manifeste: &UpdateManifest) {
    out.push(code_canal(manifeste.channel));
    put_version(out, manifeste.latest);
    put_version(out, manifeste.min_supported);
    put_bytes(out, manifeste.url.as_bytes());
    put_bytes(out, manifeste.sha256.as_bytes());
    match manifeste.delta_from {
        Some(version) => {
            out.push(1);
            put_version(out, version);
        }
        None => out.push(0),
    }
}

fn read_manifeste(d: &[u8], p: &mut usize) -> Option<UpdateManifest> {
    let channel = canal_depuis_code(read_u8(d, p)?)?;
    let latest = read_version(d, p)?;
    let min_supported = read_version(d, p)?;
    let url = read_string(d, p)?;
    let sha256 = read_string(d, p)?;
    let delta_from = match read_u8(d, p)? {
        0 => None,
        1 => Some(read_version(d, p)?),
        _ => return None,
    };
    Some(UpdateManifest {
        channel,
        latest,
        min_supported,
        url,
        sha256,
        delta_from,
    })
}

// ---------------------------------------------------------------------------
// Requêtes
// ---------------------------------------------------------------------------

/// Requête d'un client de l'API applicative (l'octet de tag est indiqué par
/// variante). Toutes portent un jeton de session **sauf** [`Request::CheckUpdate`],
/// volontairement anonyme (un client pas encore connecté doit pouvoir se
/// mettre à jour).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// Tag 1 — ajoute (ou renomme) un contact du carnet.
    AddContact {
        jeton: String,
        id: u64,
        alias: String,
    },
    /// Tag 2 — liste les contacts du carnet.
    ListContacts { jeton: String },
    /// Tag 3 — attribue `role` à `compte` sur `ressource` (RBAC).
    AssignRole {
        jeton: String,
        compte: String,
        ressource: String,
        role: Role,
    },
    /// Tag 4 — `compte` possède-t-il `permission` sur `ressource` ?
    HasPermission {
        jeton: String,
        compte: String,
        ressource: String,
        permission: Permission,
    },
    /// Tag 5 — crée un groupe vide, renvoie [`Response::GroupeCree`].
    CreateGroup { jeton: String, nom: String },
    /// Tag 6 — ajoute `compte` au groupe `groupe` (idempotent).
    AddMember {
        jeton: String,
        groupe: u64,
        compte: String,
    },
    /// Tag 7 — groupes dont `compte` est membre, renvoie [`Response::Groupes`].
    ListGroups { jeton: String, compte: String },
    /// Tag 8 — partage `appareil` avec `beneficiaire` au rôle `role`.
    ShareDevice {
        jeton: String,
        appareil: u64,
        beneficiaire: Beneficiaire,
        role: Role,
    },
    /// Tag 9 — appareils partagés avec `compte` (direct ou via ses groupes),
    /// renvoie [`Response::Appareils`].
    DevicesSharedWith { jeton: String, compte: String },
    /// Tag 10 — rôle effectif de `compte` sur `appareil`, renvoie
    /// [`Response::RoleEffectif`].
    EffectiveRole {
        jeton: String,
        compte: String,
        appareil: u64,
    },
    /// Tag 11 — le client annonce sa version sur un canal, renvoie
    /// [`Response::MiseAJour`]. Anonyme (pas de jeton).
    CheckUpdate {
        canal: ReleaseChannel,
        version: Version,
    },
    /// Tag 12 — publie (ou remplace) le manifeste du canal qu'il désigne.
    /// Opération d'administration ; non persistée (voir [`crate::services`]).
    PublishManifest {
        jeton: String,
        manifeste: UpdateManifest,
    },
    /// Tag 13 — configuration effective de l'organisation `org`, renvoie
    /// [`Response::Config`] (paires triées par clé).
    EffectiveConfig { jeton: String, org: String },
    /// Tag 14 — fixe (ou remplace) la politique `cle = valeur` pour `org`.
    /// Opération d'administration ; non persistée (voir [`crate::services`]).
    SetPolicy {
        jeton: String,
        org: String,
        cle: String,
        valeur: String,
    },
    /// Tag 15 — alloue un nouvel ID NovaDesk lié au compte du jeton et à la
    /// clé statique du client (32 octets), renvoie [`Response::IdAlloue`]
    /// (l'ID et le jeton d'enregistrement exigé par le rendez-vous).
    AllocateId { jeton: String, cle_client: [u8; 32] },
}

impl Request {
    /// Jeton de session porté par la requête, `None` pour les requêtes anonymes.
    #[must_use]
    pub fn jeton(&self) -> Option<&str> {
        match self {
            Request::AddContact { jeton, .. }
            | Request::ListContacts { jeton }
            | Request::AssignRole { jeton, .. }
            | Request::HasPermission { jeton, .. }
            | Request::CreateGroup { jeton, .. }
            | Request::AddMember { jeton, .. }
            | Request::ListGroups { jeton, .. }
            | Request::ShareDevice { jeton, .. }
            | Request::DevicesSharedWith { jeton, .. }
            | Request::EffectiveRole { jeton, .. }
            | Request::PublishManifest { jeton, .. }
            | Request::EffectiveConfig { jeton, .. }
            | Request::SetPolicy { jeton, .. }
            | Request::AllocateId { jeton, .. } => Some(jeton),
            Request::CheckUpdate { .. } => None,
        }
    }

    /// Sérialisation côté client (le serveur désérialise avec [`Self::from_bytes`]).
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            Request::AddContact { jeton, id, alias } => {
                out.push(1);
                put_bytes(&mut out, jeton.as_bytes());
                out.extend_from_slice(&id.to_be_bytes());
                put_bytes(&mut out, alias.as_bytes());
            }
            Request::ListContacts { jeton } => {
                out.push(2);
                put_bytes(&mut out, jeton.as_bytes());
            }
            Request::AssignRole {
                jeton,
                compte,
                ressource,
                role,
            } => {
                out.push(3);
                put_bytes(&mut out, jeton.as_bytes());
                put_bytes(&mut out, compte.as_bytes());
                put_bytes(&mut out, ressource.as_bytes());
                out.push(code_role(*role));
            }
            Request::HasPermission {
                jeton,
                compte,
                ressource,
                permission,
            } => {
                out.push(4);
                put_bytes(&mut out, jeton.as_bytes());
                put_bytes(&mut out, compte.as_bytes());
                put_bytes(&mut out, ressource.as_bytes());
                out.push(code_permission(*permission));
            }
            Request::CreateGroup { jeton, nom } => {
                out.push(5);
                put_bytes(&mut out, jeton.as_bytes());
                put_bytes(&mut out, nom.as_bytes());
            }
            Request::AddMember {
                jeton,
                groupe,
                compte,
            } => {
                out.push(6);
                put_bytes(&mut out, jeton.as_bytes());
                out.extend_from_slice(&groupe.to_be_bytes());
                put_bytes(&mut out, compte.as_bytes());
            }
            Request::ListGroups { jeton, compte } => {
                out.push(7);
                put_bytes(&mut out, jeton.as_bytes());
                put_bytes(&mut out, compte.as_bytes());
            }
            Request::ShareDevice {
                jeton,
                appareil,
                beneficiaire,
                role,
            } => {
                out.push(8);
                put_bytes(&mut out, jeton.as_bytes());
                out.extend_from_slice(&appareil.to_be_bytes());
                put_beneficiaire(&mut out, beneficiaire);
                out.push(code_role(*role));
            }
            Request::DevicesSharedWith { jeton, compte } => {
                out.push(9);
                put_bytes(&mut out, jeton.as_bytes());
                put_bytes(&mut out, compte.as_bytes());
            }
            Request::EffectiveRole {
                jeton,
                compte,
                appareil,
            } => {
                out.push(10);
                put_bytes(&mut out, jeton.as_bytes());
                put_bytes(&mut out, compte.as_bytes());
                out.extend_from_slice(&appareil.to_be_bytes());
            }
            Request::CheckUpdate { canal, version } => {
                out.push(11);
                out.push(code_canal(*canal));
                put_version(&mut out, *version);
            }
            Request::PublishManifest { jeton, manifeste } => {
                out.push(12);
                put_bytes(&mut out, jeton.as_bytes());
                put_manifeste(&mut out, manifeste);
            }
            Request::EffectiveConfig { jeton, org } => {
                out.push(13);
                put_bytes(&mut out, jeton.as_bytes());
                put_bytes(&mut out, org.as_bytes());
            }
            Request::SetPolicy {
                jeton,
                org,
                cle,
                valeur,
            } => {
                out.push(14);
                put_bytes(&mut out, jeton.as_bytes());
                put_bytes(&mut out, org.as_bytes());
                put_bytes(&mut out, cle.as_bytes());
                put_bytes(&mut out, valeur.as_bytes());
            }
            Request::AllocateId { jeton, cle_client } => {
                out.push(15);
                put_bytes(&mut out, jeton.as_bytes());
                out.extend_from_slice(cle_client);
            }
        }
        out
    }

    /// Désérialisation côté serveur. `None` si le tag est inconnu ou la charge
    /// utile tronquée/mal formée.
    #[must_use]
    pub fn from_bytes(d: &[u8]) -> Option<Request> {
        let mut p = 0;
        match read_u8(d, &mut p)? {
            1 => {
                let jeton = read_string(d, &mut p)?;
                let id = read_u64(d, &mut p)?;
                let alias = read_string(d, &mut p)?;
                Some(Request::AddContact { jeton, id, alias })
            }
            2 => Some(Request::ListContacts {
                jeton: read_string(d, &mut p)?,
            }),
            3 => {
                let jeton = read_string(d, &mut p)?;
                let compte = read_string(d, &mut p)?;
                let ressource = read_string(d, &mut p)?;
                let role = role_depuis_code(read_u8(d, &mut p)?)?;
                Some(Request::AssignRole {
                    jeton,
                    compte,
                    ressource,
                    role,
                })
            }
            4 => {
                let jeton = read_string(d, &mut p)?;
                let compte = read_string(d, &mut p)?;
                let ressource = read_string(d, &mut p)?;
                let permission = permission_depuis_code(read_u8(d, &mut p)?)?;
                Some(Request::HasPermission {
                    jeton,
                    compte,
                    ressource,
                    permission,
                })
            }
            5 => {
                let jeton = read_string(d, &mut p)?;
                let nom = read_string(d, &mut p)?;
                Some(Request::CreateGroup { jeton, nom })
            }
            6 => {
                let jeton = read_string(d, &mut p)?;
                let groupe = read_u64(d, &mut p)?;
                let compte = read_string(d, &mut p)?;
                Some(Request::AddMember {
                    jeton,
                    groupe,
                    compte,
                })
            }
            7 => {
                let jeton = read_string(d, &mut p)?;
                let compte = read_string(d, &mut p)?;
                Some(Request::ListGroups { jeton, compte })
            }
            8 => {
                let jeton = read_string(d, &mut p)?;
                let appareil = read_u64(d, &mut p)?;
                let beneficiaire = read_beneficiaire(d, &mut p)?;
                let role = role_depuis_code(read_u8(d, &mut p)?)?;
                Some(Request::ShareDevice {
                    jeton,
                    appareil,
                    beneficiaire,
                    role,
                })
            }
            9 => {
                let jeton = read_string(d, &mut p)?;
                let compte = read_string(d, &mut p)?;
                Some(Request::DevicesSharedWith { jeton, compte })
            }
            10 => {
                let jeton = read_string(d, &mut p)?;
                let compte = read_string(d, &mut p)?;
                let appareil = read_u64(d, &mut p)?;
                Some(Request::EffectiveRole {
                    jeton,
                    compte,
                    appareil,
                })
            }
            11 => {
                let canal = canal_depuis_code(read_u8(d, &mut p)?)?;
                let version = read_version(d, &mut p)?;
                Some(Request::CheckUpdate { canal, version })
            }
            12 => {
                let jeton = read_string(d, &mut p)?;
                let manifeste = read_manifeste(d, &mut p)?;
                Some(Request::PublishManifest { jeton, manifeste })
            }
            13 => {
                let jeton = read_string(d, &mut p)?;
                let org = read_string(d, &mut p)?;
                Some(Request::EffectiveConfig { jeton, org })
            }
            14 => {
                let jeton = read_string(d, &mut p)?;
                let org = read_string(d, &mut p)?;
                let cle = read_string(d, &mut p)?;
                let valeur = read_string(d, &mut p)?;
                Some(Request::SetPolicy {
                    jeton,
                    org,
                    cle,
                    valeur,
                })
            }
            15 => {
                let jeton = read_string(d, &mut p)?;
                let cle_client: [u8; 32] = d.get(p..p + 32)?.try_into().ok()?;
                p += 32;
                // La clé statique clôt la charge : rien ne doit suivre.
                (p == d.len()).then_some(Request::AllocateId { jeton, cle_client })
            }
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Réponses
// ---------------------------------------------------------------------------

/// Réponse du serveur (l'octet de tag est indiqué par variante).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Response {
    /// Tag 0 — opération réussie, rien à renvoyer.
    Ok,
    /// Tag 1 — contacts du carnet.
    Contacts(Vec<Contact>),
    /// Tag 2 — erreur métier ou de persistance (message lisible).
    Erreur { message: String },
    /// Tag 3 — groupe créé, id attribué.
    GroupeCree { id: u64 },
    /// Tag 4 — groupes (avec membres), triés par id.
    Groupes(Vec<Group>),
    /// Tag 5 — réponse booléenne (ex. [`Request::HasPermission`]).
    Booleen(bool),
    /// Tag 6 — appareils partagés (id, rôle effectif), triés par id.
    Appareils(Vec<(u64, Role)>),
    /// Tag 7 — rôle effectif, `None` si l'appareil n'est pas partagé au compte.
    RoleEffectif(Option<Role>),
    /// Tag 8 — décision de mise à jour (sous-tag : 0 = à jour,
    /// 1 = disponible + manifeste, 2 = forcée + manifeste).
    MiseAJour(UpdateDecision),
    /// Tag 9 — configuration effective : paires (clé, valeur) triées par clé.
    Config(Vec<(String, String)>),
    /// Tag 10 — ID NovaDesk alloué + jeton d'enregistrement sérialisé
    /// (voir [`crate::auth::JetonEnregistrement`]), à présenter au rendez-vous.
    IdAlloue {
        id: u64,
        jeton_enregistrement: Vec<u8>,
    },
}

impl Response {
    /// Sérialisation côté serveur (le client désérialise avec [`Self::from_bytes`]).
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            Response::Ok => out.push(0),
            Response::Contacts(contacts) => {
                out.push(1);
                out.extend_from_slice(&(contacts.len() as u32).to_be_bytes());
                for c in contacts {
                    out.extend_from_slice(&c.id.to_be_bytes());
                    put_bytes(&mut out, c.alias.as_bytes());
                }
            }
            Response::Erreur { message } => {
                out.push(2);
                put_bytes(&mut out, message.as_bytes());
            }
            Response::GroupeCree { id } => {
                out.push(3);
                out.extend_from_slice(&id.to_be_bytes());
            }
            Response::Groupes(groupes) => {
                out.push(4);
                out.extend_from_slice(&(groupes.len() as u32).to_be_bytes());
                for groupe in groupes {
                    put_groupe(&mut out, groupe);
                }
            }
            Response::Booleen(valeur) => {
                out.push(5);
                out.push(u8::from(*valeur));
            }
            Response::Appareils(appareils) => {
                out.push(6);
                out.extend_from_slice(&(appareils.len() as u32).to_be_bytes());
                for (appareil, role) in appareils {
                    out.extend_from_slice(&appareil.to_be_bytes());
                    out.push(code_role(*role));
                }
            }
            Response::RoleEffectif(role) => {
                out.push(7);
                match role {
                    Some(role) => {
                        out.push(1);
                        out.push(code_role(*role));
                    }
                    None => out.push(0),
                }
            }
            Response::MiseAJour(decision) => {
                out.push(8);
                match decision {
                    UpdateDecision::UpToDate => out.push(0),
                    UpdateDecision::UpdateAvailable(manifeste) => {
                        out.push(1);
                        put_manifeste(&mut out, manifeste);
                    }
                    UpdateDecision::ForcedUpdate(manifeste) => {
                        out.push(2);
                        put_manifeste(&mut out, manifeste);
                    }
                }
            }
            Response::Config(paires) => {
                out.push(9);
                out.extend_from_slice(&(paires.len() as u32).to_be_bytes());
                for (cle, valeur) in paires {
                    put_bytes(&mut out, cle.as_bytes());
                    put_bytes(&mut out, valeur.as_bytes());
                }
            }
            Response::IdAlloue {
                id,
                jeton_enregistrement,
            } => {
                out.push(10);
                out.extend_from_slice(&id.to_be_bytes());
                put_bytes(&mut out, jeton_enregistrement);
            }
        }
        out
    }

    /// Désérialisation côté client. `None` si le tag est inconnu ou la charge
    /// utile tronquée/mal formée.
    #[must_use]
    pub fn from_bytes(d: &[u8]) -> Option<Response> {
        let mut p = 0;
        match read_u8(d, &mut p)? {
            0 => Some(Response::Ok),
            1 => {
                let n = read_u32(d, &mut p)?;
                let mut contacts = Vec::new();
                for _ in 0..n {
                    contacts.push(Contact {
                        id: read_u64(d, &mut p)?,
                        alias: read_string(d, &mut p)?,
                    });
                }
                Some(Response::Contacts(contacts))
            }
            2 => Some(Response::Erreur {
                message: read_string(d, &mut p)?,
            }),
            3 => Some(Response::GroupeCree {
                id: read_u64(d, &mut p)?,
            }),
            4 => {
                let n = read_u32(d, &mut p)?;
                let mut groupes = Vec::new();
                for _ in 0..n {
                    groupes.push(read_groupe(d, &mut p)?);
                }
                Some(Response::Groupes(groupes))
            }
            5 => Some(Response::Booleen(read_bool(d, &mut p)?)),
            6 => {
                let n = read_u32(d, &mut p)?;
                let mut appareils = Vec::new();
                for _ in 0..n {
                    let appareil = read_u64(d, &mut p)?;
                    let role = role_depuis_code(read_u8(d, &mut p)?)?;
                    appareils.push((appareil, role));
                }
                Some(Response::Appareils(appareils))
            }
            7 => match read_u8(d, &mut p)? {
                0 => Some(Response::RoleEffectif(None)),
                1 => Some(Response::RoleEffectif(Some(role_depuis_code(read_u8(
                    d, &mut p,
                )?)?))),
                _ => None,
            },
            8 => {
                let decision = match read_u8(d, &mut p)? {
                    0 => UpdateDecision::UpToDate,
                    1 => UpdateDecision::UpdateAvailable(read_manifeste(d, &mut p)?),
                    2 => UpdateDecision::ForcedUpdate(read_manifeste(d, &mut p)?),
                    _ => return None,
                };
                Some(Response::MiseAJour(decision))
            }
            9 => {
                let n = read_u32(d, &mut p)?;
                let mut paires = Vec::new();
                for _ in 0..n {
                    let cle = read_string(d, &mut p)?;
                    let valeur = read_string(d, &mut p)?;
                    paires.push((cle, valeur));
                }
                Some(Response::Config(paires))
            }
            10 => {
                let id = read_u64(d, &mut p)?;
                let longueur = read_u32(d, &mut p)? as usize;
                let jeton_enregistrement = d.get(p..p + longueur)?.to_vec();
                Some(Response::IdAlloue {
                    id,
                    jeton_enregistrement,
                })
            }
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn manifeste() -> UpdateManifest {
        UpdateManifest {
            channel: ReleaseChannel::Stable,
            latest: Version::new(2, 5, 1),
            min_supported: Version::new(2, 0, 0),
            url: "https://updates.novadesk.example/stable/2.5.1/novadesk-setup.exe".into(),
            sha256: "deadbeef".repeat(8),
            delta_from: Some(Version::new(2, 5, 0)),
        }
    }

    #[test]
    fn requetes_aller_retour() {
        let jeton = || "jeton".to_string();
        let requetes = vec![
            Request::AddContact {
                jeton: jeton(),
                id: 42,
                alias: "PC".into(),
            },
            Request::ListContacts { jeton: jeton() },
            Request::AssignRole {
                jeton: jeton(),
                compte: "alice".into(),
                ressource: "org-1".into(),
                role: Role::Admin,
            },
            Request::HasPermission {
                jeton: jeton(),
                compte: "alice".into(),
                ressource: "org-1".into(),
                permission: Permission::ManageMembers,
            },
            Request::CreateGroup {
                jeton: jeton(),
                nom: "Support".into(),
            },
            Request::AddMember {
                jeton: jeton(),
                groupe: 7,
                compte: "bob".into(),
            },
            Request::ListGroups {
                jeton: jeton(),
                compte: "bob".into(),
            },
            Request::ShareDevice {
                jeton: jeton(),
                appareil: 123_456,
                beneficiaire: Beneficiaire::Groupe(7),
                role: Role::Operator,
            },
            Request::ShareDevice {
                jeton: jeton(),
                appareil: 123_456,
                beneficiaire: Beneficiaire::Compte("carol".into()),
                role: Role::Viewer,
            },
            Request::DevicesSharedWith {
                jeton: jeton(),
                compte: "carol".into(),
            },
            Request::EffectiveRole {
                jeton: jeton(),
                compte: "carol".into(),
                appareil: 123_456,
            },
            Request::CheckUpdate {
                canal: ReleaseChannel::Beta,
                version: Version::new(1, 2, 3),
            },
            Request::PublishManifest {
                jeton: jeton(),
                manifeste: manifeste(),
            },
            Request::EffectiveConfig {
                jeton: jeton(),
                org: "acme".into(),
            },
            Request::SetPolicy {
                jeton: jeton(),
                org: "acme".into(),
                cle: "require_2fa".into(),
                valeur: "true".into(),
            },
            Request::AllocateId {
                jeton: jeton(),
                cle_client: [7u8; 32],
            },
        ];
        for requete in requetes {
            let octets = requete.to_bytes();
            assert_eq!(
                Request::from_bytes(&octets).expect("décodage"),
                requete,
                "aller-retour requête"
            );
            // Charge utile tronquée : refusée proprement.
            assert!(Request::from_bytes(&octets[..octets.len() - 1]).is_none());
        }
        // Vide ou tag inconnu : refusés.
        assert!(Request::from_bytes(&[]).is_none());
        assert!(Request::from_bytes(&[99]).is_none());
    }

    #[test]
    fn reponses_aller_retour() {
        let reponses = vec![
            Response::Ok,
            Response::Contacts(vec![Contact {
                id: 7,
                alias: "Portable".into(),
            }]),
            Response::Erreur {
                message: "jeton invalide ou absent".into(),
            },
            Response::GroupeCree { id: 3 },
            Response::Groupes(vec![Group {
                id: 3,
                name: "Support".into(),
                members: vec!["alice".into(), "bob".into()],
            }]),
            Response::Booleen(true),
            Response::Booleen(false),
            Response::Appareils(vec![(100, Role::Viewer), (200, Role::Admin)]),
            Response::RoleEffectif(None),
            Response::RoleEffectif(Some(Role::Operator)),
            Response::MiseAJour(UpdateDecision::UpToDate),
            Response::MiseAJour(UpdateDecision::UpdateAvailable(manifeste())),
            Response::MiseAJour(UpdateDecision::ForcedUpdate(manifeste())),
            Response::Config(vec![
                ("allow_file_transfer".into(), "true".into()),
                ("require_2fa".into(), "false".into()),
            ]),
            Response::IdAlloue {
                id: 123_456_789,
                jeton_enregistrement: vec![1, 2, 3, 4, 5],
            },
        ];
        for reponse in reponses {
            assert_eq!(
                Response::from_bytes(&reponse.to_bytes()).expect("décodage"),
                reponse,
                "aller-retour réponse"
            );
        }
        assert!(Response::from_bytes(&[]).is_none());
        assert!(Response::from_bytes(&[99]).is_none());
    }

    #[test]
    fn jeton_des_requetes() {
        // Requête authentifiée : le jeton est exposé pour vérification.
        let requete = Request::ListContacts { jeton: "t".into() };
        assert_eq!(requete.jeton(), Some("t"));
        // CheckUpdate est anonyme.
        let anonyme = Request::CheckUpdate {
            canal: ReleaseChannel::Stable,
            version: Version::new(1, 0, 0),
        };
        assert_eq!(anonyme.jeton(), None);
    }

    #[test]
    fn trame_trop_grande_refusee() {
        // Longueur annoncée au-delà de TRAME_MAX : refus immédiat, sans allocation.
        let annonce = ((TRAME_MAX + 1) as u32).to_be_bytes();
        let mut lecteur = std::io::Cursor::new(annonce.to_vec());
        let erreur = read_frame(&mut lecteur).expect_err("trame refusée");
        assert_eq!(erreur.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn trame_aller_retour() {
        let mut tampon = Vec::new();
        write_frame(&mut tampon, b"charge utile").expect("écriture");
        let mut lecteur = std::io::Cursor::new(tampon);
        assert_eq!(read_frame(&mut lecteur).expect("lecture"), b"charge utile");
    }
}
