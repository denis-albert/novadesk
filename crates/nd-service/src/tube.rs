//! **Tube nommé Windows** (named pipe) portant le canal service ↔ assistant.
//!
//! Le service crée le **serveur** de tube (`CreateNamedPipeW`) puis attend la
//! connexion de l'assistant (`ConnectNamedPipe`) ; l'assistant, lancé dans la
//! session active, s'y **connecte** en client (`CreateFileW`). Le tube est en mode
//! **octet, bloquant** (`PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT`) et
//! **duplex** : chaque bout lit et écrit sur la **même** poignée.
//!
//! # Partage lecture/écriture entre threads
//!
//! Le pont côté service dédie un thread à la **lecture** des trames et écrit les
//! **entrées** depuis un autre. `ReadFile`/`WriteFile` sur une poignée de tube
//! duplex sont sûrs en parallèle (sens indépendants). On partage donc la poignée
//! via un [`Arc`] : [`LecteurTube`] (`Read`) et [`EcrivainTube`] (`Write`) en
//! détiennent chacun un clone ; la poignée est fermée à la libération du dernier.
//!
//! Tout le `unsafe` FFI du tube est concentré ici.
#![allow(unsafe_code)]

use std::io::{self, Read, Write};
use std::iter;
use std::sync::Arc;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    CloseHandle, ERROR_BROKEN_PIPE, ERROR_NO_DATA, ERROR_PIPE_CONNECTED, ERROR_PIPE_NOT_CONNECTED,
    GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_NONE, OPEN_EXISTING,
    PIPE_ACCESS_DUPLEX,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE,
    PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
};

/// Préfixe obligatoire des tubes nommés locaux.
const PREFIXE_PIPE: &str = r"\\.\pipe\";
/// Taille des tampons internes du tube (indice au système, 64 Kio par sens).
const TAILLE_TAMPON: u32 = 64 * 1024;

/// Construit un **nom de base** unique pour une session assistant, à partir du PID
/// du service et d'un discriminant. Forme : `\\.\pipe\novadesk-…`.
#[must_use]
pub fn chemin_unique(discriminant: u64) -> String {
    format!(
        "{PREFIXE_PIPE}novadesk-assistant-{}-{discriminant}",
        std::process::id()
    )
}

/// Dérive les **deux tubes** (un par sens) d'un nom de base :
/// `(assistant→service, service→assistant)`.
///
/// # Pourquoi deux tubes plutôt qu'un duplex
///
/// Une poignée de tube **synchrone** sérialise ses E/S : une `ReadFile` bloquée
/// (thread lecteur en attente de trame) **empêche** une `WriteFile` concurrente sur
/// la **même** poignée d'aboutir (thread écrivain) — les deux bouts se figent. En
/// dédiant **une poignée par sens** (chacune n'est jamais lue *et* écrite en même
/// temps), on garde une E/S bloquante simple sans interblocage ni recours à
/// l'overlapped I/O.
#[must_use]
pub fn noms_duplex(base: &str) -> (String, String) {
    (format!("{base}-a2s"), format!("{base}-s2a"))
}

/// Poignée de tube possédée, refermée à la libération (RAII).
struct PoigneeTube(HANDLE);

// SAFETY : la poignée est un identifiant noyau opaque ; `ReadFile`/`WriteFile` sur
// un tube duplex sont sûrs depuis des threads distincts (sens indépendants). On la
// partage donc entre un lecteur et un écrivain via `Arc`.
unsafe impl Send for PoigneeTube {}
unsafe impl Sync for PoigneeTube {}

impl Drop for PoigneeTube {
    fn drop(&mut self) {
        // SAFETY : poignée valide obtenue de `CreateNamedPipeW`/`CreateFileW`,
        // fermée une seule fois (le `Drop` d'un `Arc` unique).
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// Bout **lecteur** du tube (implémente [`Read`]).
pub struct LecteurTube(Arc<PoigneeTube>);

/// Bout **écrivain** du tube (implémente [`Write`]).
pub struct EcrivainTube(Arc<PoigneeTube>);

impl Read for LecteurTube {
    fn read(&mut self, tampon: &mut [u8]) -> io::Result<usize> {
        if tampon.is_empty() {
            return Ok(0);
        }
        let mut lus: u32 = 0;
        // SAFETY : `tampon` est un slice valide en écriture ; `lus` reçoit le
        // nombre d'octets lus ; poignée valide tant que l'`Arc` vit.
        let resultat = unsafe { ReadFile(self.0 .0, Some(tampon), Some(&mut lus), None) };
        match resultat {
            Ok(()) => Ok(lus as usize),
            // Fin normale : l'autre bout a fermé le tube → EOF (`Ok(0)`), ce qui fait
            // remonter `read_exact` en `UnexpectedEof` (déconnexion propre).
            Err(e) if est_fin_de_tube(&e) => Ok(0),
            Err(e) => Err(io::Error::from(e)),
        }
    }
}

impl Write for EcrivainTube {
    fn write(&mut self, tampon: &[u8]) -> io::Result<usize> {
        if tampon.is_empty() {
            return Ok(0);
        }
        let mut ecrits: u32 = 0;
        // SAFETY : `tampon` est un slice valide en lecture ; `ecrits` reçoit le
        // nombre d'octets écrits ; poignée valide tant que l'`Arc` vit.
        let resultat = unsafe { WriteFile(self.0 .0, Some(tampon), Some(&mut ecrits), None) };
        match resultat {
            Ok(()) => Ok(ecrits as usize),
            Err(e) if est_fin_de_tube(&e) => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "tube assistant fermé",
            )),
            Err(e) => Err(io::Error::from(e)),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        // Les tubes nommés ne bufferisent pas côté appelant : rien à vider.
        Ok(())
    }
}

/// Une extrémité connectée du tube : lecteur + écrivain sur la **même** poignée.
pub struct ExtremiteTube {
    poignee: Arc<PoigneeTube>,
}

impl ExtremiteTube {
    fn nouvelle(handle: HANDLE) -> Self {
        ExtremiteTube {
            poignee: Arc::new(PoigneeTube(handle)),
        }
    }

    /// Scinde l'extrémité en un lecteur et un écrivain partageant la poignée.
    #[must_use]
    pub fn scinder(self) -> (LecteurTube, EcrivainTube) {
        (
            LecteurTube(Arc::clone(&self.poignee)),
            EcrivainTube(self.poignee),
        )
    }
}

/// **Serveur** de tube nommé : crée l'instance et attend la connexion du client.
pub struct ServeurTube {
    poignee: Arc<PoigneeTube>,
}

impl ServeurTube {
    /// Crée le serveur de tube au chemin `chemin` (`\\.\pipe\…`), une instance,
    /// duplex octet bloquant, refusant les clients distants.
    ///
    /// # Errors
    /// Erreur si `CreateNamedPipeW` échoue (nom déjà pris, droits insuffisants…).
    pub fn creer(chemin: &str) -> io::Result<Self> {
        let nom = utf16z(chemin);
        // SAFETY : `nom` (UTF-16 terminé par 0) vit jusqu'après l'appel ; pas
        // d'attributs de sécurité (poignée non héritable, DACL par défaut du
        // service SYSTEM).
        let handle = unsafe {
            CreateNamedPipeW(
                PCWSTR::from_raw(nom.as_ptr()),
                PIPE_ACCESS_DUPLEX | FILE_FLAGS_AND_ATTRIBUTES(0),
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                1,
                TAILLE_TAMPON,
                TAILLE_TAMPON,
                0,
                None,
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::other(format!(
                "création du tube « {chemin} » impossible : {}",
                io::Error::last_os_error()
            )));
        }
        Ok(ServeurTube {
            poignee: Arc::new(PoigneeTube(handle)),
        })
    }

    /// Attend qu'un client (l'assistant) se connecte, puis rend l'extrémité serveur.
    ///
    /// `ConnectNamedPipe` réussit aussi si le client s'est connecté **avant**
    /// l'appel (erreur `ERROR_PIPE_CONNECTED`, traitée comme un succès).
    ///
    /// # Errors
    /// Erreur si l'attente de connexion échoue pour une autre raison.
    pub fn attendre_client(&self) -> io::Result<ExtremiteTube> {
        // SAFETY : poignée serveur valide ; connexion synchrone (pas d'OVERLAPPED).
        let resultat = unsafe { ConnectNamedPipe(self.poignee.0, None) };
        match resultat {
            Ok(()) => {}
            // Le client a déjà ouvert le tube : c'est un succès (documenté MSDN).
            Err(e) if e.code() == windows::core::HRESULT::from_win32(ERROR_PIPE_CONNECTED.0) => {}
            Err(e) => return Err(io::Error::from(e)),
        }
        // L'instance serveur du tube **est** l'extrémité connectée : on partage la
        // même poignée (lecteur + écrivain la scindent ensuite).
        Ok(ExtremiteTube {
            poignee: Arc::clone(&self.poignee),
        })
    }

    /// Déconnecte le client courant (fin de session) sans détruire le serveur.
    pub fn deconnecter(&self) {
        // SAFETY : poignée serveur valide ; ignore l'échec (déjà déconnecté).
        unsafe {
            let _ = DisconnectNamedPipe(self.poignee.0);
        }
    }
}

/// **Client** : connexion de l'assistant au tube nommé du service.
///
/// # Errors
/// Erreur si `CreateFileW` échoue (serveur absent, occupé, ou accès refusé).
pub fn connecter_client(chemin: &str) -> io::Result<ExtremiteTube> {
    let nom = utf16z(chemin);
    // SAFETY : `nom` vit jusqu'après l'appel ; accès lecture+écriture, sans partage,
    // ouverture d'un tube existant.
    let handle = unsafe {
        CreateFileW(
            PCWSTR::from_raw(nom.as_ptr()),
            GENERIC_READ.0 | GENERIC_WRITE.0,
            FILE_SHARE_NONE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            HANDLE::default(),
        )
    }
    .map_err(io::Error::from)?;
    Ok(ExtremiteTube::nouvelle(handle))
}

/// Fin de tube « attendue » : l'autre bout a fermé/rompu le tube.
fn est_fin_de_tube(e: &windows::core::Error) -> bool {
    let code = e.code();
    code == windows::core::HRESULT::from_win32(ERROR_BROKEN_PIPE.0)
        || code == windows::core::HRESULT::from_win32(ERROR_PIPE_NOT_CONNECTED.0)
        || code == windows::core::HRESULT::from_win32(ERROR_NO_DATA.0)
}

/// Encode une chaîne en tampon UTF-16 terminé par un zéro (API `*W`).
fn utf16z(texte: &str) -> Vec<u16> {
    texte.encode_utf16().chain(iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canal;
    use std::thread;

    /// Aller-retour **réel** sur un tube nommé : un thread serveur attend le
    /// client, l'écoute d'un message service, et renvoie une trame ; le client
    /// (thread principal) émet puis lit. Prouve l'encadrement de bout en bout à
    /// travers `ReadFile`/`WriteFile`.
    #[test]
    fn tube_nomme_transporte_le_canal() {
        let chemin = chemin_unique(0xA11CE);
        let chemin_serveur = chemin.clone();

        let serveur = ServeurTube::creer(&chemin).expect("création serveur");
        let jeton = thread::spawn(move || {
            let extremite = serveur.attendre_client().expect("client connecté");
            let (mut lecteur, mut ecrivain) = extremite.scinder();
            // Lit un message service émis par le client.
            let recu = canal::lire_service(&mut lecteur).expect("lecture service");
            assert_eq!(recu, canal::MessageService::BasculerMoniteur(2));
            // Répond par un message assistant (Prêt).
            canal::ecrire_assistant(&mut ecrivain, &canal::MessageAssistant::Pret)
                .expect("écriture assistant");
            let _ = chemin_serveur; // gardé vivant le temps du thread
        });

        // Client : petite attente que le serveur soit prêt à accepter.
        let extremite = {
            let mut tentative = 0;
            loop {
                match connecter_client(&chemin) {
                    Ok(e) => break e,
                    Err(_) if tentative < 50 => {
                        tentative += 1;
                        thread::sleep(std::time::Duration::from_millis(10));
                    }
                    Err(e) => panic!("connexion client impossible : {e}"),
                }
            }
        };
        let (mut lecteur, mut ecrivain) = extremite.scinder();
        canal::ecrire_service(&mut ecrivain, &canal::MessageService::BasculerMoniteur(2))
            .expect("écriture service");
        let reponse = canal::lire_assistant(&mut lecteur).expect("lecture assistant");
        assert_eq!(reponse, canal::MessageAssistant::Pret);

        jeton.join().expect("thread serveur");
    }
}
