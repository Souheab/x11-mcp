use std::collections::HashMap;

use x11rb::{
    connection::Connection,
    protocol::xproto::{ConnectionExt as _, GetKeyboardMappingReply, GetModifierMappingReply},
    rust_connection::RustConnection,
};

use crate::{ControllerError, ErrorCode, Result};

const XK_BACKSPACE: u32 = 0xff08;
const XK_TAB: u32 = 0xff09;
const XK_RETURN: u32 = 0xff0d;
const XK_ESCAPE: u32 = 0xff1b;
const XK_HOME: u32 = 0xff50;
const XK_LEFT: u32 = 0xff51;
const XK_UP: u32 = 0xff52;
const XK_RIGHT: u32 = 0xff53;
const XK_DOWN: u32 = 0xff54;
const XK_PAGE_UP: u32 = 0xff55;
const XK_PAGE_DOWN: u32 = 0xff56;
const XK_END: u32 = 0xff57;
const XK_INSERT: u32 = 0xff63;
const XK_DELETE: u32 = 0xffff;
const XK_SHIFT_L: u32 = 0xffe1;
const XK_CONTROL_L: u32 = 0xffe3;
const XK_META_L: u32 = 0xffe7;
const XK_ALT_L: u32 = 0xffe9;
const XK_SUPER_L: u32 = 0xffeb;
const XK_MODE_SWITCH: u32 = 0xff7e;

#[derive(Debug, Clone, Copy)]
pub(crate) struct KeyStroke {
    pub keycode: u8,
    pub shift: bool,
    pub mode_switch: bool,
}

pub(crate) struct KeyboardMap {
    first_keycode: u8,
    reply: GetKeyboardMappingReply,
    modifiers: GetModifierMappingReply,
}

impl KeyboardMap {
    pub(crate) fn load(connection: &RustConnection) -> Result<Self> {
        let setup = connection.setup();
        let first_keycode = setup.min_keycode;
        let count = setup.max_keycode.saturating_sub(first_keycode) + 1;
        let reply = connection
            .get_keyboard_mapping(first_keycode, count)
            .map_err(|error| ControllerError::x11("request keyboard map", error))?
            .reply()
            .map_err(|error| ControllerError::x11("read keyboard map", error))?;
        let modifiers = connection
            .get_modifier_mapping()
            .map_err(|error| ControllerError::x11("request modifier map", error))?
            .reply()
            .map_err(|error| ControllerError::x11("read modifier map", error))?;
        Ok(Self {
            first_keycode,
            reply,
            modifiers,
        })
    }

    pub(crate) fn text_strokes(&self, text: &str) -> Option<Vec<KeyStroke>> {
        text.chars()
            .map(|character| self.for_char(character))
            .collect()
    }

    pub(crate) fn named_key(&self, name: &str) -> Result<KeyStroke> {
        let normalized = name.trim().to_uppercase().replace(['-', ' '], "_");
        let keysym = named_keysyms()
            .get(normalized.as_str())
            .copied()
            .or_else(|| {
                let mut characters = name.chars();
                let character = characters.next()?;
                if characters.next().is_some() {
                    None
                } else if character.is_ascii_alphabetic() {
                    Some(char_to_keysym(character.to_ascii_lowercase()))
                } else {
                    Some(char_to_keysym(character))
                }
            })
            .ok_or_else(|| {
                ControllerError::new(ErrorCode::InvalidInput, format!("unknown key name: {name}"))
            })?;
        self.find(keysym).ok_or_else(|| {
            ControllerError::new(
                ErrorCode::UnsupportedCapability,
                format!("key is not present in the active keyboard map: {name}"),
            )
        })
    }

    pub(crate) fn shift_keycode(&self) -> Result<u8> {
        self.modifier_keycode(0)
            .or_else(|| self.find(XK_SHIFT_L).map(|stroke| stroke.keycode))
            .ok_or_else(|| {
                ControllerError::new(ErrorCode::UnsupportedCapability, "no Shift key in keymap")
            })
    }

    pub(crate) fn mode_switch_keycode(&self) -> Result<u8> {
        self.find(XK_MODE_SWITCH)
            .map(|stroke| stroke.keycode)
            .or_else(|| self.modifier_keycode(7))
            .ok_or_else(|| {
                ControllerError::new(
                    ErrorCode::UnsupportedCapability,
                    "no level-three modifier in keymap",
                )
            })
    }

    fn for_char(&self, character: char) -> Option<KeyStroke> {
        self.find(char_to_keysym(character))
    }

    fn find(&self, keysym: u32) -> Option<KeyStroke> {
        let per_keycode = usize::from(self.reply.keysyms_per_keycode);
        if per_keycode == 0 {
            return None;
        }
        self.reply
            .keysyms
            .chunks(per_keycode)
            .enumerate()
            .find_map(|(index, symbols)| {
                symbols
                    .iter()
                    .take(4)
                    .position(|symbol| *symbol == keysym)
                    .and_then(|column| {
                        let keycode = usize::from(self.first_keycode).checked_add(index)?;
                        Some(KeyStroke {
                            keycode: u8::try_from(keycode).ok()?,
                            shift: column % 2 == 1,
                            mode_switch: column >= 2,
                        })
                    })
            })
    }

    fn modifier_keycode(&self, modifier_index: usize) -> Option<u8> {
        let per_modifier = usize::from(self.modifiers.keycodes_per_modifier());
        self.modifiers
            .keycodes
            .get(modifier_index * per_modifier..(modifier_index + 1) * per_modifier)?
            .iter()
            .copied()
            .find(|keycode| *keycode != 0)
    }
}

fn char_to_keysym(character: char) -> u32 {
    let value = u32::from(character);
    if value <= 0xff {
        value
    } else {
        0x0100_0000 | value
    }
}

fn named_keysyms() -> HashMap<&'static str, u32> {
    let mut keys = HashMap::from([
        ("BACKSPACE", XK_BACKSPACE),
        ("TAB", XK_TAB),
        ("ENTER", XK_RETURN),
        ("RETURN", XK_RETURN),
        ("ESC", XK_ESCAPE),
        ("ESCAPE", XK_ESCAPE),
        ("HOME", XK_HOME),
        ("LEFT", XK_LEFT),
        ("UP", XK_UP),
        ("RIGHT", XK_RIGHT),
        ("DOWN", XK_DOWN),
        ("PAGE_UP", XK_PAGE_UP),
        ("PAGEUP", XK_PAGE_UP),
        ("PAGE_DOWN", XK_PAGE_DOWN),
        ("PAGEDOWN", XK_PAGE_DOWN),
        ("END", XK_END),
        ("INSERT", XK_INSERT),
        ("DELETE", XK_DELETE),
        ("SPACE", u32::from(b' ')),
        ("SHIFT", XK_SHIFT_L),
        ("CTRL", XK_CONTROL_L),
        ("CONTROL", XK_CONTROL_L),
        ("ALT", XK_ALT_L),
        ("META", XK_META_L),
        ("SUPER", XK_SUPER_L),
    ]);
    for index in 1..=12_u32 {
        let name: &'static str = Box::leak(format!("F{index}").into_boxed_str());
        keys.insert(name, 0xffbd + index);
    }
    keys
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unicode_keysym_uses_x11_encoding() {
        assert_eq!(char_to_keysym('A'), 65);
        assert_eq!(char_to_keysym('世'), 0x0100_4e16);
    }
}
