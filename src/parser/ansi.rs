//! Strip ANSI CSI/OSC sequences. The raw bytes are kept for display.

use memchr::memchr;

pub fn strip_ansi(input: &str) -> String {
    if memchr(0x1b, input.as_bytes()).is_none() {
        return input.to_string();
    }
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != 0x1b {
            let ch = input[i..].chars().next().unwrap_or('\u{FFFD}');
            out.push(ch);
            i += ch.len_utf8();
            continue;
        }
        if i + 1 < bytes.len() && bytes[i + 1] == b'[' {
            i += 2;
            while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                i += 1;
            }
            if i < bytes.len() {
                i += 1;
            }
            continue;
        }
        if i + 1 < bytes.len() && bytes[i + 1] == b']' {
            i += 2;
            while i < bytes.len() {
                if bytes[i] == 0x07 {
                    i += 1;
                    break;
                }
                if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'\\' {
                    i += 2;
                    break;
                }
                i += 1;
            }
            continue;
        }
        // Other ESC sequences: drop ESC and the next byte.
        i += 1;
        if i < bytes.len() {
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_color() {
        let s = strip_ansi("\u{1b}[31mERROR\u{1b}[0m boom");
        assert_eq!(s, "ERROR boom");
    }
}
