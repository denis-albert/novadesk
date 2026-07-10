//! Correspondance **caractère Unicode → keysym X11**, agnostique de l'OS.
//!
//! Utilisée par la saisie Unicode du backend Linux/XTEST (`linux.rs::unicode`,
//! remappage temporaire d'un keycode libre — technique xdotool). Isolée ici —
//! logique pure, sans appel système — pour être compilée et **testée sur toutes
//! les plateformes**, y compris Windows où le backend Linux ne compile pas.
//!
//! Règles (annexe « Keysyms » du protocole X11, fichier `keysymdef.h`) :
//!
//! * **Latin-1 imprimable** (U+0020..U+007E, U+00A0..U+00FF) : keysym = point
//!   de code (identité) ;
//! * **caractères de contrôle usuels** : keysyms dédiés de la plage `0xFFxx`
//!   (Return, Tab, BackSpace, Escape, Delete). La forme « Unicode »
//!   `0x0100_0000 + cp` n'est **pas** définie pour eux (la spec ne la prévoit
//!   que pour U+0100..U+10FFFF) : sans ce cas particulier, « Entrée » ou
//!   « Tab » collés dans du texte distant seraient ignorés par le serveur X ;
//! * **tout le reste** (U+0100..U+10FFFF) : convention keysym Unicode
//!   `0x0100_0000 + point de code`.

/// Keysym X11 « Return » (Entrée). Convention xdotool : `\n` et `\r` tapés en
/// texte produisent tous deux Entrée.
const KEYSYM_RETURN: u32 = 0xff0d;
/// Keysym X11 « Tab ».
const KEYSYM_TAB: u32 = 0xff09;
/// Keysym X11 « BackSpace » (retour arrière, U+0008).
const KEYSYM_BACKSPACE: u32 = 0xff08;
/// Keysym X11 « Escape » (U+001B).
const KEYSYM_ESCAPE: u32 = 0xff1b;
/// Keysym X11 « Delete » (U+007F).
const KEYSYM_DELETE: u32 = 0xffff;

/// Keysym X11 d'un caractère Unicode (voir la doc du module).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn keysym_pour_char(ch: char) -> u32 {
    match ch {
        // Contrôles usuels : keysyms dédiés (la forme 0x0100_0000+cp ne leur
        // est pas définie par la spec — le serveur X les ignorerait).
        '\u{08}' => KEYSYM_BACKSPACE,
        '\t' => KEYSYM_TAB,
        '\n' | '\r' => KEYSYM_RETURN,
        '\u{1b}' => KEYSYM_ESCAPE,
        '\u{7f}' => KEYSYM_DELETE,
        _ => {
            let cp = u32::from(ch);
            if (0x20..=0x7e).contains(&cp) || (0xa0..=0xff).contains(&cp) {
                cp
            } else {
                0x0100_0000 + cp
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ASCII et Latin-1 imprimables : identité.
    #[test]
    fn keysym_latin1_identite() {
        assert_eq!(keysym_pour_char('a'), 0x61);
        assert_eq!(keysym_pour_char(' '), 0x20);
        assert_eq!(keysym_pour_char('~'), 0x7e);
        assert_eq!(keysym_pour_char('é'), 0xE9);
        assert_eq!(keysym_pour_char('\u{a0}'), 0xA0); // espace insécable
    }

    /// Hors Latin-1 : convention keysym Unicode `0x0100_0000 + point de code`.
    #[test]
    fn keysym_unicode_decale() {
        assert_eq!(keysym_pour_char('€'), 0x0100_0000 + 0x20AC);
        assert_eq!(keysym_pour_char('Ω'), 0x0100_0000 + 0x03A9);
        assert_eq!(keysym_pour_char('😀'), 0x0100_0000 + 0x1F600);
        // Premier point de code couvert par la convention (U+0100).
        assert_eq!(keysym_pour_char('\u{100}'), 0x0100_0100);
    }

    /// Contrôles usuels : keysyms dédiés `0xFFxx` — jamais la forme Unicode,
    /// non définie pour eux (un « Entrée » collé doit produire Return).
    #[test]
    fn keysym_controles_dedies() {
        assert_eq!(keysym_pour_char('\n'), 0xff0d); // Return
        assert_eq!(keysym_pour_char('\r'), 0xff0d); // Return
        assert_eq!(keysym_pour_char('\t'), 0xff09); // Tab
        assert_eq!(keysym_pour_char('\u{08}'), 0xff08); // BackSpace
        assert_eq!(keysym_pour_char('\u{1b}'), 0xff1b); // Escape
        assert_eq!(keysym_pour_char('\u{7f}'), 0xffff); // Delete
    }
}
