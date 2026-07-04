//! `nd-core` — orchestration d'une session NovaDesk.
//!
//! Assemble les composants (transport, session sécurisée, capture/codec/input…) et
//! porte la **machine à états** de session. À ce stade, seuls la machine à états et le
//! squelette d'assemblage existent ; le câblage réel des étages arrive en Phase 1
//! (voir `../../plan-technique/16-roadmap-planning.md`).

use std::time::Duration;

use nd_capture::{CaptureConfig, CapturedFrame, ScreenCapturer};
use nd_codec::{CodecKind, EncodedChunk, EncoderConfig, VideoDecoder, VideoEncoder};
use nd_crypto::SecureSession;
use nd_features::Permissions;
use nd_proto::{ChannelKind, MonitorId, NdError, NovaId, ProtocolVersion, Reliability, Result};
use nd_transport::{ChannelHandle, Transport};

/// Rôle du poste local dans la session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionRole {
    /// Ce poste pilote l'autre.
    Controller,
    /// Ce poste est piloté.
    Controlled,
}

/// État courant de la session (voir le pipeline en plan 01 §2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    /// Aucune session active.
    Idle,
    /// Résolution de l'ID pair via le rendez-vous (plan 05).
    Resolving,
    /// Établissement du transport (NAT traversal / relais).
    Connecting,
    /// Handshake cryptographique en cours (plan 06).
    Handshaking,
    /// Session établie et média en cours.
    Active,
    /// Coupure réseau : tentative de reconnexion rapide (plan 04).
    Reconnecting,
    /// Session terminée.
    Closed,
}

/// Paramètres de démarrage d'une session.
#[derive(Debug, Clone)]
pub struct SessionConfig {
    pub role: SessionRole,
    pub local_id: NovaId,
    /// Pair à joindre (requis pour le rôle contrôleur).
    pub peer_id: Option<NovaId>,
    /// Permissions initiales (le contrôlé fait foi ; plan 13).
    pub permissions: Permissions,
}

/// Composants branchés sur une session active.
///
/// `Option` car ils sont installés au fil de la progression de la machine à états.
#[derive(Default)]
pub struct SessionComponents {
    pub transport: Option<Box<dyn Transport>>,
    pub secure: Option<Box<dyn SecureSession>>,
}

/// Une session NovaDesk et sa machine à états.
pub struct Session {
    config: SessionConfig,
    state: SessionState,
    components: SessionComponents,
}

impl Session {
    /// Crée une session au repos.
    #[must_use]
    pub fn new(config: SessionConfig) -> Self {
        Session {
            config,
            state: SessionState::Idle,
            components: SessionComponents::default(),
        }
    }

    #[must_use]
    pub fn state(&self) -> SessionState {
        self.state
    }

    #[must_use]
    pub fn role(&self) -> SessionRole {
        self.config.role
    }

    #[must_use]
    pub fn permissions(&self) -> Permissions {
        self.config.permissions
    }

    /// Démarre la séquence de connexion.
    ///
    /// Le rôle contrôleur exige un `peer_id`. Les transitions ultérieures
    /// (Connecting → Handshaking → Active) seront pilotées par les événements du
    /// transport et du handshake une fois ces couches implémentées.
    pub fn begin(&mut self) -> Result<()> {
        if self.config.role == SessionRole::Controller && self.config.peer_id.is_none() {
            return Err(NdError::Protocol(
                "le rôle contrôleur nécessite un peer_id".to_owned(),
            ));
        }
        self.transition(SessionState::Resolving);
        Ok(())
    }

    /// Termine la session proprement.
    pub fn close(&mut self) {
        self.components.transport = None;
        self.components.secure = None;
        self.transition(SessionState::Closed);
    }

    fn transition(&mut self, next: SessionState) {
        // Point d'accroche pour la journalisation/observabilité (plan 11/14).
        self.state = next;
    }
}

/// Version du protocole implémentée par ce moteur.
#[must_use]
pub fn engine_version() -> ProtocolVersion {
    ProtocolVersion::CURRENT
}

/// Étage **hôte** de la tranche verticale : capture d'écran → encodage H.264 → envoi
/// sur le canal vidéo du transport. Assemble les composants réels (voir plan 01 §2).
///
/// Si l'écran est statique, la dernière image disponible est ré-encodée, comme le fait
/// un vrai flux temps réel (images delta minuscules).
pub struct HostPipeline {
    capturer: Box<dyn ScreenCapturer>,
    encoder: Box<dyn VideoEncoder>,
    transport: Box<dyn Transport>,
    video_channel: ChannelHandle,
    configured: bool,
    last_frame: Option<CapturedFrame>,
    sent: usize,
}

impl HostPipeline {
    /// Construit l'étage hôte : démarre la capture et ouvre le canal vidéo.
    pub fn new(
        mut capturer: Box<dyn ScreenCapturer>,
        encoder: Box<dyn VideoEncoder>,
        mut transport: Box<dyn Transport>,
    ) -> Result<Self> {
        capturer.start(CaptureConfig {
            monitor: MonitorId(0),
            target_fps: 60,
            capture_cursor: false,
        })?;
        let video_channel = transport.open_channel(ChannelKind::Video(MonitorId(0)));
        Ok(Self {
            capturer,
            encoder,
            transport,
            video_channel,
            configured: false,
            last_frame: None,
            sent: 0,
        })
    }

    /// Capture, encode et envoie jusqu'à `target` images. Renvoie le nombre envoyé.
    pub fn run(&mut self, target: usize) -> Result<usize> {
        let max_attempts = target.saturating_mul(50) + 1000;
        let mut attempts = 0usize;
        while self.sent < target && attempts < max_attempts {
            attempts += 1;
            let frame = self.capturer.next_frame()?;
            if frame.image.is_some() {
                if !self.configured {
                    self.encoder.configure(EncoderConfig {
                        kind: CodecKind::H264,
                        width: frame.width,
                        height: frame.height,
                        target_bitrate_kbps: 8_000,
                        max_fps: 60,
                    })?;
                    self.configured = true;
                }
                self.last_frame = Some(frame);
            }
            if !self.configured {
                std::thread::sleep(Duration::from_millis(5));
                continue;
            }
            let force_keyframe = self.sent == 0;
            let chunk = {
                let frame = self
                    .last_frame
                    .as_ref()
                    .expect("configuré implique une image capturée");
                self.encoder.encode(frame, force_keyframe)?
            };
            self.transport
                .send(self.video_channel, chunk.data, Reliability::UnreliableFec)?;
            self.sent += 1;
        }
        Ok(self.sent)
    }
}

/// Étage **viewer** de la tranche verticale : réception → décodage H.264 (voir plan 01 §2).
pub struct ViewerPipeline {
    transport: Box<dyn Transport>,
    decoder: Box<dyn VideoDecoder>,
    decoded: usize,
    last_dimensions: Option<(u32, u32)>,
}

impl ViewerPipeline {
    /// Construit l'étage viewer.
    #[must_use]
    pub fn new(transport: Box<dyn Transport>, decoder: Box<dyn VideoDecoder>) -> Self {
        Self {
            transport,
            decoder,
            decoded: 0,
            last_dimensions: None,
        }
    }

    /// Reçoit et décode jusqu'à `target` images.
    /// Renvoie `(nombre décodé, dernières dimensions vues)`.
    pub fn run(&mut self, target: usize) -> Result<(usize, Option<(u32, u32)>)> {
        let max_attempts = target.saturating_mul(200) + 5000;
        let mut attempts = 0usize;
        while self.decoded < target && attempts < max_attempts {
            attempts += 1;
            match self.transport.poll_recv()? {
                Some((_handle, data)) => {
                    let chunk = EncodedChunk {
                        data,
                        is_keyframe: false,
                        monitor: MonitorId(0),
                        timestamp_us: 0,
                    };
                    if let Some(frame) = self.decoder.decode(&chunk)? {
                        self.decoded += 1;
                        self.last_dimensions = Some((frame.width, frame.height));
                    }
                }
                None => std::thread::sleep(Duration::from_millis(2)),
            }
        }
        Ok((self.decoded, self.last_dimensions))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(role: SessionRole, peer: Option<NovaId>) -> SessionConfig {
        SessionConfig {
            role,
            local_id: NovaId(123_456_789),
            peer_id: peer,
            permissions: Permissions::default(),
        }
    }

    #[test]
    fn nouvelle_session_est_idle() {
        let s = Session::new(cfg(SessionRole::Controlled, None));
        assert_eq!(s.state(), SessionState::Idle);
    }

    #[test]
    fn controleur_sans_pair_echoue() {
        let mut s = Session::new(cfg(SessionRole::Controller, None));
        assert!(s.begin().is_err());
    }

    #[test]
    fn controleur_avec_pair_passe_en_resolving() {
        let mut s = Session::new(cfg(SessionRole::Controller, Some(NovaId(987_654_321))));
        s.begin().unwrap();
        assert_eq!(s.state(), SessionState::Resolving);
    }

    #[test]
    fn close_remet_en_closed() {
        let mut s = Session::new(cfg(SessionRole::Controlled, None));
        s.close();
        assert_eq!(s.state(), SessionState::Closed);
    }
}
