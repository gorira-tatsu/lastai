use anyhow::{Result, bail};

pub fn encode_u64(mut value: u64, out: &mut Vec<u8>) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

pub fn decode_u64(input: &[u8], cursor: &mut usize) -> Result<u64> {
    let mut shift = 0;
    let mut value = 0u64;
    while *cursor < input.len() {
        let byte = input[*cursor];
        *cursor += 1;
        value |= ((byte & 0x7f) as u64) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
        shift += 7;
        if shift > 63 {
            bail!("varint is too large");
        }
    }
    bail!("truncated varint")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varint_roundtrip() {
        for value in [0, 1, 127, 128, 255, 16_384, u32::MAX as u64, u64::MAX / 2] {
            let mut bytes = Vec::new();
            encode_u64(value, &mut bytes);
            let mut cursor = 0;
            assert_eq!(decode_u64(&bytes, &mut cursor).unwrap(), value);
            assert_eq!(cursor, bytes.len());
        }
    }
}
