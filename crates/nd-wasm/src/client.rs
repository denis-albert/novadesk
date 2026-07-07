//! `client` — client web `#[wasm_bindgen]` : WebTransport + WebCodecs + Canvas.
//! **Compilé uniquement pour `target_arch = "wasm32"`.**
//!
//! [`WebClient`] est la surface JS de la crate. Cycle de vie :
//! * [`WebClient::new`] lie un `<canvas>` et prépare le contexte 2D ;
//! * [`WebClient::connect`] ouvre une session **WebTransport**, décode le flux H.264
//!   reçu (WebCodecs `VideoDecoder`) et le peint sur le canvas, tout en émettant les
//!   entrées souris/clavier (sérialisées par `nd-proto`) sur un flux sortant ;
//! * [`WebClient::disconnect`] libère la session et les codecs.
//!
//! Sans pont WebTransport (dépendance d'infrastructure, cf. `lib.rs`), les modes démo
//! [`WebClient::demarrer_demo_codec`] (boucle encode→decode→canvas, preuve du chemin
//! de décodage) et [`WebClient::demarrer_demo_motif`] (RGBA→canvas direct) tournent
//! **sans aucun serveur**.

use std::cell::RefCell;
use std::rc::Rc;

use wasm_bindgen::prelude::*;
use wasm_bindgen::Clamped;
use wasm_bindgen_futures::spawn_local;
use web_sys::{
    console, CanvasRenderingContext2d, EncodedVideoChunk, EncodedVideoChunkInit,
    EncodedVideoChunkMetadata, EncodedVideoChunkType, HtmlCanvasElement, ImageData, KeyboardEvent,
    LatencyMode, MouseEvent, ReadableStreamDefaultReader, VideoDecoder, VideoDecoderConfig,
    VideoDecoderInit, VideoEncoder, VideoEncoderConfig, VideoEncoderInit, VideoFrame,
    VideoFrameBufferInit, VideoPixelFormat, WebTransport, WebTransportSendStream, WheelEvent,
    WritableStreamDefaultWriter,
};

use crate::{demo, entree, h264};

/// Codec H.264 négocié (profil Baseline, niveau 3.1) — socle universel WebCodecs,
/// aligné sur le choix « H.264 d'abord » de `nd-codec` (plan 03).
const CODEC_H264: &str = "avc1.42001f";

/// Cadence de démo : durée d'une image en microsecondes (~30 img/s) pour horodater
/// les frames encodées/décodées.
const DUREE_IMAGE_US: i32 = 33_333;

/// Closure de boucle `requestAnimationFrame`, conservée en vie par elle-même
/// (auto-référence via `Rc`). Alias pour éviter la complexité de type (clippy).
type BoucleRaf = Rc<RefCell<Option<Closure<dyn FnMut()>>>>;

/// État mutable partagé entre l'objet, les closures d'événements et la tâche réseau.
#[derive(Default)]
struct Etat {
    /// Session WebTransport courante (clone conservé pour la fermeture).
    transport: Option<WebTransport>,
    /// Écrivain du flux d'entrées (client→pair), une fois la session prête.
    entree_writer: Option<WritableStreamDefaultWriter>,
    /// Décodeur vidéo courant (chemin réel **ou** démo codec).
    decodeur: Option<VideoDecoder>,
    /// Encodeur (mode démo codec uniquement — génère le flux H.264 de test).
    encodeur: Option<VideoEncoder>,
    /// Callback JS optionnel notifié `(largeur, hauteur)` à chaque frame peinte.
    on_frame: Option<js_sys::Function>,
    /// Closures maintenues en vie (sinon JS rappellerait du code libéré).
    closures: Vec<JsValue>,
    /// Compteur d'images du mode démo « motif ».
    tick: u32,
    /// Vrai dès qu'une image-clé a été reçue : le décodeur ignore les deltas avant.
    cle_recue: bool,
}

/// Client web NovaDesk (contrôle sortant) exposé à JavaScript.
#[wasm_bindgen]
pub struct WebClient {
    /// Contexte 2D du canvas d'affichage.
    ctx: CanvasRenderingContext2d,
    /// Canvas d'affichage (source des dimensions et cible des entrées).
    canvas: HtmlCanvasElement,
    /// État partagé (Rc : cloné dans les closures et la tâche réseau).
    etat: Rc<RefCell<Etat>>,
}

#[wasm_bindgen]
impl WebClient {
    /// Lie le client au `<canvas>` d'identifiant `canvas_id` et prépare le contexte 2D.
    ///
    /// # Errors
    /// Si la fenêtre/le document sont absents, si le canvas est introuvable ou n'est
    /// pas un `<canvas>`, ou si le contexte 2D est indisponible.
    #[wasm_bindgen(constructor)]
    pub fn new(canvas_id: &str) -> Result<WebClient, JsValue> {
        let fenetre = web_sys::window().ok_or_else(|| err("aucune fenêtre (window)"))?;
        let document = fenetre.document().ok_or_else(|| err("aucun document"))?;
        let element = document
            .get_element_by_id(canvas_id)
            .ok_or_else(|| err(&format!("canvas introuvable : #{canvas_id}")))?;
        let canvas: HtmlCanvasElement = element
            .dyn_into()
            .map_err(|_| err("l'élément ciblé n'est pas un <canvas>"))?;
        let ctx = canvas
            .get_context("2d")?
            .ok_or_else(|| err("contexte 2D indisponible"))?
            .dyn_into::<CanvasRenderingContext2d>()?;
        Ok(WebClient {
            ctx,
            canvas,
            etat: Rc::new(RefCell::new(Etat::default())),
        })
    }

    /// Version du moteur/protocole exposée au web (ex. écran « À propos »).
    #[wasm_bindgen(js_name = versionMoteur)]
    #[must_use]
    pub fn version_moteur(&self) -> String {
        crate::engine_version_string()
    }

    /// Enregistre un callback JS `(largeur, hauteur)` appelé après chaque frame peinte
    /// (fil pour l'affichage de statistiques/FPS côté UI).
    #[wasm_bindgen(js_name = setOnFrame)]
    pub fn set_on_frame(&self, callback: js_sys::Function) {
        self.etat.borrow_mut().on_frame = Some(callback);
    }

    /// Se connecte à un pair via **WebTransport** et démarre la réception/affichage.
    ///
    /// `signaling_url` est l'URL HTTP/3 du **pont WebTransport** du relais NovaDesk
    /// (dépendance d'infrastructure, cf. `lib.rs`). `peer_id` et `token` sont annoncés
    /// dans la première trame du flux d'entrées afin que le pont route la session.
    ///
    /// La réception vidéo, le décodage H.264 (WebCodecs) et le rendu sur le canvas se
    /// déroulent en tâche de fond ; les entrées capturées sur le canvas sont émises sur
    /// le flux sortant. Voir la note d'honnêteté : ce chemin **compile** mais n'a pas pu
    /// être exercé ici (ni navigateur ni pont).
    ///
    /// # Errors
    /// Si la création de la session, du décodeur ou l'attache des entrées échoue.
    #[wasm_bindgen]
    pub fn connect(
        &self,
        signaling_url: String,
        peer_id: String,
        token: String,
    ) -> Result<(), JsValue> {
        let transport = WebTransport::new(&signaling_url)?;
        let decodeur = self.creer_decodeur()?;
        // Chemin réel : flux Annex B reçu du pair → aucune `description` → mode Annex B.
        decodeur.configure(&VideoDecoderConfig::new(CODEC_H264))?;
        {
            let mut e = self.etat.borrow_mut();
            e.transport = Some(transport.clone());
            e.decodeur = Some(decodeur.clone());
            e.cle_recue = false;
        }
        self.attacher_entrees()?;

        let etat = self.etat.clone();
        spawn_local(async move {
            if let Err(e) = piloter_connexion(transport, decodeur, etat, peer_id, token).await {
                journal(&format!("connexion échouée : {e:?}"));
            }
        });
        Ok(())
    }

    /// Ferme la session et les codecs, et libère l'écrivain d'entrées.
    #[wasm_bindgen]
    pub fn disconnect(&self) {
        let mut e = self.etat.borrow_mut();
        if let Some(t) = e.transport.take() {
            t.close();
        }
        if let Some(d) = e.decodeur.take() {
            let _ = d.close();
        }
        if let Some(enc) = e.encodeur.take() {
            let _ = enc.close();
        }
        e.entree_writer = None;
        e.cle_recue = false;
    }

    // -- Entrées : envoi direct depuis JS (API `send_input(...)` granulaire) ---------

    /// Envoie un déplacement souris absolu à partir d'une position pixel dans le canvas.
    #[wasm_bindgen(js_name = sendMouseMove)]
    pub fn send_mouse_move(&self, x: f64, y: f64) {
        let ev = entree::souris_deplacement_abs(
            x,
            y,
            f64::from(self.canvas.width()),
            f64::from(self.canvas.height()),
            0,
        );
        envoyer_octets(&self.etat, &ev.to_bytes());
    }

    /// Envoie un événement bouton souris (`MouseEvent.button` du DOM).
    #[wasm_bindgen(js_name = sendMouseButton)]
    pub fn send_mouse_button(&self, dom_button: i16, down: bool) {
        envoyer_octets(
            &self.etat,
            &entree::souris_bouton(dom_button, down).to_bytes(),
        );
    }

    /// Envoie un événement molette (`WheelEvent.deltaX/deltaY`).
    #[wasm_bindgen(js_name = sendScroll)]
    pub fn send_scroll(&self, delta_x: f64, delta_y: f64) {
        envoyer_octets(
            &self.etat,
            &entree::souris_molette(delta_x, delta_y).to_bytes(),
        );
    }

    /// Envoie une touche à partir d'un `KeyboardEvent.code` (identité physique).
    /// Les touches hors table (voir `entree::scancode_depuis_code`) sont ignorées ici ;
    /// utiliser [`WebClient::send_unicode`] pour le texte.
    #[wasm_bindgen(js_name = sendKey)]
    pub fn send_key(&self, code: &str, down: bool) {
        if let Some(sc) = entree::scancode_depuis_code(code) {
            envoyer_octets(&self.etat, &entree::touche(sc, down).to_bytes());
        }
    }

    /// Envoie un caractère Unicode (point de code) — saisie de texte.
    #[wasm_bindgen(js_name = sendUnicode)]
    pub fn send_unicode(&self, codepoint: u32) {
        envoyer_octets(&self.etat, &entree::unicode(codepoint).to_bytes());
    }

    // -- Modes démo (sans infrastructure) --------------------------------------------

    /// **Démo codec** : preuve du chemin **decode→canvas** sans aucun serveur.
    ///
    /// Génère `images` frames de test ([`demo::motif_rgba`]), les encode en H.264 avec
    /// le `VideoEncoder` du navigateur, puis réinjecte les `EncodedVideoChunk` produits
    /// dans un `VideoDecoder` dont la sortie est peinte sur le canvas. Le décodeur est
    /// configuré à la volée depuis la `description` (avcC) fournie par l'encodeur.
    ///
    /// # Errors
    /// Si la création/configuration des codecs ou d'une frame échoue.
    #[wasm_bindgen(js_name = demarrerDemoCodec)]
    pub fn demarrer_demo_codec(&self, images: u32) -> Result<(), JsValue> {
        let largeur = 320u32;
        let hauteur = 240u32;
        let nb = images.clamp(1, 600);

        let decodeur = self.creer_decodeur()?;

        // Sortie encodeur : configure le décodeur à la 1re trame (avcC via metadata),
        // puis lui transmet chaque morceau encodé.
        let dec_pour_sortie = decodeur.clone();
        let mut configure = false;
        let sortie_enc = Closure::wrap(Box::new(
            move |chunk: EncodedVideoChunk, meta: EncodedVideoChunkMetadata| {
                if !configure {
                    if let Some(cfg) = meta.get_decoder_config() {
                        match dec_pour_sortie.configure(&cfg) {
                            Ok(()) => configure = true,
                            Err(e) => journal(&format!("config décodeur (démo) : {e:?}")),
                        }
                    }
                }
                if configure {
                    if let Err(e) = dec_pour_sortie.decode(&chunk) {
                        journal(&format!("decode (démo) : {e:?}"));
                    }
                }
            },
        )
            as Box<dyn FnMut(EncodedVideoChunk, EncodedVideoChunkMetadata)>);
        let erreur_enc = Closure::wrap(Box::new(move |e: JsValue| {
            journal(&format!("VideoEncoder erreur : {e:?}"));
        }) as Box<dyn FnMut(JsValue)>);

        let enc_init = VideoEncoderInit::new(
            erreur_enc.as_ref().unchecked_ref(),
            sortie_enc.as_ref().unchecked_ref(),
        );
        let encodeur = VideoEncoder::new(&enc_init)?;
        let cfg = VideoEncoderConfig::new(CODEC_H264, hauteur, largeur);
        cfg.set_bitrate(2_000_000);
        cfg.set_framerate(30.0);
        cfg.set_latency_mode(LatencyMode::Realtime);
        encodeur.configure(&cfg)?;

        {
            let mut e = self.etat.borrow_mut();
            e.decodeur = Some(decodeur);
            e.encodeur = Some(encodeur.clone());
            e.closures.push(sortie_enc.into_js_value());
            e.closures.push(erreur_enc.into_js_value());
        }

        for n in 0..nb {
            let mut rgba = demo::motif_rgba(largeur, hauteur, n);
            let ts = (n as i32).wrapping_mul(DUREE_IMAGE_US);
            let binit = VideoFrameBufferInit::new(hauteur, largeur, VideoPixelFormat::Rgba, ts);
            let frame =
                VideoFrame::new_with_u8_slice_and_video_frame_buffer_init(&mut rgba, &binit)?;
            if let Err(e) = encodeur.encode(&frame) {
                journal(&format!("encode (démo) : {e:?}"));
            }
            frame.close();
        }
        let _ = encodeur.flush();
        journal(&format!(
            "démo codec : {nb} images encodées puis décodées vers le canvas"
        ));
        Ok(())
    }

    /// **Démo motif** : peint un motif RGBA animé directement sur le canvas
    /// (`put_image_data`), sans codec ni réseau — vérifie le pont wasm→canvas. Tourne
    /// indéfiniment via `requestAnimationFrame`.
    ///
    /// # Errors
    /// Si la première planification `requestAnimationFrame` échoue.
    #[wasm_bindgen(js_name = demarrerDemoMotif)]
    pub fn demarrer_demo_motif(&self) -> Result<(), JsValue> {
        let ctx = self.ctx.clone();
        let canvas = self.canvas.clone();
        let etat = self.etat.clone();

        let boucle: BoucleRaf = Rc::new(RefCell::new(None));
        let amorce = boucle.clone();
        *amorce.borrow_mut() = Some(Closure::wrap(Box::new(move || {
            let l = canvas.width();
            let h = canvas.height();
            let tick = {
                let mut e = etat.borrow_mut();
                e.tick = e.tick.wrapping_add(1);
                e.tick
            };
            let rgba = demo::motif_rgba(l, h, tick);
            match ImageData::new_with_u8_clamped_array_and_sh(Clamped(&rgba), l, h) {
                Ok(img) => {
                    let _ = ctx.put_image_data(&img, 0, 0);
                }
                Err(e) => journal(&format!("ImageData : {e:?}")),
            }
            // Reprogramme l'image suivante (la closure se maintient en vie via `boucle`).
            if let (Some(w), Some(cb)) = (web_sys::window(), boucle.borrow().as_ref()) {
                let _ = w.request_animation_frame(cb.as_ref().unchecked_ref());
            }
        }) as Box<dyn FnMut()>));

        let fenetre = web_sys::window().ok_or_else(|| err("aucune fenêtre (window)"))?;
        if let Some(cb) = amorce.borrow().as_ref() {
            fenetre.request_animation_frame(cb.as_ref().unchecked_ref())?;
        }
        Ok(())
    }

    // -- Interne ----------------------------------------------------------------------

    /// Crée un `VideoDecoder` dont la sortie peint chaque frame sur le canvas (mise à
    /// l'échelle) et notifie l'éventuel `on_frame`. Les closures sont conservées en vie.
    fn creer_decodeur(&self) -> Result<VideoDecoder, JsValue> {
        let ctx = self.ctx.clone();
        let canvas = self.canvas.clone();
        let etat = self.etat.clone();

        let sortie = Closure::wrap(Box::new(move |frame: VideoFrame| {
            let dw = f64::from(canvas.width());
            let dh = f64::from(canvas.height());
            if let Err(e) = ctx.draw_image_with_video_frame_and_dw_and_dh(&frame, 0.0, 0.0, dw, dh)
            {
                journal(&format!("draw_image : {e:?}"));
            }
            let (largeur, hauteur) = (frame.display_width(), frame.display_height());
            frame.close();
            let rappel = etat.borrow().on_frame.clone();
            if let Some(cb) = rappel {
                let _ = cb.call2(
                    &JsValue::NULL,
                    &JsValue::from(largeur),
                    &JsValue::from(hauteur),
                );
            }
        }) as Box<dyn FnMut(VideoFrame)>);
        let erreur = Closure::wrap(Box::new(move |e: JsValue| {
            journal(&format!("VideoDecoder erreur : {e:?}"));
        }) as Box<dyn FnMut(JsValue)>);

        let init = VideoDecoderInit::new(
            erreur.as_ref().unchecked_ref(),
            sortie.as_ref().unchecked_ref(),
        );
        let decodeur = VideoDecoder::new(&init)?;
        {
            let mut e = self.etat.borrow_mut();
            e.closures.push(sortie.into_js_value());
            e.closures.push(erreur.into_js_value());
        }
        Ok(decodeur)
    }

    /// Attache les écouteurs souris/clavier/molette sur le canvas ; chaque événement est
    /// converti (module `entree`), sérialisé (`nd-proto`) et émis sur le flux d'entrées.
    fn attacher_entrees(&self) -> Result<(), JsValue> {
        // Déplacement souris.
        {
            let etat = self.etat.clone();
            let cv = self.canvas.clone();
            let f = Closure::wrap(Box::new(move |e: MouseEvent| {
                let ev = entree::souris_deplacement_abs(
                    e.offset_x(),
                    e.offset_y(),
                    f64::from(cv.width()),
                    f64::from(cv.height()),
                    0,
                );
                envoyer_octets(&etat, &ev.to_bytes());
            }) as Box<dyn FnMut(MouseEvent)>);
            self.canvas
                .add_event_listener_with_callback("mousemove", f.as_ref().unchecked_ref())?;
            self.etat.borrow_mut().closures.push(f.into_js_value());
        }
        // Boutons souris (down/up).
        for (nom, enfonce) in [("mousedown", true), ("mouseup", false)] {
            let etat = self.etat.clone();
            let f = Closure::wrap(Box::new(move |e: MouseEvent| {
                envoyer_octets(
                    &etat,
                    &entree::souris_bouton(e.button(), enfonce).to_bytes(),
                );
            }) as Box<dyn FnMut(MouseEvent)>);
            self.canvas
                .add_event_listener_with_callback(nom, f.as_ref().unchecked_ref())?;
            self.etat.borrow_mut().closures.push(f.into_js_value());
        }
        // Molette.
        {
            let etat = self.etat.clone();
            let f = Closure::wrap(Box::new(move |e: WheelEvent| {
                envoyer_octets(
                    &etat,
                    &entree::souris_molette(e.delta_x(), e.delta_y()).to_bytes(),
                );
            }) as Box<dyn FnMut(WheelEvent)>);
            self.canvas
                .add_event_listener_with_callback("wheel", f.as_ref().unchecked_ref())?;
            self.etat.borrow_mut().closures.push(f.into_js_value());
        }
        // Clavier (down/up) : scancode physique, repli Unicode pour le texte imprimable.
        for (nom, enfonce) in [("keydown", true), ("keyup", false)] {
            let etat = self.etat.clone();
            let f = Closure::wrap(Box::new(move |e: KeyboardEvent| {
                if let Some(sc) = entree::scancode_depuis_code(&e.code()) {
                    envoyer_octets(&etat, &entree::touche(sc, enfonce).to_bytes());
                } else if enfonce {
                    let k = e.key();
                    if k.chars().count() == 1 {
                        if let Some(c) = k.chars().next() {
                            envoyer_octets(&etat, &entree::unicode(u32::from(c)).to_bytes());
                        }
                    }
                }
            }) as Box<dyn FnMut(KeyboardEvent)>);
            self.canvas
                .add_event_listener_with_callback(nom, f.as_ref().unchecked_ref())?;
            self.etat.borrow_mut().closures.push(f.into_js_value());
        }
        Ok(())
    }
}

/// Pilote la session WebTransport : attente `ready`, ouverture du flux d'entrées
/// (annonce pair/jeton), puis boucle de réception des unités d'accès H.264 par
/// datagrammes → décodage. L'attente de la première image-clé évite un décodage sur un
/// delta.
async fn piloter_connexion(
    transport: WebTransport,
    decodeur: VideoDecoder,
    etat: Rc<RefCell<Etat>>,
    peer_id: String,
    token: String,
) -> Result<(), JsValue> {
    // Session prête (Promise typée → castée en Promise non typée pour `.await`).
    let pret = transport.ready().unchecked_into::<js_sys::Promise>();
    pret.await?;
    journal("WebTransport : session prête");

    // Flux sortant fiable (entrées) ; 1re trame = annonce {peer, token} pour le pont.
    let flux_js = transport
        .create_unidirectional_stream()
        .unchecked_into::<js_sys::Promise>()
        .await?;
    let flux_entrees: WebTransportSendStream = flux_js.dyn_into()?;
    let writer = WritableStreamDefaultWriter::new(&flux_entrees)?;
    let annonce = format!("{{\"peer\":\"{peer_id}\",\"token\":\"{token}\"}}");
    let arr = js_sys::Uint8Array::from(annonce.as_bytes());
    let _ = writer.write_with_chunk(arr.as_ref()).await;
    etat.borrow_mut().entree_writer = Some(writer);

    // Réception vidéo par datagrammes (canal Video non fiable du modèle NovaDesk).
    let readable = transport.datagrams().readable();
    let lecteur = ReadableStreamDefaultReader::new(&readable)?;
    let mut horodatage: i32 = 0;
    loop {
        let resultat = lecteur.read().await?;
        let done = js_sys::Reflect::get(&resultat, &JsValue::from_str("done"))?
            .as_bool()
            .unwrap_or(true);
        if done {
            break;
        }
        let valeur = js_sys::Reflect::get(&resultat, &JsValue::from_str("value"))?;
        let octets = valeur.dyn_into::<js_sys::Uint8Array>()?.to_vec();
        if octets.is_empty() {
            continue;
        }

        let cle = h264::contient_idr(&octets);
        {
            let mut e = etat.borrow_mut();
            if !e.cle_recue {
                if !cle {
                    continue; // on attend la première image-clé
                }
                e.cle_recue = true;
            }
        }

        let type_ = if cle {
            EncodedVideoChunkType::Key
        } else {
            EncodedVideoChunkType::Delta
        };
        let arr = js_sys::Uint8Array::from(&octets[..]);
        let init = EncodedVideoChunkInit::new(arr.as_ref(), horodatage, type_);
        let chunk = EncodedVideoChunk::new(&init)?;
        if let Err(e) = decodeur.decode(&chunk) {
            journal(&format!("decode : {e:?}"));
        }
        horodatage = horodatage.wrapping_add(DUREE_IMAGE_US);
    }
    journal("WebTransport : flux terminé");
    Ok(())
}

/// Émet des octets bruts sur le flux d'entrées, si la session est établie.
fn envoyer_octets(etat: &Rc<RefCell<Etat>>, octets: &[u8]) {
    let writer = etat.borrow().entree_writer.clone();
    if let Some(w) = writer {
        let arr = js_sys::Uint8Array::from(octets);
        let _ = w.write_with_chunk(arr.as_ref());
    }
}

/// Journalise un message dans la console du navigateur.
fn journal(message: &str) {
    console::log_1(&JsValue::from_str(message));
}

/// Construit une erreur JS lisible à partir d'un message.
fn err(message: &str) -> JsValue {
    JsValue::from_str(message)
}
