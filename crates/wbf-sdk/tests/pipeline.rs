//! 上傳／下載／seek／續傳／串流，對著記憶體版 server（`support/fake_server.rs`）跑。
//! 真 server 的差異由 `e2e_local_server.rs` 抓；這裡管的是 SDK 自己的邏輯與每一條拒絕路徑。

mod support;

use std::io::Cursor;

use sha2::{Digest, Sha256};
use support::fake_server::FakeServer;
use wbf_sdk::chunk_crypto::chunk_count;
use wbf_sdk::{ChunkedBlock, Cipher, FileCipher, Manifest, SdkError, UploadState, WbfClient};

const SERVER: &str = "http://fake";
const USER: &str = "@alice:fake";

fn sample(len: usize) -> Vec<u8> {
    (0..len)
        .map(|position| ((position * 7919) % 251) as u8)
        .collect()
}

fn block_for(name: &str, file_size: Option<u64>) -> ChunkedBlock {
    ChunkedBlock {
        v: 1,
        cipher: Cipher::None,
        key: None,
        nonce_base: None,
        chunk_size: 0,
        file_size,
        name: Some(name.to_string()),
        mimetype: Some("application/octet-stream".to_string()),
        sha256: None,
    }
}

/// create → send_chunks → seal，回 manifest。
async fn upload_fixed(
    server: &mut FakeServer,
    cipher: Cipher,
    chunk_size: u32,
    plaintext: &[u8],
) -> Manifest {
    let mut client = WbfClient::new(&mut *server);
    let file_cipher = FileCipher::with_fixed(cipher, [7; 32], [9; 8], chunk_size);
    let state = client
        .create_upload(
            SERVER,
            USER,
            &file_cipher,
            &block_for("sample.bin", Some(plaintext.len() as u64)),
        )
        .await
        .expect("create");
    let summary = client
        .send_chunks(&state, &mut Cursor::new(plaintext), 0, true, &mut |_, _| {})
        .await
        .expect("send");
    assert_eq!(
        summary.chunks_sent,
        chunk_count(plaintext.len() as u64, chunk_size).unwrap()
    );
    let mut final_block = state.block.clone();
    final_block.sha256 = summary.sha256;
    client
        .seal_upload(&state, &final_block)
        .await
        .expect("seal")
}

async fn download_all(server: &mut FakeServer, manifest: &Manifest) -> Result<Vec<u8>, SdkError> {
    let mut client = WbfClient::new(server);
    let mut out = Vec::new();
    client.download(manifest, &mut out, &mut |_, _| {}).await?;
    Ok(out)
}

#[tokio::test]
async fn roundtrip_every_cipher() {
    for cipher in [Cipher::ChaCha20Poly1305, Cipher::Aes256Gcm, Cipher::None] {
        let mut server = FakeServer::new();
        let plaintext = sample(100_000);
        let manifest = upload_fixed(&mut server, cipher, 4096, &plaintext).await;
        assert_eq!(manifest.block.cipher, cipher);
        assert_eq!(
            manifest.block.sha256.as_deref(),
            Some(hex::encode(Sha256::digest(&plaintext)).as_str())
        );

        let stored = &server.media[&manifest.mxc];
        assert_eq!(stored.chunks.len(), 25);
        let overhead = if cipher.is_encrypting() { 16 } else { 0 };
        assert_eq!(stored.chunks[0].len(), 4096 + overhead);
        assert_eq!(stored.chunks[24].len(), 100_000 - 24 * 4096 + overhead);
        if cipher.is_encrypting() {
            assert_ne!(
                &stored.chunks[0][..16],
                &plaintext[..16],
                "ciphertext must not leak plaintext"
            );
        }

        let mut client = WbfClient::new(&mut server);
        let mut out = Vec::new();
        let report = client
            .download(&manifest, &mut out, &mut |_, _| {})
            .await
            .expect("download");
        assert_eq!(out, plaintext, "{cipher:?}");
        assert!(report.sha256_verified);
        assert_eq!(report.chunks, 25);
    }
}

#[tokio::test]
async fn manifest_roundtrips_through_json_and_keeps_key_secret_shape() {
    let mut server = FakeServer::new();
    let manifest = upload_fixed(&mut server, Cipher::ChaCha20Poly1305, 16, &sample(40)).await;
    let json = manifest.to_json();
    assert!(String::from_utf8_lossy(&json).contains("\"key\""));
    let parsed = Manifest::from_json(&json).expect("parses");
    assert_eq!(parsed, manifest);
    let mut tampered: serde_json::Value = serde_json::from_slice(&json).unwrap();
    tampered["block"].as_object_mut().unwrap().remove("key");
    assert!(matches!(
        Manifest::from_json(tampered.to_string().as_bytes()),
        Err(SdkError::Integrity(_))
    ));
}

#[tokio::test]
async fn seek_semantics_match_cli_spec_3_3_1() {
    // chunk_size 64，位置 70：對應 CLI 規格 §3.3.1 的 64K／70K 例子，縮小 1024 倍。
    let mut server = FakeServer::new();
    let plaintext = sample(200);
    let manifest = upload_fixed(&mut server, Cipher::Aes256Gcm, 64, &plaintext).await;
    let mut client = WbfClient::new(&mut server);

    let to_chunk_end = client.seek_read(&manifest, 70, None).await.unwrap();
    assert_eq!(to_chunk_end.bytes, &plaintext[70..128]);
    assert_eq!(to_chunk_end.chunks_read, vec![1]);
    assert!(!to_chunk_end.truncated);

    let across = client.seek_read(&manifest, 70, Some(70)).await.unwrap();
    assert_eq!(across.bytes, &plaintext[70..140]);
    assert_eq!(across.chunks_read, vec![1, 2]);

    let exact = client.seek_read(&manifest, 64, Some(64)).await.unwrap();
    assert_eq!(exact.bytes, &plaintext[64..128]);
    assert_eq!(exact.chunks_read, vec![1]);

    // 200 byte、64 一塊 = 四塊（最後一塊 192..200）。
    let past_end = client.seek_read(&manifest, 190, Some(100)).await.unwrap();
    assert_eq!(past_end.bytes, &plaintext[190..200]);
    assert_eq!(past_end.chunks_read, vec![2, 3]);
    assert!(past_end.truncated);

    let mid_chunk_no_len = client.seek_read(&manifest, 130, None).await.unwrap();
    assert_eq!(
        mid_chunk_no_len.bytes,
        &plaintext[130..192],
        "no --len stops at the chunk end, not the file end"
    );

    let last_chunk_no_len = client.seek_read(&manifest, 195, None).await.unwrap();
    assert_eq!(last_chunk_no_len.bytes, &plaintext[195..200]);
    assert_eq!(last_chunk_no_len.chunks_read, vec![3]);

    assert!(matches!(
        client.seek_read(&manifest, 200, None).await,
        Err(SdkError::Usage(_))
    ));
    assert!(matches!(
        client.seek_read(&manifest, 5000, Some(1)).await,
        Err(SdkError::Usage(_))
    ));
}

#[tokio::test]
async fn resume_after_dropped_ack_continues_from_status() {
    let mut server = FakeServer::new();
    let plaintext = sample(10_000);
    let file_cipher = FileCipher::with_fixed(Cipher::ChaCha20Poly1305, [1; 32], [2; 8], 1024);
    let state = {
        let mut client = WbfClient::new(&mut server);
        client
            .create_upload(
                SERVER,
                USER,
                &file_cipher,
                &block_for("big.bin", Some(10_000)),
            )
            .await
            .unwrap()
    };
    let state_json = state.to_json();
    server.drop_ack_once_at = Some((state.upload_id, 3));

    let first_try = {
        let mut client = WbfClient::new(&mut server);
        client
            .send_chunks(
                &state,
                &mut Cursor::new(&plaintext),
                0,
                false,
                &mut |_, _| {},
            )
            .await
    };
    assert!(matches!(first_try, Err(SdkError::Network(_))));

    // 「再跑一次同一條命令」：讀狀態檔、問 Status、從 received 接著送，同一把 key。
    let state = UploadState::from_json(&state_json).unwrap();
    assert!(state.is_for(SERVER, USER));
    assert!(!state.is_for("http://other", USER));
    let mut client = WbfClient::new(&mut server);
    let status = client.upload_status(state.upload_id).await.unwrap();
    assert_eq!(
        status.received, 4,
        "chunk 3 was stored even though its ack was lost"
    );
    let mut progress = Vec::new();
    let summary = client
        .send_chunks(
            &state,
            &mut Cursor::new(&plaintext),
            status.received,
            true,
            &mut |done, _| progress.push(done),
        )
        .await
        .unwrap();
    assert_eq!(progress.first(), Some(&5));
    assert_eq!(summary.chunks_sent, 10);
    let mut final_block = state.block.clone();
    final_block.sha256 = summary.sha256;
    let manifest = client.seal_upload(&state, &final_block).await.unwrap();
    assert_eq!(
        download_all(&mut server, &manifest).await.unwrap(),
        plaintext
    );
}

/// 狀態檔比 server 新（OutOfOrder，跳回 expected_seq）與比 server 舊（冪等 Ack，照 received 跳過去）兩種。
#[tokio::test]
async fn stale_or_ahead_resume_point_resyncs_with_server() {
    let mut server = FakeServer::new();
    let plaintext = sample(5_000);
    let file_cipher = FileCipher::with_fixed(Cipher::None, [0; 32], [0; 8], 1000);
    let state = {
        let mut client = WbfClient::new(&mut server);
        client
            .create_upload(SERVER, USER, &file_cipher, &block_for("x", Some(5_000)))
            .await
            .unwrap()
    };
    // server 收了 0..3（第 2 塊 Ack 掉了）。
    server.drop_ack_once_at = Some((state.upload_id, 2));
    {
        let mut client = WbfClient::new(&mut server);
        let result = client
            .send_chunks(
                &state,
                &mut Cursor::new(&plaintext),
                0,
                false,
                &mut |_, _| {},
            )
            .await;
        assert!(matches!(result, Err(SdkError::Network(_))));
    }
    server.requests.clear();

    // 狀態檔比 server 新（宣稱從 4 開始）：server 回 OutOfOrder expected_seq 3，我們跳回去。
    let mut client = WbfClient::new(&mut server);
    let summary = client
        .send_chunks(
            &state,
            &mut Cursor::new(&plaintext),
            4,
            false,
            &mut |_, _| {},
        )
        .await
        .unwrap();
    assert_eq!(summary.chunks_sent, 5);
    let chunk_seqs: Vec<u32> = server
        .requests
        .iter()
        .filter(|(kind, subtype, _)| {
            *kind == wbf_wire::Kind::Upload && *subtype == wbf_wire::pack::upload::CHUNK
        })
        .map(|(_, _, seq)| *seq)
        .collect();
    assert_eq!(chunk_seqs, vec![4, 3, 4]);

    // 狀態檔比 server 舊（從 0 重送）：冪等 Ack 帶 received，我們照它跳。
    let mut server2 = FakeServer::new();
    let state2 = {
        let mut client = WbfClient::new(&mut server2);
        client
            .create_upload(SERVER, USER, &file_cipher, &block_for("x", Some(5_000)))
            .await
            .unwrap()
    };
    server2.drop_ack_once_at = Some((state2.upload_id, 2));
    {
        let mut client = WbfClient::new(&mut server2);
        let _ = client
            .send_chunks(
                &state2,
                &mut Cursor::new(&plaintext),
                0,
                false,
                &mut |_, _| {},
            )
            .await;
    }
    server2.requests.clear();
    let mut client = WbfClient::new(&mut server2);
    client
        .send_chunks(
            &state2,
            &mut Cursor::new(&plaintext),
            0,
            false,
            &mut |_, _| {},
        )
        .await
        .unwrap();
    let chunk_seqs: Vec<u32> = server2
        .requests
        .iter()
        .filter(|(kind, subtype, _)| {
            *kind == wbf_wire::Kind::Upload && *subtype == wbf_wire::pack::upload::CHUNK
        })
        .map(|(_, _, seq)| *seq)
        .collect();
    assert_eq!(chunk_seqs, vec![0, 3, 4]);
}

#[tokio::test]
async fn stream_upload_handles_every_boundary() {
    for len in [1usize, 999, 1000, 1001, 2000, 3500] {
        let mut server = FakeServer::new();
        let plaintext = sample(len);
        let file_cipher = FileCipher::with_fixed(Cipher::ChaCha20Poly1305, [3; 32], [4; 8], 1000);
        let mut client = WbfClient::new(&mut server);
        let state = client
            .create_upload(SERVER, USER, &file_cipher, &block_for("stream", None))
            .await
            .unwrap();
        assert_eq!(state.block.file_size, None);
        let summary = client
            .send_stream(&state, &mut Cursor::new(&plaintext), &mut |_, _| {})
            .await
            .unwrap();
        assert_eq!(summary.file_size, len as u64, "len {len}");
        assert_eq!(summary.chunks_sent, len.div_ceil(1000) as u32);
        let mut final_block = state.block.clone();
        final_block.file_size = Some(summary.file_size);
        final_block.sha256 = summary.sha256;
        let manifest = client.seal_upload(&state, &final_block).await.unwrap();
        assert_eq!(
            server.media[&manifest.mxc].file_size, None,
            "server never learns the size of a stream"
        );
        assert_eq!(
            download_all(&mut server, &manifest).await.unwrap(),
            plaintext,
            "len {len}"
        );
    }
}

#[tokio::test]
async fn empty_inputs_are_usage_errors() {
    let mut server = FakeServer::new();
    let file_cipher = FileCipher::with_fixed(Cipher::None, [0; 32], [0; 8], 16);
    let mut client = WbfClient::new(&mut server);
    assert!(matches!(
        client
            .create_upload(SERVER, USER, &file_cipher, &block_for("empty", Some(0)))
            .await,
        Err(SdkError::Usage(_))
    ));
    let state = client
        .create_upload(SERVER, USER, &file_cipher, &block_for("stream", None))
        .await
        .unwrap();
    assert!(matches!(
        client
            .send_stream(&state, &mut Cursor::new(Vec::new()), &mut |_, _| {})
            .await,
        Err(SdkError::Usage(_))
    ));
}

#[tokio::test]
async fn corrupted_chunk_fails_closed() {
    let mut server = FakeServer::new();
    let plaintext = sample(300);
    let manifest = upload_fixed(&mut server, Cipher::Aes256Gcm, 100, &plaintext).await;
    server.media.get_mut(&manifest.mxc).unwrap().chunks[1][5] ^= 0x80;
    assert!(matches!(
        download_all(&mut server, &manifest).await,
        Err(SdkError::Integrity(_))
    ));
    let mut client = WbfClient::new(&mut server);
    assert!(matches!(
        client.seek_read(&manifest, 150, None).await,
        Err(SdkError::Integrity(_))
    ));
    // 別的塊沒壞，seek 到那裡照樣行：完整性是每塊各自驗。
    assert_eq!(
        client.seek_read(&manifest, 250, None).await.unwrap().bytes,
        &plaintext[250..300]
    );
}

#[tokio::test]
async fn plaintext_mode_chunk_with_wrong_length_fails_closed() {
    let mut server = FakeServer::new();
    let plaintext = sample(300);
    let manifest = upload_fixed(&mut server, Cipher::None, 100, &plaintext).await;
    server.media.get_mut(&manifest.mxc).unwrap().chunks[0].push(0);
    assert!(matches!(
        download_all(&mut server, &manifest).await,
        Err(SdkError::Integrity(_))
    ));
}

#[tokio::test]
async fn manifest_that_disagrees_with_server_fails_closed() {
    let mut server = FakeServer::new();
    let plaintext = sample(300);
    let manifest = upload_fixed(&mut server, Cipher::ChaCha20Poly1305, 100, &plaintext).await;

    let mut wrong_chunk_size = manifest.clone();
    wrong_chunk_size.block.chunk_size = 50;
    assert!(matches!(
        download_all(&mut server, &wrong_chunk_size).await,
        Err(SdkError::Integrity(_))
    ));

    let mut wrong_file_size = manifest.clone();
    wrong_file_size.block.file_size = Some(1_000);
    assert!(matches!(
        download_all(&mut server, &wrong_file_size).await,
        Err(SdkError::Integrity(_))
    ));

    let mut wrong_sha = manifest.clone();
    wrong_sha.block.sha256 = Some("0".repeat(64));
    assert!(matches!(
        download_all(&mut server, &wrong_sha).await,
        Err(SdkError::Integrity(_))
    ));

    let mut uppercase_sha = manifest.clone();
    uppercase_sha.block.sha256 = Some(manifest.block.sha256.clone().unwrap().to_uppercase());
    assert!(matches!(
        download_all(&mut server, &uppercase_sha).await,
        Err(SdkError::Integrity(_))
    ));

    let mut wrong_key = manifest.clone();
    wrong_key.block.key = Some([0xAA; 32]);
    assert!(matches!(
        download_all(&mut server, &wrong_key).await,
        Err(SdkError::Integrity(_))
    ));

    let mut wrong_name = manifest.clone();
    wrong_name.block.name = Some("other.bin".into());
    assert!(
        matches!(
            download_all(&mut server, &wrong_name).await,
            Err(SdkError::Integrity(_))
        ),
        "description cross-check"
    );

    let mut unknown_mxc = manifest.clone();
    unknown_mxc.mxc = "mxc://fake/0000000000000000".into();
    assert!(matches!(
        download_all(&mut server, &unknown_mxc).await,
        Err(SdkError::Server { .. })
    ));

    // 沒改的那份還是好的。
    assert_eq!(
        download_all(&mut server, &manifest).await.unwrap(),
        plaintext
    );
}

#[tokio::test]
async fn server_description_tampered_fails_closed() {
    let mut server = FakeServer::new();
    let plaintext = sample(300);
    let manifest = upload_fixed(&mut server, Cipher::ChaCha20Poly1305, 100, &plaintext).await;
    server.media.get_mut(&manifest.mxc).unwrap().description[0] ^= 1;
    assert!(matches!(
        download_all(&mut server, &manifest).await,
        Err(SdkError::Integrity(_))
    ));
    server
        .media
        .get_mut(&manifest.mxc)
        .unwrap()
        .description
        .clear();
    assert!(matches!(
        download_all(&mut server, &manifest).await,
        Err(SdkError::Integrity(_))
    ));
}

#[tokio::test]
async fn hello_ping_status_abort() {
    let mut server = FakeServer::new();
    let mut client = WbfClient::new(&mut server);
    let hello = client.hello("wbf-sdk-test").await.unwrap();
    assert!(hello.features.iter().any(|feature| feature == "upload"));
    client.ping().await.unwrap();
    let file_cipher = FileCipher::with_fixed(Cipher::None, [0; 32], [0; 8], 16);
    let state = client
        .create_upload(SERVER, USER, &file_cipher, &block_for("x", Some(40)))
        .await
        .unwrap();
    let status = client.upload_status(state.upload_id).await.unwrap();
    assert_eq!(
        (status.received, status.chunk_count, status.file_size),
        (0, Some(3), Some(40))
    );
    client.abort_upload(state.upload_id).await.unwrap();
    let after = client.upload_status(state.upload_id).await;
    assert_eq!(after.unwrap_err().server_code(), Some("NotFound"));
}

/// wbfuwunel 對 `Create` 的回應標頭 id 是新上傳 id（plan-v1 §6 記的順帶發現）：兩種都要收；
/// 標頭 id 是別的值、或非 `Create` 的回應不抄回 id，都要拒。
#[tokio::test]
async fn create_ack_header_id_variants() {
    let file_cipher = FileCipher::with_fixed(Cipher::None, [0; 32], [0; 8], 16);

    let mut server = FakeServer::new();
    server.create_ack_header_is_upload_id = true;
    let mut client = WbfClient::new(&mut server);
    let state = client
        .create_upload(SERVER, USER, &file_cipher, &block_for("x", Some(40)))
        .await
        .expect("header id == new upload id is accepted");
    assert_ne!(state.upload_id, 0);

    let mut server = FakeServer::new();
    server.wrong_response_id_once = Some(0xDEAD);
    let mut client = WbfClient::new(&mut server);
    let error = client
        .create_upload(SERVER, USER, &file_cipher, &block_for("x", Some(40)))
        .await
        .unwrap_err();
    assert!(matches!(error, SdkError::Protocol(_)), "{error}");

    let mut server = FakeServer::new();
    server.wrong_response_id_once = Some(1);
    let mut client = WbfClient::new(&mut server);
    let error = client.hello("test").await.unwrap_err();
    assert!(
        matches!(error, SdkError::Protocol(_)),
        "non-Create must echo id 0: {error}"
    );
}
