//! Boucle de contrôle **bidirectionnelle** sur une seule connexion QUIC :
//! - l'**hôte** envoie la vidéo (capture → encode → QUIC) ET reçoit les entrées
//!   (QUIC → injection `SendInput`) ;
//! - le **viewer** reçoit et décode la vidéo ET envoie des entrées.
//!
//! Les mouvements souris scriptés sont relatifs et s'annulent (le curseur revient à
//! sa place). Aucune frappe clavier n'est injectée (pas de saisie dans une fenêtre
//! tierce). Lancer : `cargo run --release --example control_loop -p nd-core`

use std::thread;
use std::time::Duration;

use nd_capture::{create_capturer, CaptureConfig, CapturedFrame};
use nd_codec::{create_decoder, create_encoder, CodecKind, EncodedChunk, EncoderConfig};
use nd_core::apply_input;
use nd_input::create_injector;
use nd_proto::{ChannelKind, InputEvent, MonitorId, Reliability};
use nd_transport::{bind, connect};

const VIDEO_N: usize = 10;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let listener = bind("127.0.0.1:0".parse()?)?;
    let addr = listener.local_addr();
    let cert = listener.server_cert_der();
    println!("Boucle de contrôle — hôte (serveur QUIC) sur {addr}");

    // Entrées scriptées : déplacements relatifs qui s'annulent (le curseur revient).
    let script: Vec<InputEvent> = vec![
        InputEvent::MouseMoveRel { dx: 20.0, dy: 0.0 },
        InputEvent::MouseMoveRel { dx: 0.0, dy: 20.0 },
        InputEvent::MouseMoveRel { dx: -20.0, dy: 0.0 },
        InputEvent::MouseMoveRel { dx: 0.0, dy: -20.0 },
        InputEvent::MouseMoveRel { dx: 15.0, dy: 15.0 },
        InputEvent::MouseMoveRel {
            dx: -15.0,
            dy: -15.0,
        },
        InputEvent::MouseMoveRel {
            dx: 10.0,
            dy: -10.0,
        },
        InputEvent::MouseMoveRel {
            dx: -10.0,
            dy: 10.0,
        },
    ];
    let input_n = script.len();

    // Viewer (client) : reçoit + décode la vidéo, envoie les entrées.
    let viewer = thread::spawn(move || -> Result<(usize, usize), String> {
        let mut transport = connect(addr, &cert).map_err(|e| e.to_string())?;
        let mut decoder = create_decoder(CodecKind::H264).map_err(|e| e.to_string())?;
        let input_ch = transport.open_channel(ChannelKind::Input);

        let mut video_decoded = 0usize;
        let decode_pending = |transport: &mut Box<dyn nd_transport::Transport>,
                              decoder: &mut Box<dyn nd_codec::VideoDecoder>,
                              count: &mut usize|
         -> Result<(), String> {
            while let Some((_h, data)) = transport.poll_recv().map_err(|e| e.to_string())? {
                let chunk = EncodedChunk {
                    data,
                    is_keyframe: false,
                    monitor: MonitorId(0),
                    timestamp_us: 0,
                };
                if decoder.decode(&chunk).map_err(|e| e.to_string())?.is_some() {
                    *count += 1;
                }
            }
            Ok(())
        };

        for ev in &script {
            transport
                .send(input_ch, ev.to_bytes(), Reliability::Reliable)
                .map_err(|e| e.to_string())?;
            thread::sleep(Duration::from_millis(15));
            decode_pending(&mut transport, &mut decoder, &mut video_decoded)?;
        }

        let mut idle = 0;
        while video_decoded < VIDEO_N && idle < 2000 {
            let before = video_decoded;
            decode_pending(&mut transport, &mut decoder, &mut video_decoded)?;
            if video_decoded == before {
                idle += 1;
                thread::sleep(Duration::from_millis(2));
            }
        }
        Ok((input_n, video_decoded))
    });

    // Hôte (serveur) : accepte, envoie la vidéo, reçoit + injecte les entrées.
    let mut transport = listener.accept()?;
    let mut capturer = create_capturer()?;
    capturer.start(CaptureConfig {
        monitor: MonitorId(0),
        target_fps: 60,
        capture_cursor: false,
    })?;
    let mut encoder = create_encoder(CodecKind::H264)?;
    let injector = create_injector()?;
    let video_ch = transport.open_channel(ChannelKind::Video(MonitorId(0)));

    let mut video_sent = 0usize;
    let mut input_injected = 0usize;
    let mut configured = false;
    let mut last: Option<CapturedFrame> = None;
    let mut attempts = 0usize;
    while (video_sent < VIDEO_N || input_injected < input_n) && attempts < 5000 {
        attempts += 1;

        if video_sent < VIDEO_N {
            let frame = capturer.next_frame()?;
            if frame.image.is_some() {
                if !configured {
                    encoder.configure(EncoderConfig {
                        kind: CodecKind::H264,
                        width: frame.width,
                        height: frame.height,
                        target_bitrate_kbps: 8_000,
                        max_fps: 60,
                    })?;
                    configured = true;
                }
                last = Some(frame);
            }
            if configured {
                if let Some(frame) = &last {
                    let chunk = encoder.encode(frame, video_sent == 0)?;
                    transport.send(video_ch, chunk.data, Reliability::UnreliableFec)?;
                    video_sent += 1;
                }
            }
        }

        while let Some((_h, data)) = transport.poll_recv()? {
            if let Some(event) = InputEvent::from_bytes(&data) {
                apply_input(injector.as_ref(), &event)?;
                input_injected += 1;
            }
        }

        thread::sleep(Duration::from_millis(2));
    }

    let (input_sent, video_decoded) = viewer
        .join()
        .expect("thread viewer")
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

    println!(
        "Hôte   : {video_sent} images envoyées, {input_injected}/{input_n} entrées injectées."
    );
    println!("Viewer : {input_sent} entrées envoyées, {video_decoded} images décodées.");

    drop(transport);

    if input_injected == input_n && video_sent == VIDEO_N && video_decoded >= VIDEO_N - 1 {
        println!(
            "OK : boucle de contrôle bidirectionnelle validée (vidéo hôte→viewer, entrées viewer→hôte)."
        );
        Ok(())
    } else {
        Err(format!(
            "boucle incomplète : vidéo {video_decoded}/{VIDEO_N} décodées, entrées {input_injected}/{input_n} injectées"
        )
        .into())
    }
}
