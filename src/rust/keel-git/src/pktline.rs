//! git "pkt-line" wire framing: each line is a 4-hex length prefix (the length *includes* the
//! 4 prefix bytes) followed by the payload. `0000` is a flush-pkt (section boundary); `0001` a
//! delimiter (protocol v2). This is the envelope for the whole smart transport — ref
//! advertisement and upload-pack/receive-pack negotiation are all pkt-line streams.

/// Encode one pkt-line around `data` (which typically already ends in `\n`).
pub fn encode(data: &[u8]) -> Vec<u8> {
    let n = data.len() + 4;
    assert!(n <= 0xffff, "pkt-line too long");
    let mut out = format!("{n:04x}").into_bytes();
    out.extend_from_slice(data);
    out
}

/// Encode a text pkt-line (convenience).
pub fn encode_str(s: &str) -> Vec<u8> {
    encode(s.as_bytes())
}

/// The flush-pkt (`0000`) — ends a section / signals "no more".
pub const FLUSH: &[u8] = b"0000";

/// One decoded pkt-line.
#[derive(Debug, PartialEq, Eq)]
pub enum Pkt<'a> {
    Flush,
    Delim,
    Line(&'a [u8]),
}

/// Read one pkt-line from `buf` starting at `*pos`, advancing `*pos`; returns the line as a slice
/// into `buf`. `None` at end of input or on a malformed length.
pub fn read_at<'a>(buf: &'a [u8], pos: &mut usize) -> Option<Pkt<'a>> {
    if *pos + 4 > buf.len() {
        return None;
    }
    let len = usize::from_str_radix(std::str::from_utf8(&buf[*pos..*pos + 4]).ok()?, 16).ok()?;
    if len == 0 {
        *pos += 4;
        return Some(Pkt::Flush);
    }
    if len == 1 {
        *pos += 4;
        return Some(Pkt::Delim);
    }
    if len < 4 || *pos + len > buf.len() {
        return None;
    }
    let line = &buf[*pos + 4..*pos + len];
    *pos += len;
    Some(Pkt::Line(line))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_known_lengths() {
        // "hello\n" is 6 bytes → 6+4 = 10 = 0x000a
        assert_eq!(encode_str("hello\n"), b"000ahello\n");
        assert_eq!(FLUSH, b"0000");
    }

    #[test]
    fn roundtrips_a_stream() {
        let mut stream = Vec::new();
        stream.extend(encode_str("want abc\n"));
        stream.extend(encode_str("have def\n"));
        stream.extend_from_slice(FLUSH);
        let mut pos = 0;
        assert_eq!(read_at(&stream, &mut pos), Some(Pkt::Line(b"want abc\n")));
        assert_eq!(read_at(&stream, &mut pos), Some(Pkt::Line(b"have def\n")));
        assert_eq!(read_at(&stream, &mut pos), Some(Pkt::Flush));
        assert_eq!(read_at(&stream, &mut pos), None);
    }
}
