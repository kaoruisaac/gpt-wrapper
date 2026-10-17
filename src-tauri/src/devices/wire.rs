//! Bounded Protocomm protobuf subset and the P6/X1 authentication primitives.
use hmac::{Hmac, Mac};
use rand::{rngs::OsRng, RngCore};
use sha2::Sha256;

pub fn random<const N: usize>() -> [u8; N] {
    let mut bytes = [0; N];
    OsRng.fill_bytes(&mut bytes);
    bytes
}
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
pub fn unhex(s: &str, n: usize) -> Result<Vec<u8>, String> {
    if s.len() != n * 2
        || !s
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        return Err("invalid_hex".into());
    }
    (0..n)
        .map(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).map_err(|_| "invalid_hex".into()))
        .collect()
}
pub fn sign(key: &[u8], base: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(base.as_bytes());
    format!("{base}|{}", hex(&mac.finalize().into_bytes()))
}
pub fn verified<'a>(key: &[u8], packet: &'a str) -> Option<Vec<&'a str>> {
    if packet.len() > 512 || !packet.bytes().all(|b| b.is_ascii_graphic()) {
        return None;
    }
    let (base, tag) = packet.rsplit_once('|')?;
    let tag = unhex(tag, 32).ok()?;
    let mut mac = Hmac::<Sha256>::new_from_slice(key).ok()?;
    mac.update(base.as_bytes());
    mac.verify_slice(&tag).ok()?;
    Some(base.split('|').collect())
}
pub fn var(mut n: u64) -> Vec<u8> {
    let mut out = Vec::new();
    while n >= 128 {
        out.push((n as u8 & 127) | 128);
        n >>= 7;
    }
    out.push(n as u8);
    out
}
pub fn num(field: u32, n: u64) -> Vec<u8> {
    [var((field as u64) << 3), var(n)].concat()
}
pub fn blob(field: u32, b: &[u8]) -> Vec<u8> {
    [
        var(((field as u64) << 3) | 2),
        var(b.len() as u64),
        b.to_vec(),
    ]
    .concat()
}
pub fn message(kind: u64, field: u32, b: &[u8]) -> Vec<u8> {
    [num(1, kind), blob(field, b)].concat()
}
fn take_var(b: &mut &[u8]) -> Result<u64, String> {
    let mut n = 0u64;
    for shift in (0..70).step_by(7) {
        let (&byte, rest) = b.split_first().ok_or("truncated_protobuf")?;
        *b = rest;
        if shift == 63 && byte > 1 {
            return Err("protobuf_overflow".into());
        }
        n |= ((byte & 127) as u64) << shift;
        if byte < 128 {
            return Ok(n);
        }
    }
    Err("protobuf_overflow".into())
}
pub struct Field<'a> {
    pub id: u32,
    pub number: u64,
    pub bytes: &'a [u8],
}
pub fn fields(mut b: &[u8]) -> Result<Vec<Field<'_>>, String> {
    if b.len() > 8192 {
        return Err("protobuf_too_large".into());
    }
    let mut out = Vec::new();
    while !b.is_empty() {
        let tag = take_var(&mut b)?;
        if tag >> 3 == 0 {
            return Err("protobuf_field_zero".into());
        }
        let mut f = Field {
            id: (tag >> 3) as u32,
            number: 0,
            bytes: &[],
        };
        match tag & 7 {
            0 => f.number = take_var(&mut b)?,
            2 => {
                let n = usize::try_from(take_var(&mut b)?).map_err(|_| "protobuf_length")?;
                if n > b.len() {
                    return Err("truncated_protobuf".into());
                }
                f.bytes = &b[..n];
                b = &b[n..];
            }
            _ => return Err("unsupported_protobuf_wire".into()),
        }
        out.push(f);
        if out.len() > 256 {
            return Err("protobuf_field_limit".into());
        }
    }
    Ok(out)
}
pub fn bytes(b: &[u8], id: u32) -> Result<&[u8], String> {
    let fs = fields(b)?;
    let matches: Vec<_> = fs.iter().filter(|f| f.id == id).collect();
    if matches.len() != 1 {
        return Err("missing_or_duplicate_protobuf_field".into());
    }
    Ok(matches[0].bytes)
}
pub fn number(b: &[u8], id: u32) -> Result<u64, String> {
    let fs = fields(b)?;
    let matches: Vec<_> = fs.iter().filter(|f| f.id == id).collect();
    if matches.len() > 1 {
        return Err("duplicate_protobuf_field".into());
    }
    Ok(matches.first().map_or(0, |f| f.number))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn signed_frames_are_exact() {
        let key = [17; 32];
        let p = sign(&key, "P6|H|AABBCCDDEEFF|0011223344556677|8899aabbccddeeff");
        assert!(verified(&key, &p).is_some());
        assert!(verified(&[18; 32], &p).is_none());
        assert!(verified(&key, &format!("{p}\n")).is_none());
        assert!(verified(&key, &p.replace("P6|H", "P6|A")).is_none());
    }
    #[test]
    fn bounded_protobuf() {
        let b = message(2, 22, &blob(2, &[1; 32]));
        assert_eq!(number(&b, 1).unwrap(), 2);
        assert_eq!(bytes(bytes(&b, 22).unwrap(), 2).unwrap(), [1; 32]);
        assert!(fields(&[0x12, 0xff]).is_err());
        assert!(fields(&[0]).is_err());
        assert!(number(&[8, 1, 8, 2], 1).is_err());
    }
}
