# wbf-client v1 規劃：SDK crate 加 CLI，UI 之後

> 狀態：**維護者 2026-09-04 同意**，開始實作。程式碼一律開分支送 PR 審查，不直接合到 `main`（維護者 2026-09-04 定）。
> 進度：§6 第 1 步 `wbf-wire` 做完（PR #1，2026-09-04 合併）。client 約定規格書草案在 `wbf-client-convention-for-chunk.md`。維護者當日的決定：**先不做 UI，先做 SDK crate 加 CLI**；
> 一個 repo（`crates/` 加 `apps/`）；v1 範圍是最小可用（登入、房間列表、收發文字、分塊上傳／下載媒體）；
> matrix-rust-sdk 先用上游，需要改再 fork。
>
> server 端的權威：[wbfuwunel](http://ai.zooy.cc:30008/amaid/wbfuwunel) 的
> `docs/design/chunked-upload-spec.md`（線上規格）與 `docs/design/wbf-vectors.json`（黃金向量）。
> 本 repo **不共用 server 的程式碼**，共用的是規格與向量（維護者 2026-09-04 定）。
> **第一步只放設計文件與上游 matrix-rust-sdk 的 submodule，程式碼先不寫**（維護者 2026-09-04）。

## 1. 這個 repo 是什麼

wbfuwunel 的 client 端：一個可以拿來寫 desktop、Android、CLI 的 Rust SDK，加上第一個用它的程式（CLI）。
UI 框架的選擇延後到 SDK 能用之後。

## 2. 佈局

```
wbf-client/
  Cargo.toml                 workspace（之後）
  docs/design/               規劃與決定（本檔起）
  vendor/matrix-rust-sdk/    上游 submodule（維護者選 submodule：要改就在這裡改、這裡 fork）
  crates/
    wbf-wire/                協議 codec：pack、EncryptedFileInfo、CRC-32C；純函數、無 tokio、無 matrix
                             tests/vectors.rs 對著 docs/design/wbf-vectors.json（從 server 複製）跑
    wbf-sdk/                 用得上的東西：WebSocket 通道、pack 收發管線、分塊上傳／下載、每塊 AEAD、續傳、
                             與 matrix-sdk 的接縫（登入、房間、事件、金鑰）
  apps/
    wbf-cli/                 命令列：login、rooms、send、recv、upload、download、play（seek 驗證）
    desktop/                 之後
```

`wbf-wire` 與 `wbf-sdk` 分開的理由：codec 沒有 async、沒有 matrix，Android 的 Rust 核心與任何測試都能單獨拿它；
`wbf-sdk` 才拖 tokio 與 matrix-sdk。

## 3. 依賴

| 用途 | crate | 備註 |
|---|---|---|
| Matrix 基礎：登入、sync、房間、E2EE | `matrix-sdk`，`vendor/matrix-rust-sdk` submodule（上游 `matrix-org/matrix-rust-sdk`，目前 crates.io 是 0.18） | 維護者 2026-09-04 定：submodule，不是 git dependency。先指上游；要改就把 submodule 改指自己的 fork。workspace 用 path dependency 指進去 |
| async | `tokio` | |
| WebSocket | `tokio-tungstenite` | `GET /_wbf/v1/ws`，一個 binary message 一個 pack |
| CRC | `crc32c` | 與 server 同一個 crate、同一組向量 |
| 每塊加密 | `chacha20poly1305`、`aes-gcm` | 二選一，見 wbf-client-convention-for-chunk.md §3；`nonce_i = nonce_base ‖ i` |
| JSON meta | `serde_json` | Ack／Error／Info 的 meta |
| HTTP 備援 | `reqwest`（matrix-sdk 已帶） | `POST /_wbf/v1/pack`，測試與腳本用 |

## 4. v1 範圍與驗收

| 功能 | 靠什麼 | 驗收（對本機 wbfuwunel 跑） |
|---|---|---|
| 登入、房間列表 | matrix-sdk | `wbf-cli login`、`wbf-cli rooms` 印出房間 |
| 收發文字（含 E2EE 房） | matrix-sdk | 兩個帳號互發，收到且解得開 |
| 分塊上傳 | wbf-sdk | `wbf-cli upload <file>`：Create（`EncryptedFileInfo` 加加密描述）→ 逐塊加密送 → Seal → 把 `wbf.chunked` 事件送進房間；server 的標準下載拿到的密文與本地密文逐 byte 相同 |
| 分塊下載 | wbf-sdk | `wbf-cli download <mxc>`：Info 拿描述解出金鑰 → 逐塊 Read 解密 → 與原檔逐 byte 相同 |
| seek | wbf-sdk | `wbf-cli play <mxc> --at <明文位置>`：只讀含該位置的那一塊就能解出對的 bytes（核心設計的驗收：大於 1 GB 的檔中途 seek 不必下載前面） |
| 續傳 | wbf-sdk | 上傳中殺掉 CLI，重跑同一命令從 `Status` 接著送，結果逐 byte 相同 |
| 串流上傳 | wbf-sdk | `wbf-cli upload --stream` 從 stdin 讀、`0/0` 哨兵、`IS_LAST` 收尾、Seal 帶最終描述 |
| 協議不漂移 | wbf-wire | `cargo test -p wbf-wire` 對著複製來的 `wbf-vectors.json` 全過 |

## 5. 事件格式與 client 之間的約定

由 [wbf-client-convention-for-chunk.md](wbf-client-convention-for-chunk.md) 定：每塊怎麼加密、描述長什麼樣、串流怎麼收尾、房間事件怎麼放、seek 怎麼算。
server 不讀那些內容。原本這裡寫的 `m.file` 加 `wbf.chunked` 作廢：`wbf.` 違反 Matrix 的反向網域命名慣例，
而規格的 `file` 欄位語意是 AES-CTR，放 ChaCha20 的參數進去是說謊。

## 6. 順序

0. repo 只有設計文件與 `vendor/matrix-rust-sdk` submodule，等維護者同意規劃。（2026-09-04 同意）
1. `wbf-wire` 加向量測試。（2026-09-04 做完）向量檔是 `docs/design/wbf-vectors.json`，從 server repo 的同名檔**整份複製**，不手改；
   server 規格改了就重新複製一次，`cargo test -p wbf-wire` 紅了就是漂移。工具鏈釘在 `rust-toolchain.toml`（1.95.0，與 submodule 的 `rust-version` 一致）。
2. `wbf-sdk` 的通道與上傳／下載（不接 matrix-sdk；`login` 用純 HTTP 打 `/_matrix/client/v3/login` 拿 token），CLI 的 `login`／`upload`／`download`／`play`／續傳。CLI 介面見 [wbf-cli-spec.md](wbf-cli-spec.md)。
3. 接 matrix-sdk：登入、房間、事件、金鑰分發；CLI 的 `login`／`rooms`／`send`／`recv`。
4. UI 框架決定與 `apps/desktop`。

## 7. 明確不在 v1

- UI。
- Android 打包（但 `wbf-wire`／`wbf-sdk` 不能用到 Android 上沒有的東西）。
- 邊上傳邊下載（server 還沒有推送）。
- 縮圖（密文，做不到）。

## 8. 要維護者決定的

1. 這份規劃可以嗎？
2. Forgejo 上建 `wbf-client` repo（我的 token 只有 wbfuwunel），並給 `claude` 帳號 write 權限；GitHub 的 `wbftw` 那邊要不要也建一份。
3. 事件格式：見 `wbf-client-convention-for-chunk.md` §11。
