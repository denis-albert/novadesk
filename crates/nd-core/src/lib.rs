//! `nd-core` — orchestration d'une session NovaDesk.
//!
//! Assemble les composants (transport, session sécurisée, capture/codec/input…) et
//! porte la **machine à états** de session. À ce stade, seuls la machine à états et le
//! squelette d'assemblage existent ; le câblage réel des étages arrive en Phase 1
//! (voir `../../plan-technique/16-roadmap-planning.md`).

use std::time::Duration;

use nd_capture::{CaptureConfig, CapturedFrame, ScreenCapturer};
use nd_codec::{CodecKind, EncodedChunk, EncoderConfig, VideoDecoder, VideoEncoder};
use nd_crypto::{HandshakeRole, NoiseHandshake, NoiseSession, PeerFingerprint, SecureSession};
use nd_features::Permissions;
use nd_input::{InputInjector, MouseButton};
use nd_proto::{
    ChannelKind, InputEvent, MonitorId, NdError, NovaId, ProtocolVersion, Reliability, Result,
};
use nd_transport::{ChannelHandle, PathEstimate, Transport};

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

/// Applique un événement d'entrée reçu à un injecteur (côté machine contrôlée).
///
/// Convertit le message de protocole [`InputEvent`] (voir `nd-proto`) en appels au
/// trait [`InputInjector`] (voir `nd-input`). Voir plan 07.
pub fn apply_input(injector: &dyn InputInjector, event: &InputEvent) -> Result<()> {
    match *event {
        InputEvent::MouseMoveAbs { x, y, monitor } => {
            injector.mouse_move_abs(x, y, MonitorId(monitor))
        }
        InputEvent::MouseMoveRel { dx, dy } => injector.mouse_move_rel(dx, dy),
        InputEvent::MouseButton { button, down } => {
            let btn = match button {
                0 => MouseButton::Left,
                1 => MouseButton::Right,
                2 => MouseButton::Middle,
                3 => MouseButton::X1,
                _ => MouseButton::X2,
            };
            injector.mouse_button(btn, down)
        }
        InputEvent::Scroll { dx, dy } => injector.scroll(dx, dy),
        InputEvent::Key { scancode, down } => injector.key(scancode, down),
        InputEvent::Unicode { codepoint } => match char::from_u32(codepoint) {
            Some(ch) => injector.unicode(ch),
            None => Ok(()),
        },
    }
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

/// Longueur maximale de clair par message Noise (marge sous 65535 − tag AEAD).
const NOISE_MAX_PLAINTEXT: usize = 60_000;

/// Transport **chiffré de bout en bout** : enveloppe un [`Transport`] et chiffre toutes
/// les charges via une session Noise (voir plan 06). Le transport/relais sous-jacent ne
/// voit que du ciphertext — connaissance nulle côté serveur.
pub struct EncryptedTransport {
    inner: Box<dyn Transport>,
    session: NoiseSession,
}

impl EncryptedTransport {
    /// Empreinte de la clé statique locale (à afficher/comparer, voir plan 06 §SAS).
    #[must_use]
    pub fn local_fingerprint(&self) -> PeerFingerprint {
        self.session.local_fingerprint()
    }

    /// Empreinte de la clé statique du pair distant (après handshake).
    #[must_use]
    pub fn remote_fingerprint(&self) -> Option<PeerFingerprint> {
        self.session.remote_fingerprint()
    }
}

/// Établit une session chiffrée de bout en bout par-dessus un transport, en réalisant
/// le handshake Noise XX sur le canal de contrôle. Voir plan 06.
pub fn establish(
    mut inner: Box<dyn Transport>,
    role: HandshakeRole,
    static_private_key: &[u8],
) -> Result<EncryptedTransport> {
    let mut handshake = NoiseHandshake::new(role, static_private_key)?;
    let control = inner.open_channel(ChannelKind::Control);
    // XX : l'initiateur écrit le premier message, puis on alterne écriture/lecture.
    let mut my_turn_to_write = matches!(role, HandshakeRole::Initiator);
    while !handshake.is_finished() {
        if my_turn_to_write {
            let msg = handshake.write_message(&[])?;
            inner.send(control, msg, Reliability::Reliable)?;
        } else {
            let msg = recv_blocking(inner.as_mut())?;
            handshake.read_message(&msg)?;
        }
        my_turn_to_write = !my_turn_to_write;
    }
    let session = handshake.into_session()?;
    Ok(EncryptedTransport { inner, session })
}

/// Attend (avec délai de garde) le prochain message reçu du transport.
fn recv_blocking(inner: &mut dyn Transport) -> Result<Vec<u8>> {
    for _ in 0..3000 {
        if let Some((_handle, data)) = inner.poll_recv()? {
            return Ok(data);
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    Err(NdError::Crypto("délai de handshake Noise dépassé".into()))
}

fn read_be_u32(d: &[u8], p: &mut usize) -> Result<u32> {
    let bytes = d
        .get(*p..*p + 4)
        .ok_or_else(|| NdError::Crypto("cadre chiffré tronqué".into()))?;
    *p += 4;
    Ok(u32::from_be_bytes(
        bytes.try_into().expect("tranche de 4 octets"),
    ))
}

impl Transport for EncryptedTransport {
    fn open_channel(&mut self, kind: ChannelKind) -> ChannelHandle {
        self.inner.open_channel(kind)
    }

    fn send(&mut self, ch: ChannelHandle, data: Vec<u8>, reliability: Reliability) -> Result<()> {
        // Découpe le clair en morceaux ≤ NOISE_MAX_PLAINTEXT, chiffre chacun, et encadre :
        // [u32 n][ (u32 len, ciphertext) × n ]. Ordre préservé (flux fiable ordonné) → les
        // compteurs de nonce Noise restent synchronisés entre les deux pairs.
        let mut framed = Vec::with_capacity(data.len() + 32);
        if data.is_empty() {
            framed.extend_from_slice(&1u32.to_be_bytes());
            let ct = self.session.encrypt(&[])?;
            framed.extend_from_slice(&(ct.len() as u32).to_be_bytes());
            framed.extend_from_slice(&ct);
        } else {
            let count = data.len().div_ceil(NOISE_MAX_PLAINTEXT) as u32;
            framed.extend_from_slice(&count.to_be_bytes());
            for chunk in data.chunks(NOISE_MAX_PLAINTEXT) {
                let ct = self.session.encrypt(chunk)?;
                framed.extend_from_slice(&(ct.len() as u32).to_be_bytes());
                framed.extend_from_slice(&ct);
            }
        }
        self.inner.send(ch, framed, reliability)
    }

    fn poll_recv(&mut self) -> Result<Option<(ChannelHandle, Vec<u8>)>> {
        let Some((handle, framed)) = self.inner.poll_recv()? else {
            return Ok(None);
        };
        let mut pos = 0usize;
        let count = read_be_u32(&framed, &mut pos)? as usize;
        let mut plaintext = Vec::new();
        for _ in 0..count {
            let clen = read_be_u32(&framed, &mut pos)? as usize;
            let end = pos
                .checked_add(clen)
                .ok_or_else(|| NdError::Crypto("cadre chiffré invalide".into()))?;
            let ciphertext = framed
                .get(pos..end)
                .ok_or_else(|| NdError::Crypto("cadre chiffré tronqué".into()))?;
            pos = end;
            let pt = self.session.decrypt(ciphertext)?;
            plaintext.extend_from_slice(&pt);
        }
        Ok(Some((handle, plaintext)))
    }

    fn path_estimate(&self) -> PathEstimate {
        self.inner.path_estimate()
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
