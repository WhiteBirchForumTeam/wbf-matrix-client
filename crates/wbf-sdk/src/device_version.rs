//! 裝置版本號與房間版本號（wbfuwunel `wbf-room-device-version.md`）：client 這邊怎麼讀、怎麼比、怎麼自己重算。
//!
//! server 給的兩個號碼：
//! - **裝置版本號** `序號-雜湊`（每個帳號一個）：序號是他的裝置集合變動了幾次，雜湊是「任何人查得到的金鑰」的指紋。
//!   成員清單裡每個已加入成員的 `unsigned["org.wbftw.device_version"]`、`Event/DeviceChanged` 的 `device_version`。
//! - **房間版本號** u64（每個房間一個）：成員清單最外層的 `org.wbftw.room_version`；送加密訊息時帶在
//!   `Event/Send` 的 `room_version`，對不上被 1506 擋。
//!
//! 這裡只有 JSON 與雜湊，沒有網路：成員清單的 body 從哪來（HTTP `/members` 或橋 `0x13 0x29`）是呼叫端的事。
//!
//! 🚨 fail closed：讀不出號碼就是錯（`SdkError::Protocol`），🚫 不用 0 或空字串頂——帶錯的號碼送出去，
//! server 擋不擋取決於它剛好等不等於目前的值，而「剛好相等」正是這條防線要排除的巧合。

use std::collections::BTreeMap;

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::SdkError;

/// 成員清單最外層的房間版本號。
pub const ROOM_VERSION_KEY: &str = "org.wbftw.room_version";
/// 每個已加入成員 `unsigned` 裡的裝置版本號。
pub const DEVICE_VERSION_KEY: &str = "org.wbftw.device_version";
/// server 算不出雜湊時寫的佔位字（§3.2）：序號照樣前進，雜湊永遠對不上任何重算結果。
pub const UNHASHABLE: &str = "unhashable";
/// 雜湊是 SHA-256 十六進位小寫的前幾個字元（§3.4）。
pub const HASH_HEX_LEN: usize = 10;

/// 一個帳號的裝置版本號 `序號-雜湊`。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceVersion {
    /// 從 1 起；每次裝置集合變動（換金鑰、交叉簽章、簽章、刪裝置）+1。
    pub seq: u64,
    /// 10 個小寫十六進位字元，或 [`UNHASHABLE`]。
    pub hash: String,
}

impl DeviceVersion {
    /// Args:
    ///     text: example: "3-810b7c3be4"
    /// Return:
    ///     Some(DeviceVersion)  序號是正整數、雜湊是 10 個小寫十六進位字元或 `unhashable`
    ///     None                 其他任何形狀（沒有 `-`、序號 0、雜湊長度不對、大寫）
    pub fn parse(text: &str) -> Option<DeviceVersion> {
        let (seq_text, hash) = text.split_once('-')?;
        let seq: u64 = seq_text.parse().ok()?;
        if seq == 0 {
            return None;
        }
        let is_hex_hash = hash.len() == HASH_HEX_LEN
            && hash
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
        if !(is_hex_hash || hash == UNHASHABLE) {
            return None;
        }
        Some(DeviceVersion {
            seq,
            hash: hash.to_string(),
        })
    }

    /// Return:
    ///     bool  1 = 雜湊是真的指紋；0 = server 算不出來（`unhashable`），client 重算永遠對不上
    pub fn is_hashable(&self) -> bool {
        self.hash != UNHASHABLE
    }

    /// Return:
    ///     String  線上的形狀, example: "3-810b7c3be4"
    pub fn to_text(&self) -> String {
        format!("{}-{}", self.seq, self.hash)
    }
}

/// 一次成員清單讀到的房間版本號與每個已加入成員的裝置版本號——兩者是同一次讀到的房間狀態算的（§5.1）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoomDeviceVersions {
    pub room_version: u64,
    /// 已加入（`membership` 是 `join`）的成員 → 他的裝置版本號。
    pub members: BTreeMap<String, DeviceVersion>,
}

/// 兩份 [`RoomDeviceVersions`] 的差：收到 1506 之後只對這些人動作（§7.2）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MembersDiff {
    /// 要重新 `/keys/query` 的人：新加入的、或裝置版本號跟上次不同的。
    pub changed: Vec<String>,
    /// 上次有、這次不在了的人：有人離開就要換一把新的房間金鑰。
    pub left: Vec<String>,
}

impl RoomDeviceVersions {
    /// Args:
    ///     body: `GET /rooms/{room_id}/members` 的回應 JSON（HTTP 的 body，或橋 `0x13 0x29` Ack 的 data）,
    ///           example: {"chunk":[{"type":"m.room.member","state_key":"@bob:localhost","content":{"membership":"join"},"unsigned":{"org.wbftw.device_version":"3-810b7c3be4"}}],"org.wbftw.room_version":81234}
    /// Return:
    ///     Ok(RoomDeviceVersions)   最外層有房間版本號，而且每個 `join` 成員都有解得開的裝置版本號
    ///     Err(SdkError::Protocol)  沒有房間版本號（server 不支援，或不是成員清單）、`chunk` 不是陣列、
    ///                              某個 `join` 成員沒有 `state_key`、沒有裝置版本號、或它的形狀不對
    pub fn from_members_body(body: &Value) -> Result<RoomDeviceVersions, SdkError> {
        let room_version = body
            .get(ROOM_VERSION_KEY)
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                SdkError::Protocol(format!(
                    "members response has no `{ROOM_VERSION_KEY}`: this server does not give room versions"
                ))
            })?;
        let chunk = body
            .get("chunk")
            .and_then(Value::as_array)
            .ok_or_else(|| SdkError::Protocol("members response has no `chunk` array".into()))?;
        let mut members = BTreeMap::new();
        for member_event in chunk {
            let membership = member_event
                .pointer("/content/membership")
                .and_then(Value::as_str);
            if membership != Some("join") {
                continue;
            }
            let user_id = member_event
                .get("state_key")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    SdkError::Protocol("a joined member event has no `state_key`".into())
                })?;
            let version_text = member_event
                .pointer(&format!("/unsigned/{DEVICE_VERSION_KEY}"))
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    SdkError::Protocol(format!(
                        "joined member {user_id} has no `{DEVICE_VERSION_KEY}`"
                    ))
                })?;
            let version = DeviceVersion::parse(version_text).ok_or_else(|| {
                SdkError::Protocol(format!(
                    "joined member {user_id} has a malformed device version `{version_text}`"
                ))
            })?;
            members.insert(user_id.to_string(), version);
        }
        Ok(RoomDeviceVersions {
            room_version,
            members,
        })
    }

    /// Args:
    ///     previous: 上一次拿到的（發上一輪房間金鑰時依據的那份）
    /// Return:
    ///     MembersDiff  `changed`＝新加入或版本號不同的人（排序）；`left`＝上次有這次沒有的人（排序）
    pub fn diff_from(&self, previous: &RoomDeviceVersions) -> MembersDiff {
        let changed = self
            .members
            .iter()
            .filter(|(user_id, version)| previous.members.get(*user_id) != Some(version))
            .map(|(user_id, _)| user_id.clone())
            .collect();
        let left = previous
            .members
            .keys()
            .filter(|user_id| !self.members.contains_key(*user_id))
            .cloned()
            .collect();
        MembersDiff { changed, left }
    }
}

/// 照 §3.4 從 `/keys/query` 的回應重算某人的裝置雜湊，跟 server 給的比：對得上表示看到的是同一組金鑰。
///
/// 放進去的：主金鑰、自簽金鑰、每台上傳過金鑰的裝置（依裝置 ID 排序）；每一項去掉 `unsigned`、只留擁有者自己的簽章
/// （過濾完空了就整個 `signatures` 拿掉）；沒有主金鑰或自簽金鑰時那一項是空的（長度 0），不是跳過。
/// 每項轉 Matrix 規範化 JSON、前面加 4 byte 大端長度、串起來 SHA-256，取十六進位小寫前 10 個字元。
///
/// 規範化 JSON 靠 `serde_json` 本身：這個 crate 沒開 `preserve_order`，物件的鍵已經按 byte 序排好；
/// 緊湊輸出沒有空白；非 ASCII 原樣輸出。金鑰 JSON 沒有浮點數，所以不必另外擋。
///
/// Args:
///     user_id: example: "@bob:localhost"
///     keys_query: `POST /keys/query` 的回應 JSON（`master_keys`、`self_signing_keys`、`device_keys` 三張都以 user_id 為鍵）
/// Return:
///     String  10 個小寫十六進位字元, example: "810b7c3be4"；這個人一把金鑰都沒有也是一個雜湊（三項全空）
pub fn compute_device_keys_hash(user_id: &str, keys_query: &Value) -> String {
    let master_key = keys_query.pointer(&format!("/master_keys/{user_id}"));
    let self_signing_key = keys_query.pointer(&format!("/self_signing_keys/{user_id}"));
    // serde_json 的 Map 是 BTreeMap：走訪就是裝置 ID 的 byte 序。
    let devices = keys_query
        .pointer(&format!("/device_keys/{user_id}"))
        .and_then(Value::as_object)
        .map(|by_device_id| by_device_id.values().collect::<Vec<_>>())
        .unwrap_or_default();

    let items = [master_key, self_signing_key]
        .into_iter()
        .chain(devices.into_iter().map(Some));
    let mut framed = Vec::new();
    for item in items {
        let canonical = match item {
            None => Vec::new(),
            Some(key) => serde_json::to_vec(&only_what_everyone_sees(user_id, key))
                .expect("a serde_json::Value always serializes"),
        };
        let len = u32::try_from(canonical.len()).expect("a key is far smaller than 4 GiB");
        framed.extend_from_slice(&len.to_be_bytes());
        framed.extend_from_slice(&canonical);
    }
    hex::encode(Sha256::digest(&framed))[..HASH_HEX_LEN].to_string()
}

/// Args:
///     user_id: 擁有者, example: "@bob:localhost"
///     key: 一把金鑰的 JSON（主金鑰、自簽金鑰或某台裝置的 device keys）
/// Return:
///     Value  去掉 `unsigned`；`signatures` 只留擁有者那一層，留完是空的就整個欄位拿掉
fn only_what_everyone_sees(user_id: &str, key: &Value) -> Value {
    let mut visible = key.clone();
    if let Some(fields) = visible.as_object_mut() {
        fields.remove("unsigned");
        let own_signatures = fields
            .get("signatures")
            .and_then(Value::as_object)
            .and_then(|by_signer| by_signer.get(user_id))
            .cloned();
        match own_signatures {
            // 🚨 沒被簽過的金鑰根本沒有 `signatures`；只被別人簽過的過濾後會剩 `{}`——兩者要算成同一個值，
            // 否則擁有者自己的 client（看得到那筆別人的簽章）跟外人的 client 會算出不同的雜湊（server PR #75）。
            None => {
                fields.remove("signatures");
            }
            Some(own) => {
                let mut only_own = serde_json::Map::new();
                only_own.insert(user_id.to_string(), own);
                fields.insert("signatures".into(), Value::Object(only_own));
            }
        }
    }
    visible
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn master() -> Value {
        json!({
            "user_id": "@bob:localhost",
            "usage": ["master"],
            "keys": { "ed25519:MASTER": "bWFzdGVy" },
            "signatures": { "@bob:localhost": { "ed25519:DEV1": "c2VsZg" } }
        })
    }

    fn self_signing() -> Value {
        json!({
            "user_id": "@bob:localhost",
            "usage": ["self_signing"],
            "keys": { "ed25519:SSK": "c3Nr" },
            "signatures": { "@bob:localhost": { "ed25519:MASTER": "bXNr" } }
        })
    }

    fn device(id: &str, key: &str) -> Value {
        json!({
            "user_id": "@bob:localhost",
            "device_id": id,
            "algorithms": ["m.olm.v1.curve25519-aes-sha2", "m.megolm.v1.aes-sha2"],
            "keys": { format!("curve25519:{id}"): key, format!("ed25519:{id}"): key },
            "signatures": { "@bob:localhost": { format!("ed25519:{id}"): "ZGV2" } }
        })
    }

    /// `/keys/query` 回應的形狀；裝置故意倒著放，鍵序由 serde_json 排。
    fn keys_query(
        master: Option<Value>,
        self_signing: Option<Value>,
        devices: &[(&str, Value)],
    ) -> Value {
        let mut response = json!({ "device_keys": { "@bob:localhost": {} } });
        for (device_id, keys) in devices {
            response["device_keys"]["@bob:localhost"][*device_id] = keys.clone();
        }
        if let Some(master) = master {
            response["master_keys"] = json!({ "@bob:localhost": master });
        }
        if let Some(self_signing) = self_signing {
            response["self_signing_keys"] = json!({ "@bob:localhost": self_signing });
        }
        response
    }

    fn the_documented_input() -> Value {
        keys_query(
            Some(master()),
            Some(self_signing()),
            &[
                ("DEV2", device("DEV2", "dHdv")),
                ("DEV1", device("DEV1", "b25l")),
            ],
        )
    }

    /// server 規格 §3.4 的黃金向量（server 端 `keys_hash.rs::the_documented_vector` 釘住同一個值）。
    #[test]
    fn the_documented_vector() {
        assert_eq!(
            compute_device_keys_hash("@bob:localhost", &the_documented_input()),
            "810b7c3be4"
        );
    }

    /// 別人的簽章（Alice 驗證 Bob）只有簽的人看得到，不進雜湊。
    #[test]
    fn a_signature_by_someone_else_does_not_move_the_hash() {
        let mut signed_by_alice = the_documented_input();
        signed_by_alice["master_keys"]["@bob:localhost"]["signatures"]["@alice:localhost"] =
            json!({ "ed25519:ALICE_USK": "YWxpY2U" });
        assert_eq!(
            compute_device_keys_hash("@bob:localhost", &signed_by_alice),
            "810b7c3be4"
        );
    }

    /// 🚨 server PR #75 抓到的：只被別人簽過的金鑰，過濾後要跟從沒被簽過的一樣。
    #[test]
    fn a_key_left_with_no_signatures_hashes_as_one_that_never_had_any() {
        let mut unsigned_master = master();
        unsigned_master
            .as_object_mut()
            .unwrap()
            .remove("signatures");
        let mut signed_by_alice_only = unsigned_master.clone();
        signed_by_alice_only["signatures"] =
            json!({ "@alice:localhost": { "ed25519:ALICE_USK": "YWxpY2U" } });
        let mut empty_signatures = unsigned_master.clone();
        empty_signatures["signatures"] = json!({});

        // 🚨 期望值不經過被測的過濾函式：直接照規則框出「主金鑰（沒有 signatures 欄位）＋ 空的自簽 ＋ 沒有裝置」。
        // 三個輸入互相比是不夠的——過濾函式若把「沒有」寫成 `{}`，三個會一起錯、一起相等（變異驗證抓到的）。
        let canonical_master = serde_json::to_vec(&unsigned_master).unwrap();
        let mut framed = (canonical_master.len() as u32).to_be_bytes().to_vec();
        framed.extend_from_slice(&canonical_master);
        framed.extend_from_slice(&0u32.to_be_bytes());
        let expected = hex::encode(Sha256::digest(&framed))[..HASH_HEX_LEN].to_string();

        for (label, master) in [
            ("從沒被簽過", unsigned_master),
            ("只被 Alice 簽過", signed_by_alice_only),
            ("signatures 是空物件", empty_signatures),
        ] {
            assert_eq!(
                compute_device_keys_hash("@bob:localhost", &keys_query(Some(master), None, &[])),
                expected,
                "{label}"
            );
        }
    }

    #[test]
    fn unsigned_does_not_move_the_hash() {
        let mut with_display_name = the_documented_input();
        with_display_name["device_keys"]["@bob:localhost"]["DEV1"]["unsigned"] =
            json!({ "device_display_name": "手機" });
        assert_eq!(
            compute_device_keys_hash("@bob:localhost", &with_display_name),
            "810b7c3be4"
        );
    }

    /// 沒有主金鑰是一個空項目，不是跳過：「沒主金鑰、一台裝置」跟「有主金鑰、沒裝置」不能排成同一串。
    #[test]
    fn a_missing_key_is_an_empty_item_not_a_skipped_one() {
        let no_master_one_device = keys_query(
            None,
            Some(self_signing()),
            &[("DEV1", device("DEV1", "b25l"))],
        );
        let master_no_device = keys_query(Some(master()), Some(self_signing()), &[]);
        let no_master = compute_device_keys_hash("@bob:localhost", &no_master_one_device);
        assert_ne!(
            no_master,
            compute_device_keys_hash("@bob:localhost", &master_no_device)
        );
        assert_ne!(no_master, "810b7c3be4");
        // 一把都沒有也是一個雜湊：三個長度 0 的項目。
        let nothing = compute_device_keys_hash("@bob:localhost", &json!({}));
        assert_eq!(nothing.len(), HASH_HEX_LEN);
        assert_eq!(
            nothing,
            hex::encode(Sha256::digest([0u8; 8]))[..HASH_HEX_LEN]
        );
    }

    #[test]
    fn a_new_device_or_an_own_signature_each_move_the_hash() {
        let mut third_device = the_documented_input();
        third_device["device_keys"]["@bob:localhost"]["DEV3"] = device("DEV3", "dGhyZWU");
        assert_ne!(
            compute_device_keys_hash("@bob:localhost", &third_device),
            "810b7c3be4"
        );
        let mut resigned = the_documented_input();
        resigned["master_keys"]["@bob:localhost"]["signatures"]["@bob:localhost"]["ed25519:DEV2"] =
            json!("c2lnMg");
        assert_ne!(
            compute_device_keys_hash("@bob:localhost", &resigned),
            "810b7c3be4"
        );
    }

    #[test]
    fn device_version_parses_only_the_documented_shape() {
        let parsed = DeviceVersion::parse("3-810b7c3be4").unwrap();
        assert_eq!(
            parsed,
            DeviceVersion {
                seq: 3,
                hash: "810b7c3be4".into()
            }
        );
        assert!(parsed.is_hashable());
        assert_eq!(parsed.to_text(), "3-810b7c3be4");
        let placeholder = DeviceVersion::parse("7-unhashable").unwrap();
        assert!(!placeholder.is_hashable());
        for bad in [
            "",
            "3",
            "-810b7c3be4",
            "0-810b7c3be4",
            "3-810B7C3BE4",
            "3-810b7c3be",
            "3-810b7c3be45",
            "3-unknown",
            "x-810b7c3be4",
            "3-810b7c3bez",
        ] {
            assert_eq!(DeviceVersion::parse(bad), None, "{bad:?}");
        }
    }

    fn member(user_id: &str, membership: &str, version: Option<&str>) -> Value {
        let mut event = json!({
            "type": "m.room.member",
            "state_key": user_id,
            "content": { "membership": membership }
        });
        if let Some(version) = version {
            event["unsigned"] = json!({ DEVICE_VERSION_KEY: version });
        }
        event
    }

    /// server 範例（bridge-specs `0x13-room.md` `0x29`）：只讀 `join` 的；`leave`、`invite` 沒有版本號也不是錯。
    #[test]
    fn room_device_versions_read_the_documented_members_response() {
        let body = json!({
            "chunk": [
                member("@bob:localhost", "join", Some("3-810b7c3be4")),
                member("@weil:localhost", "leave", None),
                member("@carol:localhost", "invite", None),
                member("@alice:localhost", "join", Some("1-unhashable")),
            ],
            ROOM_VERSION_KEY: 81234
        });
        let versions = RoomDeviceVersions::from_members_body(&body).unwrap();
        assert_eq!(versions.room_version, 81234);
        assert_eq!(versions.members.len(), 2);
        assert_eq!(versions.members["@bob:localhost"].seq, 3);
        assert!(!versions.members["@alice:localhost"].is_hashable());
    }

    /// 🚨 fail closed：讀不出號碼就是錯，不是 0。
    #[test]
    fn room_device_versions_refuse_a_response_without_the_numbers() {
        let plain_matrix = json!({ "chunk": [member("@bob:localhost", "join", None)] });
        assert!(
            RoomDeviceVersions::from_members_body(&plain_matrix).is_err(),
            "沒有房間版本號"
        );
        let joined_without_version = json!({
            "chunk": [member("@bob:localhost", "join", None)],
            ROOM_VERSION_KEY: 1
        });
        assert!(
            RoomDeviceVersions::from_members_body(&joined_without_version).is_err(),
            "join 的人沒有裝置版本號"
        );
        let malformed = json!({
            "chunk": [member("@bob:localhost", "join", Some("three-abc"))],
            ROOM_VERSION_KEY: 1
        });
        assert!(
            RoomDeviceVersions::from_members_body(&malformed).is_err(),
            "版本號形狀不對"
        );
        let string_room_version = json!({ "chunk": [], ROOM_VERSION_KEY: "81234" });
        assert!(
            RoomDeviceVersions::from_members_body(&string_room_version).is_err(),
            "字串的房間版本號不算"
        );
    }

    /// 收到 1506 之後只對變了的人動作：新加入與版本號不同的要重查，離開的要換房間金鑰。
    #[test]
    fn diff_names_who_to_requery_and_who_left() {
        let before = RoomDeviceVersions::from_members_body(&json!({
            "chunk": [
                member("@alice:localhost", "join", Some("1-aaaaaaaaaa")),
                member("@bob:localhost", "join", Some("3-810b7c3be4")),
                member("@weil:localhost", "join", Some("2-bbbbbbbbbb")),
            ],
            ROOM_VERSION_KEY: 81234
        }))
        .unwrap();
        let after = RoomDeviceVersions::from_members_body(&json!({
            "chunk": [
                member("@alice:localhost", "join", Some("1-aaaaaaaaaa")),
                member("@bob:localhost", "join", Some("4-0123456789")),
                member("@carol:localhost", "join", Some("1-cccccccccc")),
                member("@weil:localhost", "leave", None),
            ],
            ROOM_VERSION_KEY: 81240
        }))
        .unwrap();
        assert_eq!(
            after.diff_from(&before),
            MembersDiff {
                changed: vec!["@bob:localhost".into(), "@carol:localhost".into()],
                left: vec!["@weil:localhost".into()],
            }
        );
        assert_eq!(
            after.diff_from(&after),
            MembersDiff::default(),
            "跟自己比沒有差"
        );
    }
}
