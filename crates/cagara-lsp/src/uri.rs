//! `file://` URIs to paths and back.

use lsp_types::Uri;
use std::path::{Path, PathBuf};
use url::Url;

/// The path behind a `file://` URI, or `None` for another scheme or an
/// unparseable URI.
pub fn to_path(uri: &Uri) -> Option<PathBuf> {
    let url = Url::parse(uri.as_str()).ok()?;
    if url.scheme() != "file" {
        return None;
    }
    url.to_file_path().ok()
}

/// The `file://` URI for a path, or `None` if the path cannot be a file URL.
pub fn from_path(path: &Path) -> Option<Uri> {
    Url::from_file_path(path).ok()?.as_str().parse().ok()
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

    #[test]
    fn rejects_non_file_uris() {
        let uri: Uri = "untitled:cagara".parse().unwrap();
        assert!(to_path(&uri).is_none());
    }
}
