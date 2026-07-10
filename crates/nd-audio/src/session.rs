//! Flux audio **de session** : l'abstraction que `nd-core` pilote pour faire
//! circuler l'audio duplex sur un canal non fiable (plan 08).
//!
//! Deux demi-flux, réunis par [`AudioSession`] :
//!
//! - **Émission** ([`EmetteurAudio`], côté hôte) : capture une source
//!   ([`SourceAudio::Systeme`] loopback ou [`SourceAudio::Micro`]) via un
//!   [`AudioCapturer`], qui encode déjà en Opus, et **produit des
//!   [`AudioPacket`] horodatés** prêts pour le transport. `nd-core` n'a qu'à
//!   émettre `paquet.data` avec `paquet.timestamp_us` sur le canal audio.
//! - **Lecture** ([`RecepteurAudio`], côté contrôleur) : **reçoit les
//!   [`AudioPacket`]**, les remet à un [`JitterBuffer`] (réordonnancement,
//!   lissage de gigue), puis les restitue à leur échéance via un
//!   [`AudioPlayer`] (qui décode l'Opus et joue le PCM). Un trou signalé par le
//!   jitter buffer est comblé par répétition de la dernière trame (dissimulation
//!   de perte simple, sans nouvelle API de décodage).
//!
//! Cette couche est **pure orchestration** au-dessus des briques publiques
//! existantes (codec, jitter, niveaux, traits de capture/lecture) : aucun FFI,
//! aucun `unsafe`, aucune dépendance à un OS. Les fabriques matérielles
//! ([`EmetteurAudio::systeme`], [`RecepteurAudio::vers_sortie_systeme`]…)
//! délèguent aux fonctions de création par OS de la racine du crate, mais les
//! constructeurs par **injection** ([`EmetteurAudio::nouveau`],
//! [`RecepteurAudio::nouveau`]) acceptent n'importe quels [`AudioCapturer`] /
//! [`AudioPlayer`] : tout le pipeline (capture → paquets → jitter → décodage →
//! lecture) se teste ainsi **sans périphérique réel** (voir les tests de ce
//! module, qui font transiter un signal synthétique de bout en bout).

use nd_proto::Result;

use crate::capture_mixte::CapteurMixte;
use crate::codec::DecodeurOpus;
use crate::jitter::{JitterBuffer, SortieJitter, StatsJitter};
use crate::level::LevelMeter;
use crate::{AudioCapturer, AudioFormat, AudioPacket, AudioPlayer};

/// Source d'émission audio d'une session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceAudio {
    /// Audio **système** (boucle de retour / loopback) : ce que joue la machine.
    /// Format de référence : 48 kHz stéréo (profil audio générique).
    Systeme,
    /// **Microphone** (voix bidirectionnelle) : 48 kHz mono, profil voix Opus.
    Micro,
}

impl SourceAudio {
    /// Format de session par défaut de la source (celui produit par les
    /// capteurs correspondants) : stéréo pour le système, mono pour le micro.
    #[must_use]
    pub fn format_par_defaut(self) -> AudioFormat {
        match self {
            SourceAudio::Systeme => AudioFormat::default(),
            SourceAudio::Micro => AudioFormat {
                sample_rate: 48_000,
                channels: 1,
            },
        }
    }

    /// Nom court et stable de la source (piste de [`crate::Mixer`], journal…).
    #[must_use]
    pub fn nom(self) -> &'static str {
        match self {
            SourceAudio::Systeme => "systeme",
            SourceAudio::Micro => "micro",
        }
    }
}

/// **Source d'émission commutable** d'une session (ce que l'hôte transmet).
///
/// À la différence de [`SourceAudio`], qui décrit *un* périphérique de capture,
/// `SourceEmission` décrit le **mode** d'émission de la session, que
/// [`AudioSession::definir_source_emission`] fait basculer à chaud :
///
/// - [`SourceEmission::SystemeSeul`] : seul l'audio système (loopback) part —
///   le comportement historique ;
/// - [`SourceEmission::MicroSeul`] : seul le microphone part (voix) ;
/// - [`SourceEmission::SystemeEtMicro`] : les deux **mélangés** en une piste
///   stéréo bornée (voir [`crate::CapteurMixte`]).
///
/// Le mélange et le micro seul reposent sur les capteurs existants
/// ([`crate::create_microphone_capturer`]) : rien n'est ré-implémenté.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceEmission {
    /// Audio système seul (loopback) — mode par défaut, format stéréo.
    SystemeSeul,
    /// Microphone seul (voix) — format mono.
    MicroSeul,
    /// Système **et** micro mélangés (borné, anti-saturation) — format stéréo.
    SystemeEtMicro,
}

impl SourceEmission {
    /// Vrai si ce mode sollicite le microphone ([`Self::MicroSeul`] ou
    /// [`Self::SystemeEtMicro`]).
    #[must_use]
    pub fn utilise_micro(self) -> bool {
        matches!(
            self,
            SourceEmission::MicroSeul | SourceEmission::SystemeEtMicro
        )
    }

    /// Nom court et stable du mode (journal, diagnostic).
    #[must_use]
    pub fn nom(self) -> &'static str {
        match self {
            SourceEmission::SystemeSeul => "systeme",
            SourceEmission::MicroSeul => "micro",
            SourceEmission::SystemeEtMicro => "systeme+micro",
        }
    }
}

/// Résultat d'un pas de lecture ([`RecepteurAudio::tick`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvenementLecture {
    /// Une trame était due et a été décodée puis jouée.
    Joue,
    /// Trame manquante à l'échéance : comblée par répétition de la dernière
    /// trame jouée (dissimulation de perte).
    Comble,
    /// Rien à jouer pour l'instant (échéance non atteinte, tampon en cours de
    /// remplissage, ou gel après une coupure hors de portée du PLC).
    Silence,
    /// La lecture est désactivée.
    Inactif,
}

/// Demi-flux d'**émission** : une source de capture → paquets Opus horodatés.
///
/// C'est le côté « hôte » : `nd-core` appelle [`prochain_paquet`] en boucle
/// (typiquement dans un thread dédié — l'appel se cadence sur la trame de 20 ms
/// du capteur) et transporte chaque paquet renvoyé sur le canal audio non
/// fiable.
///
/// [`prochain_paquet`]: EmetteurAudio::prochain_paquet
pub struct EmetteurAudio {
    source: SourceAudio,
    capteur: Box<dyn AudioCapturer>,
    format: AudioFormat,
    actif: bool,
    vu: LevelMeter,
    mesurer_niveau: bool,
    /// Décodeur dédié à la mesure de niveau (le capteur ne fournit que de
    /// l'Opus) ; construit à la demande quand la mesure est activée.
    decodeur_vu: Option<DecodeurOpus>,
}

impl EmetteurAudio {
    /// Construit un émetteur autour d'un capteur **injecté** (testable sans
    /// matériel). L'émetteur démarre **actif**, mesure de niveau **désactivée**.
    #[must_use]
    pub fn nouveau(source: SourceAudio, capteur: Box<dyn AudioCapturer>) -> Self {
        let format = capteur.format();
        EmetteurAudio {
            source,
            capteur,
            format,
            actif: true,
            vu: LevelMeter::new(),
            mesurer_niveau: false,
            decodeur_vu: None,
        }
    }

    /// Émetteur de l'audio **système** (loopback) via la fabrique de la racine
    /// du crate (WASAPI sous Windows, PulseAudio sous Linux, ScreenCaptureKit
    /// sous macOS ; `NotImplemented` ailleurs).
    pub fn systeme() -> Result<Self> {
        Ok(Self::nouveau(
            SourceAudio::Systeme,
            crate::create_system_capturer()?,
        ))
    }

    /// Émetteur du **microphone** via la fabrique de la racine du crate.
    pub fn micro() -> Result<Self> {
        Ok(Self::nouveau(
            SourceAudio::Micro,
            crate::create_microphone_capturer()?,
        ))
    }

    /// Émetteur pour la source demandée (fabrique matérielle correspondante).
    pub fn pour_source(source: SourceAudio) -> Result<Self> {
        match source {
            SourceAudio::Systeme => Self::systeme(),
            SourceAudio::Micro => Self::micro(),
        }
    }

    /// Source de cet émetteur.
    #[must_use]
    pub fn source(&self) -> SourceAudio {
        self.source
    }

    /// Format des paquets produits (celui de session du capteur).
    #[must_use]
    pub fn format(&self) -> AudioFormat {
        self.format
    }

    /// Vrai si l'émission est active.
    #[must_use]
    pub fn actif(&self) -> bool {
        self.actif
    }

    /// Active ou coupe l'émission. Coupée, [`prochain_paquet`] renvoie
    /// `Ok(None)` sans solliciter le périphérique ; le VU-mètre se relâche vers
    /// zéro. La reprise fait redémarrer les paquets à l'horodatage média courant
    /// du capteur (le jitter buffer distant se resynchronise sur le saut).
    ///
    /// [`prochain_paquet`]: EmetteurAudio::prochain_paquet
    pub fn definir_actif(&mut self, actif: bool) {
        self.actif = actif;
    }

    /// Active/désactive la mesure de niveau d'émission. Activée, chaque paquet
    /// produit est décodé une fois pour alimenter le VU-mètre (surcoût CPU d'un
    /// décodage Opus par trame) ; désactivée par défaut.
    pub fn activer_mesure_niveau(&mut self, actif: bool) -> Result<()> {
        self.mesurer_niveau = actif;
        if actif && self.decodeur_vu.is_none() {
            self.decodeur_vu = Some(DecodeurOpus::new(self.format)?);
        }
        Ok(())
    }

    /// VU-mètre d'émission (niveaux lissés du dernier paquet mesuré).
    #[must_use]
    pub fn niveau(&self) -> &LevelMeter {
        &self.vu
    }

    /// Produit le prochain paquet à transporter, ou `Ok(None)` si l'émission est
    /// coupée. Bloque le temps d'une trame de capture (≈ 20 ms) quand elle est
    /// active — voir [`AudioCapturer::next_packet`].
    pub fn prochain_paquet(&mut self) -> Result<Option<AudioPacket>> {
        if !self.actif {
            if self.mesurer_niveau {
                self.vu.traiter(&[]); // relâchement du VU vers zéro
            }
            return Ok(None);
        }
        let paquet = self.capteur.next_packet()?;
        if self.mesurer_niveau {
            if let Some(dec) = self.decodeur_vu.as_mut() {
                if let Ok(pcm) = dec.decoder(&paquet.data) {
                    self.vu.traiter(&pcm);
                }
            }
        }
        Ok(Some(paquet))
    }
}

/// Demi-flux de **lecture** : paquets reçus → jitter buffer → décodage → sortie.
///
/// C'est le côté « contrôleur » : `nd-core` dépose chaque paquet reçu du réseau
/// via [`inserer`] (avec son instant d'arrivée local), puis appelle [`tick`] à
/// la cadence des trames pour restituer ce qui est dû. Le lissage de gigue, le
/// réordonnancement et la détection des trous reviennent au [`JitterBuffer`] ;
/// la restitution effective (décodage Opus + rendu) revient à l'[`AudioPlayer`].
///
/// [`inserer`]: RecepteurAudio::inserer
/// [`tick`]: RecepteurAudio::tick
pub struct RecepteurAudio {
    format: AudioFormat,
    jitter: JitterBuffer,
    lecteur: Box<dyn AudioPlayer>,
    actif: bool,
    vu: LevelMeter,
    mesurer_niveau: bool,
    decodeur_vu: Option<DecodeurOpus>,
    /// Dernière trame jouée, répétée pour combler un trou (PLC simple).
    dernier_paquet: Option<AudioPacket>,
}

impl RecepteurAudio {
    /// Construit un récepteur autour d'un lecteur **injecté** (testable sans
    /// matériel). Le `format` doit être celui du flux entrant (canaux du
    /// décodage de niveau, décodage du lecteur). Démarre **actif**, mesure de
    /// niveau **désactivée**.
    #[must_use]
    pub fn nouveau(format: AudioFormat, lecteur: Box<dyn AudioPlayer>) -> Self {
        RecepteurAudio {
            format,
            jitter: JitterBuffer::new(),
            lecteur,
            actif: true,
            vu: LevelMeter::new(),
            mesurer_niveau: false,
            decodeur_vu: None,
            dernier_paquet: None,
        }
    }

    /// Récepteur vers la **sortie système** par défaut (fabrique de la racine :
    /// `WasapiPlayer` sous Windows, `PulsePlayer` sous Linux, `CoreAudioPlayer`
    /// sous macOS). Le format est 48 kHz stéréo, celui qu'attend le décodeur
    /// interne de ces lecteurs — donc adapté au flux **système**. Un flux mono
    /// (micro) nécessiterait une sortie mono, hors périmètre de cette fabrique.
    pub fn vers_sortie_systeme() -> Result<Self> {
        Ok(Self::nouveau(
            AudioFormat::default(),
            crate::create_system_player()?,
        ))
    }

    /// Format attendu du flux entrant.
    #[must_use]
    pub fn format(&self) -> AudioFormat {
        self.format
    }

    /// Vrai si la lecture est active.
    #[must_use]
    pub fn actif(&self) -> bool {
        self.actif
    }

    /// Active ou coupe la lecture. Coupée, [`tick`](Self::tick) renvoie
    /// `EvenementLecture::Inactif` sans rien restituer ; les paquets déposés
    /// continuent d'alimenter le jitter buffer.
    pub fn definir_actif(&mut self, actif: bool) {
        self.actif = actif;
    }

    /// Active/désactive la mesure de niveau de lecture. Activée, chaque trame
    /// jouée est décodée une fois de plus pour le VU-mètre (surcoût d'un
    /// décodage Opus par trame) ; désactivée par défaut.
    pub fn activer_mesure_niveau(&mut self, actif: bool) -> Result<()> {
        self.mesurer_niveau = actif;
        if actif && self.decodeur_vu.is_none() {
            self.decodeur_vu = Some(DecodeurOpus::new(self.format)?);
        }
        Ok(())
    }

    /// VU-mètre de lecture (niveaux lissés de la dernière trame jouée).
    #[must_use]
    pub fn niveau(&self) -> &LevelMeter {
        &self.vu
    }

    /// Dépose un paquet reçu du réseau, `arrivee_us` étant l'instant local de
    /// réception (horloge monotone en µs, la même que celle passée à
    /// [`tick`](Self::tick)). Les paquets en désordre, en double ou trop vieux
    /// sont gérés par le jitter buffer (voir [`StatsJitter`]).
    pub fn inserer(&mut self, paquet: AudioPacket, arrivee_us: u64) {
        self.jitter.inserer(paquet, arrivee_us);
    }

    /// Restitue ce qui est dû à l'instant `maintenant_us` (même horloge que les
    /// `arrivee_us`). Au plus une trame par appel ; à appeler à la cadence des
    /// trames (≈ toutes les 20 ms).
    pub fn tick(&mut self, maintenant_us: u64) -> Result<EvenementLecture> {
        if !self.actif {
            return Ok(EvenementLecture::Inactif);
        }
        match self.jitter.suivant(maintenant_us) {
            SortieJitter::Paquet(paquet) => {
                self.lecteur.play(&paquet)?;
                if self.mesurer_niveau {
                    if let Some(dec) = self.decodeur_vu.as_mut() {
                        if let Ok(pcm) = dec.decoder(&paquet.data) {
                            self.vu.traiter(&pcm);
                        }
                    }
                }
                self.dernier_paquet = Some(paquet);
                Ok(EvenementLecture::Joue)
            }
            SortieJitter::Trou { .. } => {
                // Dissimulation de perte simple : on rejoue la dernière trame
                // (le jitter buffer borne déjà les trous consécutifs). Sans
                // trame précédente (démarrage), on laisse un silence.
                if let Some(precedent) = self.dernier_paquet.clone() {
                    self.lecteur.play(&precedent)?;
                    Ok(EvenementLecture::Comble)
                } else {
                    Ok(EvenementLecture::Silence)
                }
            }
            SortieJitter::Attente => {
                if self.mesurer_niveau {
                    self.vu.traiter(&[]); // relâchement du VU sur silence
                }
                Ok(EvenementLecture::Silence)
            }
        }
    }

    /// Compteurs d'événements du jitter buffer (retards, doublons, trous,
    /// resynchronisations).
    #[must_use]
    pub fn stats(&self) -> StatsJitter {
        self.jitter.stats()
    }

    /// Profondeur cible courante du jitter buffer (µs).
    #[must_use]
    pub fn delai_cible_us(&self) -> u64 {
        self.jitter.delai_cible_us()
    }

    /// Nombre de paquets en attente dans le jitter buffer.
    #[must_use]
    pub fn en_attente(&self) -> usize {
        self.jitter.longueur()
    }
}

/// Session audio **duplex** : réunit une émission et une lecture optionnelles.
///
/// C'est la façade que `nd-core` manipule pour un point de terminaison (hôte ou
/// contrôleur). Les paquets ne font que **circuler** : `nd-core` les tire par
/// [`produire`](Self::produire) pour les envoyer, et les redonne par
/// [`recevoir`](Self::recevoir) à leur arrivée, puis cadence la restitution par
/// [`tick_lecture`](Self::tick_lecture). Les deux directions sont indépendantes
/// (une session peut être émettrice seule, réceptrice seule, ou duplex).
pub struct AudioSession {
    emission: Option<EmetteurAudio>,
    lecture: Option<RecepteurAudio>,
    /// Mode d'émission courant, maintenu par
    /// [`AudioSession::definir_source_emission`] (et déduit de l'émetteur
    /// injecté à la construction).
    source_emission: SourceEmission,
    /// Résultat de la dernière tentative d'activation du micro par
    /// [`AudioSession::definir_source_emission`] : `true` si le micro a bien été
    /// ouvert, `false` s'il était absent (repli système) ou jamais sollicité.
    micro_disponible: bool,
}

impl AudioSession {
    /// Construit une session à partir de demi-flux **injectés** (testable sans
    /// matériel). Chacun est optionnel : `None` désactive la direction.
    ///
    /// Le mode d'émission ([`SourceEmission`]) est déduit de la source de
    /// l'émetteur injecté (micro → [`SourceEmission::MicroSeul`], sinon
    /// [`SourceEmission::SystemeSeul`]) ; `micro_disponible()` démarre à `false`
    /// tant qu'aucune bascule matérielle n'a sondé le micro.
    #[must_use]
    pub fn nouvelle(emission: Option<EmetteurAudio>, lecture: Option<RecepteurAudio>) -> Self {
        let source_emission = match emission.as_ref().map(EmetteurAudio::source) {
            Some(SourceAudio::Micro) => SourceEmission::MicroSeul,
            _ => SourceEmission::SystemeSeul,
        };
        AudioSession {
            emission,
            lecture,
            source_emission,
            micro_disponible: false,
        }
    }

    /// Session duplex « système » via les fabriques matérielles : capture de
    /// l'audio système en émission, sortie système en lecture (48 kHz stéréo
    /// de bout en bout). Constructeur indépendant de l'OS (délègue aux fabriques
    /// de la racine, qui gèrent chaque plateforme).
    pub fn duplex_systeme() -> Result<Self> {
        Ok(Self::nouvelle(
            Some(EmetteurAudio::systeme()?),
            Some(RecepteurAudio::vers_sortie_systeme()?),
        ))
    }

    /// Remplace (ou retire avec `None`) le demi-flux d'émission — permet de
    /// **changer de source** à chaud (`nd-core` fournit un nouvel émetteur, ex.
    /// [`EmetteurAudio::pour_source`]).
    pub fn definir_emission(&mut self, emission: Option<EmetteurAudio>) {
        self.emission = emission;
    }

    /// Remplace (ou retire avec `None`) le demi-flux de lecture.
    pub fn definir_lecture(&mut self, lecture: Option<RecepteurAudio>) {
        self.lecture = lecture;
    }

    /// Tire le prochain paquet à transporter (`Ok(None)` si pas d'émission ou
    /// émission coupée). Voir [`EmetteurAudio::prochain_paquet`].
    pub fn produire(&mut self) -> Result<Option<AudioPacket>> {
        match self.emission.as_mut() {
            Some(e) => e.prochain_paquet(),
            None => Ok(None),
        }
    }

    /// Dépose un paquet reçu du réseau dans le demi-flux de lecture (ignoré s'il
    /// n'y a pas de lecture). Voir [`RecepteurAudio::inserer`].
    pub fn recevoir(&mut self, paquet: AudioPacket, arrivee_us: u64) {
        if let Some(r) = self.lecture.as_mut() {
            r.inserer(paquet, arrivee_us);
        }
    }

    /// Cadence la restitution (`EvenementLecture::Inactif` s'il n'y a pas de
    /// lecture). Voir [`RecepteurAudio::tick`].
    pub fn tick_lecture(&mut self, maintenant_us: u64) -> Result<EvenementLecture> {
        match self.lecture.as_mut() {
            Some(r) => r.tick(maintenant_us),
            None => Ok(EvenementLecture::Inactif),
        }
    }

    /// Active/coupe l'émission (sans effet s'il n'y a pas d'émetteur).
    pub fn definir_emission_active(&mut self, actif: bool) {
        if let Some(e) = self.emission.as_mut() {
            e.definir_actif(actif);
        }
    }

    /// Active/coupe la lecture (sans effet s'il n'y a pas de récepteur).
    pub fn definir_lecture_active(&mut self, actif: bool) {
        if let Some(r) = self.lecture.as_mut() {
            r.definir_actif(actif);
        }
    }

    /// Vrai si une émission existe et est active.
    #[must_use]
    pub fn emission_active(&self) -> bool {
        self.emission.as_ref().is_some_and(EmetteurAudio::actif)
    }

    /// Vrai si une lecture existe et est active.
    #[must_use]
    pub fn lecture_active(&self) -> bool {
        self.lecture.as_ref().is_some_and(RecepteurAudio::actif)
    }

    /// Source d'émission courante, le cas échéant.
    ///
    /// Décrit le **capteur** sous-jacent de l'émetteur ([`SourceAudio`], à deux
    /// états). Pour le mélange système+micro, il vaut [`SourceAudio::Systeme`]
    /// (la sortie est stéréo, au format système) ; le mode d'émission complet à
    /// trois états s'obtient par [`Self::source_emission_courante`].
    #[must_use]
    pub fn source_emission(&self) -> Option<SourceAudio> {
        self.emission.as_ref().map(EmetteurAudio::source)
    }

    /// **Mode d'émission courant** (à trois états, [`SourceEmission`]).
    ///
    /// Reflète le dernier [`Self::definir_source_emission`] réussi (ou son repli
    /// effectif si le micro était absent), ou la source déduite à la
    /// construction.
    #[must_use]
    pub fn source_emission_courante(&self) -> SourceEmission {
        self.source_emission
    }

    /// Vrai si la dernière bascule de source a réellement ouvert le micro.
    ///
    /// `false` si le micro était absent (repli système effectué), si la
    /// plateforme ne sait pas le capturer, ou si aucune bascule sollicitant le
    /// micro n'a encore eu lieu. Statut **interrogeable** du repli dégradé.
    #[must_use]
    pub fn micro_disponible(&self) -> bool {
        self.micro_disponible
    }

    /// **Commute la source d'émission** à chaud en fabriquant les capteurs
    /// matériels via les fabriques de la racine du crate
    /// ([`crate::create_system_capturer`] / [`crate::create_microphone_capturer`]).
    ///
    /// Repli **sûr** : si un mode réclame le micro mais qu'il est absent
    /// (périphérique manquant, plateforme sans capture micro), la session
    /// bascule sur [`SourceEmission::SystemeSeul`] au lieu d'échouer, et
    /// [`Self::micro_disponible`] renvoie `false`. L'état actif/coupé et la
    /// mesure de niveau de l'émetteur précédent sont **préservés** au travers de
    /// la bascule (pas de coupure perceptible).
    ///
    /// N'échoue que si le capteur **système** — le plancher garanti — ne peut
    /// être ouvert (`Err`, l'émission précédente est alors laissée en place).
    ///
    /// C'est le point d'entrée que `nd-core` appelle pour « transmettre le
    /// micro » : `audio.definir_source_emission(SourceEmission::SystemeEtMicro)`.
    pub fn definir_source_emission(&mut self, source: SourceEmission) -> Result<()> {
        self.definir_source_emission_avec(
            source,
            crate::create_system_capturer,
            crate::create_microphone_capturer,
        )
    }

    /// Variante **injectable** de [`Self::definir_source_emission`] : les
    /// capteurs sont fabriqués par `fabrique_systeme` / `fabrique_micro` plutôt
    /// que par les fabriques matérielles. Même logique de repli et de
    /// préservation d'état — testable sans périphérique réel.
    ///
    /// `fabrique_micro` renvoyant `Err` **simule un micro absent** : la bascule
    /// se replie alors sur le système sans propager l'erreur (repli dégradé).
    /// Une `fabrique_systeme` en échec, elle, est propagée (aucun repli possible
    /// pour le plancher système).
    pub fn definir_source_emission_avec<FabSys, FabMic>(
        &mut self,
        source: SourceEmission,
        fabrique_systeme: FabSys,
        fabrique_micro: FabMic,
    ) -> Result<()>
    where
        FabSys: FnOnce() -> Result<Box<dyn AudioCapturer>>,
        FabMic: FnOnce() -> Result<Box<dyn AudioCapturer>>,
    {
        // État à reporter sur le nouvel émetteur (bascule transparente).
        let (etait_actif, mesurait) = self
            .emission
            .as_ref()
            .map_or((true, false), |e| (e.actif(), e.mesurer_niveau));

        // Fabrique le capteur voulu, avec repli système si le micro manque.
        // `capteur` = capteur final ; `mode` = mode réellement atteint ;
        // `micro_ok` = le micro a-t-il bien été ouvert ?
        let (capteur, mode, micro_ok): (Box<dyn AudioCapturer>, SourceEmission, bool) = match source
        {
            SourceEmission::SystemeSeul => {
                (fabrique_systeme()?, SourceEmission::SystemeSeul, false)
            }
            SourceEmission::MicroSeul => match fabrique_micro() {
                Ok(micro) => (micro, SourceEmission::MicroSeul, true),
                // Micro absent : repli sur le système seul.
                Err(_) => (fabrique_systeme()?, SourceEmission::SystemeSeul, false),
            },
            SourceEmission::SystemeEtMicro => {
                // Le système est requis dans tous les cas (plancher + repli).
                let systeme = fabrique_systeme()?;
                match fabrique_micro() {
                    Ok(micro) => (
                        Box::new(CapteurMixte::nouveau(systeme, micro)?),
                        SourceEmission::SystemeEtMicro,
                        true,
                    ),
                    // Micro absent : on émet le système seul (déjà fabriqué).
                    Err(_) => (systeme, SourceEmission::SystemeSeul, false),
                }
            }
        };

        // Le capteur mixte comme le loopback produisent du stéréo au format
        // système ; le micro seul, du mono. On étiquette l'émetteur d'après le
        // mode atteint (le mélange reste étiqueté « système » car stéréo).
        let source_capteur = match mode {
            SourceEmission::MicroSeul => SourceAudio::Micro,
            _ => SourceAudio::Systeme,
        };
        let mut emetteur = EmetteurAudio::nouveau(source_capteur, capteur);
        emetteur.definir_actif(etait_actif);
        if mesurait {
            emetteur.activer_mesure_niveau(true)?;
        }

        self.emission = Some(emetteur);
        self.source_emission = mode;
        self.micro_disponible = micro_ok;
        Ok(())
    }

    /// Active/désactive la mesure de niveau sur les deux directions présentes.
    pub fn activer_mesures(&mut self, actif: bool) -> Result<()> {
        if let Some(e) = self.emission.as_mut() {
            e.activer_mesure_niveau(actif)?;
        }
        if let Some(r) = self.lecture.as_mut() {
            r.activer_mesure_niveau(actif)?;
        }
        Ok(())
    }

    /// VU-mètre d'émission, le cas échéant.
    #[must_use]
    pub fn niveau_emission(&self) -> Option<&LevelMeter> {
        self.emission.as_ref().map(EmetteurAudio::niveau)
    }

    /// VU-mètre de lecture, le cas échéant.
    #[must_use]
    pub fn niveau_lecture(&self) -> Option<&LevelMeter> {
        self.lecture.as_ref().map(RecepteurAudio::niveau)
    }

    /// Compteurs du jitter buffer de lecture, le cas échéant.
    #[must_use]
    pub fn stats_lecture(&self) -> Option<StatsJitter> {
        self.lecture.as_ref().map(RecepteurAudio::stats)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use nd_proto::NdError;

    use super::*;
    use crate::codec::{echantillons_par_trame, DecodeurOpus, EncodeurOpus};
    use crate::rms;

    /// Capteur **synthétique** : encode une sinusoïde continue en Opus, trame
    /// par trame, avec un horodatage média régulier (20 ms). Remplace un
    /// périphérique réel pour prouver le pipeline sans matériel.
    struct CapteurSynthetique {
        format: AudioFormat,
        enc: EncodeurOpus,
        freq: f32,
        trame: u64,
    }

    impl CapteurSynthetique {
        fn nouveau(format: AudioFormat, freq: f32) -> Result<Self> {
            Ok(CapteurSynthetique {
                format,
                enc: EncodeurOpus::new(format)?,
                freq,
                trame: 0,
            })
        }
    }

    impl AudioCapturer for CapteurSynthetique {
        fn format(&self) -> AudioFormat {
            self.format
        }

        fn next_packet(&mut self) -> Result<AudioPacket> {
            let par_canal = echantillons_par_trame(self.format);
            let base = self.trame as usize * par_canal;
            let mut pcm = Vec::with_capacity(self.enc.valeurs_par_trame());
            for i in 0..par_canal {
                // Phase continue d'une trame à l'autre : sinusoïde propre.
                let t = (base + i) as f32 / self.format.sample_rate as f32;
                let v = (std::f32::consts::TAU * self.freq * t).sin() * 0.5;
                for _ in 0..self.format.channels {
                    pcm.push(v);
                }
            }
            let data = self.enc.encoder(&pcm)?;
            let timestamp_us =
                self.trame * par_canal as u64 * 1_000_000 / u64::from(self.format.sample_rate);
            self.trame += 1;
            Ok(AudioPacket { data, timestamp_us })
        }
    }

    /// Poignées partagées vers ce qu'un [`LecteurMemoire`] a restitué : le PCM
    /// décodé accumulé et le nombre de trames jouées (relues par le test).
    #[derive(Clone)]
    struct SortieCapturee {
        pcm: Arc<Mutex<Vec<f32>>>,
        trames: Arc<Mutex<usize>>,
    }

    impl SortieCapturee {
        fn nouvelle() -> Self {
            SortieCapturee {
                pcm: Arc::new(Mutex::new(Vec::new())),
                trames: Arc::new(Mutex::new(0)),
            }
        }

        fn nb_trames(&self) -> usize {
            *self.trames.lock().unwrap()
        }

        fn pcm(&self) -> Vec<f32> {
            self.pcm.lock().unwrap().clone()
        }
    }

    /// Lecteur **mémoire** : décode chaque paquet et accumule le PCM dans une
    /// [`SortieCapturee`] partagée (le test vérifie ensuite la cohérence).
    struct LecteurMemoire {
        dec: DecodeurOpus,
        sortie: SortieCapturee,
    }

    impl LecteurMemoire {
        fn nouveau(format: AudioFormat, sortie: SortieCapturee) -> Result<Self> {
            Ok(LecteurMemoire {
                dec: DecodeurOpus::new(format)?,
                sortie,
            })
        }
    }

    impl AudioPlayer for LecteurMemoire {
        fn play(&mut self, packet: &AudioPacket) -> Result<()> {
            let pcm = self.dec.decoder(&packet.data)?;
            self.sortie.pcm.lock().unwrap().extend_from_slice(&pcm);
            *self.sortie.trames.lock().unwrap() += 1;
            Ok(())
        }
    }

    fn emetteur_synthetique(format: AudioFormat, freq: f32) -> EmetteurAudio {
        let capteur = Box::new(CapteurSynthetique::nouveau(format, freq).expect("capteur"));
        EmetteurAudio::nouveau(SourceAudio::Systeme, capteur)
    }

    fn recepteur_memoire(format: AudioFormat) -> (RecepteurAudio, SortieCapturee) {
        let sortie = SortieCapturee::nouvelle();
        let lecteur = Box::new(LecteurMemoire::nouveau(format, sortie.clone()).expect("lecteur"));
        (RecepteurAudio::nouveau(format, lecteur), sortie)
    }

    #[test]
    fn source_formats_par_defaut() {
        assert_eq!(SourceAudio::Systeme.format_par_defaut().channels, 2);
        assert_eq!(SourceAudio::Micro.format_par_defaut().channels, 1);
        assert_eq!(SourceAudio::Micro.format_par_defaut().sample_rate, 48_000);
    }

    #[test]
    fn emetteur_actif_produit_inactif_rien() {
        let format = AudioFormat::default();
        let mut em = emetteur_synthetique(format, 440.0);
        assert!(em.actif());
        assert_eq!(em.source(), SourceAudio::Systeme);

        let p = em.prochain_paquet().expect("production").expect("actif");
        assert!(!p.data.is_empty());
        assert_eq!(p.timestamp_us, 0);
        // La trame suivante avance l'horloge média d'exactement 20 ms.
        let p2 = em.prochain_paquet().expect("production").expect("actif");
        assert_eq!(p2.timestamp_us, 20_000);

        // Coupée : plus aucun paquet, sans solliciter le capteur.
        em.definir_actif(false);
        assert!(em.prochain_paquet().expect("production").is_none());
        // Reprise : les paquets repartent (horloge média du capteur poursuivie).
        em.definir_actif(true);
        assert!(em.prochain_paquet().expect("production").is_some());
    }

    /// Preuve **sans matériel** : signal synthétique → paquets Opus → jitter →
    /// décodage → lecture, avec cohérence du signal restitué (nombre exact
    /// d'échantillons et énergie préservée).
    #[test]
    fn bout_en_bout_signal_synthetique_coherent() {
        let format = AudioFormat::default();
        let mut em = emetteur_synthetique(format, 440.0);
        let (mut rec, sortie) = recepteur_memoire(format);

        let n = 25usize;
        let mut paquets = Vec::with_capacity(n);
        for _ in 0..n {
            paquets.push(em.prochain_paquet().expect("production").expect("actif"));
        }
        let dernier_ts = paquets.last().unwrap().timestamp_us;

        // Dépôt sans gigue (arrivée = horodatage média), puis restitution.
        for p in paquets {
            let ts = p.timestamp_us;
            rec.inserer(p, ts);
        }
        // Tampon plein et contigu : n tirages bien après les échéances donnent
        // n trames jouées, sans trou.
        for _ in 0..n {
            assert_eq!(
                rec.tick(dernier_ts + 1_000_000).expect("tick"),
                EvenementLecture::Joue
            );
        }
        assert_eq!(rec.stats().trous, 0);
        assert_eq!(sortie.nb_trames(), n);

        let pcm = sortie.pcm();
        assert_eq!(
            pcm.len(),
            n * echantillons_par_trame(format) * usize::from(format.channels)
        );
        // Sinusoïde 0.5 → RMS théorique ≈ 0.354 ; Opus la restitue fidèlement.
        let sortie_rms = rms(&pcm);
        assert!(
            (0.2..0.5).contains(&sortie_rms),
            "RMS restitué incohérent : {sortie_rms}"
        );
    }

    #[test]
    fn lecture_comble_un_trou_par_repetition() {
        let format = AudioFormat::default();
        let mut em = emetteur_synthetique(format, 330.0);
        let (mut rec, sortie) = recepteur_memoire(format);

        let p0 = em.prochain_paquet().expect("prod").expect("actif");
        let _p1 = em.prochain_paquet().expect("prod").expect("actif"); // perdu
        let p2 = em.prochain_paquet().expect("prod").expect("actif");

        rec.inserer(p0, 0);
        rec.inserer(p2, 40_000); // la trame de 20 ms manque

        // Échéance p0 = 20 ms (délai min par défaut).
        assert_eq!(rec.tick(20_000).expect("tick"), EvenementLecture::Joue);
        // Trame de 20 ms absente : comblée par répétition de p0.
        assert_eq!(rec.tick(40_000).expect("tick"), EvenementLecture::Comble);
        // La lecture reprend sur p2 à son échéance.
        assert_eq!(rec.tick(60_000).expect("tick"), EvenementLecture::Joue);

        assert_eq!(rec.stats().trous, 1);
        // 2 trames jouées + 1 comblée = 3 restitutions au lecteur.
        assert_eq!(sortie.nb_trames(), 3);
    }

    #[test]
    fn lecture_inactive_ne_restitue_rien() {
        let format = AudioFormat::default();
        let mut em = emetteur_synthetique(format, 440.0);
        let (mut rec, sortie) = recepteur_memoire(format);
        let p = em.prochain_paquet().expect("prod").expect("actif");
        rec.inserer(p, 0);
        rec.definir_actif(false);
        assert_eq!(
            rec.tick(1_000_000).expect("tick"),
            EvenementLecture::Inactif
        );
        assert_eq!(sortie.nb_trames(), 0);
    }

    #[test]
    fn mesure_niveau_emission_detecte_le_signal() {
        let format = AudioFormat::default();
        let mut em = emetteur_synthetique(format, 440.0);
        em.activer_mesure_niveau(true).expect("mesure");
        // Un VU-mètre neuf est au silence.
        assert!(em.niveau().est_silence());
        for _ in 0..10 {
            em.prochain_paquet().expect("prod").expect("actif");
        }
        // Après quelques trames de sinusoïde, le niveau monte au-dessus du seuil.
        assert!(!em.niveau().est_silence());
        assert!(em.niveau().rms() > 0.1);
    }

    #[test]
    fn session_duplex_boucle_locale() {
        let format = AudioFormat::default();
        let em = emetteur_synthetique(format, 440.0);
        let (rec, sortie) = recepteur_memoire(format);
        let mut session = AudioSession::nouvelle(Some(em), Some(rec));

        assert!(session.emission_active());
        assert!(session.lecture_active());
        assert_eq!(session.source_emission(), Some(SourceAudio::Systeme));

        // Boucle « réseau parfait » : produire → recevoir → tick.
        let n = 20usize;
        let mut paquets = Vec::with_capacity(n);
        for _ in 0..n {
            paquets.push(session.produire().expect("produire").expect("actif"));
        }
        let dernier_ts = paquets.last().unwrap().timestamp_us;
        for p in paquets {
            let ts = p.timestamp_us;
            session.recevoir(p, ts);
        }
        for _ in 0..n {
            assert_eq!(
                session.tick_lecture(dernier_ts + 1_000_000).expect("tick"),
                EvenementLecture::Joue
            );
        }
        assert_eq!(sortie.nb_trames(), n);
        assert!(!sortie.pcm().is_empty());
        assert_eq!(session.stats_lecture().map(|s| s.trous), Some(0));
    }

    #[test]
    fn session_sans_lecture_tick_inactif() {
        let format = AudioFormat::default();
        let em = emetteur_synthetique(format, 440.0);
        let mut session = AudioSession::nouvelle(Some(em), None);
        assert!(!session.lecture_active());
        assert_eq!(
            session.tick_lecture(1_000_000).expect("tick"),
            EvenementLecture::Inactif
        );
        // La direction d'émission reste opérationnelle.
        assert!(session.produire().expect("produire").is_some());
    }

    #[test]
    fn changement_de_source_a_chaud() {
        let format = AudioFormat::default();
        let mut session = AudioSession::nouvelle(Some(emetteur_synthetique(format, 440.0)), None);
        assert_eq!(session.source_emission(), Some(SourceAudio::Systeme));

        // Nouvelle source injectée (ici un émetteur micro synthétique mono).
        let mono = AudioFormat {
            sample_rate: 48_000,
            channels: 1,
        };
        let capteur = Box::new(CapteurSynthetique::nouveau(mono, 220.0).expect("capteur"));
        session.definir_emission(Some(EmetteurAudio::nouveau(SourceAudio::Micro, capteur)));
        assert_eq!(session.source_emission(), Some(SourceAudio::Micro));
        let p = session.produire().expect("produire").expect("actif");
        assert!(!p.data.is_empty());

        // Retrait de l'émission.
        session.definir_emission(None);
        assert!(!session.emission_active());
        assert!(session.produire().expect("produire").is_none());
    }

    #[test]
    fn session_est_send() {
        fn assert_send<T: Send>() {}
        assert_send::<AudioSession>();
        assert_send::<EmetteurAudio>();
        assert_send::<RecepteurAudio>();
    }

    /// Format mono du micro synthétique de test.
    const FORMAT_MICRO: AudioFormat = AudioFormat {
        sample_rate: 48_000,
        channels: 1,
    };

    /// Fabrique un capteur **système** synthétique (stéréo 48 kHz).
    fn fabrique_systeme_ok() -> Result<Box<dyn AudioCapturer>> {
        Ok(Box::new(
            CapteurSynthetique::nouveau(AudioFormat::default(), 440.0).expect("capteur"),
        ))
    }

    /// Fabrique un capteur **micro** synthétique (mono 48 kHz).
    fn fabrique_micro_ok() -> Result<Box<dyn AudioCapturer>> {
        Ok(Box::new(
            CapteurSynthetique::nouveau(FORMAT_MICRO, 220.0).expect("capteur"),
        ))
    }

    /// Simule un micro **absent** (fabrique en échec).
    fn fabrique_micro_absent() -> Result<Box<dyn AudioCapturer>> {
        Err(NdError::NotImplemented("micro simulé absent"))
    }

    /// Décode un paquet et renvoie le nombre de valeurs `f32` (échantillons ×
    /// canaux) — sert à distinguer une trame stéréo (1920) d'une mono (960).
    fn valeurs_decodees(paquet: &AudioPacket, format: AudioFormat) -> usize {
        DecodeurOpus::new(format)
            .expect("décodeur")
            .decoder(&paquet.data)
            .expect("décodage")
            .len()
    }

    #[test]
    fn nouvelle_deduit_le_mode_depuis_l_emetteur() {
        let sys = AudioSession::nouvelle(
            Some(emetteur_synthetique(AudioFormat::default(), 440.0)),
            None,
        );
        assert_eq!(sys.source_emission_courante(), SourceEmission::SystemeSeul);
        assert!(!sys.micro_disponible());

        let capteur = Box::new(CapteurSynthetique::nouveau(FORMAT_MICRO, 220.0).expect("capteur"));
        let mic = AudioSession::nouvelle(
            Some(EmetteurAudio::nouveau(SourceAudio::Micro, capteur)),
            None,
        );
        assert_eq!(mic.source_emission_courante(), SourceEmission::MicroSeul);
    }

    #[test]
    fn commutation_source_emission_les_trois_modes() {
        let mut session = AudioSession::nouvelle(None, None);
        assert_eq!(
            session.source_emission_courante(),
            SourceEmission::SystemeSeul
        );
        assert!(!session.micro_disponible());

        // Micro seul : mono, micro disponible.
        session
            .definir_source_emission_avec(
                SourceEmission::MicroSeul,
                fabrique_systeme_ok,
                fabrique_micro_ok,
            )
            .expect("bascule micro");
        assert_eq!(
            session.source_emission_courante(),
            SourceEmission::MicroSeul
        );
        assert!(session.micro_disponible());
        assert_eq!(session.source_emission(), Some(SourceAudio::Micro));
        let p = session.produire().expect("produire").expect("actif");
        assert_eq!(valeurs_decodees(&p, FORMAT_MICRO), 960);

        // Système + micro : mélange stéréo, micro disponible.
        session
            .definir_source_emission_avec(
                SourceEmission::SystemeEtMicro,
                fabrique_systeme_ok,
                fabrique_micro_ok,
            )
            .expect("bascule mixte");
        assert_eq!(
            session.source_emission_courante(),
            SourceEmission::SystemeEtMicro
        );
        assert!(session.micro_disponible());
        // Le mélange est étiqueté « système » (sortie stéréo).
        assert_eq!(session.source_emission(), Some(SourceAudio::Systeme));
        let p = session.produire().expect("produire").expect("actif");
        assert_eq!(valeurs_decodees(&p, AudioFormat::default()), 1920);

        // Retour au système seul.
        session
            .definir_source_emission_avec(
                SourceEmission::SystemeSeul,
                fabrique_systeme_ok,
                fabrique_micro_ok,
            )
            .expect("bascule système");
        assert_eq!(
            session.source_emission_courante(),
            SourceEmission::SystemeSeul
        );
        assert!(!session.micro_disponible());
        let p = session.produire().expect("produire").expect("actif");
        assert_eq!(valeurs_decodees(&p, AudioFormat::default()), 1920);
    }

    #[test]
    fn repli_systeme_si_micro_absent() {
        let mut session = AudioSession::nouvelle(None, None);

        // Micro seul demandé, micro absent → repli système sans erreur.
        session
            .definir_source_emission_avec(
                SourceEmission::MicroSeul,
                fabrique_systeme_ok,
                fabrique_micro_absent,
            )
            .expect("repli sans erreur");
        assert_eq!(
            session.source_emission_courante(),
            SourceEmission::SystemeSeul
        );
        assert!(!session.micro_disponible());
        let p = session.produire().expect("produire").expect("actif");
        assert_eq!(valeurs_decodees(&p, AudioFormat::default()), 1920);

        // Système + micro demandé, micro absent → système seul émis.
        session
            .definir_source_emission_avec(
                SourceEmission::SystemeEtMicro,
                fabrique_systeme_ok,
                fabrique_micro_absent,
            )
            .expect("repli sans erreur");
        assert_eq!(
            session.source_emission_courante(),
            SourceEmission::SystemeSeul
        );
        assert!(!session.micro_disponible());
        let p = session.produire().expect("produire").expect("actif");
        assert_eq!(valeurs_decodees(&p, AudioFormat::default()), 1920);
    }

    #[test]
    fn bascule_source_preserve_l_etat_actif() {
        let mut session = AudioSession::nouvelle(None, None);
        session
            .definir_source_emission_avec(
                SourceEmission::SystemeSeul,
                fabrique_systeme_ok,
                fabrique_micro_ok,
            )
            .expect("bascule système");
        // On coupe l'émission…
        session.definir_emission_active(false);
        assert!(!session.emission_active());
        // …et la bascule de source conserve l'état coupé.
        session
            .definir_source_emission_avec(
                SourceEmission::MicroSeul,
                fabrique_systeme_ok,
                fabrique_micro_ok,
            )
            .expect("bascule micro");
        assert_eq!(
            session.source_emission_courante(),
            SourceEmission::MicroSeul
        );
        assert!(!session.emission_active());
        assert!(session.produire().expect("produire").is_none()); // toujours coupée
    }

    #[test]
    fn echec_systeme_propage_et_laisse_l_emission() {
        fn fabrique_systeme_ko() -> Result<Box<dyn AudioCapturer>> {
            Err(NdError::NotImplemented("système simulé absent"))
        }
        let mut session = AudioSession::nouvelle(
            Some(emetteur_synthetique(AudioFormat::default(), 440.0)),
            None,
        );
        // Le plancher système ne peut être ouvert : erreur propagée…
        let r = session.definir_source_emission_avec(
            SourceEmission::SystemeEtMicro,
            fabrique_systeme_ko,
            fabrique_micro_ok,
        );
        assert!(r.is_err());
        // …et l'émetteur précédent reste opérationnel.
        assert!(session.produire().expect("produire").is_some());
    }
}
