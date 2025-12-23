use bytes::{Buf, BufMut};

use crate::packed::{pack_uint, unpack_uint};

pub fn pack_bytes<B: BufMut>(buf: &mut B, bytes: &[u8]) {
    pack_uint(buf, bytes.len() as u64);
    buf.put_slice(bytes);
}

pub fn unpack_bytes<B: Buf>(buf: &mut B) -> Vec<u8> {
    let len = unpack_uint(buf) as usize;
    let mut bytes = vec![0; len];
    buf.copy_to_slice(&mut bytes);
    bytes
}

pub fn pack_string<B: BufMut>(buf: &mut B, s: &str) {
    pack_bytes(buf, s.as_bytes());
}

pub fn unpack_string<B: Buf>(buf: &mut B) -> String {
    String::from_utf8(unpack_bytes(buf)).expect("valid utf-8")
}

#[cfg(test)]
mod tests {
    use quickcheck::quickcheck;

    use super::*;

    quickcheck! {
        fn pack_bytes_roundtrip(bytes: Vec<u8>) -> bool {
            let mut buf = Vec::new();
            pack_bytes(&mut buf, &bytes);
            unpack_bytes(&mut &buf[..]) == bytes
        }

        fn pack_string_roundtrip(s: String) -> bool {
            let mut buf = Vec::new();
            pack_string(&mut buf, &s);
            unpack_string(&mut &buf[..]) == s
        }
    }
}
