//! 媒體快取整條路（/docs/design/media/media-download.md、/docs/design/media/media-pool.md §5）對著記憶體版 server 跑：
//! `MediaDownload` 下載進池、去重、從檔案續傳、seek 暫存檔、sha256 不符；配額清理、掃孤兒。要 `--features cache`（SQLCipher）。
//!
//! 排隊、取消、進度推播在 core（`wbf_core::download_queue` 的測試）；這裡只測一個檔在主檔、暫存檔與網路之間怎麼拿塊。
#![cfg(feature = "cache")]

mod support;

use std::collections::HashSet;
use std::io::{Cursor, Read, Write};
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use support::fake_server::FakeServer;
use wbf_sdk::cache::{Cache, CacheIdentity, MediaDescription, MediaEntry};
use wbf_sdk::channel::PackChannel;
use wbf_sdk::media::{self, MediaDownload};
use wbf_sdk::media_pool::{MediaPool, SEGMENT_SIZE};
use wbf_sdk::{ChunkedBlock, Cipher, FileCipher, Key32, Manifest, SdkError, WbfClient};
use wbf_wire::pack::download;
use wbf_wire::Kind;

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
    upload_with_chunk(server, name, plaintext, CHUNK).await
}

async fn upload_with_chunk(
    server: &mut FakeServer,
    name: &str,
    plaintext: &[u8],
    chunk_size: u32,
) -> Manifest {
    let mut client = WbfClient::new(&mut *server);
    let file_cipher = FileCipher::with_fixed(Cipher::ChaCha20Poly1305, [7; 32], [9; 8], chunk_size);
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

fn reads_so_far(server: &FakeServer) -> usize {
    server
        .requests
        .iter()
        .filter(|(kind, subtype, _)| *kind == Kind::Download && *subtype == download::READ)
        .count()
}

fn open_download(cache: &mut Cache, pool: &MediaPool, manifest: &Manifest) -> MediaDownload {
    let description = MediaDescription::of_chunked_block(&manifest.block).unwrap();
    cache.media_begin(&manifest.mxc, &description).unwrap();
    let name = cache.media_pending_name(&manifest.mxc).unwrap().unwrap();
    MediaDownload::open(pool, &name, manifest).unwrap()
}

/// 拿第 `index` 塊的明文：還沒驗過就先 `Info`。core 的下載處理端收到回覆時做的就是這兩步（/docs/design/media/media-download.md §3.2、§3.3），
/// 只是它的請求走線的發送 queue、回覆另外交回來；這裡直接一問一答。
async fn fetch_chunk<C: PackChannel>(
    download: &mut MediaDownload,
    client: &mut WbfClient<C>,
    index: u32,
) -> Result<Vec<u8>, SdkError> {
    let mxc = download.manifest().mxc.clone();
    if download.needs_info() {
        let (info, description) = client.fetch_info(&mxc).await?;
        download.accept_info(&info, &description)?;
    }
    let (_read, data) = client.read_chunk(&mxc, index).await?;
    Ok(download.open_chunk(index, &data)?.to_vec())
}

/// 主檔往前一塊，從 server 拉。
async fn advance_from_server<C: PackChannel>(
    download: &mut MediaDownload,
    client: &mut WbfClient<C>,
) -> Result<(), SdkError> {
    let index = download.next_chunk();
    let plain = fetch_chunk(download, client, index).await?;
    download.land_chunk(index, &plain)
}

/// core 的下載處理端做的那幾步（/docs/design/media/media-download.md §5.4），不含排隊與取消。
async fn download_whole(
    server: &mut FakeServer,
    cache: &mut Cache,
    pool: &MediaPool,
    manifest: &Manifest,
) -> Result<MediaEntry, SdkError> {
    let mut download = open_download(cache, pool, manifest);
    let mut client = WbfClient::new(&mut *server);
    while !download.is_written() {
        let landed = match download.advance_from_seek_store() {
            Ok(true) => Ok(()),
            Ok(false) => advance_from_server(&mut download, &mut client).await,
            Err(error) => Err(error),
        };
        // 壞檔：主檔與暫存檔都刪（core 的下載處理端一樣做）。
        if let Err(error) = landed {
            if matches!(error, SdkError::Integrity(_)) {
                download.discard(pool);
            }
            return Err(error);
        }
    }
    let (finished, verified) = download.finish(pool)?;
    let bytes_on_disk = pool.bytes_on_disk(&finished.hash_hex)?;
    cache.media_finish(
        &manifest.mxc,
        &finished.hash_hex,
        finished.segments,
        finished.plain_len,
        bytes_on_disk,
        verified,
    )?;
    Ok(cache.find_media(&manifest.mxc)?.unwrap())
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
async fn a_download_lands_in_the_pool_and_the_same_content_dedups() {
    let dir = scratch("fetch");
    let (mut cache, pool) = open_cache_and_pool(&dir);
    let mut server = FakeServer::new();
    let plain = sample(CHUNK as usize * 2 + 500, 1);
    let manifest = upload(&mut server, "a.bin", &plain).await;

    let entry = download_whole(&mut server, &mut cache, &pool, &manifest)
        .await
        .unwrap();
    assert_eq!(reads_so_far(&server), 3);
    let pool_file = entry.pool_file.clone().unwrap();
    assert_eq!(pool_file, blake3::hash(&plain).to_hex().to_string());
    assert!(entry.complete);
    assert_eq!(entry.segments_written, 3);
    // upload 時算了 sha256（summary.sha256 進區塊），所以 hash 是上傳者的 sha256，不是 blake3 補的。
    let expected_sha = {
        use sha2::Digest;
        format!("sha256:{}", hex::encode(sha2::Sha256::digest(&plain)))
    };
    assert_eq!(entry.hash.as_deref(), Some(expected_sha.as_str()));
    assert_eq!(read_pool(&pool, &pool_file), plain);
    assert!(pool.list_pending().unwrap().is_empty());
    assert!(media::open_complete(&pool, &entry).is_some());

    // 同內容另一個 mxc：池裡只有一份，兩列 media 指同一個 pool_file。
    let manifest2 = upload(&mut server, "b.bin", &plain).await;
    let second = download_whole(&mut server, &mut cache, &pool, &manifest2)
        .await
        .unwrap();
    assert_eq!(second.pool_file.as_deref(), Some(pool_file.as_str()));
    assert_eq!(cache.media_references(&pool_file).unwrap(), 2);
    assert_eq!(cache.media_bytes_on_disk().unwrap(), entry.bytes_on_disk);
    // 列說的長度跟檔不一樣：不算完整。
    let mut wrong = entry.clone();
    wrong.file_size = Some(1);
    assert!(media::open_complete(&pool, &wrong).is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_download_resumes_from_the_whole_segments_in_the_file() {
    let dir = scratch("resume");
    let (mut cache, pool) = open_cache_and_pool(&dir);
    let mut server = FakeServer::new();
    // 塊跟段不對齊：40 KiB 的塊、64 KiB 的段。
    let chunk = 40960u32;
    let plain = sample(chunk as usize * 9 + 100, 2);
    let manifest = upload_with_chunk(&mut server, "r.bin", &plain, chunk).await;

    // 第一次拉 5 塊（200 KiB ＝ 3 個完整段 ＋ 8 KiB 在記憶體）就斷：只有完整的段在檔裡。
    let mut download = open_download(&mut cache, &pool, &manifest);
    {
        let mut client = WbfClient::new(&mut server);
        for _ in 0..5 {
            advance_from_server(&mut download, &mut client)
                .await
                .unwrap();
        }
    }
    assert_eq!(download.segments_written(), 3);
    drop(download);
    assert_eq!(reads_so_far(&server), 5);

    // 重開：檔案本身就是進度，從涵蓋第 3 段結尾的那一塊（第 4 塊）接著拉，那塊前面的部分丟掉。
    let download = open_download(&mut cache, &pool, &manifest);
    assert_eq!(download.next_chunk(), 3 * SEGMENT_SIZE / chunk);
    drop(download);
    let entry = download_whole(&mut server, &mut cache, &pool, &manifest)
        .await
        .unwrap();
    assert_eq!(reads_so_far(&server), 5 + (10 - 4));
    assert_eq!(read_pool(&pool, entry.pool_file.as_deref().unwrap()), plain);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn the_main_file_takes_a_seek_fetched_chunk_without_the_network() {
    let dir = scratch("seek");
    let (mut cache, pool) = open_cache_and_pool(&dir);
    let mut server = FakeServer::new();
    let plain = sample(CHUNK as usize * 4 + 7, 3);
    let manifest = upload(&mut server, "v.mkv", &plain).await;

    // 播放器 seek 到第 2 塊：現拉、存進 seek 暫存檔。
    let mut download = open_download(&mut cache, &pool, &manifest);
    let fetched = {
        let mut client = WbfClient::new(&mut server);
        let plain = fetch_chunk(&mut download, &mut client, 2).await.unwrap();
        download.store_seek_chunk(&pool, 2, &plain).unwrap();
        plain
    };
    assert_eq!(&fetched[..], &plain[CHUNK as usize * 2..CHUNK as usize * 3]);
    assert_eq!(reads_so_far(&server), 1);
    assert!(download.read_local_chunk(2).unwrap().is_some());
    assert!(download.read_local_chunk(1).unwrap().is_none());
    // daemon 重開：暫存檔留著，位置表重建。
    drop(download);
    let mut download = open_download(&mut cache, &pool, &manifest);
    assert_eq!(
        &download.read_local_chunk(2).unwrap().unwrap()[..],
        &plain[CHUNK as usize * 2..CHUNK as usize * 3]
    );
    // 主檔順序拉：到第 2 塊時從暫存檔搬、不走網路。
    {
        let mut client = WbfClient::new(&mut server);
        while !download.is_written() {
            if !download.advance_from_seek_store().unwrap() {
                advance_from_server(&mut download, &mut client)
                    .await
                    .unwrap();
            }
            // 主檔已封的段讀得回來。
            if download.segments_written() >= 1 {
                assert_eq!(
                    &download.read_local_chunk(0).unwrap().unwrap()[..],
                    &plain[..CHUNK as usize]
                );
            }
        }
    }
    assert_eq!(
        reads_so_far(&server),
        1 + 4,
        "chunk 2 came from the seek file"
    );
    let (finished, verified) = download.finish(&pool).unwrap();
    assert_eq!(
        verified,
        wbf_sdk::media_kind::Verification::Matched,
        "the block's sha256 matches the whole file"
    );
    assert!(
        !pool.seek_path("m1").exists(),
        "the seek file goes when the main file is done"
    );
    assert_eq!(read_pool(&pool, &finished.hash_hex), plain);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_block_the_server_does_not_confirm_discards_the_pending_files() {
    let dir = scratch("sha");
    let (mut cache, pool) = open_cache_and_pool(&dir);
    let mut server = FakeServer::new();
    let plain = sample(CHUNK as usize + 3, 4);
    let mut manifest = upload(&mut server, "x.bin", &plain).await;
    manifest.block.sha256 = Some("00".repeat(32));
    let error = download_whole(&mut server, &mut cache, &pool, &manifest)
        .await
        .unwrap_err();
    assert!(matches!(error, SdkError::Integrity(_)), "{error}");
    assert!(pool.list_pending().unwrap().is_empty());
    assert!(pool.list_files().unwrap().is_empty());
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
        let entry = download_whole(&mut server, &mut cache, &pool, &manifest)
            .await
            .unwrap();
        pool_files.push((manifest.mxc.clone(), entry.pool_file.unwrap()));
    }
    // 同內容的第五個 mxc 指到第 0 個檔。
    let plain0 = sample(CHUNK as usize, 10);
    let manifest_dup = upload(&mut server, "dup.bin", &plain0).await;
    download_whole(&mut server, &mut cache, &pool, &manifest_dup)
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

/// 🚨 有把手開著的檔不刪、列也不動（/docs/design/media/media-pool.md §5）；把手關了，下一輪照常收。
#[tokio::test]
async fn garbage_collection_leaves_a_file_that_is_being_read() {
    let dir = scratch("gc-open");
    let (mut cache, pool) = open_cache_and_pool(&dir);
    let mut server = FakeServer::new();
    let plain = sample(CHUNK as usize + 3, 40);
    let manifest = upload(&mut server, "open.bin", &plain).await;
    let entry = download_whole(&mut server, &mut cache, &pool, &manifest)
        .await
        .unwrap();
    let pool_file = entry.pool_file.unwrap();
    let now: i64 = 10_000_000_000_000;
    set_last_used(&mut cache, &manifest.mxc, now - 30 * 24 * 3600 * 1000);

    let reader = pool.open_read(&pool_file).unwrap();
    assert!(pool.is_open(&pool_file).unwrap());
    let report = media::collect_garbage(
        &mut cache,
        &pool,
        0,
        Duration::from_secs(7 * 24 * 3600),
        now,
    )
    .unwrap();
    assert_eq!(report.files_removed, 0, "{report:?}");
    assert_eq!(report.files_in_use, 1, "{report:?}");
    assert!(
        cache.find_media(&manifest.mxc).unwrap().unwrap().complete,
        "列也不動"
    );
    let swept = media::sweep(
        &mut cache,
        &pool,
        Duration::from_secs(7 * 24 * 3600),
        SystemTime::now(),
        &HashSet::new(),
    )
    .unwrap();
    assert_eq!(swept.removed_orphan_files, 0, "{swept:?}");

    drop(reader);
    assert!(!pool.is_open(&pool_file).unwrap());
    let report = media::collect_garbage(
        &mut cache,
        &pool,
        0,
        Duration::from_secs(7 * 24 * 3600),
        now,
    )
    .unwrap();
    assert_eq!(report.files_removed, 1, "把手關了就照常刪：{report:?}");
    assert!(pool.open_read(&pool_file).is_err());
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
async fn sweep_resets_missing_files_and_removes_what_nobody_claims() {
    let dir = scratch("sweep");
    let (mut cache, pool) = open_cache_and_pool(&dir);
    let mut server = FakeServer::new();
    let plain = sample(CHUNK as usize, 42);
    let manifest = upload(&mut server, "s.bin", &plain).await;
    let entry = download_whole(&mut server, &mut cache, &pool, &manifest)
        .await
        .unwrap();
    let pool_file = entry.pool_file.unwrap();
    // 有人手動刪了池檔；pending/ 裡有沒人認領的主檔與 seek 暫存檔。
    pool.remove(&pool_file).unwrap();
    std::fs::write(pool.pending_path("m999"), b"junk").unwrap();
    std::fs::write(pool.seek_path("m998"), b"junk").unwrap();
    // 還有一個沒人指的完成檔（forget-account 之後會留下這種）。
    let mut orphan = pool.create_pending("orphan", "mxc://fake/orphan").unwrap();
    orphan.write_all(b"nobody points at me").unwrap();
    let orphan_hash = orphan.finish().unwrap().hash_hex;
    pool.adopt("orphan", &orphan_hash).unwrap();
    // 一個認領了、正在下載的主檔（下載處理端握著）與一個認領了、但還是池格式 v1 的主檔。
    let busy = upload(&mut server, "busy.bin", &sample(CHUNK as usize * 2, 43)).await;
    let mut busy_download = open_download(&mut cache, &pool, &busy);
    {
        let mut client = WbfClient::new(&mut server);
        advance_from_server(&mut busy_download, &mut client)
            .await
            .unwrap();
    }
    let busy_name = cache.media_pending_name(&busy.mxc).unwrap().unwrap();
    let old = upload(&mut server, "old.bin", &sample(CHUNK as usize, 44)).await;
    drop(open_download(&mut cache, &pool, &old));
    let old_name = cache.media_pending_name(&old.mxc).unwrap().unwrap();
    let mut bytes = std::fs::read(pool.pending_path(&old_name)).unwrap();
    bytes[4] = 1;
    std::fs::write(pool.pending_path(&old_name), &bytes).unwrap();

    let in_use: HashSet<String> = [busy_name.clone()].into();
    let swept = media::sweep(
        &mut cache,
        &pool,
        Duration::from_secs(7 * 24 * 3600),
        SystemTime::now(),
        &in_use,
    )
    .unwrap();
    assert_eq!(
        (
            swept.reset_rows,
            swept.removed_pending,
            swept.removed_orphan_files
        ),
        (1, 3, 1),
        "{swept:?}"
    );
    assert!(pool.open_read(&orphan_hash).is_err());
    assert!(!cache.find_media(&manifest.mxc).unwrap().unwrap().complete);
    assert_eq!(pool.list_pending().unwrap(), vec![busy_name.clone()]);

    // 過了保護期：還在下載的（下載處理端握著）照樣不碰——線斷了很久的下載，檔案很久沒動，但它還是活的。
    let later = SystemTime::now() + Duration::from_secs(8 * 24 * 3600);
    let swept = media::sweep(
        &mut cache,
        &pool,
        Duration::from_secs(7 * 24 * 3600),
        later,
        &in_use,
    )
    .unwrap();
    assert_eq!(swept.removed_pending, 0, "{swept:?}");
    assert_eq!(pool.list_pending().unwrap(), vec![busy_name.clone()]);
    // 沒人在下載它了：過了保護期的半成品刪掉。
    drop(busy_download);
    let swept = media::sweep(
        &mut cache,
        &pool,
        Duration::from_secs(7 * 24 * 3600),
        later,
        &HashSet::new(),
    )
    .unwrap();
    assert_eq!(swept.removed_pending, 1, "{swept:?}");
    assert!(pool.list_pending().unwrap().is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}
