//! RPC 自己的極簡 pack（rpc-spec §1）：`ver(1) ‖ type(1) ‖ data`。
//!
//! 📎 跟 homeserver 那套 `wbf-pack`（`wbf-wire`）**無關**，只借「極簡二進位前綴」的做法，
//! 🚫 不共用 codec。
//!
//! 這個檔只做 bytes ↔ (type, JSON bytes)：明文直接放、密文用 token 導出的兩把金鑰
//! （architecture-v2 §4.4）。**哪些該加密**不在這裡判斷——那是 `connection.rs` 的事，
//! 而且只能在那一個地方判斷。

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use zeroize::{Zeroize, ZeroizeOnDrop};

/// pack 格式的版本。⚠️ 是**封裝**的版本，🚫 不是 `hello` 談的 `protocol`。
pub const PACK_VERSION: u8 = 0x01;
/// 一個 frame 的上限（含前綴）。跟「超過就走資料平面」是同一個數。
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

const NONCE_LEN: usize = 24;
const AAD: &[u8] = b"wbf-rpc v1";
const CONTEXT_CLIENT_TO_DAEMON: &str = "wbf-matrix-client rpc client-to-daemon v1";
const CONTEXT_DAEMON_TO_CLIENT: &str = "wbf-matrix-client rpc daemon-to-client v1";

/// `type` 只回答一件事：這包是明文還是密文。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PackType {
    Plain,
    Cipher,
}

impl PackType {
    fn to_byte(self) -> u8 {
        match self {
            PackType::Plain => 0x01,
            PackType::Cipher => 0x02,
        }
    }

    /// `0x00`（未定）與其他值都是 `None`：不是正面認得就拒絕。
    fn from_byte(byte: u8) -> Option<PackType> {
        match byte {
            0x01 => Some(PackType::Plain),
            0x02 => Some(PackType::Cipher),
            _ => None,
        }
    }
}

/// 解包失敗的種類。對到 rpc-spec §1.4 的 close：前三種是 `BAD_FRAME`，最後一種是 `BAD_TOKEN`。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PackError {
    TooShort,
    UnknownVersion(u8),
    UnknownType(u8),
    TooLarge(usize),
    /// AEAD 標籤驗不過：token 不對，或有人改過內容。
    CannotDecrypt,
}

/// 兩個方向各一把金鑰（反射攻擊擋在這裡），從 `daemon.token` 的內容導出。
#[derive(Zeroize, ZeroizeOnDrop)]
pub struct RpcKeys {
    client_to_daemon: [u8; 32],
    daemon_to_client: [u8; 32],
}

impl RpcKeys {
    /// Args:
    ///     token: `daemon.token` 整檔的內容, example: 256 個隨機 byte
    pub fn from_token(token: &[u8]) -> RpcKeys {
        RpcKeys {
            client_to_daemon: blake3::derive_key(CONTEXT_CLIENT_TO_DAEMON, token),
            daemon_to_client: blake3::derive_key(CONTEXT_DAEMON_TO_CLIENT, token),
        }
    }
}

/// 哪一端在說話。daemon 用 `Daemon` 封、`Client` 解；前端反過來。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Client,
    Daemon,
}

/// 封一包。
///
/// Args:
///     side: 誰在送, example: Side::Daemon
///     pack_type: example: PackType::Cipher
///     json: 序列化好的 JSON bytes, example: br#"{"code":0}"#
/// Return:
///     Vec<u8>  `ver ‖ type ‖ data`；`Cipher` 時 data = nonce(24) ‖ 密文
pub fn seal(keys: &RpcKeys, side: Side, pack_type: PackType, json: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(2 + NONCE_LEN + json.len() + 16);
    frame.push(PACK_VERSION);
    frame.push(pack_type.to_byte());
    match pack_type {
        PackType::Plain => frame.extend_from_slice(json),
        PackType::Cipher => {
            let mut nonce = [0u8; NONCE_LEN];
            getrandom::getrandom(&mut nonce).expect("OS randomness");
            let cipher = XChaCha20Poly1305::new(sending_key(keys, side).into());
            let ciphertext = cipher
                .encrypt(
                    XNonce::from_slice(&nonce),
                    Payload {
                        msg: json,
                        aad: AAD,
                    },
                )
                .expect("XChaCha20-Poly1305 encrypt cannot fail on in-memory data");
            frame.extend_from_slice(&nonce);
            frame.extend_from_slice(&ciphertext);
        }
    }
    frame
}

/// 拆一包。
///
/// Args:
///     side: **收的人**是誰（決定用哪把金鑰解）, example: Side::Daemon
///     frame: 整個 WS binary frame
/// Return:
///     Ok((PackType, Vec<u8>))  type 與 JSON bytes（這裡不驗它是不是 JSON）
///     Err(PackError)           見各 variant
pub fn open(keys: &RpcKeys, side: Side, frame: &[u8]) -> Result<(PackType, Vec<u8>), PackError> {
    if frame.len() > MAX_FRAME_BYTES {
        return Err(PackError::TooLarge(frame.len()));
    }
    let [version, type_byte, data @ ..] = frame else {
        return Err(PackError::TooShort);
    };
    if *version != PACK_VERSION {
        return Err(PackError::UnknownVersion(*version));
    }
    let Some(pack_type) = PackType::from_byte(*type_byte) else {
        return Err(PackError::UnknownType(*type_byte));
    };
    match pack_type {
        PackType::Plain => Ok((pack_type, data.to_vec())),
        PackType::Cipher => {
            if data.len() < NONCE_LEN {
                return Err(PackError::TooShort);
            }
            let (nonce, ciphertext) = data.split_at(NONCE_LEN);
            let cipher = XChaCha20Poly1305::new(receiving_key(keys, side).into());
            let json = cipher
                .decrypt(
                    XNonce::from_slice(nonce),
                    Payload {
                        msg: ciphertext,
                        aad: AAD,
                    },
                )
                .map_err(|_| PackError::CannotDecrypt)?;
            Ok((pack_type, json))
        }
    }
}

fn sending_key(keys: &RpcKeys, side: Side) -> &[u8; 32] {
    match side {
        Side::Client => &keys.client_to_daemon,
        Side::Daemon => &keys.daemon_to_client,
    }
}

fn receiving_key(keys: &RpcKeys, side: Side) -> &[u8; 32] {
    match side {
        Side::Client => &keys.daemon_to_client,
        Side::Daemon => &keys.client_to_daemon,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys() -> RpcKeys {
        RpcKeys::from_token(&[7u8; 256])
    }

    #[test]
    fn a_cipher_pack_round_trips_between_the_two_sides() {
        let keys = keys();
        let frame = seal(
            &keys,
            Side::Client,
            PackType::Cipher,
            br#"{"method":"hello"}"#,
        );
        assert_eq!(&frame[..2], &[0x01, 0x02]);
        let (pack_type, json) = open(&keys, Side::Daemon, &frame).unwrap();
        assert_eq!(pack_type, PackType::Cipher);
        assert_eq!(json, br#"{"method":"hello"}"#);
    }

    #[test]
    fn a_plain_pack_is_just_the_prefix_and_the_json() {
        let keys = keys();
        let frame = seal(&keys, Side::Daemon, PackType::Plain, b"{}");
        assert_eq!(frame, vec![0x01, 0x01, b'{', b'}']);
        assert_eq!(
            open(&keys, Side::Client, &frame).unwrap(),
            (PackType::Plain, b"{}".to_vec())
        );
    }

    #[test]
    fn the_two_directions_use_different_keys_so_a_reply_cannot_be_reflected() {
        let keys = keys();
        let from_daemon = seal(&keys, Side::Daemon, PackType::Cipher, b"{}");
        // 把 daemon 的回應原封送回去當請求：daemon 用 client→daemon 的鑰解，解不開。
        assert_eq!(
            open(&keys, Side::Daemon, &from_daemon),
            Err(PackError::CannotDecrypt)
        );
    }

    #[test]
    fn a_wrong_token_or_a_flipped_bit_is_cannot_decrypt() {
        let frame = seal(&keys(), Side::Client, PackType::Cipher, b"{}");
        let other = RpcKeys::from_token(&[8u8; 256]);
        assert_eq!(
            open(&other, Side::Daemon, &frame),
            Err(PackError::CannotDecrypt)
        );
        let mut tampered = frame.clone();
        let last = tampered.len() - 1;
        tampered[last] ^= 1;
        assert_eq!(
            open(&keys(), Side::Daemon, &tampered),
            Err(PackError::CannotDecrypt)
        );
    }

    #[test]
    fn unknown_version_or_type_and_short_or_huge_frames_are_refused() {
        let keys = keys();
        assert_eq!(open(&keys, Side::Daemon, &[]), Err(PackError::TooShort));
        assert_eq!(open(&keys, Side::Daemon, &[0x01]), Err(PackError::TooShort));
        assert_eq!(
            open(&keys, Side::Daemon, &[0x02, 0x01]),
            Err(PackError::UnknownVersion(0x02))
        );
        // 0x00 是「未定」：不能送。
        assert_eq!(
            open(&keys, Side::Daemon, &[0x01, 0x00]),
            Err(PackError::UnknownType(0x00))
        );
        assert_eq!(
            open(&keys, Side::Daemon, &[0x01, 0x03]),
            Err(PackError::UnknownType(0x03))
        );
        // 密文短到連 nonce 都不夠。
        assert_eq!(
            open(&keys, Side::Daemon, &[0x01, 0x02, 1, 2, 3]),
            Err(PackError::TooShort)
        );
        let huge = vec![0x01; MAX_FRAME_BYTES + 1];
        assert_eq!(
            open(&keys, Side::Daemon, &huge),
            Err(PackError::TooLarge(MAX_FRAME_BYTES + 1))
        );
    }
}
