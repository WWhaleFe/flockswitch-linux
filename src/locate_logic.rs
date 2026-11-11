//! The parts of finding the cursor and of the keyboard shortcuts on Wayland that are
//! plain logic, with no desktop to talk to: what is asked of the system's shortcut
//! service (the XDG GlobalShortcuts portal) and in which notation, whether the key the
//! system chose is the one asked for, how the cursor highlight is drawn in this session,
//! and where its surface goes on a layer-shell output. Only `std` is used, so the tests
//! run on any computer (`rustc --edition 2021 --test src/locate_logic.rs`).

use std::time::Duration;

/// How long the pointer has to come over the surfaces that read where it is, once they
/// are on the screen. The compositor picks the surface under a resting pointer again
/// when one appears there, within a frame or two.
pub const CATCH_WAIT: Duration = Duration::from_millis(150);
/// However the effect is drawn, those surfaces never stay longer than this (seconds).
pub const CATCH_MAX: f64 = 2.0;

/// The id of the find-the-cursor shortcut in the system. Ids stay the same for good:
/// the system keeps the key the user chose under the id.
pub const LOCATE_ID: &str = "find-cursor";

/// The slot of the find-the-cursor shortcut (hotkey::LOCATE), next to channels 1-3.
pub const LOCATE_SLOT: u8 = 0;

/// The id of the shortcut that sends everything to channel `n`.
pub fn switch_id(n: u8) -> String {
    format!("switch-to-channel-{n}")
}

/// The slot (0 = find the cursor, 1-3 = a channel) a shortcut id stands for.
pub fn slot_of(id: &str) -> Option<u8> {
    if id == LOCATE_ID {
        return Some(LOCATE_SLOT);
    }
    id.strip_prefix("switch-to-channel-")?.parse().ok().filter(|n| (1..=3).contains(n))
}

/// A shortcut flockswitch asks the system for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wanted {
    pub id: String,
    pub slot: u8,
    /// As written in the settings ("ctrl+alt+1").
    pub key: String,
    /// The same in the shortcuts specification's notation ("CTRL+ALT+1"), when it can be
    /// written so; without it the system lets the user choose.
    pub trigger: Option<String>,
}

/// What to ask for: the channels' shortcuts (`channels` is None while switching or the
/// shortcuts are off) and the find-the-cursor one (None while that is off), as the X11
/// shortcuts register them. Empty keys are left out.
pub fn wanted(channels: Option<[&str; 3]>, locate: Option<&str>) -> Vec<Wanted> {
    let mut out = Vec::new();
    if let Some(keys) = channels {
        for (i, key) in keys.iter().enumerate() {
            let n = i as u8 + 1;
            if !key.trim().is_empty() {
                out.push(Wanted { id: switch_id(n), slot: n, key: key.trim().to_lowercase(), trigger: portal_trigger(key) });
            }
        }
    }
    if let Some(key) = locate.filter(|k| !k.trim().is_empty()) {
        out.push(Wanted { id: LOCATE_ID.into(), slot: LOCATE_SLOT, key: key.trim().to_lowercase(), trigger: portal_trigger(key) });
    }
    out
}

/// True when every shortcut of `now` was in a request the user turned down: asking again
/// would only bring the same question back (a pause from the menu leaves some out).
pub fn already_refused(now: &[Wanted], refused: &[Wanted]) -> bool {
    !now.is_empty() && now.iter().all(|w| refused.contains(w))
}

/// A shortcut from the settings ("ctrl+alt+l") as the shortcuts specification writes a
/// trigger: XKB modifier names (CTRL, ALT, SHIFT, LOGO) joined by "+", then the key's
/// keysym name without its XKB_KEY_ prefix ("CTRL+ALT+l", "CTRL+ALT+1", "CTRL+F5").
/// None when the text has no key or one this cannot name.
pub fn portal_trigger(text: &str) -> Option<String> {
    let parts: Vec<String> = text.split('+').map(|p| p.trim().to_lowercase()).collect();
    let (key, mods) = parts.split_last()?;
    let mut flags = [false; 4];
    for m in mods {
        let i = match m.as_str() {
            "ctrl" | "control" | "ctl" => 0,
            "alt" | "option" | "opt" => 1,
            "shift" => 2,
            "super" | "cmd" | "command" | "meta" | "logo" | "win" => 3,
            _ => return None,
        };
        flags[i] = true;
    }
    let key = keysym_name(key)?;
    let names = ["CTRL", "ALT", "SHIFT", "LOGO"];
    let mut out: Vec<String> = names.iter().zip(flags).filter(|(_, on)| *on).map(|(n, _)| n.to_string()).collect();
    out.push(key);
    Some(out.join("+"))
}

/// The keysym name of a key as the settings write it: letters in lower case (the base
/// level), digits, F1-F35, and a few named keys. Also the "KeyL" / "Digit1" names a
/// shortcut written by hand may use.
fn keysym_name(key: &str) -> Option<String> {
    let key = key.trim().to_lowercase();
    let key = key.strip_prefix("key").filter(|k| k.len() == 1).or_else(|| key.strip_prefix("digit").filter(|k| k.len() == 1)).unwrap_or(&key);
    let mut chars = key.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        return c.is_ascii_alphanumeric().then(|| c.to_string());
    }
    if let Some(n) = key.strip_prefix('f').and_then(|n| n.parse::<u8>().ok()).filter(|n| (1..=35).contains(n)) {
        return Some(format!("F{n}"));
    }
    let named = match key {
        "space" => "space",
        "enter" | "return" => "Return",
        "tab" => "Tab",
        "escape" | "esc" => "Escape",
        "backspace" => "BackSpace",
        "delete" => "Delete",
        "insert" => "Insert",
        "home" => "Home",
        "end" => "End",
        "pageup" => "Page_Up",
        "pagedown" => "Page_Down",
        _ => return None,
    };
    Some(named.into())
}

/// The modifiers a word names, in one spelling.
fn modifier(word: &str) -> Option<&'static str> {
    Some(match word {
        "ctrl" | "control" | "ctl" | "primary" => "ctrl",
        "alt" | "option" | "opt" | "mod1" => "alt",
        "shift" => "shift",
        "super" | "cmd" | "command" | "meta" | "logo" | "win" | "windows" | "mod4" => "super",
        _ => return None,
    })
}

/// Whether the system's text for how a shortcut is triggered ("Press <ctrl><alt>l" on
/// GNOME, "Ctrl+Alt+L" on KDE) names the shortcut from the settings ("ctrl+alt+l"): every
/// modifier and the key, and no other modifier. Words that are neither (GNOME's
/// "Press", in any language) do not matter.
pub fn trigger_matches(config: &str, description: &str) -> bool {
    let parts: Vec<String> = config.split('+').map(|p| p.trim().to_lowercase()).collect();
    let Some((key, mods)) = parts.split_last() else { return false };
    let Some(key) = keysym_name(key).map(|k| k.to_lowercase()) else { return false };
    let Some(mut want): Option<Vec<&str>> = mods.iter().map(|m| modifier(m)).collect() else { return false };
    want.sort_unstable();
    want.dedup();
    let words: Vec<String> = description
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect();
    let mut have: Vec<&str> = words.iter().filter_map(|w| modifier(w)).collect();
    have.sort_unstable();
    have.dedup();
    // A key that is itself a modifier's name cannot be told apart; none of ours is.
    have == want && words.iter().any(|w| *w == key || (key == "return" && w == "enter"))
}

/// The system's text for a trigger as people read keys: GNOME's GTK accelerator text
/// ("Press <ctrl><alt>l") becomes "Press Ctrl+Alt+L"; anything else is kept as it is.
pub fn pretty_trigger(description: &str) -> String {
    let mut out = String::new();
    let mut rest = description;
    while let Some(start) = rest.find('<') {
        out.push_str(&rest[..start]);
        let mut names = Vec::new();
        let mut tail = &rest[start..];
        while let Some(inner) = tail.strip_prefix('<') {
            let Some(end) = inner.find('>') else { break };
            names.push(&inner[..end]);
            tail = &inner[end + 1..];
        }
        if names.is_empty() {
            // A lone "<": keep it.
            out.push('<');
            rest = &rest[start + 1..];
            continue;
        }
        let key_len = tail.find(|c: char| c.is_whitespace()).unwrap_or(tail.len());
        let key = &tail[..key_len];
        let mut parts: Vec<String> = names
            .iter()
            .map(|n| match modifier(&n.to_lowercase()) {
                Some("ctrl") => "Ctrl".to_string(),
                Some("alt") => "Alt".to_string(),
                Some("shift") => "Shift".to_string(),
                Some(_) => "Super".to_string(),
                None => capitalized(n),
            })
            .collect();
        if !key.is_empty() {
            parts.push(capitalized(key));
        }
        out.push_str(&parts.join("+"));
        rest = &tail[key_len..];
    }
    out.push_str(rest);
    out
}

fn capitalized(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// How the cursor highlight is drawn in this session.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LocatePath {
    /// An X11 session: the cursor is read from the X server, as always.
    X11,
    /// A Wayland session without layer shell (GNOME), with XWayland's screens lining up
    /// with the outputs (XWayland coordinates = output coordinates × the factor): a
    /// transparent XWayland window over every screen learns where the pointer is.
    XWayland(f64),
    /// A compositor with wlr-layer-shell (KDE, sway, Hyprland): a transparent surface
    /// over each output learns where the pointer is.
    LayerShell,
    None(NoLocate),
}

/// Why the highlight cannot be drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoLocate {
    /// Neither WAYLAND_DISPLAY nor DISPLAY.
    NoDisplay,
    /// No layer shell, and XWayland's screens are not the outputs (or it reports none).
    Misaligned,
    /// No layer shell and no XWayland.
    Nothing,
}

impl NoLocate {
    pub fn explain(self) -> &'static str {
        match self {
            NoLocate::NoDisplay => "no display (neither WAYLAND_DISPLAY nor DISPLAY)",
            NoLocate::Misaligned => "no layer shell, and XWayland's screens do not line up with the Wayland outputs",
            NoLocate::Nothing => "the compositor has neither wlr-layer-shell nor XWayland",
        }
    }
}

/// Picks the way: `layer_shell` None when the compositor could not be reached,
/// `xwayland` the factor from `edge_logic::xwayland_scale` (None: no XWayland screens,
/// or they do not line up with the outputs).
pub fn locate_path(wayland: bool, x11_display: bool, layer_shell: Option<bool>, xwayland: Option<f64>) -> LocatePath {
    if !wayland {
        return if x11_display { LocatePath::X11 } else { LocatePath::None(NoLocate::NoDisplay) };
    }
    if layer_shell == Some(true) {
        return LocatePath::LayerShell;
    }
    match (x11_display, xwayland) {
        (true, Some(k)) => LocatePath::XWayland(k),
        (true, None) => LocatePath::None(NoLocate::Misaligned),
        (false, _) => LocatePath::None(NoLocate::Nothing),
    }
}

/// A point on a surface whose top-left corner is at `origin` on the desktop, in desktop
/// (logical) pixels.
pub fn on_desktop(origin: (i32, i32), local: (f64, f64)) -> (f64, f64) {
    (f64::from(origin.0) + local.0, f64::from(origin.1) + local.1)
}

/// Where a square surface of `side` logical pixels around `cursor` goes on its output
/// (x, y, width, height on the desktop): the margins from the output's left and top edges,
/// the side (no larger than the output), and the cursor's place inside the square. The
/// square stays on the output; near an edge the cursor is off its centre instead.
pub fn place_square(output: (i32, i32, i32, i32), cursor: (f64, f64), side: i32) -> (i32, i32, i32, (f64, f64)) {
    let (ox, oy, w, h) = output;
    let side = side.min(w).min(h).max(1);
    let half = f64::from(side) / 2.0;
    let left = ((cursor.0 - f64::from(ox) - half).round() as i32).clamp(0, (w - side).max(0));
    let top = ((cursor.1 - f64::from(oy) - half).round() as i32).clamp(0, (h - side).max(0));
    let center = (cursor.0 - f64::from(ox + left), cursor.1 - f64::from(oy + top));
    (left, top, side, center)
}

/// What the shortcut service did, as the running app writes it for the settings window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortalState {
    /// Nothing to register (shortcuts off), or an X11 session.
    Off,
    /// The system is asking the user.
    Asking,
    /// Registered; the portal's version (2 can open the system's settings for them).
    Bound(u32),
    /// The user turned the request down.
    Denied,
    /// The desktop has no GlobalShortcuts portal.
    Missing,
    /// The portal needs an app id, and flockswitch has none (no flockswitch.desktop).
    NoAppId,
    Error,
}

impl PortalState {
    pub fn code(self) -> String {
        match self {
            PortalState::Off => "off".into(),
            PortalState::Asking => "asking".into(),
            PortalState::Bound(v) => format!("bound:{v}"),
            PortalState::Denied => "denied".into(),
            PortalState::Missing => "missing".into(),
            PortalState::NoAppId => "no-app-id".into(),
            PortalState::Error => "error".into(),
        }
    }

    /// None for an empty or unknown code (an older running app, or none running).
    pub fn parse(code: &str) -> Option<PortalState> {
        let (name, arg) = code.split_once(':').unwrap_or((code, ""));
        Some(match name {
            "off" => PortalState::Off,
            "asking" => PortalState::Asking,
            "bound" => PortalState::Bound(arg.parse().unwrap_or(1)),
            "denied" => PortalState::Denied,
            "missing" => PortalState::Missing,
            "no-app-id" => PortalState::NoAppId,
            "error" => PortalState::Error,
            _ => return None,
        })
    }
}

/// The commands a custom system shortcut can run where the shortcut service is missing
/// or refused.
pub fn fallback_commands(exe: &str) -> Vec<String> {
    (1..=3).map(|n| format!("{exe} switch {n}")).chain([format!("{exe} locate")]).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn triggers_in_the_portal_notation() {
        assert_eq!(portal_trigger("ctrl+alt+l").as_deref(), Some("CTRL+ALT+l"));
        assert_eq!(portal_trigger("ctrl+alt+1").as_deref(), Some("CTRL+ALT+1"));
        assert_eq!(portal_trigger("Ctrl+Alt+Shift+K").as_deref(), Some("CTRL+ALT+SHIFT+k"));
        assert_eq!(portal_trigger("shift+alt+ctrl+k").as_deref(), Some("CTRL+ALT+SHIFT+k"), "a fixed order");
        assert_eq!(portal_trigger("ctrl+alt+f5").as_deref(), Some("CTRL+ALT+F5"));
        assert_eq!(portal_trigger("f12").as_deref(), Some("F12"));
        assert_eq!(portal_trigger("super+l").as_deref(), Some("LOGO+l"));
        assert_eq!(portal_trigger("control+option+KeyL").as_deref(), Some("CTRL+ALT+l"));
        assert_eq!(portal_trigger("ctrl+alt+Digit3").as_deref(), Some("CTRL+ALT+3"));
        assert_eq!(portal_trigger("ctrl+alt+enter").as_deref(), Some("CTRL+ALT+Return"));
        assert_eq!(portal_trigger(" ctrl + alt + 2 ").as_deref(), Some("CTRL+ALT+2"));
        for bad in ["", "ctrl+alt+", "ctrl+alt+é", "hyper+l", "ctrl+alt+f99", "ctrl+alt+volumeup"] {
            assert_eq!(portal_trigger(bad), None, "{bad}");
        }
    }

    #[test]
    fn what_is_asked_for() {
        let all = wanted(Some(["ctrl+alt+1", "", "Ctrl+Alt+3"]), Some("ctrl+alt+l"));
        let ids: Vec<&str> = all.iter().map(|w| w.id.as_str()).collect();
        assert_eq!(ids, ["switch-to-channel-1", "switch-to-channel-3", "find-cursor"]);
        assert_eq!(all[1].key, "ctrl+alt+3");
        assert_eq!(all[1].trigger.as_deref(), Some("CTRL+ALT+3"));
        assert_eq!((all[2].slot, all[2].trigger.as_deref()), (LOCATE_SLOT, Some("CTRL+ALT+l")));
        assert!(wanted(None, None).is_empty());
        assert_eq!(wanted(None, Some("ctrl+alt+l")).len(), 1);
        assert!(wanted(Some(["", "", ""]), Some(" ")).is_empty());
        for w in &all {
            assert_eq!(slot_of(&w.id), Some(w.slot));
        }
        assert_eq!(slot_of("switch-to-channel-4"), None);
        assert_eq!(slot_of("something"), None);
    }

    #[test]
    fn a_refused_request_is_not_repeated() {
        let asked = wanted(Some(["ctrl+alt+1", "ctrl+alt+2", "ctrl+alt+3"]), Some("ctrl+alt+l"));
        assert!(already_refused(&asked, &asked));
        // Paused from the menu: only the locate one is left, which was refused too.
        assert!(already_refused(&wanted(None, Some("ctrl+alt+l")), &asked));
        // A new key asks again.
        assert!(!already_refused(&wanted(None, Some("ctrl+alt+k")), &asked));
        assert!(!already_refused(&[], &asked));
    }

    #[test]
    fn the_systems_trigger_text() {
        assert!(trigger_matches("ctrl+alt+l", "Press Ctrl+Alt+L"));
        assert!(trigger_matches("ctrl+alt+l", "Ctrl+Alt+L"));
        assert!(trigger_matches("ctrl+alt+l", "CTRL+ALT+l"));
        assert!(trigger_matches("ctrl+alt+1", "Ctrl+Alt+1 누르기"));
        assert!(trigger_matches("ctrl+shift+a", "Shift+Ctrl+A"));
        assert!(trigger_matches("ctrl+alt+f5", "Press Ctrl+Alt+F5"));
        assert!(trigger_matches("super+l", "Meta+L"));
        assert!(!trigger_matches("ctrl+alt+l", "Press Ctrl+Alt+K"));
        assert!(!trigger_matches("ctrl+alt+1", "Press Shift+Ctrl+Alt+1"), "an extra modifier");
        assert!(!trigger_matches("ctrl+alt+1", "Press Ctrl+1"), "a modifier missing");
        assert!(!trigger_matches("ctrl+alt+1", ""));
        assert!(!trigger_matches("", "Ctrl+Alt+1"));
    }

    #[test]
    fn gnome_accelerator_text() {
        assert!(trigger_matches("ctrl+alt+l", "Press <ctrl><alt>l"));
        assert!(trigger_matches("ctrl+alt+l", "Press <Control><Alt>l"));
        assert!(trigger_matches("ctrl+alt+1", "Press <Primary><Alt>1"));
        assert!(trigger_matches("ctrl+alt+f5", "Press <ctrl><alt>F5"));
        assert!(!trigger_matches("ctrl+alt+l", "Press <ctrl><alt><shift>l"));
        assert_eq!(pretty_trigger("Press <ctrl><alt>l"), "Press Ctrl+Alt+L");
        assert_eq!(pretty_trigger("Press <Control><Alt>1 or <super>F5"), "Press Ctrl+Alt+1 or Super+F5");
        assert_eq!(pretty_trigger("<ctrl><alt>l 누르기"), "Ctrl+Alt+L 누르기");
        assert_eq!(pretty_trigger("Ctrl+Alt+L"), "Ctrl+Alt+L");
        assert_eq!(pretty_trigger("a < b"), "a < b");
        assert_eq!(pretty_trigger(""), "");
        assert!(trigger_matches("ctrl+alt+l", &pretty_trigger("Press <ctrl><alt>l")));
    }

    #[test]
    fn choosing_how_to_draw() {
        assert_eq!(locate_path(false, true, None, None), LocatePath::X11);
        assert_eq!(locate_path(false, false, None, None), LocatePath::None(NoLocate::NoDisplay));
        // KDE, sway, Hyprland: layer shell even with XWayland there.
        assert_eq!(locate_path(true, true, Some(true), Some(1.0)), LocatePath::LayerShell);
        assert_eq!(locate_path(true, false, Some(true), None), LocatePath::LayerShell);
        // GNOME: XWayland when its screens are the outputs (or a multiple of them).
        assert_eq!(locate_path(true, true, Some(false), Some(1.0)), LocatePath::XWayland(1.0));
        assert_eq!(locate_path(true, true, Some(false), Some(2.0)), LocatePath::XWayland(2.0));
        assert_eq!(locate_path(true, true, Some(false), None), LocatePath::None(NoLocate::Misaligned));
        assert_eq!(locate_path(true, false, Some(false), None), LocatePath::None(NoLocate::Nothing));
        // The compositor did not answer, XWayland is there and lines up with nothing known.
        assert_eq!(locate_path(true, true, None, None), LocatePath::None(NoLocate::Misaligned));
    }

    #[test]
    fn surface_points_on_the_desktop() {
        assert_eq!(on_desktop((0, 0), (10.5, 20.0)), (10.5, 20.0));
        assert_eq!(on_desktop((1536, -100), (4.0, 300.25)), (1540.0, 200.25));
    }

    #[test]
    fn the_square_stays_on_its_output() {
        // ThinkPad eDP-1: logical 1536x864, a 420 px square.
        let out = (0, 0, 1536, 864);
        assert_eq!(place_square(out, (768.0, 432.0), 420), (558, 222, 420, (210.0, 210.0)));
        // Near the top-left corner the square stops at the edges; the cursor is off centre.
        assert_eq!(place_square(out, (10.0, 5.0), 420), (0, 0, 420, (10.0, 5.0)));
        // Bottom-right.
        assert_eq!(place_square(out, (1530.0, 860.0), 420), (1116, 444, 420, (414.0, 416.0)));
        // An output to the right of the first one.
        assert_eq!(place_square((1536, 0, 1920, 1080), (1600.0, 500.0), 420), (0, 290, 420, (64.0, 210.0)));
        // Larger than the output: as large as it allows.
        assert_eq!(place_square((0, 0, 300, 200), (150.0, 100.0), 420), (50, 0, 200, (100.0, 100.0)));
    }

    #[test]
    fn portal_states_round_trip() {
        for s in [
            PortalState::Off,
            PortalState::Asking,
            PortalState::Bound(1),
            PortalState::Bound(2),
            PortalState::Denied,
            PortalState::Missing,
            PortalState::NoAppId,
            PortalState::Error,
        ] {
            assert_eq!(PortalState::parse(&s.code()), Some(s));
        }
        assert_eq!(PortalState::parse(""), None);
        assert_eq!(PortalState::parse("bound"), Some(PortalState::Bound(1)));
        assert_eq!(PortalState::parse("future-state"), None);
    }

    #[test]
    fn fallback_commands_cover_every_shortcut() {
        assert_eq!(
            fallback_commands("/home/me/.local/bin/flockswitch"),
            [
                "/home/me/.local/bin/flockswitch switch 1",
                "/home/me/.local/bin/flockswitch switch 2",
                "/home/me/.local/bin/flockswitch switch 3",
                "/home/me/.local/bin/flockswitch locate"
            ]
        );
    }
}
