# Prompt Fable 12 — Parité multiplateforme : macOS audio, Wayland input, multi-écran

**Priorité : P2** · **Crates ciblées : `crates/nd-audio`, `crates/nd-input`, `crates/nd-capture`** · **Parallélisable avec : tout** (mais ⚠ **non testable sur le poste Windows actuel** — code sous `cfg`, à valider sur macOS/Linux).

---

Projet **NovaDesk** (bureau à distance en Rust). Code/commentaires en **FRANÇAIS**. **Mission** : combler les trous multiplateformes documentés — **capture audio système macOS** (ScreenCaptureKit), **injection d'entrées Wayland** (portail RemoteDesktop/libei), et **injection multi-écran** correcte.

## LOGISTIQUE
- Édite **UNIQUEMENT** sous `C:\Users\udohkak\Desktop\Anydesk\novadesk\crates\nd-audio\`, `crates\nd-input\`, `crates\nd-capture\`.
- Cargo **toujours** `--manifest-path C:\Users\udohkak\Desktop\Anydesk\novadesk\Cargo.toml`.
- **AUCUN git.** Verrou cargo parallèle = normal.
- **Réalité du poste** : Windows, donc les chemins macOS/Linux compilent sous `cfg` mais **ne s'exécutent pas ici**. Vise une compilation `--target`/`cfg` propre + tests unitaires **agnostiques OS** ; documente ce qui devra être validé sur la vraie plateforme.

## BARRE QUALITÉ
- `cargo clippy -p nd-audio -p nd-input -p nd-capture --all-targets -- -D warnings` = **ZÉRO** (côté Windows). Idéalement aussi `--target x86_64-apple-darwin`/`x86_64-unknown-linux-gnu` si les toolchains sont dispos (sinon, revue de code + `cfg` corrects).
- `cargo fmt` sur les trois.

## ÉTAT ACTUEL (à respecter, NE PAS casser)
- `nd-audio` : capture système **macOS = `NotImplemented`** (`lib.rs:104-107,143-145`) faute d'API loopback avant macOS 13 ; ScreenCaptureKit non implémenté. Windows/Linux OK.
- `nd-input` : Windows `SendInput` complet mais **multi-écran non fait** (écran primaire seul, param moniteur **ignoré** `win.rs:319-321`) ; macOS `CGEventPost` OK ; Linux **XTEST** OK, **Wayland absent** (documenté).
- `nd-capture` : Windows DXGI OK ; Linux X11 OK, **Wayland/PipeWire = `NotImplemented`** (`linux.rs:124-127`).

## TÂCHE
1. **macOS audio système (ScreenCaptureKit)** (`nd-audio/src/macos.rs`) : implémenter la capture audio système via **ScreenCaptureKit** (`SCStream` audio, macOS 13+), avec repli documenté sous macOS 13. Remplacer le `NotImplemented` par le vrai chemin sous `cfg(target_os="macos")`. Convertir en PCM pour Opus (réutilise le DSP existant).
2. **Wayland input** (`nd-input`) : injecter les entrées via le **portail `org.freedesktop.portal.RemoteDesktop`** + **libei** (émulation clavier/souris), avec repli **uinput** documenté. Sous `cfg(target_os="linux")`, sélectionner Wayland si `WAYLAND_DISPLAY` est défini, sinon XTEST existant. Ne régresse pas X11.
3. **Wayland capture** (`nd-capture`, optionnel si temps) : chemin **PipeWire + portail ScreenCast** pour la capture sous Wayland, sinon TODO ciblé documenté (ne prétends pas que c'est fait).
4. **Injection multi-écran** (`nd-input/src/win.rs` + trait) : honorer le paramètre **moniteur** dans `mouse_move_abs` — mapper les coordonnées normalisées vers le **rectangle virtuel du bon écran** (offsets multi-moniteur via `GetSystemMetrics`/`EnumDisplayMonitors`). Étendre le trait/impl pour macOS/Linux de façon cohérente. Ajouter des tests **de calcul de coordonnées** (agnostiques OS : donner un layout d'écrans fictif et vérifier le mapping).
5. Documenter clairement, pour chaque point, **ce qui est compilé** vs **ce qui reste à valider sur la vraie plateforme**.

## VÉRIF (obligatoire)
- `cargo build -p nd-audio -p nd-input -p nd-capture --manifest-path ...` (Windows) → OK ; si toolchains croisées dispo, `--target` macOS/linux → OK ou revue documentée.
- `cargo test -p nd-audio -p nd-input -p nd-capture --manifest-path ...` → verts (incl. nouveaux tests de mapping multi-écran, agnostiques OS). Reporte le compte.
- `cargo clippy ... --all-targets -- -D warnings` → **0** ; `cargo fmt`.
- **Régression** : les impls Windows/X11 existantes inchangées ; exemples `input_probe`/`capture_probe`/`audio_probe` compilent.

## RÉPONSE FINALE ATTENDUE
- Fichiers modifiés/créés par plateforme.
- Ce qui **compile** vs ce qui reste **à valider sur macOS/Wayland réels** (honnêteté).
- État EXACT des vérifs (tests de mapping multi-écran surtout).
- **Pas de git.**
