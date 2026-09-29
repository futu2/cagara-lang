//! `file://` URIs to paths and back (lsp-types 0.97 no longer uses `url`).

use lsp_types::Uri;
use std::path::{Path, PathBuf};

pub fn to_path(uri: &Uri) -> Option<PathBuf> {
    let rest = uri.as_str().strip_prefix("file://")?;
    Some(PathBuf::from(decode(rest)?))
}

pub fn from_path(path: &Path) -> Option<Uri> {
    let mut s = String::from("file://");
    for b in path.to_str()?.bytes() {
        if b.is_ascii_alphanumeric() || b"/-._~".contains(&b) {
            s.push(b as char);
        } else {
            s.push_str(&format!("%{b:02X}"));
        }
    }
    s.parse().ok()
}

fn decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let h = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(h, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let p = Path::new("/home/me/my queries/réport.cagara");
        let u = from_path(p).unwrap();
        assert_eq!(
            u.as_str(),
            "file:///home/me/my%20queries/r%C3%A9port.cagara"
        );
        assert_eq!(to_path(&u).unwrap(), p);
    }
}
