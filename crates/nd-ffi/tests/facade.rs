//! Tests d'intégration de la façade UI (`nd_ffi::api`) — le contrat que
//! l'application Flutter consommera via `flutter_rust_bridge` (plan 10).

use nd_ffi::{
    app_info, decode_input_event, encode_input_event, format_nova_id, new_session_config,
    parse_nova_id, session_status, InputEventDto, PermissionsDto, SessionConfigDto, SessionRoleDto,
    SessionStateDto,
};

// ---------------------------------------------------------------------------
// Informations générales
// ---------------------------------------------------------------------------

#[test]
fn app_info_non_vide_et_coherente_avec_le_moteur() {
    let info = app_info();
    assert!(!info.version.is_empty());
    assert_eq!(info.version, nd_core::engine_version().to_string());
}

// ---------------------------------------------------------------------------
// ID NovaDesk
// ---------------------------------------------------------------------------

#[test]
fn nova_id_aller_retour_avec_format_groupe() {
    assert_eq!(format_nova_id(123_456_789), "123 456 789");
    assert_eq!(parse_nova_id("123 456 789"), Ok(123_456_789));

    for id in [0, 1, 999, 123_456_789, 9_876_543_210, u64::MAX] {
        assert_eq!(parse_nova_id(&format_nova_id(id)), Ok(id));
    }
}

#[test]
fn parse_nova_id_tolere_les_espacements() {
    assert_eq!(parse_nova_id("  123456789 "), Ok(123_456_789));
    assert_eq!(parse_nova_id("123\t456\t789"), Ok(123_456_789));
    // Espaces insécables typiques d'un copier-coller.
    assert_eq!(parse_nova_id("123\u{a0}456\u{a0}789"), Ok(123_456_789));
    // Les zéros de tête du format d'affichage sont acceptés.
    assert_eq!(parse_nova_id("000 000 042"), Ok(42));
}

#[test]
fn parse_nova_id_erreurs_lisibles() {
    assert!(parse_nova_id("").is_err());
    assert!(parse_nova_id("   ").is_err());
    assert!(parse_nova_id("123-456-789").is_err());
    // Dépasse u64::MAX : refusé proprement, sans panique.
    assert!(parse_nova_id("99999999999999999999999").is_err());

    // Le message cite le caractère fautif pour guider l'utilisateur.
    let err = parse_nova_id("12a34").unwrap_err();
    assert!(err.contains('a'), "message peu utile : {err}");
}

// ---------------------------------------------------------------------------
// Événements d'entrée
// ---------------------------------------------------------------------------

/// Une valeur de chaque variante, avec des champs non triviaux.
fn toutes_les_variantes() -> Vec<InputEventDto> {
    vec![
        InputEventDto::MouseMoveAbs {
            x: 0.25,
            y: 0.75,
            monitor: 1,
        },
        InputEventDto::MouseMoveRel { dx: -3.5, dy: 12.0 },
        InputEventDto::MouseButton {
            button: 2,
            down: true,
        },
        InputEventDto::Scroll { dx: 0.0, dy: -1.0 },
        InputEventDto::Key {
            scancode: 0x1C,
            down: false,
        },
        InputEventDto::Unicode { codepoint: 0xE9 }, // « é »
    ]
}

#[test]
fn input_event_aller_retour_binaire_pour_chaque_variante() {
    for evt in toutes_les_variantes() {
        let octets = encode_input_event(evt);
        assert_eq!(decode_input_event(&octets), Ok(evt));
    }
}

#[test]
fn input_event_encodage_identique_a_nd_proto() {
    for evt in toutes_les_variantes() {
        let interne = nd_proto::InputEvent::from(evt);
        assert_eq!(encode_input_event(evt), interne.to_bytes());
        // Conversion DTO <-> type interne : aller-retour sans perte.
        assert_eq!(InputEventDto::from(interne), evt);
    }
}

#[test]
fn decode_input_event_erreurs_lisibles() {
    assert!(decode_input_event(&[]).is_err());
    assert!(decode_input_event(&[255]).is_err()); // étiquette inconnue
    assert!(decode_input_event(&[0, 1, 2]).is_err()); // charge tronquée
}

// ---------------------------------------------------------------------------
// Rôles, états et statut de session
// ---------------------------------------------------------------------------

#[test]
fn role_conversions_aller_retour() {
    let cas = [
        (nd_core::SessionRole::Controller, SessionRoleDto::Controller),
        (nd_core::SessionRole::Controlled, SessionRoleDto::Controlled),
    ];
    for (interne, dto) in cas {
        assert_eq!(SessionRoleDto::from(interne), dto);
        assert_eq!(nd_core::SessionRole::from(dto), interne);
    }
}

#[test]
fn etat_conversions_et_libelles() {
    let cas = [
        (nd_core::SessionState::Idle, SessionStateDto::Idle),
        (nd_core::SessionState::Resolving, SessionStateDto::Resolving),
        (
            nd_core::SessionState::Connecting,
            SessionStateDto::Connecting,
        ),
        (
            nd_core::SessionState::Handshaking,
            SessionStateDto::Handshaking,
        ),
        (nd_core::SessionState::Active, SessionStateDto::Active),
        (
            nd_core::SessionState::Reconnecting,
            SessionStateDto::Reconnecting,
        ),
        (nd_core::SessionState::Closed, SessionStateDto::Closed),
    ];
    for (interne, dto) in cas {
        assert_eq!(SessionStateDto::from(interne), dto);
        assert_eq!(nd_core::SessionState::from(dto), interne);
        assert!(!dto.label().is_empty());
    }
}

#[test]
fn statut_de_session_affichable() {
    let statut = session_status(SessionStateDto::Active, Some(123_456_789));
    assert_eq!(statut.state, "active");
    assert_eq!(statut.peer.as_deref(), Some("123 456 789"));

    let statut = session_status(SessionStateDto::Idle, None);
    assert_eq!(statut.state, "inactive");
    assert_eq!(statut.peer, None);
}

// ---------------------------------------------------------------------------
// Permissions et configuration de session
// ---------------------------------------------------------------------------

#[test]
fn permissions_conversions_aller_retour() {
    assert_eq!(
        nd_features::Permissions::from(PermissionsDto::full()),
        nd_features::Permissions::full()
    );
    assert_eq!(
        PermissionsDto::from(nd_features::Permissions::view_only()),
        PermissionsDto::view_only()
    );
    // Le défaut du DTO reste aligné sur le défaut prudent du moteur.
    assert_eq!(
        nd_features::Permissions::from(PermissionsDto::default()),
        nd_features::Permissions::default()
    );
}

#[test]
fn nouvelle_config_controleur_exige_un_pair() {
    let err = new_session_config(SessionRoleDto::Controller, 1, None, PermissionsDto::full())
        .unwrap_err();
    assert!(err.contains("pair"), "message peu utile : {err}");

    // Le rôle contrôlé, lui, n'a pas besoin de pair au démarrage.
    assert!(
        new_session_config(SessionRoleDto::Controlled, 1, None, PermissionsDto::full()).is_ok()
    );
}

#[test]
fn nouvelle_config_refuse_l_auto_connexion() {
    assert!(new_session_config(
        SessionRoleDto::Controller,
        42,
        Some(42),
        PermissionsDto::full()
    )
    .is_err());
}

#[test]
fn config_conversions_et_demarrage_moteur() {
    let dto = new_session_config(
        SessionRoleDto::Controller,
        111_111_111,
        Some(222_222_222),
        PermissionsDto::full(),
    )
    .expect("configuration valide");

    // DTO -> types internes.
    let interne = nd_core::SessionConfig::from(dto);
    assert_eq!(interne.role, nd_core::SessionRole::Controller);
    assert_eq!(interne.local_id, nd_proto::NovaId(111_111_111));
    assert_eq!(interne.peer_id, Some(nd_proto::NovaId(222_222_222)));
    assert!(interne.permissions.allows_input());

    // Types internes -> DTO : aller-retour sans perte.
    assert_eq!(SessionConfigDto::from(interne), dto);

    // La configuration produite démarre bien une session côté moteur.
    let mut session = nd_core::Session::new(dto.into());
    assert_eq!(
        SessionStateDto::from(session.state()),
        SessionStateDto::Idle
    );
    session.begin().expect("begin doit réussir avec un pair");
    assert_eq!(
        SessionStateDto::from(session.state()),
        SessionStateDto::Resolving
    );
}
