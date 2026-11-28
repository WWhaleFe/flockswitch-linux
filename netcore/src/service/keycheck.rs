//! The key check (2.5): a master shows, for a key pressed on any computer of its group, what
//! every other computer gets for it under the key rules. Each pair of computers has its own
//! `KeyMapper`, so chords and the Korean/English key come out as they would really be sent.
use super::KeyMapper;
use crate::keymap::{chord_name, key_name};
use crate::{DirRules, InputEvent, KeyCheckEntry, KeyCheckTarget, Os};
use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Instant;

/// Entries kept in the window's log.
pub(super) const KEEP: usize = 40;

/// A computer taking part: its id, name and system.
#[derive(Clone)]
pub(super) struct Computer {
    pub id: String,
    pub name: String,
    pub os: Os,
}

#[derive(Default)]
struct Pair {
    mapper: KeyMapper,
    /// The destination keys this pair holds down so far (for "Command+C").
    held: HashSet<u16>,
}

#[derive(Default)]
pub(super) struct KeyCheck {
    pairs: HashMap<(String, String), Pair>,
    /// Source keys held per computer (for the pressed key's own name).
    held: HashMap<String, HashSet<u16>>,
    pub entries: VecDeque<KeyCheckEntry>,
    seq: u64,
}

impl KeyCheck {
    /// Records a key edge from `from` and what each computer in `to` gets for it. `live`:
    /// the computer the key really went to. Returns whether an entry was added.
    #[allow(clippy::too_many_arguments)]
    pub fn record(
        &mut self,
        from: &Computer,
        usage: u16,
        down: bool,
        to: &[Computer],
        live: Option<&str>,
        rules: impl Fn(Os, Os) -> Option<DirRules>,
        now: Instant,
    ) -> bool {
        let held = self.held.entry(from.id.clone()).or_default();
        let key = chord_name(held, usage, from.os);
        if down {
            held.insert(usage);
        } else {
            held.remove(&usage);
        }
        let mut targets = Vec::new();
        let mut any = false;
        for dst in to.iter().filter(|c| c.id != from.id) {
            let pair = self.pairs.entry((from.id.clone(), dst.id.clone())).or_default();
            let event = InputEvent::Key { usage, down };
            let out = match rules(from.os, dst.os) {
                Some(r) => {
                    pair.mapper.set_lang1_toggle(true);
                    pair.mapper.map(event, from.os, dst.os, &r, now)
                }
                None => vec![event],
            };
            let keys = describe(&mut pair.held, &out, dst.os);
            any |= !keys.is_empty();
            targets.push(KeyCheckTarget { name: dst.name.clone(), os: dst.os, keys, live: live == Some(dst.id.as_str()) });
        }
        // A release is worth a line only when it sends something (a Caps Lock tap).
        if !down && !any {
            return false;
        }
        self.seq += 1;
        self.entries.push_back(KeyCheckEntry { seq: self.seq, from: from.name.clone(), from_os: from.os, key, released: !down, to: targets });
        while self.entries.len() > KEEP {
            self.entries.pop_front();
        }
        true
    }
}

/// The presses in `out` by name, each with the modifiers held at that point; `held` follows
/// the destination's modifiers across calls. A lone modifier press is named by itself.
fn describe(held: &mut HashSet<u16>, out: &[InputEvent], os: Os) -> String {
    let mut parts = Vec::new();
    for event in out {
        let InputEvent::Key { usage, down } = *event else { continue };
        let modifier = (0xe0..=0xe7).contains(&usage);
        if down {
            parts.push(if modifier { key_name(usage, os) } else { chord_name(held, usage, os) });
            if modifier {
                held.insert(usage);
            }
        } else {
            held.remove(&usage);
        }
    }
    // A chord written out as modifier presses and one key ("Command", "Command+Left") reads
    // best as the chord alone.
    if parts.len() > 1 {
        if let Some(last) = parts.iter().rposition(|p| p.contains('+')) {
            let chord = parts[last].clone();
            let rest: Vec<_> = parts.iter().enumerate().filter(|(i, p)| *i != last && !chord.split('+').any(|c| c == p.as_str())).map(|(_, p)| p.clone()).collect();
            return rest.into_iter().chain(std::iter::once(chord)).collect::<Vec<_>>().join(", ");
        }
    }
    parts.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    fn pc(id: &str, os: Os) -> Computer {
        Computer { id: id.into(), name: id.to_uppercase(), os }
    }
    fn rules(src: Os, dst: Os) -> Option<DirRules> {
        (src != dst).then(|| DirRules::default_for(src, dst))
    }
    fn press(k: &mut KeyCheck, from: &Computer, to: &[Computer], keys: &[(u16, bool)]) -> Vec<KeyCheckEntry> {
        let before = k.entries.len();
        let now = Instant::now();
        for &(u, d) in keys {
            k.record(from, u, d, to, None, rules, now);
        }
        k.entries.iter().skip(before).cloned().collect()
    }
    fn got(e: &KeyCheckEntry, name: &str) -> String {
        e.to.iter().find(|t| t.name == name).unwrap().keys.clone()
    }

    #[test]
    fn a_key_from_one_computer_is_shown_as_every_other_computer_gets_it() {
        let (win, mac, linux) = (pc("w", Os::Windows), pc("m", Os::Mac), pc("l", Os::Linux));
        let all = [win.clone(), mac.clone(), linux.clone()];
        let mut k = KeyCheck::default();
        // Windows Ctrl+C: Command+C on the Mac (by function), Ctrl+C on Linux.
        let e = press(&mut k, &win, &all, &[(0xe0, true), (0x06, true), (0x06, false), (0xe0, false)]);
        assert_eq!(e.len(), 2, "{e:?}");
        assert_eq!((e[0].key.as_str(), got(&e[0], "M"), got(&e[0], "L")), ("Ctrl", "Command".into(), "Ctrl".into()));
        assert_eq!((e[1].key.as_str(), got(&e[1], "M"), got(&e[1], "L")), ("Ctrl+C", "Command+C".into(), "Ctrl+C".into()));
        assert!(e[1].to.iter().all(|t| t.name != "W"), "not the pressing computer itself");
        // A sub's key towards another sub: Mac Command is Ctrl on Linux.
        let e = press(&mut k, &mac, &all, &[(0xe3, true), (0xe3, false)]);
        assert_eq!((e[0].key.as_str(), got(&e[0], "L"), got(&e[0], "W")), ("Command", "Ctrl".into(), "Ctrl".into()));
        // Shift stays Shift: Shift+Q types the doubled consonant everywhere.
        let e = press(&mut k, &linux, &all, &[(0xe1, true), (0x14, true), (0x14, false), (0xe1, false)]);
        assert_eq!((e[1].key.as_str(), got(&e[1], "M"), got(&e[1], "W")), ("Shift+Q", "Shift+Q".into(), "Shift+Q".into()));
    }

    #[test]
    fn rewritten_chords_and_the_korean_english_key_show_what_is_really_sent() {
        let (win, mac) = (pc("w", Os::Windows), pc("m", Os::Mac));
        let all = [win.clone(), mac.clone()];
        let mut k = KeyCheck::default();
        // Windows Home goes to a Mac as Command+Left.
        let e = press(&mut k, &win, &all, &[(0x4a, true), (0x4a, false)]);
        assert_eq!(got(&e[0], "M"), "Command+←");
        // The Hangul key goes to a Mac as its Korean/English switch (Lang1 for a 2.3+ Mac).
        let e = press(&mut k, &win, &all, &[(0x90, true), (0x90, false)]);
        assert_eq!((e[0].key.as_str(), got(&e[0], "M")), ("한/영", "한/영".into()));
        // A Mac Caps Lock tap is sent on release: the release gets its own line.
        let e = press(&mut k, &mac, &all, &[(0x39, true), (0x39, false)]);
        assert_eq!(got(&e[0], "W"), "", "nothing yet on the press");
        assert!(e[1].released);
        assert_eq!(got(&e[1], "W"), "한/영");
    }

    #[test]
    fn the_live_computer_is_marked_and_the_log_is_bounded() {
        let (win, mac, linux) = (pc("w", Os::Windows), pc("m", Os::Mac), pc("l", Os::Linux));
        let all = [win.clone(), mac.clone(), linux.clone()];
        let mut k = KeyCheck::default();
        assert!(k.record(&win, 0x04, true, &all, Some("m"), rules, Instant::now()));
        let e = k.entries.back().unwrap();
        assert!(e.to.iter().find(|t| t.name == "M").unwrap().live);
        assert!(!e.to.iter().find(|t| t.name == "L").unwrap().live);
        assert!(!k.record(&win, 0x04, false, &all, Some("m"), rules, Instant::now()), "a plain release adds nothing");
        for _ in 0..KEEP + 5 {
            k.record(&win, 0x05, true, &all, None, rules, Instant::now());
            k.record(&win, 0x05, false, &all, None, rules, Instant::now());
        }
        assert_eq!(k.entries.len(), KEEP);
        assert_eq!(crate::keymap::key_name(0xe7, Os::Windows), "R Win");
    }
}
