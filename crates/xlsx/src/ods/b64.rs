//! Base64 for pictures stored inside flat ODF (`office:binary-data`).

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub(crate) fn encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk.first().copied().unwrap_or(0), chunk.get(1).copied().unwrap_or(0), chunk.get(2).copied().unwrap_or(0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for (i, shift) in [18u32, 12, 6, 0].iter().enumerate() {
            if i <= chunk.len() {
                out.push(char::from(ALPHABET.get(((n >> shift) & 63) as usize).copied().unwrap_or(b'A')));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Decodes standard base64, whitespace ignored; `None` for other characters or more than `limit`
/// bytes of output.
pub(crate) fn decode(s: &str, limit: usize) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity((s.len() / 4 * 3).min(limit));
    let (mut buf, mut bits) = (0u32, 0u32);
    for c in s.bytes().filter(|c| !c.is_ascii_whitespace() && *c != b'=') {
        let v = ALPHABET.iter().position(|a| *a == c)? as u32;
        buf = (buf << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
            buf &= (1 << bits) - 1;
            if out.len() > limit {
                return None;
            }
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        for data in [&b""[..], b"a", b"ab", b"abc", b"abcd", &[0u8, 255, 128, 7, 9][..]] {
            assert_eq!(decode(&encode(data), 1 << 20).as_deref(), Some(data));
        }
        assert_eq!(encode(b"Man"), "TWFu");
        assert_eq!(decode("TW Fu\n", 10).as_deref(), Some(&b"Man"[..]));
        assert_eq!(decode("TW!u", 10), None);
        assert_eq!(decode("TWFuTWFu", 3), None);
    }
}
