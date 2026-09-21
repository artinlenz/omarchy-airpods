// SPDX-License-Identifier: AGPL-3.0-only
// Protocol/cryptographic routines adapted from LibrePods by Kavish Devar and
// contributors: linux-rust/src/bluetooth/{aacp,le}.rs and src/utils.rs.
// https://github.com/kavishdevar/librepods
// Changes: bounds-checked pure parsers, no logging/persistence, conservative
// unknown values, no unsolicited feature writes or media takeover.
use aes::{Aes128, cipher::{BlockDecrypt, BlockEncrypt, KeyInit, generic_array::GenericArray}};
use crate::model::{Battery, Cell, Mode};

pub const HANDSHAKE: &[u8] = &[0, 0, 4, 0, 1, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0];
pub const NOTIFICATIONS: &[u8] = &[4, 0, 4, 0, 0x0f, 0, 0xff, 0xff, 0xff, 0xff];
pub const KEYS_REQUEST: &[u8] = &[4, 0, 4, 0, 0x30, 0, 5, 0];

pub fn mode_packet(mode: Mode) -> [u8; 11] { [4, 0, 4, 0, 9, 0, 0x0d, mode.value(), 0, 0, 0] }

// Keys deliberately do not implement Debug or Serialize at the packet layer.
#[derive(Clone, Default)]
pub struct ReceivedKeys { pub irk: Option<[u8; 16]>, pub enc: Option<[u8; 16]> }
pub enum Packet {
    Ear([Option<bool>; 2]),
    Batteries { battery: Battery, primary_left: Option<bool> },
    Mode(Option<Mode>),
    Keys(ReceivedKeys),
}

fn ear(value: u8) -> Option<bool> {
    match value { 0 => Some(true), 1 | 2 => Some(false), _ => None }
}

pub fn parse(packet: &[u8]) -> Option<Packet> {
    if !packet.starts_with(&[4, 0, 4, 0]) { return None; }
    let p = packet.get(4..)?;
    if p.get(1) != Some(&0) { return None; }
    match *p.first()? {
        6 => Some(Packet::Ear([ear(*p.get(2)?), ear(*p.get(3)?)])),
        9 if p.get(2) == Some(&0x0d) && p.len() >= 7 => Some(Packet::Mode(Mode::from_value(p[3]))),
        4 => {
            let count = *p.get(2)? as usize;
            let entries = p.get(3..3 + count * 5)?;
            let mut batteries = Battery::default();
            // AAP's first earbud entry identifies its primary, independently
            // of the rotating BLE advertiser's primary role.
            let primary_left = entries.chunks_exact(5).find_map(|e| match e[0] {
                4 => Some(true), 2 => Some(false), _ => None,
            });
            for e in entries.chunks_exact(5) {
                let cell = if e[2] <= 100 && matches!(e[3], 1 | 2) {
                    Cell { percent: Some(e[2]), charging: Some(e[3] == 1) }
                } else { Cell::default() };
                match e[0] { 4 => batteries.left = cell, 2 => batteries.right = cell, 8 => batteries.case = cell, _ => {} }
            }
            Some(Packet::Batteries { battery: batteries, primary_left })
        }
        0x31 => {
            let count = *p.get(2)? as usize;
            let mut offset = 3;
            let mut keys = ReceivedKeys::default();
            for _ in 0..count {
                let h = p.get(offset..offset + 4)?;
                let len = u16::from_le_bytes([h[2], h[3]]) as usize;
                offset += 4;
                let bytes = p.get(offset..offset.checked_add(len)?)?;
                match h[0] {
                    1 | 4 => {
                        let key: [u8; 16] = bytes.try_into().ok()?;
                        if key == [0; 16] { return None; }
                        let slot = if h[0] == 1 { &mut keys.irk } else { &mut keys.enc };
                        if slot.is_some() { return None; }
                        *slot = Some(key);
                    }
                    _ => {}
                }
                offset += len;
            }
            Some(Packet::Keys(keys))
        }
        _ => None,
    }
}

pub fn resolve_rpa(address: [u8; 6], irk: &[u8; 16]) -> bool {
    // Address is canonical display order. The most significant random bits
    // must mark a resolvable private address (01).
    if address[0] & 0xc0 != 0x40 { return false; }
    let mut key = *irk;
    key.reverse();
    let mut input = [0u8; 16];
    input[13..].copy_from_slice(&address[..3]);
    let cipher = Aes128::new(GenericArray::from_slice(&key));
    let mut block = GenericArray::clone_from_slice(&input);
    cipher.encrypt_block(&mut block);
    block[13..] == address[3..]
}

pub struct Advertisement { pub ears: [Option<bool>; 2], pub battery: Battery }

fn battery_byte(value: u8) -> Cell {
    if value == 0xff || value & 0x7f > 100 { Cell::default() }
    else { Cell { percent: Some(value & 0x7f), charging: Some(value & 0x80 != 0) } }
}

pub fn advertisement(data: &[u8], enc: &[u8; 16]) -> Option<Advertisement> {
    // Only the established 27-byte paired proximity layout is understood.
    // Unknown future/pairing frames cannot authorize a connection.
    if data.len() != 27 || data[0] != 7 || data[1] != 25 || data[2] == 0 { return None; }
    let status = data[5];
    let primary_left = status & 0x20 != 0;
    let primary_in_case = status & 0x40 != 0;
    let flip = primary_left ^ primary_in_case;
    let left = status & if flip { 2 } else { 8 } != 0;
    let right = status & if flip { 8 } else { 2 } != 0;
    let ears = if status & 4 != 0 {
        if left || right { [None; 2] } else { [Some(false); 2] }
    } else { [Some(left), Some(right)] };
    let cipher = Aes128::new(GenericArray::from_slice(enc));
    let mut block = GenericArray::clone_from_slice(&data[11..27]);
    cipher.decrypt_block(&mut block);
    let battery = Battery {
        left: battery_byte(block[if primary_left { 1 } else { 2 }]),
        right: battery_byte(block[if primary_left { 2 } else { 1 }]),
        case: battery_byte(block[3]),
    };
    Some(Advertisement { ears, battery })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn identity_resolution_requires_matching_hash_and_private_address_type() {
        let irk = [0x9b, 0x7d, 0x39, 0x0a, 0xa6, 0x10, 0x10, 0x34, 0x05, 0xad, 0xc8, 0x57, 0xa3, 0x34, 0x02, 0xec];
        assert!(resolve_rpa([0x70, 0x81, 0x94, 0x0d, 0xfb, 0xaa], &irk));
        assert!(!resolve_rpa([0x70, 0x81, 0x94, 0x0d, 0xfb, 0xab], &irk));
        assert!(!resolve_rpa([0xf0, 0x81, 0x94, 0x0d, 0xfb, 0xaa], &irk));
    }

    #[test]
    fn short_ear_and_key_packets_are_not_observations() {
        for n in 0..8 { assert!(parse(&[4, 0, 4, 0, 6, 0, 0, 0][..n]).is_none()); }
        let mut keys = vec![4, 0, 4, 0, 0x31, 0, 1, 1, 0, 16, 0];
        keys.extend([1; 16]);
        for n in 0..keys.len() { assert!(parse(&keys[..n]).is_none()); }
        assert!(matches!(parse(&keys), Some(Packet::Keys(_))));
    }
    #[test]
    fn unknown_ear_status_never_authorizes() {
        let Some(Packet::Ear(ears)) = parse(&[4, 0, 4, 0, 6, 0, 3, 0xff]) else { panic!("ear packet") };
        assert!(!crate::model::wearing(ears));
    }
    #[test]
    fn disconnected_battery_is_unknown_not_zero() {
        let Some(Packet::Batteries { battery: b, .. }) = parse(&[4, 0, 4, 0, 4, 0, 1, 4, 0, 0, 4, 0]) else { panic!("battery packet") };
        assert_eq!(b.left, Cell::default());
    }
    #[test]
    fn aap_primary_follows_first_earbud_not_case_or_component_number() {
        let Some(Packet::Batteries { primary_left, .. }) = parse(&[
            4, 0, 4, 0, 4, 0, 3,
            8, 1, 90, 2, 1, 2, 1, 80, 2, 1, 4, 1, 70, 2, 1,
        ]) else { panic!("battery packet") };
        assert_eq!(primary_left, Some(false));
        let Some(Packet::Batteries { primary_left, .. }) = parse(&[
            4, 0, 4, 0, 4, 0, 2,
            4, 1, 70, 2, 1, 2, 1, 80, 2, 1,
        ]) else { panic!("battery packet") };
        assert_eq!(primary_left, Some(true));
    }
    #[test]
    fn contradictory_case_bits_never_authorize() {
        let mut bytes = [0; 27]; bytes[0] = 7; bytes[1] = 25; bytes[2] = 1; bytes[5] = 4 | 2;
        assert!(!crate::model::wearing(advertisement(&bytes, &[1; 16]).unwrap().ears));
        bytes[5] = 0;
        assert!(!crate::model::wearing(advertisement(&bytes, &[1; 16]).unwrap().ears));
    }
}
