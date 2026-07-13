//! Minimal OSC 1.0 message decoder + Vance address convention.
//!
//! Enough of OSC to drive a control surface from a phone app (TouchOSC,
//! OSC/PILOT, …) over UDP: a single message with an address, a type-tag
//! string, and `int32`/`float32`/`string` arguments (big-endian, 4-byte
//! aligned). Bundles are not decoded (v1 uses plain messages).

use crate::ControlEvent;

/// A decoded OSC argument (only the types we act on).
#[derive(Clone, Debug, PartialEq)]
pub enum OscArg {
    /// 32-bit int.
    Int(i32),
    /// 32-bit float.
    Float(f32),
    /// UTF-8 string.
    Str(String),
}

impl OscArg {
    /// Numeric view (`Int`/`Float`), else `None`.
    pub fn as_f32(&self) -> Option<f32> {
        match self {
            OscArg::Int(i) => Some(*i as f32),
            OscArg::Float(f) => Some(*f),
            OscArg::Str(_) => None,
        }
    }
}

/// A decoded OSC message.
#[derive(Clone, Debug, PartialEq)]
pub struct OscMessage {
    /// Address pattern, e.g. `/vance/fader/0`.
    pub address: String,
    /// Arguments in order.
    pub args: Vec<OscArg>,
}

/// Decode a single OSC packet into a message. Returns `None` on anything
/// malformed or on a bundle (`#bundle`).
pub fn decode_message(packet: &[u8]) -> Option<OscMessage> {
    let mut pos = 0;
    let address = read_osc_string(packet, &mut pos)?;
    if !address.starts_with('/') {
        return None; // bundles ("#bundle") + junk rejected
    }
    // Type-tag string. Absent tags → no args (some senders omit it).
    let tags = match read_osc_string(packet, &mut pos) {
        Some(t) if t.starts_with(',') => t,
        _ => return Some(OscMessage { address, args: Vec::new() }),
    };
    let mut args = Vec::new();
    for tag in tags[1..].chars() {
        match tag {
            'i' => args.push(OscArg::Int(i32::from_be_bytes(read_4(packet, &mut pos)?))),
            'f' => args.push(OscArg::Float(f32::from_be_bytes(read_4(packet, &mut pos)?))),
            's' => args.push(OscArg::Str(read_osc_string(packet, &mut pos)?)),
            // Types we don't act on: consume nothing we can't size → bail.
            'T' | 'F' | 'N' | 'I' => {} // no payload (bool/nil/infinitum)
            _ => return None,
        }
    }
    Some(OscMessage { address, args })
}

/// Map a decoded message to a [`ControlEvent`] via the Vance convention:
/// - `/vance/button/<n>` (arg > 0.5 = press) → `Button`
/// - `/vance/fader/<n>` (arg 0..1) → `Fader`
/// - `/vance/jog` (arg = ticks) → `Jog`
pub fn to_control_event(msg: &OscMessage) -> Option<ControlEvent> {
    let a = msg.address.as_str();
    let first = msg.args.first().and_then(|x| x.as_f32());
    if let Some(n) = a.strip_prefix("/vance/button/") {
        let id: u32 = n.parse().ok()?;
        return Some(ControlEvent::Button {
            id,
            pressed: first.unwrap_or(1.0) > 0.5,
        });
    }
    if let Some(n) = a.strip_prefix("/vance/fader/") {
        let id: u32 = n.parse().ok()?;
        return Some(ControlEvent::Fader {
            id,
            value: first.unwrap_or(0.0),
        });
    }
    if a == "/vance/jog" {
        return Some(ControlEvent::Jog {
            ticks: first.unwrap_or(0.0).round() as i32,
        });
    }
    None
}

/// Read a null-terminated, 4-byte-aligned OSC string, advancing `pos`.
fn read_osc_string(buf: &[u8], pos: &mut usize) -> Option<String> {
    let start = *pos;
    let nul = buf[start..].iter().position(|&b| b == 0)?;
    let s = std::str::from_utf8(&buf[start..start + nul]).ok()?.to_string();
    // Advance past the string + null, padded to the next 4-byte boundary.
    let len = nul + 1;
    let padded = (len + 3) & !3;
    *pos = start + padded;
    if *pos > buf.len() {
        return None;
    }
    Some(s)
}

fn read_4(buf: &[u8], pos: &mut usize) -> Option<[u8; 4]> {
    if *pos + 4 > buf.len() {
        return None;
    }
    let out = [buf[*pos], buf[*pos + 1], buf[*pos + 2], buf[*pos + 3]];
    *pos += 4;
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an OSC packet for `address` with one float arg.
    fn osc_float(address: &str, val: f32) -> Vec<u8> {
        let mut p = Vec::new();
        push_string(&mut p, address);
        push_string(&mut p, ",f");
        p.extend_from_slice(&val.to_be_bytes());
        p
    }
    fn push_string(p: &mut Vec<u8>, s: &str) {
        p.extend_from_slice(s.as_bytes());
        p.push(0);
        while p.len() % 4 != 0 {
            p.push(0);
        }
    }

    #[test]
    fn decodes_float_message() {
        let pkt = osc_float("/vance/fader/0", 0.75);
        let msg = decode_message(&pkt).unwrap();
        assert_eq!(msg.address, "/vance/fader/0");
        assert_eq!(msg.args, vec![OscArg::Float(0.75)]);
    }

    #[test]
    fn maps_convention_addresses() {
        let f = decode_message(&osc_float("/vance/fader/2", 0.5)).unwrap();
        assert_eq!(to_control_event(&f), Some(ControlEvent::Fader { id: 2, value: 0.5 }));
        let b = decode_message(&osc_float("/vance/button/1", 1.0)).unwrap();
        assert_eq!(to_control_event(&b), Some(ControlEvent::Button { id: 1, pressed: true }));
        let bp = decode_message(&osc_float("/vance/button/1", 0.0)).unwrap();
        assert_eq!(to_control_event(&bp), Some(ControlEvent::Button { id: 1, pressed: false }));
        let j = decode_message(&osc_float("/vance/jog", -2.0)).unwrap();
        assert_eq!(to_control_event(&j), Some(ControlEvent::Jog { ticks: -2 }));
        // Unknown address → None.
        let u = decode_message(&osc_float("/other/thing", 1.0)).unwrap();
        assert_eq!(to_control_event(&u), None);
    }

    #[test]
    fn rejects_truncated_and_bundles() {
        assert!(decode_message(b"#bundle\0").is_none());
        assert!(decode_message(&[]).is_none());
        // Address ok but the float arg is missing → None.
        let mut p = Vec::new();
        push_string(&mut p, "/vance/fader/0");
        push_string(&mut p, ",f");
        assert!(decode_message(&p).is_none());
    }
}
