//! 上傳／下載／seek／續傳／串流，對著記憶體版 server（`support/fake_server.rs`）跑。
//! 真 server 的差異由 `e2e_local_server.rs` 抓；這裡管的是 SDK 自己的邏輯與每一條拒絕路徑。

mod support;

use std::io::Cursor;

use sha2::{Digest, Sha256};
use support::fake_server::FakeServer;
use wbf_sdk::chunk_crypto::chunk_count;
use wbf_sdk::{
    ChunkedBlock, Cipher, FileCipher, Manifest, PackChannel, RecentPlan, SdkError, UploadState,
    WbfClient,
};

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
    let after = client.upload_status(state.upload_id).await.unwrap_err();
    assert_eq!(after.server_code(), Some("NotFound"), "名字給人看");
    // ⭐ 程式認的是序號：假 server 從 `WbfErrorCode` 反查補上的 `code_id` 要對得上。
    assert_eq!(
        after.wbf_code(),
        Some(wbf_sdk::error_code::WbfErrorCode::NotFound)
    );
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

/// feature gate 在 runtime 執行（PR #8 審查 rumia 🟡1）：沒問過 hello 或 server 沒宣告，`recent`／`send_event` 不送就拒。
#[tokio::test]
async fn feature_gated_commands_refuse_without_advertised_feature() {
    use wbf_sdk::protocol::{RecentRequest, SendRequest};
    let request = RecentRequest {
        rooms: None,
        limit: 10,
        cg_seq: None,
        before: None,
        batch: None,
    };
    let mut ignore = |_: &wbf_sdk::protocol::BatchMeta, _: Vec<serde_json::Value>| Ok(());
    let send = SendRequest {
        room_id: "!r:fake".into(),
        event_type: "m.room.message".into(),
        txn_id: "t".into(),
        attachments: vec![],
    };

    let mut server = FakeServer::new();
    let mut client = WbfClient::new(&mut server);
    assert!(!client.has_feature("recent"), "nothing known before hello");
    let error = client
        .recent_window(&request, std::time::Duration::from_secs(1), &mut ignore)
        .await
        .unwrap_err();
    assert!(matches!(error, SdkError::Usage(_)), "{error}");
    assert!(server.requests.is_empty(), "nothing was sent");

    let mut client = WbfClient::new(&mut server);
    client.hello("test").await.unwrap();
    assert!(client.has_feature("upload") && !client.has_feature("recent"));
    let error = client
        .recent_window(&request, std::time::Duration::from_secs(1), &mut ignore)
        .await
        .unwrap_err();
    assert!(matches!(error, SdkError::Usage(_)), "{error}");
    let error = client.send_event(&send, b"{}".to_vec()).await.unwrap_err();
    assert!(matches!(error, SdkError::Usage(_)), "{error}");
    let sent_kinds: Vec<_> = server.requests.iter().map(|(kind, _, _)| *kind).collect();
    assert_eq!(
        sent_kinds,
        vec![wbf_wire::Kind::Control],
        "only the Hello went out"
    );
}

fn recent_fixture(count: i64) -> Vec<serde_json::Value> {
    // g_seq 1000 + i，新到舊排。
    (1..=count)
        .rev()
        .map(|i| {
            serde_json::json!({
                "type": "m.room.message", "event_id": format!("$e{i}"), "room_id": "!r:fake", "sender": "@a:fake",
                "origin_server_ts": i, "content": { "msgtype": "m.text", "body": format!("m{i}") },
                "unsigned": { "org.wbftw.wbfuwunel.r_seq": i, "org.wbftw.wbfuwunel.g_seq": 1000 + i }
            })
        })
        .collect()
}

/// pack-pipeline §6：一窗多個 Batch、`more: true` 就帶 `before` 再一窗、水位是第一窗第一個 Batch 的 fs。
#[tokio::test]
async fn recent_sync_pulls_windows_until_caught_up() {
    let mut server = FakeServer::new();
    server.extra_features = vec!["recent", "batch", "seq"];
    server.recent_events = recent_fixture(25);
    let mut client = WbfClient::new(&mut server);
    client.hello("test").await.unwrap();
    let mut seen: Vec<(u32, u32, u32, i64)> = Vec::new(); // (tc, bc, r, fs)
    let mut events = 0usize;
    let plan = |window: u32, batch: Option<u32>| RecentPlan {
        max_events: None,
        window,
        batch,
    };
    let summary = client
        .recent_sync(None, plan(10, Some(4)), &mut |meta, batch| {
            seen.push((meta.tc, meta.bc, meta.r, meta.fs));
            events += batch.len();
            Ok(())
        })
        .await
        .unwrap();
    // 25 則、一窗 10、一批 4：窗 1 = 4+4+2、窗 2 = 4+4+2、窗 3 = 4+1（tc 5 < 10，追平）。
    assert!(summary.caught_up);
    assert_eq!(summary.windows, 3);
    assert_eq!(summary.events, 25);
    assert_eq!(events, 25);
    assert_eq!(summary.new_cg_seq, Some(1025), "first window's first fs");
    assert_eq!(summary.last_ls, Some(1001));
    assert_eq!(
        seen,
        vec![
            (10, 4, 6, 1025),
            (10, 4, 2, 1021),
            (10, 2, 0, 1017),
            (10, 4, 6, 1015),
            (10, 4, 2, 1011),
            (10, 2, 0, 1007),
            (5, 4, 1, 1005),
            (5, 1, 0, 1001),
        ]
    );
    // 三個 Recent 請求都送了，且用同一條連線的 id 序列（非 0）。
    let recents = server
        .requests
        .iter()
        .filter(|(kind, subtype, _)| {
            *kind == wbf_wire::Kind::Event && *subtype == wbf_wire::pack::event::RECENT
        })
        .count();
    assert_eq!(recents, 3);

    // 帶水位再同步：只拿比 1025 新的 → 空窗，一個空 Batch，水位不動（None）。
    let mut client = WbfClient::new(&mut server);
    client.hello("test").await.unwrap();
    let mut batches = 0;
    let summary = client
        .recent_sync(Some(1025), plan(10, Some(4)), &mut |meta, _| {
            batches += 1;
            assert_eq!((meta.tc, meta.bc, meta.r), (0, 0, 0));
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(batches, 1);
    assert_eq!(summary.new_cg_seq, None);
    assert_eq!(summary.windows, 1);
    assert!(summary.caught_up);

    // 剛好整窗（10 則新的、limit 10）：第一窗停在則數上限（more: true，server 也不知道後面還有沒有），第二窗空 → 追平，水位 = 第一窗的 fs。
    server.recent_events = recent_fixture(35);
    let mut client = WbfClient::new(&mut server);
    client.hello("test").await.unwrap();
    let summary = client
        .recent_sync(Some(1025), plan(10, None), &mut |_, _| Ok(()))
        .await
        .unwrap();
    assert_eq!(summary.windows, 2);
    assert_eq!(summary.events, 10);
    assert_eq!(summary.new_cg_seq, Some(1035));
}

/// 三層分工（維護者 2026-09-08）：要 1000 則不是一窗 1000，是 320、320、320、40 四窗；湊滿就停、水位照樣是第一窗的 fs。
#[tokio::test]
async fn recent_sync_total_cap_splits_into_windows_and_shrinks_the_last_one() {
    let mut server = FakeServer::new();
    server.extra_features = vec!["recent", "batch"];
    server.recent_events = recent_fixture(1200);
    let mut client = WbfClient::new(&mut server);
    client.hello("test").await.unwrap();
    let mut window_sizes: Vec<u32> = Vec::new();
    let summary = client
        .recent_sync(
            None,
            RecentPlan {
                max_events: Some(1000),
                window: 320,
                batch: Some(100),
            },
            &mut |meta, _| {
                if meta.r + meta.bc == meta.tc {
                    window_sizes.push(meta.tc); // 每窗第一個 Batch
                }
                Ok(())
            },
        )
        .await
        .unwrap();
    assert_eq!(window_sizes, vec![320, 320, 320, 40]);
    assert_eq!(summary.events, 1000);
    assert_eq!(summary.windows, 4);
    assert!(
        !summary.caught_up,
        "stopped by the cap, not by reaching cg_seq"
    );
    assert_eq!(
        summary.new_cg_seq,
        Some(2200),
        "newest of this round is still the watermark"
    );
    assert_eq!(summary.last_ls, Some(1201));

    // 水位卡在總量中間：1200 則、cg_seq = 1700 → 比它新的只有 500 則；要 1000 → 320 一窗滿了再要，第二窗只有 180（< 320）→ 追平停，
    // 不會去要第三窗；水位仍是第一窗的 fs（2200）。
    server.recent_events = recent_fixture(1200);
    let mut client = WbfClient::new(&mut server);
    client.hello("test").await.unwrap();
    let mut window_sizes: Vec<u32> = Vec::new();
    let mut oldest_seen = i64::MAX;
    let summary = client
        .recent_sync(
            Some(1700),
            RecentPlan {
                max_events: Some(1000),
                window: 320,
                batch: Some(100),
            },
            &mut |meta, events| {
                if meta.r + meta.bc == meta.tc {
                    window_sizes.push(meta.tc);
                }
                for event in &events {
                    let g = event["unsigned"]["org.wbftw.wbfuwunel.g_seq"]
                        .as_i64()
                        .unwrap();
                    oldest_seen = oldest_seen.min(g);
                }
                Ok(())
            },
        )
        .await
        .unwrap();
    assert_eq!(window_sizes, vec![320, 180]);
    assert_eq!(summary.events, 500);
    assert_eq!(summary.windows, 2);
    assert!(summary.caught_up);
    assert_eq!(summary.new_cg_seq, Some(2200));
    assert_eq!(summary.last_ls, Some(1701));
    assert_eq!(oldest_seen, 1701, "nothing at or below cg_seq came back");

    // 總量比實際少：要 1000、只有 50 → 一窗 50 就追平。
    server.recent_events = recent_fixture(50);
    let mut client = WbfClient::new(&mut server);
    client.hello("test").await.unwrap();
    let summary = client
        .recent_sync(None, RecentPlan::default(), &mut |_, _| Ok(()))
        .await
        .unwrap();
    assert_eq!((summary.windows, summary.events), (1, 50));
    assert!(summary.caught_up);
    // 剛好等於總量：320 則、總量 320 → 一窗 320 湊滿就停，不多要一窗。
    server.recent_events = recent_fixture(320);
    let mut client = WbfClient::new(&mut server);
    client.hello("test").await.unwrap();
    let summary = client
        .recent_sync(
            None,
            RecentPlan {
                max_events: Some(320),
                window: 320,
                batch: None,
            },
            &mut |_, _| Ok(()),
        )
        .await
        .unwrap();
    assert_eq!(
        (summary.windows, summary.events, summary.caught_up),
        (1, 320, false)
    );
}

/// 中途斷線：已交出的 Batch 有效、錯誤原樣回、沒有新水位；limit 超過 server 上限會先 clamp。
#[tokio::test]
async fn recent_sync_mid_window_disconnect_keeps_what_arrived_and_gives_no_watermark() {
    let mut server = FakeServer::new();
    server.extra_features = vec!["recent", "batch"];
    server.recent_events = recent_fixture(12);
    server.drop_stream_after_batches = Some(2);
    let mut client = WbfClient::new(&mut server);
    client.hello("test").await.unwrap();
    let mut got = 0usize;
    let error = client
        .recent_sync(
            None,
            RecentPlan {
                max_events: None,
                window: 10,
                batch: Some(3),
            },
            &mut |_, batch| {
                got += batch.len();
                Ok(())
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(error, SdkError::Network(_)), "{error}");
    assert_eq!(got, 6, "two batches of three arrived before the drop");

    // limit 9999 → clamp 到 server 說的 500：12 則一窗就追平（tc 12 < 500）。
    let mut client = WbfClient::new(&mut server);
    client.hello("test").await.unwrap();
    let summary = client
        .recent_sync(
            None,
            RecentPlan {
                max_events: None,
                window: 9999,
                batch: Some(100),
            },
            &mut |_, _| Ok(()),
        )
        .await
        .unwrap();
    assert_eq!((summary.windows, summary.events), (1, 12));
}

/// server 的 Hello 把上限宣告成 0（設定誤植）：當沒宣告，用 client 預設，不 panic（PR #16 審查 rumia 🟡2）。
#[tokio::test]
async fn recent_sync_survives_a_zero_max_in_hello() {
    let mut server = FakeServer::new();
    server.extra_features = vec!["recent", "batch"];
    server.hello_recent_max = Some((0, 0));
    server.recent_events = recent_fixture(7);
    let mut client = WbfClient::new(&mut server);
    client.hello("test").await.unwrap();
    let summary = client
        .recent_sync(
            None,
            RecentPlan {
                max_events: None,
                window: 9999,
                batch: Some(9999),
            },
            &mut |_, _| Ok(()),
        )
        .await
        .unwrap();
    assert_eq!((summary.windows, summary.events), (1, 7));
}

/// `Recent` 走一請求一回應的通道（HTTP）：server 回 `Error(Unsupported)`，client 回 `Server`。
#[tokio::test]
async fn recent_over_a_single_response_channel_is_unsupported() {
    let mut server = FakeServer::new();
    server.extra_features = vec!["recent", "batch"];
    let mut client = WbfClient::new(&mut server);
    client.hello("test").await.unwrap();
    // 直接打 request()（不是 request_stream）模擬 HTTP：fake 的 handle 對 Recent 回 Unsupported。
    let pack = wbf_sdk::protocol::recent(
        &wbf_sdk::protocol::RecentRequest {
            rooms: None,
            limit: 10,
            cg_seq: None,
            before: None,
            batch: None,
        },
        7,
        0,
    );
    let response = server.request(pack.clone()).await.unwrap();
    match wbf_sdk::protocol::expect_batch(&pack, response, 0) {
        Err(SdkError::Server { code, .. }) => assert_eq!(code, "Unsupported"),
        other => panic!("{other:?}"),
    }
}

/// 上傳到第一塊時被 server 回一次指定的錯誤，回傳 `send_chunks` 的結果與送過幾次 `Chunk`。
async fn upload_with_first_chunk_rejected(
    code: &'static str,
    code_id: Option<u64>,
) -> (Result<(), SdkError>, usize) {
    let mut server = FakeServer::new();
    let plaintext = sample(3_000);
    let file_cipher = FileCipher::with_fixed(Cipher::None, [0; 32], [0; 8], 1000);
    let state = {
        let mut client = WbfClient::new(&mut server);
        client
            .create_upload(SERVER, USER, &file_cipher, &block_for("x", Some(3_000)))
            .await
            .unwrap()
    };
    server.reject_next_chunk_with = Some((code, code_id));
    server.requests.clear();
    let result = {
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
            .map(|_| ())
    };
    let chunks_sent = server
        .requests
        .iter()
        .filter(|(kind, subtype, _)| {
            *kind == wbf_wire::Kind::Upload && *subtype == wbf_wire::pack::upload::CHUNK
        })
        .count();
    (result, chunks_sent)
}

/// `Corrupt`（1002）重送一次就好（server 表：再來一次就是編碼端的 bug）。
#[tokio::test]
async fn a_chunk_rejected_as_corrupt_is_resent_once() {
    let (result, chunks_sent) = upload_with_first_chunk_rejected("Corrupt", Some(1002)).await;
    result.expect("重送之後要傳完");
    assert_eq!(chunks_sent, 4, "3 塊 ＋ 被拒的那塊重送一次");
}

/// 🚨 **只有名字叫 `Corrupt`、沒有合法 `code_id` 的，不是 wbf 的 Corrupt**（issue #29 第 2 項）：
/// 🚫 不重送，原樣往上報。`SdkError::Server` 也裝 Matrix 的 errcode 與我們合成的碼，比名字等於賭它們不撞名。
#[tokio::test]
async fn an_error_merely_named_corrupt_is_not_resent() {
    let (result, chunks_sent) = upload_with_first_chunk_rejected("Corrupt", None).await;
    let error = result.expect_err("不認得就是失敗");
    assert_eq!(error.wbf_code(), None, "code_id 是 0 ＝沒有");
    assert_eq!(chunks_sent, 1, "🚫 不准重送");
}

/// ⭐ **序號是權威、名字只給人看**：名字寫的是別的，序號是 1002，就是 Corrupt。
#[tokio::test]
async fn the_code_id_decides_even_when_the_name_says_something_else() {
    let (result, chunks_sent) =
        upload_with_first_chunk_rejected("RenamedForHumans", Some(1002)).await;
    result.expect("序號說 Corrupt，就照 Corrupt 重送");
    assert_eq!(chunks_sent, 4);
}

/// 🚨 **不認得的碼：不重試、往上報**（server 表的規則）。🚫 不照序號範圍猜 —— 1599 在「狀態」那一家，但不是任何碼。
#[tokio::test]
async fn an_unknown_code_id_is_reported_not_retried() {
    let (result, chunks_sent) = upload_with_first_chunk_rejected("SomethingNew", Some(1599)).await;
    let error = result.expect_err("不認得就是失敗");
    assert_eq!(error.wbf_code(), None);
    assert!(
        error.to_string().contains("(1599)"),
        "不認得的碼要原樣留在 log 裡：{error}"
    );
    assert_eq!(chunks_sent, 1, "🚫 不准重試");
}

/// 🚨 **位元組上限切短的窗：`tc < limit` 但 `more: true` —— 要接著問，🚫 不准宣告追平**
/// （wbfuwunel 窗的位元組上限，2026-09-14 合併）。
///
/// 舊規則「`tc < limit` ＝沒有更多」在這裡會停在第一窗、`caught_up = true`，而呼叫端接著把
/// 水位存成第一窗的 `fs` —— 比水位舊、還沒拿到的那些事件就**永遠不會再被問**。
#[tokio::test]
async fn a_window_cut_short_by_bytes_is_followed_not_taken_as_caught_up() {
    let mut server = FakeServer::new();
    server.extra_features = vec!["recent", "batch"];
    server.recent_events = recent_fixture(25); // g_seq 1001..=1025
    server.window_cap_by_bytes = Some(7); // limit 是 10，但位元組只放得下 7 則
    let mut client = WbfClient::new(&mut server);
    client.hello("test").await.unwrap();
    let mut seen = 0usize;
    let summary = client
        .recent_sync(
            None,
            RecentPlan {
                max_events: None,
                window: 10,
                batch: None,
            },
            &mut |_, events| {
                seen += events.len();
                Ok(())
            },
        )
        .await
        .unwrap();
    assert_eq!(seen, 25, "每一則都要拿到");
    assert_eq!(summary.events, 25);
    assert_eq!(
        summary.windows, 4,
        "7 ＋ 7 ＋ 7 ＋ 4（最後一窗 4 < 7，more: false）"
    );
    assert!(summary.caught_up);
    assert_eq!(summary.new_cg_seq, Some(1025));
}

/// 舊 server 不帶 `more`：當成 `true`，**多問一趟**，拿到空窗才算追平 —— 🚫 不假設拿完了。
///
/// ⚠️ 空窗能當結束，是靠 server 的保證「第一則一定收進窗」：停在上限的窗不可能是空的。
#[tokio::test]
async fn a_batch_without_more_is_taken_as_more_and_costs_one_extra_window() {
    let mut server = FakeServer::new();
    server.extra_features = vec!["recent", "batch"];
    server.recent_events = recent_fixture(3);
    server.omit_more = true;
    let mut client = WbfClient::new(&mut server);
    client.hello("test").await.unwrap();
    let summary = client
        .recent_sync(
            None,
            RecentPlan {
                max_events: None,
                window: 10,
                batch: None,
            },
            &mut |_, _| Ok(()),
        )
        .await
        .unwrap();
    assert_eq!(
        summary.windows, 2,
        "第一窗 3 則（沒說 more → 當 true）、第二窗空 → 追平"
    );
    assert_eq!(summary.events, 3);
    assert!(summary.caught_up);
    assert_eq!(summary.new_cg_seq, Some(1003), "水位還是第一窗的 fs");
}
