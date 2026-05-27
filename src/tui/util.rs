use std::io;

const B64: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn base64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(B64[((triple >> 18) & 0x3f) as usize] as char);
        out.push(B64[((triple >> 12) & 0x3f) as usize] as char);
        if chunk.len() > 1 {
            out.push(B64[((triple >> 6) & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(B64[(triple & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

pub fn osc52_copy(text: &str) {
    use io::Write;
    let b64 = base64_encode(text.as_bytes());
    let mut stderr = io::stderr();
    let _ = write!(stderr, "\x1b]52;c;{}\x07", b64);
    let _ = stderr.flush();
}

pub fn char_boundary_left(s: &str, pos: usize) -> usize {
    let mut p = pos.min(s.len());
    while !s.is_char_boundary(p) {
        p -= 1;
    }
    p
}

pub fn char_boundary_right(s: &str, pos: usize) -> usize {
    let mut p = pos.min(s.len());
    while p < s.len() && !s.is_char_boundary(p) {
        p += 1;
    }
    p
}
