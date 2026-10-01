//! 這台機器上的原檔（/docs/design/rpc-specs/data-plane.md §8.1）：UI 上傳時給的 `media.source_uri` 怎麼解、什麼時候算數。
//!
//! 讀一個媒體時，本機原檔在、大小又對得上，就直接讀它，🚫 不碰媒體池、🚫 不觸發下載、🚫 不動 DB——
//! 從池拿要先確認那一段下載了沒，從原檔拿什麼都不用管（維護者 2026-10-01）。
//!
//! ⚠️ 這個 URI 的意義**由 UI 決定**，daemon 只能猜：解不了、檔不在、大小不對，一律當成「本機沒有」，回頭走池，🚫 不報錯。

use std::fs::File;
use std::path::PathBuf;

use crate::cache::MediaEntry;

/// 這個媒體在這台機器上的原檔：`source_uri` 解得出本機路徑、是一般檔、而且大小剛好等於 `file_size` 才開。
///
/// Args:
///     entry: `media` 的那一列
/// Return:
///     Some((File, PathBuf))   開好的原檔與它的路徑
///     None                    沒有 `source_uri`、解不了（§8.1 的規則）、檔不在、不是一般檔、大小不同、開不了
pub fn open_local_source(entry: &MediaEntry) -> Option<(File, PathBuf)> {
    let path = local_path_of_file_uri(entry.source_uri.as_deref()?)?;
    let file = File::open(&path).ok()?;
    let metadata = file.metadata().ok()?;
    (metadata.is_file() && metadata.len() == entry.file_size).then_some((file, path))
}

/// `file://` URI → 這台機器的路徑（/docs/design/rpc-specs/data-plane.md §8.1）。
///
/// Args:
///     uri: example: "file:///home/me/v.mkv"、"file:///C:/Users/me/v.mkv"
/// Return:
///     Some(PathBuf)   解得出來的路徑
///     None            不是 `file://`（`content://`、裸路徑 `/x`、相對路徑都算）、host 不是空的也不是 `localhost`、百分比編碼壞、不是 UTF-8、含 NUL
pub fn local_path_of_file_uri(uri: &str) -> Option<PathBuf> {
    file_uri_to_path_text(uri, cfg!(windows)).map(PathBuf::from)
}

/// 平台無關的那一半（兩種平台都能測）。
///
/// Args:
///     uri: example: "file:///C:/Users/me/v%20final.mkv"
///     windows: 照 Windows 的規則解（`/C:/…` 去掉開頭那個 `/`）
/// Return:
///     Some(String)   example: "C:/Users/me/v final.mkv"（windows）、"/home/me/v.mkv"（其他）
///     None           同 [`local_path_of_file_uri`]
fn file_uri_to_path_text(uri: &str, windows: bool) -> Option<String> {
    const SCHEME: &str = "file://";
    if !uri.get(..SCHEME.len())?.eq_ignore_ascii_case(SCHEME) {
        return None;
    }
    let after_scheme = uri.get(SCHEME.len()..)?;
    // query 與 fragment 不是路徑的一部分（RFC 3986 §3）。
    let without_suffix = after_scheme
        .split(['?', '#'])
        .next()
        .unwrap_or(after_scheme);
    let encoded_path = match without_suffix.find('/') {
        Some(0) => without_suffix,
        Some(slash) => {
            let host = without_suffix.get(..slash)?;
            if !host.eq_ignore_ascii_case("localhost") {
                return None;
            }
            without_suffix.get(slash..)?
        }
        None => return None,
    };
    let path = percent_decode(encoded_path)?;
    if path.contains('\0') {
        return None;
    }
    if windows && is_slash_drive(&path) {
        return path.get(1..).map(str::to_string);
    }
    Some(path)
}

/// `/C:` 或 `/C:/…`：Windows 的磁碟機寫法。
fn is_slash_drive(path: &str) -> bool {
    let bytes = path.as_bytes();
    matches!(bytes, [b'/', letter, b':', rest @ ..] if letter.is_ascii_alphabetic() && (rest.is_empty() || rest.first() == Some(&b'/')))
}

/// Return:
///     Some(String)   每個 `%XX` 換成那個 byte，結果是 UTF-8
///     None           `%` 後面不是兩個 hex、或結果不是 UTF-8
fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut position = 0;
    while let Some(&byte) = bytes.get(position) {
        if byte == b'%' {
            let hex = text.get(position + 1..position + 3)?;
            decoded.push(u8::from_str_radix(hex, 16).ok()?);
            position += 3;
        } else {
            decoded.push(byte);
            position += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_uri_becomes_an_absolute_path_on_both_kinds_of_system() {
        let cases = [
            ("file:///home/me/v.mkv", false, Some("/home/me/v.mkv")),
            ("FILE:///home/me/v.mkv", false, Some("/home/me/v.mkv")),
            (
                "file://localhost/home/me/v.mkv",
                false,
                Some("/home/me/v.mkv"),
            ),
            (
                "file:///home/me/v%20final%E5%BD%B1%E7%89%87.mkv",
                false,
                Some("/home/me/v final影片.mkv"),
            ),
            (
                "file:///home/me/v.mkv?x=1#t=10",
                false,
                Some("/home/me/v.mkv"),
            ),
            ("file:///C:/Users/me/v.mkv", true, Some("C:/Users/me/v.mkv")),
            ("file:///c:/v.mkv", true, Some("c:/v.mkv")),
            ("file:///C:", true, Some("C:")),
            // 沒有磁碟機：Windows 上是目前磁碟的根。
            ("file:///Users/me/v.mkv", true, Some("/Users/me/v.mkv")),
            // 非 Windows 上 `/C:/…` 就是一個奇怪但合法的路徑，🚫 不去掉 `/`。
            (
                "file:///C:/Users/me/v.mkv",
                false,
                Some("/C:/Users/me/v.mkv"),
            ),
        ];
        for (uri, windows, want) in cases {
            assert_eq!(
                file_uri_to_path_text(uri, windows).as_deref(),
                want,
                "{uri} windows={windows}"
            );
        }
    }

    /// 不是標準 `file://` URI 的一律「本機沒有」：daemon 不擋 UI 給什麼，但只走它認得的。
    #[test]
    fn anything_that_is_not_a_file_uri_means_no_local_file() {
        for uri in [
            "/home/me/v.mkv",
            "C:\\Users\\me\\v.mkv",
            "v.mkv",
            "content://media/external/video/12",
            "http://127.0.0.1/v.mkv",
            "file:/home/me/v.mkv",
            "file://server/share/v.mkv",
            "file://C:/Users/me/v.mkv",
            "file://",
            "file:///home/me/v%2.mkv",
            "file:///home/me/v%ZZ.mkv",
            "file:///home/me/%FF%FE.mkv",
            "file:///home/me/v%00.mkv",
            "",
        ] {
            for windows in [false, true] {
                assert_eq!(
                    file_uri_to_path_text(uri, windows),
                    None,
                    "{uri:?} windows={windows}"
                );
            }
        }
    }

    fn entry(source_uri: Option<String>, file_size: u64) -> MediaEntry {
        MediaEntry {
            mxc: "mxc://localhost/1".into(),
            pool_file: None,
            name: None,
            mimetype: None,
            hash: None,
            file_size,
            chunk_size: 16,
            chunks_written: 0,
            complete: false,
            bytes_on_disk: 0,
            created_at: 0,
            last_used_at: 0,
            source_uri,
        }
    }

    /// 原檔在、大小剛好對得上才算數；大小不同（被改過、換掉了）、不在、是目錄，都當本機沒有。
    #[test]
    fn the_local_file_counts_only_when_its_size_matches() {
        let dir = std::env::temp_dir().join(format!("wbf-local-source-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("v 1.bin");
        std::fs::write(&file, [7u8; 40]).unwrap();
        let mut uri = String::from("file://");
        if cfg!(windows) {
            uri.push('/');
        }
        uri.push_str(
            &file
                .display()
                .to_string()
                .replace('\\', "/")
                .replace(' ', "%20"),
        );

        let (_, path) = open_local_source(&entry(Some(uri.clone()), 40)).expect("大小對得上");
        assert_eq!(std::fs::read(path).unwrap(), vec![7u8; 40]);
        assert!(
            open_local_source(&entry(Some(uri.clone()), 41)).is_none(),
            "大小不同"
        );
        assert!(
            open_local_source(&entry(None, 40)).is_none(),
            "沒有 source_uri"
        );
        let directory_uri = uri.replace("v%201.bin", "");
        assert!(
            open_local_source(&entry(Some(directory_uri), 0)).is_none(),
            "目錄"
        );
        std::fs::remove_file(&file).unwrap();
        assert!(
            open_local_source(&entry(Some(uri), 40)).is_none(),
            "檔不在了"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
