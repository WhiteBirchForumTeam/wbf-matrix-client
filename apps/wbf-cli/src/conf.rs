//! `wbf.conf`：不用每次指定環境變數（CLI 規格 §10）。
//!
//! ```ini
//! ; 分號或井號開頭是註解
//! [general]
//! SERVER=http://localhost:6167
//!
//! [backup]
//! SERVER_BACKUP=on          ; 只正面認得 on／off
//! ```
//!
//! **鍵名就是環境變數去掉 `WBF_` 前綴**（`SERVER` ↔ `WBF_SERVER`）：兩邊不會漂移，
//! 也不必另外背一套名字。區段只是給人看的分組，🚫 **不參與查找** —— 同一個鍵放在哪一段
//! 都讀得到。這是刻意的：讓區段參與查找等於同一個鍵有兩個身分，而使用者搬一行就會壞。
//!
//! 優先序（§10.2）：**旗標 > 環境變數 > conf > 內建預設**，每個值各自比一次，
//! 🚫 不是整份取代。環境變數優先於 conf 是維護者 2026-09-09 定的。
//!
//! 🚫 這裡不放秘密（§10.5）：`ACCESS_TOKEN`、passphrase、password 一律不從 conf 來。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use wbf_sdk::SdkError;

pub const CONF_FILE_NAME: &str = "wbf.conf";

/// conf 檔讀出來的鍵值。空的（沒有檔）也是合法的一份 —— 每個查找都回 `None`。
#[derive(Debug, Default)]
pub struct Conf {
    /// 鍵名去掉 `WBF_` 前綴、大寫, example: "SERVER"
    values: BTreeMap<String, String>,
    /// 讀的時候攢下來的警告，呼叫者印到 stderr（🚫 這一層不印：它不知道 `--quiet`）。
    warnings: Vec<String>,
}

/// 🚫 conf 不支援的鍵（§10.5）：秘密不落地在明文檔裡。
/// ⚠️ 寫成「正面列出不准的」而不是「正面列出准的」——新鍵加進 CLI 時不必記得回來改這裡，
/// 而漏掉的那個會被**允許**，所以這張表只放秘密，其餘認不得的鍵本來就只是警告加忽略（§10.4）。
const REFUSED_KEYS: &[&str] = &["ACCESS_TOKEN", "TOKEN", "PASSWORD", "PASSPHRASE"];

/// 區段只是給人看的分組（🚫 不參與查找），但認不得的還是要說一聲——多半是打錯字，
/// 而那一段底下的鍵會安靜地不生效（§10.4）。
const KNOWN_SECTIONS: &[&str] = &["GENERAL", "BACKUP", "MEDIA", "RECENT"];

/// 找 conf 檔並讀進來（§10.1）。
///
/// Args:
///     explicit: `--config <path>`, example: Some(Path::new("/tmp/wbf.conf"))
///     data_dir: 已經定好的資料目錄, example: "<data dir>"
/// Return:
///     Ok(Conf)     讀到的鍵值；兩個位置都沒有檔就是空的一份
///     Err(Usage)   `--config` 明指的檔不在、或檔案語法壞了
pub fn load(explicit: Option<&Path>, data_dir: &Path) -> Result<Conf, SdkError> {
    if let Some(path) = explicit {
        // 🚫 明指了就不 fallback：讀到別的檔比讀不到更糟（§10.1）。
        if !path.exists() {
            return Err(SdkError::Usage(format!(
                "no config file at {} (--config)",
                path.display()
            )));
        }
        return parse_file(path);
    }
    let default_path = data_dir.join(CONF_FILE_NAME);
    if default_path.exists() {
        return parse_file(&default_path);
    }
    Ok(Conf::default())
}

/// ⚠️ 讀壞掉的設定檔一律**報錯 exit**，🚫 不當作沒有這個檔：讀一半比讀不到危險（§10.4）。
fn parse_file(path: &Path) -> Result<Conf, SdkError> {
    let text = std::fs::read_to_string(path).map_err(|error| {
        SdkError::Usage(format!(
            "{}: {error} (a config file must be UTF-8)",
            path.display()
        ))
    })?;
    parse(&text, &path.display().to_string())
}

/// 純函數版，給測試與 `parse_file` 共用。
///
/// Args:
///     text: 整份 conf, example: "[general]\nSERVER=http://x\n"
///     source: 訊息裡的檔名, example: "<data dir>/wbf.conf"
/// Return:
///     Ok(Conf)     `warnings` 是認不得的區段／鍵、重複的鍵、被拒的秘密鍵
///     Err(Usage)   哪一行不成句子
pub fn parse(text: &str, source: &str) -> Result<Conf, SdkError> {
    let mut conf = Conf::default();
    for (index, raw_line) in text.lines().enumerate() {
        let line_number = index + 1;
        let line = strip_comment(raw_line).trim();
        if line.is_empty() {
            continue;
        }
        if let Some(name) = line.strip_prefix('[') {
            let Some(name) = name.strip_suffix(']') else {
                return Err(SdkError::Usage(format!(
                    "{source}:{line_number}: a section header must end with ']'"
                )));
            };
            let section = name.trim().to_ascii_uppercase();
            if !KNOWN_SECTIONS.contains(&section.as_str()) {
                conf.warnings.push(format!(
                    "warning: {source}:{line_number}: unknown section [{section}]; its keys still work but the name is probably a typo"
                ));
            }
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(SdkError::Usage(format!(
                "{source}:{line_number}: expected KEY=value or [section]"
            )));
        };
        let key = key.trim().to_ascii_uppercase();
        let value = unquote(value.trim());
        if key.is_empty() {
            return Err(SdkError::Usage(format!(
                "{source}:{line_number}: the key is empty"
            )));
        }
        // 空值當作沒寫，🚫 不是空字串（全域慣例：佔位值不用空字串）。
        if value.is_empty() {
            continue;
        }
        if REFUSED_KEYS.contains(&key.as_str()) {
            conf.warnings.push(format!(
                "warning: {source}:{line_number}: {key} is not read from a config file (secrets do not go in plaintext files); ignoring it"
            ));
            continue;
        }
        if let Some(previous) = conf.values.insert(key.clone(), value) {
            conf.warnings.push(format!(
                "warning: {source}:{line_number}: {key} was already set to {previous:?}; the later one wins"
            ));
        }
    }
    Ok(conf)
}

/// `;` 或 `#` 起註解，⚠️ 只有**行首**或**前面是空白**時才算 —— `PASSWORD_FILE=/tmp/a#b`
/// 裡的 `#` 是值的一部分（§10.1）。
///
/// ⚠️ 雙引號裡面的一律不算註解：`KEY="  # 這是值  "` 的引號就是為了保住它。
/// 🚫 所以不能先剪註解再拆引號 —— 那樣剪到的是值本身（2026-09-10 寫測試時踩到）。
fn strip_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut in_quotes = false;
    let mut previous_is_space = true;
    for (index, byte) in bytes.iter().enumerate() {
        match byte {
            b'"' => in_quotes = !in_quotes,
            b';' | b'#' if !in_quotes && previous_is_space => return &line[..index],
            _ => {}
        }
        previous_is_space = (*byte as char).is_whitespace();
    }
    line
}

/// 雙引號包住整個值時，引號裡的東西原樣留著（含前後空白與 `#`）。
fn unquote(value: &str) -> String {
    match value
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
    {
        Some(inner) => inner.to_string(),
        None => value.to_string(),
    }
}

impl Conf {
    /// 這個鍵在 conf 裡有沒有值。
    ///
    /// Args:
    ///     key: 環境變數去掉 `WBF_` 前綴, example: "SERVER"
    /// Return:
    ///     Some(&str)   有，而且不是空的
    ///     None         沒寫、寫了空值、或是被拒的秘密鍵
    pub fn find(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(String::as_str)
    }

    /// 開關型的鍵（§10.4）：**只正面認得 `on` 與 `off`**（不分大小寫）。
    ///
    /// ⚠️ 判斷寫成「**正面認得 `off` 才關**」，🚫 不寫成「不等於 `on` 就關」——
    /// 壞掉的時候要壞在「備份還開著」那一邊，不是「以為開著、其實沒開」。
    /// 認不得的值（`true`、`1`、拼錯的 `of`）警告並落到 `safe`。
    ///
    /// Args:
    ///     key: example: "SERVER_BACKUP"
    ///     safe: 認不得時落到哪, example: true
    /// Return:
    ///     bool  沒寫也回 `safe`
    pub fn is_on(&self, key: &str, safe: bool, warnings: &mut Vec<String>) -> bool {
        let Some(value) = self.find(key) else {
            return safe;
        };
        if value.eq_ignore_ascii_case("off") {
            return false;
        }
        if value.eq_ignore_ascii_case("on") {
            return true;
        }
        warnings.push(format!(
            "warning: {key}={value:?} is not `on` or `off`; using {}",
            if safe { "on" } else { "off" }
        ));
        safe
    }

    /// 數值型的鍵（§10.4）：parse 不出來就警告並用內建預設，🚫 不用半套的值。
    pub fn get_number<T: std::str::FromStr>(
        &self,
        key: &str,
        default: T,
        warnings: &mut Vec<String>,
    ) -> T {
        let Some(value) = self.find(key) else {
            return default;
        };
        match value.parse() {
            Ok(parsed) => parsed,
            Err(_) => {
                warnings.push(format!(
                    "warning: {key}={value:?} is not a number; using the built-in default"
                ));
                default
            }
        }
    }

    /// 讀進來時攢下的警告（認不得的鍵、重複、被拒的秘密鍵）。呼叫者印到 stderr。
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// 認不得的鍵：CLI 知道自己吃哪些，所以這一步要它把清單傳進來（§10.4）。
    /// 印警告、忽略、命令照跑 —— conf 要往前相容，舊版 CLI 讀到新版寫的鍵不該整個掛掉。
    pub fn warn_about_unknown_keys(&self, known: &[&str]) -> Vec<String> {
        self.values
            .keys()
            .filter(|key| !known.contains(&key.as_str()))
            .map(|key| format!("warning: {CONF_FILE_NAME}: unknown key {key}; ignoring it"))
            .collect()
    }
}

/// 自動生成要寫的一行。
pub struct Entry {
    /// 只是給人看的分組（🚫 不參與查找）, example: "general"
    pub section: &'static str,
    /// example: "SERVER"
    pub key: &'static str,
    pub value: String,
    /// 這次這個值是哪來的, example: "flag or WBF_SERVER"
    pub origin: &'static str,
}

/// 這個值這次是哪來的（寫進自動生成的註解裡）。
///
/// ⚠️ 分不出「旗標」與「環境變數」：clap 把兩者合成同一個 `Option`，而為了分辨要另外
/// 拿 `ArgMatches` 的 `value_source` —— 為了一行註解不值得多一條取值路徑。
///
/// Args:
///     from_flag_or_env: example: true
///     from_conf: example: false
/// Return:
///     &str  "flag or env"／"wbf.conf"／"built-in default"
pub fn origin_of(from_flag_or_env: bool, from_conf: bool) -> &'static str {
    match (from_flag_or_env, from_conf) {
        (true, _) => "flag or env",
        (false, true) => "wbf.conf",
        (false, false) => "built-in default",
    }
}

/// 自動生成（§10.3）：把**這次實際生效的值**寫進 `<data dir>/wbf.conf`。
///
/// 三個條件都成立才寫：`--data-dir`／`WBF_DATA_DIR` 有給、那個目錄下還沒有 `wbf.conf`、
/// 這次命令成功結束（呼叫點在成功之後）。
///
/// 🚫 已經存在的**永遠不改寫**，連補鍵都不做：那是使用者的檔，不是我們的狀態檔。
/// 🚫 不寫任何秘密：`ACCESS_TOKEN` 不寫，`PASSWORD_FILE`／`PASSPHRASE_FILE` 這種
/// 「秘密在哪」的路徑自動生成時也不寫 —— 要就手動加，讓它是使用者的決定。
///
/// Args:
///     data_dir: example: "<data dir>"
///     entries: 照 `section` 分組，順序照傳進來的樣子
/// Return:
///     Ok(true)    寫了
///     Ok(false)   已經有一份了，什麼都沒做
pub fn write_if_absent(data_dir: &Path, entries: &[Entry]) -> Result<bool, SdkError> {
    let path: PathBuf = data_dir.join(CONF_FILE_NAME);
    if path.exists() {
        return Ok(false);
    }
    let mut text = String::from(
        "; wbf.conf —— wbf-cli 自動生成的一份起手式（CLI 規格 §10.3）
         ; 每個值後面註明它這次是哪來的。改這個檔不影響已經登入的帳號。
         ; 🚫 這裡不放秘密：token、password、passphrase 一律不從這裡讀。
         ; 已經存在的 wbf.conf 永遠不會被改寫——要重生成就先自己刪掉。
",
    );
    let mut sections: Vec<&str> = Vec::new();
    for entry in entries {
        if !sections.contains(&entry.section) {
            sections.push(entry.section);
        }
    }
    for section in sections {
        text.push_str(&format!(
            "
[{section}]
"
        ));
        for entry in entries.iter().filter(|entry| entry.section == section) {
            text.push_str(&format!(
                "{}={}   ; {}
",
                entry.key,
                quote_if_needed(&entry.value),
                entry.origin
            ));
        }
    }
    wbf_sdk::vault::write_private(&path, text.as_bytes())?;
    Ok(true)
}

/// 值有前後空白、以 `#`／`;` 起頭、或本身首尾就是雙引號時要包引號，
/// 不然自己寫出來的檔自己讀不回來。
///
/// ⚠️ 這幾個條件是 `strip_comment` 與 `unquote` 的鏡像：那兩個改了，這裡要跟著改，
/// 不然 round-trip 會裂開（`generating_never_overwrites_and_reads_back_the_same` 蓋著這條）。
fn quote_if_needed(value: &str) -> String {
    let needs = value.trim() != value
        || value.starts_with('#')
        || value.starts_with(';')
        || value.contains(" #")
        || value.contains(" ;")
        // 值本身就是 `"x"`：不包起來寫出去，讀回來會被 `unquote` 剝成 `x`
        //（PR #22 審查 rumia🟢1）。
        || (value.starts_with('"') && value.ends_with('"'));
    if needs {
        format!("\"{value}\"")
    } else {
        value.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_found_regardless_of_which_section_they_sit_in() {
        let conf = parse(
            "[general]\nSERVER=http://localhost:6167\n[backup]\nSERVER_BACKUP=off\n",
            "t",
        )
        .unwrap();
        assert_eq!(conf.find("SERVER"), Some("http://localhost:6167"));
        // 區段只是分組：搬一行到別的區段不該讓它消失。
        assert_eq!(conf.find("SERVER_BACKUP"), Some("off"));
    }

    #[test]
    fn a_hash_only_starts_a_comment_at_the_start_or_after_a_space() {
        let conf = parse(
            "PASSWORD_FILE=/tmp/a#b\nSERVER=http://x ; 這是註解\n# 整行註解\nACCOUNT=\"  #keep  \"\n",
            "t",
        )
        .unwrap();
        assert_eq!(conf.find("PASSWORD_FILE"), Some("/tmp/a#b"));
        assert_eq!(conf.find("SERVER"), Some("http://x"));
        assert_eq!(conf.find("ACCOUNT"), Some("  #keep  "));
    }

    #[test]
    fn an_empty_value_counts_as_not_written() {
        // 🚫 空字串不是佔位值：`KEY=` 就是沒寫，不是「設成空的」。
        let conf = parse("SERVER=\n", "t").unwrap();
        assert_eq!(conf.find("SERVER"), None);
    }

    #[test]
    fn a_secret_key_is_refused_with_a_warning() {
        let conf = parse("ACCESS_TOKEN=syt_secret\n", "t").unwrap();
        assert_eq!(conf.find("ACCESS_TOKEN"), None, "🚫 秘密不從 conf 來");
        assert_eq!(conf.warnings().len(), 1);
        assert!(
            !conf.warnings()[0].contains("syt_secret"),
            "警告不該把值印出來：{}",
            conf.warnings()[0]
        );
    }

    #[test]
    fn a_repeated_key_takes_the_later_one_and_warns() {
        let conf = parse("SERVER=http://a\nSERVER=http://b\n", "t").unwrap();
        assert_eq!(conf.find("SERVER"), Some("http://b"));
        assert_eq!(conf.warnings().len(), 1);
    }

    #[test]
    fn a_switch_only_recognises_on_and_off_and_falls_to_the_safe_value() {
        let mut warnings = Vec::new();
        let conf = parse("A=off\nB=ON\nC=true\nD=of\n", "t").unwrap();
        assert!(!conf.is_on("A", true, &mut warnings));
        assert!(conf.is_on("B", true, &mut warnings));
        // ⚠️ 認不得的值落到安全值（開著），🚫 不是「不等於 on 就關」。
        assert!(conf.is_on("C", true, &mut warnings));
        assert!(conf.is_on("D", true, &mut warnings));
        assert!(conf.is_on("MISSING", true, &mut warnings));
        assert_eq!(warnings.len(), 2, "只有 C 與 D 該警告");
    }

    #[test]
    fn a_broken_line_is_an_error_not_a_shrug() {
        // 設定檔讀一半比讀不到危險（§10.4）。
        assert!(parse("[general\nSERVER=x\n", "t").is_err());
        assert!(parse("this is not a pair\n", "t").is_err());
        assert!(parse("=novalue\n", "t").is_err());
    }

    #[test]
    fn a_number_that_does_not_parse_falls_back_to_the_default() {
        let mut warnings = Vec::new();
        let conf = parse("UNLOCK_TTL=abc\nQUOTA_MIB=2048\n", "t").unwrap();
        assert_eq!(conf.get_number("UNLOCK_TTL", 900u64, &mut warnings), 900);
        assert_eq!(conf.get_number("QUOTA_MIB", 1u64, &mut warnings), 2048);
        assert_eq!(warnings.len(), 1);
    }

    #[test]
    fn an_explicit_config_path_that_is_missing_is_an_error() {
        let dir = std::env::temp_dir().join(format!("wbf-conf-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // 🚫 明指了就不 fallback。
        assert!(load(Some(&dir.join("nope.conf")), &dir).is_err());
        // 資料目錄裡沒有就是空的一份，不是錯。
        assert!(load(None, &dir).unwrap().find("SERVER").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn generating_never_overwrites_and_reads_back_the_same() {
        let dir = std::env::temp_dir().join(format!("wbf-conf-gen-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let entries = vec![
            Entry {
                section: "general",
                key: "SERVER",
                value: "http://localhost:6167".to_string(),
                origin: "flag or env",
            },
            Entry {
                section: "general",
                key: "ACCOUNT",
                value: "  #odd  ".to_string(),
                origin: "built-in default",
            },
            Entry {
                section: "general",
                key: "TRANSPORT",
                // 值本身首尾就是雙引號的極端情形。
                value: "\"quoted\"".to_string(),
                origin: "built-in default",
            },
        ];
        assert!(write_if_absent(&dir, &entries).unwrap());
        // 自己寫出來的自己讀得回來（含要包引號的那種值）。
        let conf = load(None, &dir).unwrap();
        assert_eq!(conf.find("SERVER"), Some("http://localhost:6167"));
        assert_eq!(conf.find("ACCOUNT"), Some("  #odd  "));
        assert_eq!(conf.find("TRANSPORT"), Some("\"quoted\""));

        // 🚫 已經有一份就永遠不動它。
        assert!(!write_if_absent(&dir, &entries).unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
