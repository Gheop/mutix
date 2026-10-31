//! What happens to a recognized word: typed into the focused window through a
//! virtual keyboard, or handed to a shell command.
//!
//! A uinput keyboard sends key positions, not characters, and the compositor
//! reads them with the user's layout. So each character is looked up in that
//! layout with xkbcommon to find which key and modifiers produce it.

use std::collections::HashMap;
use std::process::{Command, Stdio};
use std::thread::sleep;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use evdev::uinput::VirtualDevice;
use evdev::{AttributeSet, EventType, InputEvent, KeyCode};
use xkbcommon::xkb;

const SHIFT: u16 = 42; // KEY_LEFTSHIFT
const ALTGR: u16 = 100; // KEY_RIGHTALT, ISO_Level3_Shift on most layouts
const CTRL: u16 = 29; // KEY_LEFTCTRL
const ALT: u16 = 56; // KEY_LEFTALT
const SUPER: u16 = 125; // KEY_LEFTMETA

/// A key combination such as "ctrl+shift+z": modifier key codes and the
/// keysym of the last key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Combo {
    pub modifiers: Vec<u16>,
    pub keysym: u32,
}

pub fn parse_keys(spec: &str) -> Result<Combo> {
    let parts: Vec<&str> = spec.split('+').map(str::trim).filter(|p| !p.is_empty()).collect();
    let Some((last, mods)) = parts.split_last() else {
        bail!("empty key combination");
    };
    let mut modifiers = Vec::new();
    for m in mods {
        modifiers.push(match m.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => CTRL,
            "shift" => SHIFT,
            "alt" => ALT,
            "super" | "meta" | "win" => SUPER,
            other => bail!("unknown modifier {other:?} in {spec:?}"),
        });
    }
    let keysym = xkb::keysym_from_name(last, xkb::KEYSYM_NO_FLAGS).raw();
    if keysym == 0 {
        bail!("unknown key {last:?} in {spec:?}; use xkb names such as Return, Escape, BackSpace or a letter");
    }
    Ok(Combo { modifiers, keysym })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stroke {
    /// evdev key code.
    pub key: u16,
    pub shift: bool,
    pub altgr: bool,
}

/// Character to key strokes for one XKB layout.
pub struct Layout {
    strokes: HashMap<u32, Stroke>,
}

/// Dead key and base letter for accented letters most layouts lack as keys.
fn decompose(c: char) -> Option<(u32, char)> {
    const CIRCUMFLEX: u32 = 0xfe52;
    const DIAERESIS: u32 = 0xfe57;
    const GRAVE: u32 = 0xfe50;
    const ACUTE: u32 = 0xfe51;
    const TILDE: u32 = 0xfe53;
    let table: &[(&str, u32, &str)] = &[
        ("âêîôûÂÊÎÔÛ", CIRCUMFLEX, "aeiouAEIOU"),
        ("äëïöüÿÄËÏÖÜŸ", DIAERESIS, "aeiouyAEIOUY"),
        ("àèìòùÀÈÌÒÙ", GRAVE, "aeiouAEIOU"),
        ("áéíóúýÁÉÍÓÚÝ", ACUTE, "aeiouyAEIOUY"),
        ("ãñõÃÑÕ", TILDE, "anoANO"),
    ];
    for (accented, dead, base) in table {
        if let Some(i) = accented.chars().position(|a| a == c) {
            return base.chars().nth(i).map(|b| (*dead, b));
        }
    }
    None
}

impl Layout {
    /// `spec` is "fr", "us+dvorak" or similar; `None` reads the GNOME setting.
    pub fn load(spec: Option<&str>) -> Result<Layout> {
        let spec = match spec {
            Some(s) => s.to_string(),
            None => gnome_layout().unwrap_or_else(|| "us".to_string()),
        };
        let (layout, variant) = spec.split_once('+').unwrap_or((&spec, ""));
        let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let keymap = xkb::Keymap::new_from_names(
            &ctx,
            "evdev",
            "pc105",
            layout,
            variant,
            None,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .with_context(|| format!("unknown keyboard layout {spec:?}"))?;
        let shift_bit = 1u32 << keymap.mod_get_index(xkb::MOD_NAME_SHIFT);
        let altgr_bit = 1u32 << keymap.mod_get_index("Mod5");

        let mut strokes = HashMap::new();
        let (min, max) = (keymap.min_keycode().raw(), keymap.max_keycode().raw());
        for raw in min..=max {
            let kc = xkb::Keycode::new(raw);
            if raw < 8 || raw - 8 > 0x2ff {
                continue;
            }
            for level in 0..keymap.num_levels_for_key(kc, 0) {
                let syms = keymap.key_get_syms_by_level(kc, 0, level);
                if syms.len() != 1 {
                    continue;
                }
                let mut masks = [0u32; 8];
                let found = keymap.key_get_mods_for_level(kc, 0, level, &mut masks);
                // Only Shift and AltGr can be pressed by us; keep the simplest mask.
                let Some(mask) = masks[..found]
                    .iter()
                    .filter(|m| **m & !(shift_bit | altgr_bit) == 0)
                    .min_by_key(|m| m.count_ones())
                else {
                    continue;
                };
                let stroke = Stroke {
                    key: (raw - 8) as u16,
                    shift: mask & shift_bit != 0,
                    altgr: mask & altgr_bit != 0,
                };
                let cost = |s: &Stroke| u32::from(s.shift) + 2 * u32::from(s.altgr);
                strokes
                    .entry(syms[0].raw())
                    .and_modify(|old: &mut Stroke| {
                        if cost(&stroke) < cost(old) {
                            *old = stroke;
                        }
                    })
                    .or_insert(stroke);
            }
        }
        Ok(Layout { strokes })
    }

    fn keysym_for(c: char) -> u32 {
        match c {
            '\n' => 0xff0d,    // Return
            '\t' => 0xff09,    // Tab
            '\u{8}' => 0xff08, // BackSpace
            _ => xkb::utf32_to_keysym(c as u32).raw(),
        }
    }

    pub fn stroke_for_keysym(&self, keysym: u32) -> Option<Stroke> {
        self.strokes.get(&keysym).copied()
    }

    /// Strokes for one character; `None` if the layout cannot type it.
    pub fn strokes(&self, c: char) -> Option<Vec<Stroke>> {
        if let Some(s) = self.strokes.get(&Self::keysym_for(c)) {
            return Some(vec![*s]);
        }
        let (dead, base) = decompose(c)?;
        Some(vec![
            *self.strokes.get(&dead)?,
            *self.strokes.get(&Self::keysym_for(base))?,
        ])
    }
}

/// First XKB source of `org.gnome.desktop.input-sources`, e.g. "fr".
fn gnome_layout() -> Option<String> {
    let out = Command::new("gsettings")
        .args(["get", "org.gnome.desktop.input-sources", "sources"])
        .output()
        .ok()?;
    let text = String::from_utf8(out.stdout).ok()?;
    parse_gnome_sources(&text)
}

fn parse_gnome_sources(text: &str) -> Option<String> {
    // [('xkb', 'fr'), ('xkb', 'us+dvorak')]
    let start = text.find("('xkb', '")? + "('xkb', '".len();
    let end = text[start..].find('\'')? + start;
    Some(text[start..end].to_string())
}

pub struct Keyboard {
    device: VirtualDevice,
    layout: Layout,
}

impl Keyboard {
    pub fn new(layout: Layout) -> Result<Keyboard> {
        let mut keys = AttributeSet::<KeyCode>::new();
        for code in 1..=248u16 {
            keys.insert(KeyCode(code));
        }
        let device = VirtualDevice::builder()
            .and_then(|b| b.name("mutix virtual keyboard").with_keys(&keys))
            .and_then(|b| b.build())
            .context("cannot create a virtual keyboard; mutix needs write access to /dev/uinput (see the README)")?;
        // The compositor needs a moment to pick up a new input device.
        sleep(Duration::from_millis(400));
        Ok(Keyboard { device, layout })
    }

    fn key(&mut self, code: u16, down: bool) -> Result<()> {
        self.device
            .emit(&[InputEvent::new(EventType::KEY.0, code, i32::from(down))])?;
        Ok(())
    }

    fn tap(&mut self, s: Stroke) -> Result<()> {
        if s.shift {
            self.key(SHIFT, true)?;
        }
        if s.altgr {
            self.key(ALTGR, true)?;
        }
        self.key(s.key, true)?;
        self.key(s.key, false)?;
        if s.altgr {
            self.key(ALTGR, false)?;
        }
        if s.shift {
            self.key(SHIFT, false)?;
        }
        sleep(Duration::from_millis(4));
        Ok(())
    }

    /// Ctrl+Shift+U, the code point, Space: GTK and IBus Unicode entry.
    fn unicode(&mut self, c: char) -> Result<()> {
        let Some(u) = self.layout.strokes('u') else {
            bail!("cannot type {c:?} with this layout");
        };
        self.key(CTRL, true)?;
        self.key(SHIFT, true)?;
        self.key(u[0].key, true)?;
        self.key(u[0].key, false)?;
        self.key(SHIFT, false)?;
        self.key(CTRL, false)?;
        for h in format!("{:x}", c as u32).chars() {
            let strokes = self.layout.strokes(h).with_context(|| format!("cannot type {c:?}"))?;
            for s in strokes {
                self.tap(s)?;
            }
        }
        if let Some(space) = self.layout.strokes(' ') {
            self.tap(space[0])?;
        }
        Ok(())
    }

    /// Presses a combination such as "ctrl+c" with the user's layout, so the
    /// letter is the one printed on their key.
    pub fn press_keys(&mut self, spec: &str) -> Result<()> {
        let combo = parse_keys(spec)?;
        let stroke = self
            .layout
            .stroke_for_keysym(combo.keysym)
            .with_context(|| format!("no key for {spec:?} in this layout"))?;
        for m in &combo.modifiers {
            self.key(*m, true)?;
        }
        self.tap(stroke)?;
        for m in combo.modifiers.iter().rev() {
            self.key(*m, false)?;
        }
        Ok(())
    }

    pub fn type_text(&mut self, text: &str) -> Result<()> {
        for c in text.chars() {
            match self.layout.strokes(c) {
                Some(strokes) => {
                    for s in strokes {
                        self.tap(s)?;
                    }
                }
                None => self.unicode(c)?,
            }
        }
        Ok(())
    }
}

/// Runs a command without waiting for it; the word is passed as $MUTIX_WORD.
pub fn run_command(command: &str, word: &str) -> Result<()> {
    Command::new("sh")
        .args(["-c", command])
        .env("MUTIX_WORD", word)
        .stdin(Stdio::null())
        .spawn()
        .with_context(|| format!("cannot run {command:?}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gnome_sources() {
        assert_eq!(parse_gnome_sources("[('xkb', 'fr')]").as_deref(), Some("fr"));
        assert_eq!(
            parse_gnome_sources("[('ibus', 'anthy'), ('xkb', 'us+dvorak')]").as_deref(),
            Some("us+dvorak")
        );
        assert_eq!(parse_gnome_sources("@a(ss) []"), None);
    }

    #[test]
    fn azerty_keys() {
        let fr = Layout::load(Some("fr")).unwrap();
        // On AZERTY the A key sits where Q is on QWERTY (KEY_Q = 16).
        assert_eq!(
            fr.strokes('a'),
            Some(vec![Stroke {
                key: 16,
                shift: false,
                altgr: false
            }])
        );
        assert_eq!(
            fr.strokes('A'),
            Some(vec![Stroke {
                key: 16,
                shift: true,
                altgr: false
            }])
        );
        // Digits need Shift on AZERTY; é is on the 2 key.
        assert_eq!(
            fr.strokes('1'),
            Some(vec![Stroke {
                key: 2,
                shift: true,
                altgr: false
            }])
        );
        assert_eq!(
            fr.strokes('é'),
            Some(vec![Stroke {
                key: 3,
                shift: false,
                altgr: false
            }])
        );
        assert_eq!(fr.strokes('@').map(|s| s[0].altgr), Some(true));
        // ê goes through the dead circumflex key.
        assert_eq!(fr.strokes('ê').map(|s| s.len()), Some(2));
    }

    #[test]
    fn key_combinations() {
        let c = parse_keys("ctrl+shift+z").unwrap();
        assert_eq!(c.modifiers, vec![CTRL, SHIFT]);
        assert_eq!(c.keysym, 0x7a);
        assert_eq!(parse_keys("Return").unwrap().keysym, 0xff0d);
        assert!(parse_keys("hyper+x").is_err());
        assert!(parse_keys("ctrl+Nope").is_err());
        // On AZERTY, Ctrl+Z must press the key printed Z (KEY_W position).
        let fr = Layout::load(Some("fr")).unwrap();
        assert_eq!(fr.stroke_for_keysym(0x7a).map(|s| s.key), Some(17));
    }

    #[test]
    fn qwerty_keys() {
        let us = Layout::load(Some("us")).unwrap();
        assert_eq!(
            us.strokes('a'),
            Some(vec![Stroke {
                key: 30,
                shift: false,
                altgr: false
            }])
        );
        assert_eq!(
            us.strokes('\n'),
            Some(vec![Stroke {
                key: 28,
                shift: false,
                altgr: false
            }])
        );
    }
}
