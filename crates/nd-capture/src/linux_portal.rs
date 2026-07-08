//! # `linux_portal` — Négociation ScreenCast via xdg-desktop-portal (Wayland)
//!
//! Ce module réalise la première moitié du backend de capture Wayland de NovaDesk :
//! la **poignée de main D-Bus** avec `org.freedesktop.portal.ScreenCast`, qui aboutit
//! à l'obtention d'un descripteur de fichier PipeWire (`OwnedFd`) et d'un identifiant
//! de nœud (`node_id`). La seconde moitié (lecture des trames via PipeWire) est dans
//! `linux_pipewire`.
//!
//! ## Pourquoi passer par le portail ?
//! Sous Wayland, une application ne peut PAS lire l'écran directement (contrairement à
//! X11 / DXGI). Le compositeur n'expose le contenu qu'à travers le portail
//! xdg-desktop-portal, après consentement de l'utilisateur (boîte de dialogue de
//! sélection de source). Le portail publie alors un flux **PipeWire** que l'on
//! consomme côté client.
//!
//! ## Les 4 étapes de la poignée de main (toutes asynchrones, via `ashpd` 0.13)
//! 1. **CreateSession** (`create_session`) : ouvre une *session* de portail. Tout le
//!    reste de l'échange s'y rattache. Fermer la session détruit le nœud PipeWire :
//!    elle doit donc rester vivante TANT que le flux tourne (cf. `SessionKeepAlive`).
//! 2. **SelectSources** (`select_sources`) : déclare ce que l'on veut capturer
//!    (moniteur / fenêtre), le mode du curseur (`CursorMode`), le multi-sélection et
//!    le mode de persistance du jeton (`PersistMode`).
//! 3. **Start** (`start`) : déclenche la boîte de dialogue de consentement puis
//!    renvoie la liste des flux négociés. On y lit `pipe_wire_node_id()` et,
//!    éventuellement, la taille (`size()`).
//! 4. **OpenPipeWireRemote** (`open_pipe_wire_remote`) : renvoie un `OwnedFd` déjà
//!    authentifié, à passer à `pw_context_connect_fd` côté PipeWire.
//!
//! ## Pont async → sync
//! Le reste de `nd-capture` est synchrone : on exécute donc tout le flux `ashpd`
//! (basé sur `zbus`) sous `pollster::block_on`. `zbus` entretient sa propre boucle
//! d'exécution interne pour la connexion D-Bus ; `block_on` se contente de piloter
//! nos `await`.
//!
//! > Validé uniquement sur un vrai Linux Wayland disposant de xdg-desktop-portal et
//! > d'un backend ScreenCast (par ex. `xdg-desktop-portal-wlr`, `-gnome`, `-kde`).
//!
//! **Honnêteté (comme les ports précédents)** : compilé seulement avec la fonction
//! `wayland-pipewire`, sur un vrai Linux. `ashpd` est du Rust pur (sur `zbus`) : cette
//! partie *portail* est vérifiable sans `libpipewire`. Les points d'API incertains
//! portent un commentaire `// NOTE (à valider sur Linux)`.

use std::os::fd::OwnedFd;

use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};
use ashpd::desktop::PersistMode;

use nd_proto::NdError;

/// Résultat de la négociation portail : tout ce dont `linux_pipewire` a besoin.
///
/// `fd` + `node_id` identifient le flux PipeWire. `size` est un simple *indice*
/// (le portail ne le renseigne pas toujours ; la taille réelle est confirmée lors
/// de la négociation de format PipeWire).
///
/// Le champ privé `_session_kept_alive` maintient la session D-Bus (et la connexion
/// zbus sous-jacente) en vie : **le libérer ferme la session et tue le nœud PipeWire**.
/// On l'extrait via [`PortalStream::into_parts`] pour le garder vivant dans le thread
/// de capture.
pub(crate) struct PortalStream {
    /// Descripteur PipeWire authentifié renvoyé par `OpenPipeWireRemote`.
    pub(crate) fd: OwnedFd,
    /// Identifiant du nœud PipeWire à cibler dans `Stream::connect`.
    pub(crate) node_id: u32,
    /// Taille éventuellement annoncée par le portail (largeur, hauteur) en pixels.
    pub(crate) size: Option<(u32, u32)>,
    /// Poignées D-Bus à garder vivantes pour la durée de vie du flux.
    _session_kept_alive: SessionKeepAlive,
}

impl PortalStream {
    /// Sépare le `fd` (à consommer par `pw_context_connect_fd`) des poignées
    /// D-Bus à garder vivantes.
    ///
    /// Le 4e élément (`SessionKeepAlive`) DOIT rester vivant tant que la boucle
    /// PipeWire tourne. Ne pas le laisser tomber prématurément.
    pub(crate) fn into_parts(self) -> (OwnedFd, u32, Option<(u32, u32)>, SessionKeepAlive) {
        (self.fd, self.node_id, self.size, self._session_kept_alive)
    }
}

/// Boîte opaque qui garde en vie la `Session` du portail ET le proxy `Screencast`
/// (donc la connexion `zbus`). Tant qu'elle n'est pas droppée, le compositeur
/// maintient le nœud PipeWire.
///
/// On type-efface volontairement (`Box<dyn Any + Send>`) : l'unique contrat est
/// « garder ces valeurs vivantes ». Cela évite d'épingler dans le code la signature
/// générique exacte de `ashpd::desktop::Session<'_, _>` (qui varie selon les versions)
/// alors que nous n'en lisons jamais le contenu.
///
// NOTE (à valider sur Linux) : `Screencast` et `Session` reposent sur `zbus`, dont les
// proxys sont `Send + Sync`. Le `+ Send` autorise le déplacement de cette boîte vers
// le thread PipeWire. Si une version d'`ashpd` rendait ces types `!Send`, il faudrait
// conserver la session sur le thread appelant et signaler l'arrêt autrement.
pub(crate) struct SessionKeepAlive {
    #[allow(dead_code)]
    keep: Box<dyn std::any::Any + Send>,
}

/// Convertit n'importe quelle erreur affichable en [`NdError::Capture`].
fn cap<E: std::fmt::Display>(e: E) -> NdError {
    NdError::Capture(format!("portail ScreenCast : {e}"))
}

/// Exécute la poignée de main complète du portail ScreenCast et renvoie le flux
/// PipeWire prêt à être consommé.
///
/// * `cursor` : si `true`, on demande un curseur **incrusté** (`CursorMode::Embedded`)
///   dans l'image — pratique pour un partage d'écran distant où le curseur doit être
///   visible côté récepteur. Sinon le curseur est masqué (`CursorMode::Hidden`).
///
/// Toutes les erreurs (D-Bus, refus utilisateur, absence de backend…) sont mappées
/// vers `NdError::Capture`.
///
// NOTE (à valider sur Linux) : `CursorMode::Metadata` fournirait la position du curseur
// hors bande (via les métadonnées SPA du buffer) au lieu de l'incruster ; cela permettrait
// de remplir `CursorState`, mais impose de lire `spa_meta_cursor` (accès brut / unsafe).
// C'est un raffinement ultérieur ; ici on reste sur Embedded/Hidden.
pub(crate) fn negotiate_screencast(cursor: bool) -> nd_proto::Result<PortalStream> {
    pollster::block_on(async move {
        // (1) CreateSession : ouvre la session de portail.
        let proxy = Screencast::new().await.map_err(cap)?;
        let session = proxy
            .create_session(Default::default())
            .await
            .map_err(cap)?;

        // (2) SelectSources : on veut un moniteur, sélection unique, sans persistance
        //     du jeton de restauration.
        let cursor_mode = if cursor {
            CursorMode::Embedded
        } else {
            CursorMode::Hidden
        };
        // NOTE (à valider sur Linux) : `ashpd` 0.13 utilise le *builder* `SelectSourcesOptions`
        // avec des setters préfixés `set_`. C'est la forme montrée par la doc 0.13.
        proxy
            .select_sources(
                &session,
                SelectSourcesOptions::default()
                    .set_cursor_mode(cursor_mode)
                    .set_sources(SourceType::Monitor)
                    .set_multiple(false)
                    .set_persist_mode(PersistMode::DoNot),
            )
            .await
            .map_err(cap)?;
        // Remarque : `await` sur ces méthodes attend déjà le signal `Response` du portail ;
        // on peut donc ignorer le `Request<()>` renvoyé.

        // (3) Start : ouvre la boîte de consentement, puis renvoie les flux négociés.
        let response = proxy
            .start(&session, None, Default::default())
            .await
            .map_err(cap)?
            .response()
            .map_err(cap)?;

        let streams = response.streams();
        let stream = streams
            .first()
            .ok_or_else(|| NdError::Capture("le portail n'a retourné aucun flux".into()))?;

        let node_id = stream.pipe_wire_node_id();
        // NOTE (à valider sur Linux) : `Stream::size()` renvoie une taille optionnelle
        // `Option<(i32, i32)>` ; le portail ne la renseigne pas toujours.
        let size = stream.size().map(|(w, h)| (w as u32, h as u32));

        // (4) OpenPipeWireRemote : le fd authentifié à passer à PipeWire.
        let fd = proxy
            .open_pipe_wire_remote(&session, Default::default())
            .await
            .map_err(cap)?;

        // On garde vivants la session ET le proxy (donc la connexion). Ordre de drop du
        // tuple : `session` en premier (ferme proprement la session), puis `proxy`.
        let keep = SessionKeepAlive {
            keep: Box::new((session, proxy)),
        };

        Ok(PortalStream {
            fd,
            node_id,
            size,
            _session_kept_alive: keep,
        })
    })
}
