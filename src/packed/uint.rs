use bytes::{Buf, BufMut};

pub fn pack_uint<B: BufMut>(buf: &mut B, mut n: u64) {
    while n > 0x7f {
        buf.put_u8((n as u8) & 0x7f | 0x80);
        n >>= 7;
    }
    buf.put_u8(n as u8);
}

pub fn unpack_uint<B: Buf>(buf: &mut B) -> u64 {
    let mut n = 0;
    let mut shift = 0;
    loop {
        let byte = buf.get_u8();
        n |= ((byte & 0x7f) as u64) << shift;
        if byte & 0x80 == 0 {
            break n;
        }
        shift += 7;
    }
}

#[cfg(test)]
mod tests {
    use quickcheck::quickcheck;

    use super::*;

    quickcheck! {
        fn pack_roundtrip(n: u64) -> bool {
            let mut buf = Vec::new();
            pack_uint(&mut buf, n);
            unpack_uint(&mut &buf[..]) == n
        }
    }
}
