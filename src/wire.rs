//! Check the small subset of Yjs used by MockPad before Yrs allocates or decodes it.
//! In particular, nested Any values and nested shared types are not part of our document.
use serde::Deserialize;
use yrs::encoding::read::{Cursor, Read};

const MAX_CLOCK: u32 = 16 * 1024 * 1024;
pub const COLORS: [&str; 6] = [
    "#237b65", "#b15e25", "#6752b8", "#246da4", "#b1446b", "#7b741d",
];

#[derive(Default)]
pub struct Cost {
    pub bytes: usize,
    pub blocks: usize,
}

pub fn validate(data: &[u8]) -> Result<Cost, ()> {
    let mut cursor = Cursor::new(data);
    let mut cost = Cost::default();
    let mut messages = 0;
    while cursor.has_content() {
        messages += 1;
        if messages > 16 {
            return Err(());
        }
        match uint(&mut cursor)? {
            0 => {
                let kind = uint(&mut cursor)?;
                let bytes = cursor.read_buf().map_err(|_| ())?;
                match kind {
                    0 => vector(bytes)?,
                    1 | 2 => {
                        cost.bytes += bytes.len();
                        cost.blocks += update(bytes)?;
                    }
                    _ => return Err(()),
                }
            }
            1 => presence(cursor.read_buf().map_err(|_| ())?)?,
            3 => {}
            _ => return Err(()),
        }
    }
    Ok(cost)
}

fn uint(cursor: &mut Cursor<'_>) -> Result<u32, ()> {
    // Yrs permits wrapping u32 varints for compatibility, while some downstream
    // counts are read as u64/usize. Reject overflow before those counts allocate.
    let mut value = 0;
    for shift in [0, 7, 14, 21, 28] {
        let byte = cursor.read_u8().map_err(|_| ())?;
        if shift == 28 && byte > 15 {
            return Err(());
        }
        value |= u32::from(byte & 127) << shift;
        if byte < 128 {
            return Ok(value);
        }
    }
    Err(())
}

fn count(cursor: &mut Cursor<'_>) -> Result<u32, ()> {
    let n = uint(cursor)?;
    if n as usize > cursor.buf.len() - cursor.next {
        return Err(());
    }
    Ok(n)
}

fn clock(cursor: &mut Cursor<'_>) -> Result<u32, ()> {
    let n = uint(cursor)?;
    if n > MAX_CLOCK {
        return Err(());
    }
    Ok(n)
}

fn id(cursor: &mut Cursor<'_>) -> Result<(), ()> {
    uint(cursor)?;
    clock(cursor)?;
    Ok(())
}

fn end(cursor: &Cursor<'_>) -> Result<(), ()> {
    if cursor.has_content() {
        Err(())
    } else {
        Ok(())
    }
}

fn vector(data: &[u8]) -> Result<(), ()> {
    let mut cursor = Cursor::new(data);
    for _ in 0..count(&mut cursor)? {
        id(&mut cursor)?;
    }
    end(&cursor)
}

fn update(data: &[u8]) -> Result<usize, ()> {
    let mut cursor = Cursor::new(data);
    let mut blocks = 0;
    for _ in 0..count(&mut cursor)? {
        let n = count(&mut cursor)?;
        blocks += n as usize;
        uint(&mut cursor)?;
        let mut position = clock(&mut cursor)?;
        for _ in 0..n {
            let info = cursor.read_u8().map_err(|_| ())?;
            let len = if info == 0 || info == 10 {
                clock(&mut cursor)?
            } else {
                let origin = info & 0x80 != 0;
                let right = info & 0x40 != 0;
                if origin {
                    id(&mut cursor)?;
                }
                if right {
                    id(&mut cursor)?;
                }
                if !origin && !right {
                    match uint(&mut cursor)? {
                        1 => match cursor.read_string().map_err(|_| ())? {
                            "code" | "settings" => {}
                            _ => return Err(()),
                        },
                        0 => id(&mut cursor)?,
                        _ => return Err(()),
                    }
                    if info & 0x20 != 0 {
                        match cursor.read_string().map_err(|_| ())? {
                            "language" | "tabSize" => {}
                            _ => return Err(()),
                        }
                    }
                }
                match info & 0x1f {
                    1 => clock(&mut cursor)?, // deleted content
                    4 => cursor.read_string().map_err(|_| ())?.encode_utf16().count() as u32,
                    8 => {
                        let values = count(&mut cursor)?;
                        for _ in 0..values {
                            match cursor.read_u8().map_err(|_| ())? {
                                119 => {
                                    cursor.read_string().map_err(|_| ())?;
                                }
                                125 => {
                                    // tabSize is the only integer in this document.
                                    // Its values fit one byte, so no untrusted signed-varint
                                    // arithmetic reaches Yrs's general-purpose decoder.
                                    if ![2, 4, 8].contains(&cursor.read_u8().map_err(|_| ())?) {
                                        return Err(());
                                    }
                                }
                                _ => return Err(()),
                            }
                        }
                        values
                    }
                    _ => return Err(()),
                }
            };
            position = position
                .checked_add(len)
                .filter(|n| *n <= MAX_CLOCK)
                .ok_or(())?;
            if len == 0 {
                return Err(());
            }
        }
    }
    for _ in 0..count(&mut cursor)? {
        uint(&mut cursor)?;
        let ranges = count(&mut cursor)?;
        blocks += ranges as usize;
        for _ in 0..ranges {
            let start = clock(&mut cursor)?;
            let len = clock(&mut cursor)?;
            if len == 0 || start.checked_add(len).is_none_or(|n| n > MAX_CLOCK) {
                return Err(());
            }
        }
    }
    end(&cursor)?;
    Ok(blocks)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Presence {
    user: Option<User>,
    selection: Option<Selection>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct User {
    name: String,
    color: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Selection {
    anchor: Position,
    head: Position,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Position {
    #[serde(rename = "type")]
    branch: Option<PositionId>,
    tname: Option<String>,
    item: Option<PositionId>,
    #[serde(default)]
    assoc: i32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PositionId {
    #[serde(rename = "client")]
    _client: u32,
    clock: u32,
}

impl Position {
    fn valid(&self) -> bool {
        self.branch.is_none()
            && self.tname.as_deref().is_none_or(|name| name == "code")
            && self.item.as_ref().is_none_or(|id| id.clock <= MAX_CLOCK)
            && (-1..=1).contains(&self.assoc)
    }
}

fn presence(data: &[u8]) -> Result<(), ()> {
    if data.len() > 2048 {
        return Err(());
    }
    let mut cursor = Cursor::new(data);
    if uint(&mut cursor)? != 1 {
        return Err(());
    }
    uint(&mut cursor)?; // socket ownership is checked separately
    let version = uint(&mut cursor)?;
    // Disconnect increments this clock; leave room for that increment.
    if version >= MAX_CLOCK {
        return Err(());
    }
    let json = cursor.read_string().map_err(|_| ())?;
    let state: Option<Presence> = serde_json::from_str(json).map_err(|_| ())?;
    if let Some(state) = state {
        if let Some(user) = state.user
            && (user.name.trim().is_empty()
                || user.name.encode_utf16().count() > 40
                || user.name.chars().any(char::is_control)
                || !COLORS.contains(&user.color.as_str()))
        {
            return Err(());
        }
        if state
            .selection
            .is_some_and(|s| !s.anchor.valid() || !s.head.valid())
        {
            return Err(());
        }
    }
    end(&cursor)
}

#[cfg(test)]
mod tests {
    use super::*;
    use yrs::{Doc, Map, ReadTxn, StateVector, Text, Transact, encoding::write::Write};

    fn packet(tag: u8, body: &[u8]) -> Vec<u8> {
        let mut result = vec![0, tag];
        result.write_buf(body);
        result
    }

    #[test]
    fn accepts_text_settings_unicode_and_deletion() {
        let doc = Doc::new();
        let text = doc.get_or_insert_text("code");
        let settings = doc.get_or_insert_map("settings");
        let mut txn = doc.transact_mut();
        text.insert(&mut txn, 0, "hello 日本語 🦀\n");
        settings.insert(&mut txn, "language", "rust");
        settings.insert(&mut txn, "tabSize", 4);
        text.remove_range(&mut txn, 0, 1);
        let update = txn.encode_state_as_update_v1(&StateVector::default());
        assert!(validate(&packet(2, &update)).is_ok());
        assert!(validate(&packet(0, &[0])).is_ok());
    }

    #[test]
    fn rejects_huge_declared_counts_truncation_and_trailing_garbage() {
        let huge = [255, 255, 255, 255, 15];
        assert!(validate(&packet(0, &huge)).is_err());
        assert!(validate(&packet(2, &huge)).is_err());
        assert!(validate(&packet(2, &[1, 1, 1, 0, 4])).is_err());
        assert!(validate(&packet(2, &[0, 0, 0])).is_err());
        let mut awareness = vec![1];
        awareness.write_buf(huge);
        assert!(validate(&awareness).is_err());
        let mut overflow = Vec::new();
        overflow.write_var((1u64 << 32) + 1);
        overflow.extend_from_slice(&[42, 1]);
        overflow.write_string("null");
        let mut awareness = vec![1];
        awareness.write_buf(overflow);
        assert!(validate(&awareness).is_err());
    }

    #[test]
    fn rejects_nested_any_before_recursive_decode() {
        // One settings item, whose Any value is an array nested 4096 times.
        let mut update = vec![1, 1, 1, 0, 0x28, 1];
        update.write_string("settings");
        update.write_string("language");
        update.write_var(1u32);
        for _ in 0..4096 {
            update.extend_from_slice(&[117, 1]);
        }
        update.extend_from_slice(&[126, 0]);
        assert!(validate(&packet(2, &update)).is_err());
        let mut bad_integer = vec![1, 1, 1, 0, 0x28, 1];
        bad_integer.write_string("settings");
        bad_integer.write_string("tabSize");
        bad_integer.extend_from_slice(&[1, 125]);
        bad_integer.extend_from_slice(&[255; 11]);
        bad_integer.push(0);
        assert!(validate(&packet(2, &bad_integer)).is_err());
    }

    #[test]
    fn checks_presence_types_selection_and_color() {
        fn check(json: &str) -> bool {
            let mut body = vec![1, 42, 1];
            body.write_string(json);
            let mut packet = vec![1];
            packet.write_buf(body);
            validate(&packet).is_ok()
        }
        assert!(check(
            r##"{"user":{"name":"面接官","color":"#237b65"},"selection":{"anchor":{"tname":"code","assoc":0},"head":{"tname":"code","item":{"client":42,"clock":0},"assoc":0}}}"##
        ));
        assert!(check("null"));
        assert!(check("{}"));
        assert!(!check(
            r##"{"user":{"name":{"toString":null},"color":"#237b65"}}"##
        ));
        assert!(!check(r##"{"user":{"name":"x","color":"red"}}"##));
        assert!(!check(
            r#"{"selection":{"anchor":{},"head":{"item":{"clock":"oops","client":42}}}}"#
        ));
        assert!(!check(
            r##"{"user":{"name":"x","color":"#237b65"},"other":{}}"##
        ));
    }
}
