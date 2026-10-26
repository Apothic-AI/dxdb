//! Base64, standard alphabet, **no padding**.
//!
//! `specs/encryption.md` section 3.8 fixes this: an envelope is stored as a
//! base64 string with the standard alphabet and no `=` padding, because the
//! length is recovery-derivable from the payload and padding would make the
//! stored representation of a given document depend on its field size mod 3.
//!
//! Implemented here rather than pulled from a crate so the exact alphabet and
//! padding behaviour are pinned by tests instead of by a dependency's defaults,
//! and so the wasm build gains no dependency for 40 lines of table lookup.

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Encodes `bytes` without padding.
pub fn encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = chunk.get(1).copied().map(u32::from);
        let b2 = chunk.get(2).copied().map(u32::from);
        let triple = (b0 << 16) | (b1.unwrap_or(0) << 8) | b2.unwrap_or(0);
        out.push(ALPHABET[((triple >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((triple >> 12) & 0x3f) as usize] as char);
        if b1.is_some() {
            out.push(ALPHABET[((triple >> 6) & 0x3f) as usize] as char);
        }
        if b2.is_some() {
            out.push(ALPHABET[(triple & 0x3f) as usize] as char);
        }
    }
    out
}

/// Decodes unpadded base64.
///
/// Rejects: a non-alphabet byte, a length that cannot arise from encoding
/// (`len % 4 == 1`), and any trailing bits that are not zero. The last rule
/// matters: a decoder that ignores stray low bits accepts several distinct
/// strings for one byte sequence, which would make an envelope's textual form
/// non-canonical and give an attacker a way to change a stored value without
/// changing its meaning.
pub fn decode(text: &str) -> Result<Vec<u8>, Base64Error> {
    let bytes = text.as_bytes();
    // Validate the alphabet first, so a garbage byte is reported as a garbage byte
    // rather than as a length problem that happens to coincide with it.
    for byte in bytes {
        decode_char(*byte)?;
    }
    if bytes.len() % 4 == 1 {
        return Err(Base64Error::Length);
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3 + 2);
    for (chunk_index, chunk) in bytes.chunks(4).enumerate() {
        let mut triple = 0u32;
        let mut values = [0u8; 4];
        for (index, byte) in chunk.iter().enumerate() {
            values[index] = decode_char(*byte).expect("validated above");
            triple |= u32::from(values[index]) << (18 - 6 * index);
        }
        let is_last = chunk_index == bytes.len().div_ceil(4) - 1;
        out.push(((triple >> 16) & 0xff) as u8);
        match chunk.len() {
            4 => {
                out.push(((triple >> 8) & 0xff) as u8);
                out.push((triple & 0xff) as u8);
            }
            3 => {
                // Two output bytes. The third sextet contributes bits 6..11, of
                // which only 8..11 are used, so bits 6..7 must be zero.
                if triple & 0b1100_0000 != 0 {
                    return Err(Base64Error::TrailingBits);
                }
                out.push(((triple >> 8) & 0xff) as u8);
            }
            2 => {
                // One output byte. The second sextet contributes bits 12..17, of
                // which only 16..17 are used, so bits 12..15 must be zero.
                if triple & 0b1111_0000_0000_0000 != 0 {
                    return Err(Base64Error::TrailingBits);
                }
            }
            _ => return Err(Base64Error::Length),
        }
        if is_last && chunk.len() == 1 {
            return Err(Base64Error::Length);
        }
    }
    Ok(out)
}

fn decode_char(byte: u8) -> Result<u8, Base64Error> {
    match byte {
        b'A'..=b'Z' => Ok(byte - b'A'),
        b'a'..=b'z' => Ok(byte - b'a' + 26),
        b'0'..=b'9' => Ok(byte - b'0' + 52),
        b'+' => Ok(62),
        b'/' => Ok(63),
        _ => Err(Base64Error::Character(byte as char)),
    }
}

/// Why decoding failed. No variant carries the input, so an error can never
/// echo a ciphertext or a plaintext.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Base64Error {
    /// The length cannot arise from encoding.
    #[error("invalid base64 length")]
    Length,
    /// A byte outside the alphabet.
    #[error("invalid base64 character {0:?}")]
    Character(char),
    /// Non-zero bits after the final complete byte, so the text is not canonical.
    #[error("non-canonical base64 trailing bits")]
    TrailingBits,
}

#[cfg(test)]
mod tests {
    use super::{decode, encode, Base64Error};

    #[test]
    fn round_trips_every_length_up_to_a_few_words() {
        for len in 0..32usize {
            let input: Vec<u8> = (0..len).map(|i| (i as u8).wrapping_mul(37)).collect();
            let text = encode(&input);
            assert_eq!(decode(&text).expect("valid"), input, "len {len}");
            assert!(!text.contains('='), "no padding is emitted");
        }
    }

    #[test]
    fn matches_the_standard_alphabet() {
        assert_eq!(encode(b""), "");
        assert_eq!(encode(b"f"), "Zg");
        assert_eq!(encode(b"fo"), "Zm8");
        assert_eq!(encode(b"foo"), "Zm9v");
        assert_eq!(encode(b"foob"), "Zm9vYg");
        assert_eq!(encode(b"fooba"), "Zm9vYmE");
        assert_eq!(encode(b"foobar"), "Zm9vYmFy");
        // The two bytes that use the '+' and '/' code points.
        assert_eq!(encode(&[0xfb, 0xff]), "+/8");
        assert_eq!(decode("+/8").expect("valid"), vec![0xfb, 0xff]);
    }

    #[test]
    fn rejects_padding_characters() {
        // The envelope never carries padding; accepting it would make two textual
        // forms of one value.
        assert_eq!(decode("Zg=="), Err(Base64Error::Character('=')));
    }

    #[test]
    fn rejects_a_length_impossible_from_encoding() {
        assert_eq!(decode("Z"), Err(Base64Error::Length));
        assert_eq!(decode("Zm9vY"), Err(Base64Error::Length));
    }

    #[test]
    fn rejects_non_canonical_trailing_bits() {
        // "Zh" decodes to the same byte as "Zg" if the low bits are ignored.
        assert_eq!(decode("Zg").expect("valid"), vec![b'f']);
        assert_eq!(decode("Zh"), Err(Base64Error::TrailingBits));
        assert_eq!(decode("Zm8").expect("valid"), vec![b'f', b'o']);
        // 61 = 0b111101 has non-zero low bits for a 3-character group.
        assert_eq!(decode("Zm9"), Err(Base64Error::TrailingBits));
    }

    #[test]
    fn rejects_a_non_alphabet_byte() {
        assert_eq!(decode("Zm9v!"), Err(Base64Error::Character('!')));
        assert_eq!(decode("Zm9v\n"), Err(Base64Error::Character('\n')));
    }
}
