# wbf-matrix-client v1 規劃：SDK crate 加 CLI，UI 之後

> 狀態：**維護者 2026-09-04 同意**，開始實作。程式碼一律開分支送 PR 審查，不直接合到 `main`（維護者 2026-09-04 定）。
> 進度：§6 第 1 步 `wbf-wire` 做完（PR #1，2026-09-04 合併）。第 2 步拆三個 PR：第 1 個（密碼層與 client 向量，PR #4）2026-09-05 合併，第 2 個（通道與上傳／下載，PR #5）同日合併，第 3 個（`apps/wbf-cli` 與驗收腳本）同日送審。client 約定規格書草案在 `wbf-client-convention-for-chunk.md`。維護者當日的決定：**先不做 UI，先做 SDK crate 加 CLI**；
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
wbf-matrix-client/
  Cargo.toml                 workspace（之後）
  docs/design/               規劃與決定（本檔起）
  vendor/matrix-rust-sdk/    上游 submodule（維護者選 submodule：要改就在這裡改、這裡 fork）
  crates/
    wbf-wire/                協議 codec：pack、EncryptedFileInfo、CRC-32C；純函數、無 tokio、無 matrix
                             tests/vectors.rs 對著 docs/design/wbf-vectors.json（從 server 複製）跑
    wbf-sdk/                 用得上的東西：WebSocket 通道、pack 收發管線、分塊上傳／下載、每塊 AEAD、續傳、
                             與 matrix-sdk 的接縫（登入、房間、事件、金鑰）
  apps/
    wbf-cli/                 命令列：login、rooms、send、watch、upload、download、seek
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
| seek | wbf-sdk | `wbf-cli seek <mxc> --at <明文位置>`：只讀含該位置的那一塊就能解出對的 bytes（核心設計的驗收：大於 1 GB 的檔中途 seek 不必下載前面） |
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
2. `wbf-sdk` 的通道與上傳／下載（不接 matrix-sdk；`login` 用純 HTTP 打 `/_matrix/client/v3/login` 拿 token），CLI 的 `login`／`upload`／`download`／`seek`／續傳。CLI 介面見 [wbf-cli-spec.md](wbf-cli-spec.md)。拆三個 PR：
   1. 密碼層與 client 向量：`cipher`、每塊與描述的 AEAD、事件區塊、seek 算法、`chunk_size` 選法；`docs/design/wbf-client-vectors.json`（約定 §9）。無 async、無網路。（PR #4，2026-09-05 合併）
   2. WebSocket／HTTP 通道、`login`、上傳（固定大小與串流）、下載、seek、續傳；`tests/pipeline.rs` 對著記憶體版 server，
      `tests/e2e_local_server.rs`（`#[ignore]`，環境變數指定 server）對著真的 wbfuwunel 跑 §4 的驗收表。（PR #5，2026-09-05 合併）
      順帶發現：wbfuwunel 對 `Create` 的回應把新發的上傳 id 放在標頭 `id`，不是線上規格 §2 說的「抄請求的」0；
      SDK 兩種都收，但標頭 id 非 0 時必須等於 Ack meta 的 `id`。要不要對 server 開 issue、還是改規格，等維護者定。
   3. `apps/wbf-cli` 與 `scripts/acceptance.sh`（CLI 規格 §8）；對本機 wbfuwunel 跑 200 MiB 全過。（2026-09-05 送審）
3. 接 matrix-sdk：登入、房間、事件、金鑰分發；CLI 的 `login`／`rooms`／`send`／`watch`。
4. UI 框架決定與 `apps/desktop`。

## 7. 明確不在 v1

- UI。
- Android 打包（但 `wbf-wire`／`wbf-sdk` 不能用到 Android 上沒有的東西）。
- 邊上傳邊下載（server 還沒有推送）。
- 縮圖（密文，做不到）。

## 7.1 本地不存東西（維護者 2026-09-05 定）

現階段所有東西都是**呼叫當下的暫留狀態**：聊天紀錄、房間訊息、房間列表、事件，本地一概不存。
會持久的只有 session 與 token（CLI 規格 §7）；第 3 步接 matrix-sdk 後，它的 store 也只放它自己非存不可的（裝置金鑰、sync 位置），不當成快取用。
**現在先把 function code 接通、讓 API 能正常互動；同步本地的事之後才開始。**

之後的版本再規劃本地資料庫，定位是**暫存快取**：刷新聊天室時更新，不是權威。設計在 [local-cache-db.md](local-cache-db.md)。要求：

- 資料庫本身要加密。解密金鑰先留在本地，相當於自解密的資料庫（防的是把檔案拷走的人，不防能登入這台機器的人）。
- 未來加上 local password 時，啟動要輸入密碼才解得開，**UI 與 CLI 一致**，沒有哪一邊繞過。
- 因此現在寫的東西不能假設本地有快取可查：每個命令的答案都從 server 來，這也是第 3 步 `watch`／`read`／`files` 的前提（CLI 規格 §3.4）。

## 7.2 耦合方向（維護者 2026-09-05 定）：上游 SDK 是可以拆掉的零件，不是地基

維護者的原話，照錄：「不要依賴太重，能切乾淨就切乾淨，蓋下去之後，要拆開來就難了。現在剛起步，這是重點的重點。」

方向：

- **我們自己的東西越多，對上游的依賴越低。** 上游 `matrix-sdk` 不是要一次淘汰，是隨著我們的 work 長大自然變薄，最後變成 fallback。
- **WS 層之後會有我們自己的協定**（現在的 pack 是起點）。到那時上游那套 E2EE 可能用不到，變成純 fallback。
- **crypto 先留**（`matrix-sdk-crypto`／vodozemac），但只當「加密解密的引用」，引擎是我們的：接法要讓「換成自己的引擎」是換一個實作，不是改呼叫者。
- **`matrix-sdk-base` 未必需要**：它與網路無關，但改 WS 可能動到加密、動到房間狀態；很高機會自己蓋一套。不要讓它的型別滲進我們的介面。
- **整體架構往 Telegram 對齊**（房間、對話、媒體的使用方式），但**不丟掉 E2EE 的本質**。
- **與聯邦對接能兼容就盡量兼容**；我們自幹的 feature 是 extension，可以不兼容。
- 🚨 **任何會 breaking Matrix 兼容的地方，都要提出來審查**，由維護者定案要不要兼容。這條沒有例外。

聊天模型（Conversation／Peer／Message／Role、怎麼接 Matrix、哪裡要審）在 [chat-model.md](chat-model.md)。

落到程式上的規則（第 3 步起適用）：

| 規則 | 意思 |
|---|---|
| CLI 與 UI 只看 `wbf-sdk` 的型別 | `Room`、`Event`、`Manifest`… 都是我們定義的；`matrix_sdk::Room`、`ruma::events::…` 不出現在 `wbf-sdk` 的 pub 介面 |
| 上游放在一個 adapter 模組裡 | 例如 `wbf-sdk/src/backend/matrix_sdk.rs`：它是唯一 `use matrix_sdk` 的地方。之後的 `backend/wbf_ws.rs` 是同一個 trait 的另一個實作 |
| 加密引擎是一個 trait | `RoomCrypto { encrypt_event, decrypt_event, … }`，第一個實作包 `OlmMachine`；換引擎是加一個實作 |
| 跨邊界只傳資料，不傳規則 | adapter 不知道 CLI 的政策（要不要警告、要不要落地）；CLI 不知道 adapter 底下是 HTTP 還是 pack |
| 每個 PR 要寫「這次新增了對上游的哪些依賴」 | 讓依賴的增長是看得見的，不是蓋下去才發現 |

## 8. 要維護者決定的

1. 這份規劃可以嗎？
2. ~~Forgejo 上建 repo~~ 定了：Forgejo `amaid/wbf-matrix-client` 開發、GitHub `WhiteBirchForumTeam/wbf-matrix-client` 鏡像（2026-09-04）。
3. 事件格式：見 `wbf-client-convention-for-chunk.md` §11。
