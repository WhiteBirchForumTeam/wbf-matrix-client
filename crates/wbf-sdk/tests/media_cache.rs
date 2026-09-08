//! 媒體快取整條路（local-cache-db.md §8.3、§8.5、§8.7）對著記憶體版 server 跑：`media::fetch` 命中／下載／續傳、
//! 配額清理、啟動掃孤兒。要 `--features cache`（SQLCipher）。
#![cfg(feature = "cache")]

mod support;

use std::io::{Cursor, Read, Write};
use std::path::PathBuf;
use std::time::Duration;

use support::fake_server::FakeServer;
use wbf_sdk::cache::{Cache, CacheIdentity};
use wbf_sdk::media::{self, FetchOutcome};
use wbf_sdk::media_pool::MediaPool;
use wbf_sdk::{ChunkedBlock, Cipher, FileCipher, Key32, Manifest, WbfClient};

const SERVER: &str = "http://fake";
const USER: &str = "@alice:fake";
const CHUNK: u32 = 65536;

fn sample(len: usize, seed: usize) -> Vec<u8> {
    (0..len)
        .map(|position| ((position * 7919 + seed) % 251) as u8)
        .collect()
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("wbf-media-cache-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn open_cache_and_pool(dir: &std::path::Path) -> (Cache, MediaPool) {
    let (cache, _) = Cache::open(
        dir,
        &Key32([1u8; 32]),
        &CacheIdentity {
            server: SERVER.into(),
        },
    )
    .unwrap();
    let pool = MediaPool::open(dir, Key32([2u8; 32])).unwrap();
    (cache, pool)
}

async fn upload(server: &mut FakeServer, name: &str, plaintext: &[u8]) -> Manifest {
    let mut client = WbfClient::new(&mut *server);
    let file_cipher = FileCipher::with_fixed(Cipher::ChaCha20Poly1305, [7; 32], [9; 8], CHUNK);
    let block = ChunkedBlock {
        v: 1,
        cipher: Cipher::None,
        key: None,
        nonce_base: None,
        chunk_size: 0,
        file_size: Some(plaintext.len() as u64),
        name: Some(name.to_string()),
        mimetype: Some("application/octet-stream".to_string()),
        sha256: None,
    };
    let state = client
        .create_upload(SERVER, USER, &file_cipher, &block)
        .await
        .expect("create");
    let summary = client
        .send_chunks(&state, &mut Cursor::new(plaintext), 0, true, &mut |_, _| {})
        .await
        .expect("send");
    let mut final_block = state.block.clone();
    final_block.sha256 = summary.sha256;
    client
        .seal_upload(&state, &final_block)
        .await
        .expect("seal")
}

fn read_pool(pool: &MediaPool, pool_file: &str) -> Vec<u8> {
    let mut out = Vec::new();
    pool.open_read(pool_file)
        .unwrap()
        .read_to_end(&mut out)
        .unwrap();
    out
}

#[tokio::test]
async fn fetch_downloads_then_hits_cache_and_dedups_same_content() {
    let dir = scratch("fetch");
    let (mut cache, pool) = open_cache_and_pool(&dir);
    let mut server = FakeServer::new();
    let plain = sample(CHUNK as usize * 2 + 500, 1);
    let manifest = upload(&mut server, "a.bin", &plain).await;

    let mut client = WbfClient::new(&mut server);
    let mut progress = Vec::new();
    let fetched = media::fetch(
        &mut client,
        &manifest,
        &mut cache,
        &pool,
        &mut |done, total| progress.push((done, total)),
    )
    .await
    .unwrap();
    assert_eq!(
        fetched.outcome,
        FetchOutcome::Downloaded {
            chunks: 3,
            resumed_from: 0
        }
    );
    assert_eq!(progress, vec![(1, 3), (2, 3), (3, 3)]);
    let pool_file = fetched.entry.pool_file.clone().unwrap();
    assert_eq!(pool_file, blake3::hash(&plain).to_hex().to_string());
    assert!(fetched.entry.complete);
    assert_eq!(read_pool(&pool, &pool_file), plain);
    assert!(pool.list_pending().unwrap().is_empty());

    // 第二次：命中，不碰 server。
    let mut dead = FakeServer::new();
    let mut client = WbfClient::new(&mut dead);
    let again = media::fetch(&mut client, &manifest, &mut cache, &pool, &mut |_, _| {})
        .await
        .unwrap();
    assert_eq!(again.outcome, FetchOutcome::CacheHit);
    assert!(again.entry.last_used_at >= fetched.entry.last_used_at);

    // 同內容另一個 mxc：池裡只有一份，兩列 media 指同一個 pool_file。
    let manifest2 = upload(&mut server, "b.bin", &plain).await;
    let mut client = WbfClient::new(&mut server);
    let second = media::fetch(&mut client, &manifest2, &mut cache, &pool, &mut |_, _| {})
        .await
        .unwrap();
    assert_eq!(second.entry.pool_file.as_deref(), Some(pool_file.as_str()));
    assert_eq!(cache.media_references(&pool_file).unwrap(), 2);
    assert_eq!(cache.count_rows("media").unwrap(), 2);
    assert_eq!(
        cache.media_bytes_on_disk().unwrap(),
        fetched.entry.bytes_on_disk
    );
    // 一個檔在磁碟上（前 2 hex 扇出）。
    let files: Vec<_> = walkdir(&pool.dir().to_path_buf());
    assert_eq!(files.iter().filter(|p| p.ends_with(&pool_file)).count(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

fn walkdir(dir: &PathBuf) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                out.extend(walkdir(&path));
            } else {
                out.push(path);
            }
        }
    }
    out
}

#[tokio::test]
async fn fetch_resumes_from_the_last_snapshot() {
    let dir = scratch("resume");
    let (mut cache, pool) = open_cache_and_pool(&dir);
    let mut server = FakeServer::new();
    let plain = sample(CHUNK as usize * 5 + 100, 2);
    let manifest = upload(&mut server, "r.bin", &plain).await;

    // 模擬「上次下到第 3 塊、快照寫了 3、然後死掉」：直接用池與 DB 的 API 造出那個狀態。
    let entry = cache
        .media_begin(
            &manifest.mxc,
            Some("r.bin"),
            None,
            plain.len() as u64,
            CHUNK,
        )
        .unwrap();
    assert!(!entry.complete);
    let pending = cache.media_pending_name(&manifest.mxc).unwrap().unwrap();
    let mut writer = pool.create_pending(&pending).unwrap();
    writer
        .write_all(&plain[..CHUNK as usize * 3 + 1234])
        .unwrap(); // 多寫的 1234 byte 是快照之後的，不可信
    writer.sync().unwrap();
    drop(writer);
    cache.media_progress(&manifest.mxc, 3, CHUNK).unwrap();

    let mut client = WbfClient::new(&mut server);
    let mut progress = Vec::new();
    let fetched = media::fetch(
        &mut client,
        &manifest,
        &mut cache,
        &pool,
        &mut |done, total| progress.push((done, total)),
    )
    .await
    .unwrap();
    assert_eq!(
        fetched.outcome,
        FetchOutcome::Downloaded {
            chunks: 3,
            resumed_from: 3
        }
    );
    assert_eq!(progress, vec![(4, 6), (5, 6), (6, 6)]);
    let pool_file = fetched.entry.pool_file.clone().unwrap();
    assert_eq!(pool_file, blake3::hash(&plain).to_hex().to_string());
    assert_eq!(read_pool(&pool, &pool_file), plain);

    // 塊大小不同的舊快照不能續：從頭。
    let manifest_b = upload(&mut server, "s.bin", &sample(CHUNK as usize * 2, 3)).await;
    cache
        .media_begin(&manifest_b.mxc, None, None, CHUNK as u64 * 2, 4096)
        .unwrap();
    cache.media_progress(&manifest_b.mxc, 5, 4096).unwrap();
    let mut client = WbfClient::new(&mut server);
    let fetched_b = media::fetch(&mut client, &manifest_b, &mut cache, &pool, &mut |_, _| {})
        .await
        .unwrap();
    assert_eq!(
        fetched_b.outcome,
        FetchOutcome::Downloaded {
            chunks: 2,
            resumed_from: 0
        }
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn garbage_collection_respects_quota_protection_and_shared_files() {
    let dir = scratch("gc");
    let (mut cache, pool) = open_cache_and_pool(&dir);
    let mut server = FakeServer::new();
    let mut pool_files = Vec::new();
    for seed in 0..4 {
        let plain = sample(CHUNK as usize + seed, 10 + seed);
        let manifest = upload(&mut server, &format!("f{seed}.bin"), &plain).await;
        let mut client = WbfClient::new(&mut server);
        let fetched = media::fetch(&mut client, &manifest, &mut cache, &pool, &mut |_, _| {})
            .await
            .unwrap();
        pool_files.push((manifest.mxc.clone(), fetched.entry.pool_file.unwrap()));
    }
    // 同內容的第五個 mxc 指到第 0 個檔。
    let plain0 = sample(CHUNK as usize, 10);
    let manifest_dup = upload(&mut server, "dup.bin", &plain0).await;
    let mut client = WbfClient::new(&mut server);
    media::fetch(
        &mut client,
        &manifest_dup,
        &mut cache,
        &pool,
        &mut |_, _| {},
    )
    .await
    .unwrap();
    assert_eq!(cache.media_references(&pool_files[0].1).unwrap(), 2);

    let total = cache.media_bytes_on_disk().unwrap();
    let per_file = total / 4;
    let now: i64 = 10_000_000_000_000;
    // 把 last_used_at 排成：f0（含 dup）最舊、f1、f2 舊到保護期外，f3 在保護期內。
    let day = 24 * 3600 * 1000;
    let stamps = [now - 30 * day, now - 20 * day, now - 10 * day, now - day];
    for ((mxc, _), stamp) in pool_files.iter().zip(stamps) {
        set_last_used(&mut cache, mxc, stamp);
    }
    set_last_used(&mut cache, &manifest_dup.mxc, now - 30 * day);

    // 配額塞成「只能留 2 個檔」；保護期 7 天。
    let report = media::collect_garbage(
        &mut cache,
        &pool,
        per_file * 2 + per_file / 2,
        Duration::from_secs(7 * 24 * 3600),
        now,
    )
    .unwrap();
    // f0 先被處理：dup 還指著同一個檔 → 只清 f0 的列、檔留著；接著 dup 也出局 → 檔真的刪；再 f1 → 刪。
    assert_eq!(report.files_removed, 2, "{report:?}");
    assert!(!report.still_over_quota, "{report:?}");
    assert!(pool.open_read(&pool_files[0].1).is_err());
    assert!(pool.open_read(&pool_files[1].1).is_err());
    assert!(pool.open_read(&pool_files[2].1).is_ok());
    assert!(pool.open_read(&pool_files[3].1).is_ok());
    assert!(
        !cache
            .find_media(&pool_files[0].0)
            .unwrap()
            .unwrap()
            .complete
    );
    assert!(
        cache
            .find_media(&pool_files[2].0)
            .unwrap()
            .unwrap()
            .complete
    );

    // 配額設成 0 但只剩 f3 在保護期內：不刪、回 still_over_quota。
    let report = media::collect_garbage(
        &mut cache,
        &pool,
        0,
        Duration::from_secs(7 * 24 * 3600),
        now,
    )
    .unwrap();
    assert!(report.still_over_quota);
    assert_eq!(report.files_removed, 1); // f2 在保護期外被刪；f3 留
    assert!(pool.open_read(&pool_files[3].1).is_ok());
    let _ = std::fs::remove_dir_all(&dir);
}

fn set_last_used(cache: &mut Cache, mxc: &str, stamp: i64) {
    // 測試用後門：直接改 last_used_at（正式碼只會 touch 成現在）。
    cache
        .debug_execute(
            "UPDATE media SET last_used_at = ?2 WHERE mxc = ?1",
            &[&mxc.to_string(), &stamp.to_string()],
        )
        .unwrap();
}

#[tokio::test]
async fn sweep_resets_missing_files_and_removes_orphan_pending() {
    let dir = scratch("sweep");
    let (mut cache, pool) = open_cache_and_pool(&dir);
    let mut server = FakeServer::new();
    let plain = sample(CHUNK as usize, 42);
    let manifest = upload(&mut server, "s.bin", &plain).await;
    let mut client = WbfClient::new(&mut server);
    let fetched = media::fetch(&mut client, &manifest, &mut cache, &pool, &mut |_, _| {})
        .await
        .unwrap();
    let pool_file = fetched.entry.pool_file.unwrap();
    // 有人手動刪了池檔；另外 pending/ 裡有個沒人認領的暫存檔。
    pool.remove(&pool_file).unwrap();
    std::fs::write(pool.pending_path("m999"), b"junk").unwrap();
    let (reset_rows, removed_pending) = media::sweep(
        &mut cache,
        &pool,
        Duration::from_secs(7 * 24 * 3600),
        10_000_000_000_000,
    )
    .unwrap();
    assert_eq!((reset_rows, removed_pending), (1, 1));
    assert!(!cache.find_media(&manifest.mxc).unwrap().unwrap().complete);
    assert!(pool.list_pending().unwrap().is_empty());
    // reset 之後再 fetch 會重新下載。
    let mut client = WbfClient::new(&mut server);
    let again = media::fetch(&mut client, &manifest, &mut cache, &pool, &mut |_, _| {})
        .await
        .unwrap();
    assert!(matches!(again.outcome, FetchOutcome::Downloaded { .. }));
    let _ = std::fs::remove_dir_all(&dir);
}
