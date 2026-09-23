# 連線池：一個帳號四條線（設計上五條），各司其職、要用才開、斷了下次再開

> 維護者 2026-09-21 定的形狀（architecture-v2 §6.1.1 四條線的落地版；daemon-runtime §11 第 4 階段 daemon 那半、第 7 階段的連線部分）。
> 前提是 PR #52：`WsLink` 能收任何 pack、每個收到的 pack 過 `ReceivedHook`（`ws-receive-dispatch.md`）。這份文件講的是**誰擁有那些 link、什麼時候開關、命令怎麼挑線**。
> 實作在 `crates/wbf-core/src/link_pool.rs`（池）與 `crates/wbf-daemon`（訂閱、推播、desync）。

## 0. 一句話

每個登入的帳號一個池，池裡**四條線**（設計上五條；房間與金鑰的訂閱暫時共用，§1），一條一個用途。命令進來先看它是哪一類、丟到那條線；線還沒開就開、開了就用、
發現死了就重開再做。**沒有背景的監督者**（那是第 8 階段）：斷了不會有人主動去接，下一個要用它的命令會。
訂閱類的線**預設不開**，收到訂閱指令才開。每條線開關都發事件；每個收到的 pack 經鉤子變成 core 的事件，daemon 的推播函數決定要不要送 UI。

## 1. 四條線（暫時；設計上是五條）

| 角色 `LinkRole` | 只做 | 誰開它 | 為什麼單獨一條 |
|---|---|---|---|
| `Misc` | 一問一答：`Hello`／`Ping`、`Info`、橋、`Event/Send`、`Recent`（拉窗）、`Device/Fetch`／`ItemsDestroy`（拉、銷毀） | 第一個要用的命令 | 一問一答的東西不該排在別人的長佇列後面 |
| `Upload` | `Upload/*` | 第一個上傳 | 資料平面，長時間高頻寫，最會塞爆佇列——只能塞爆自己 |
| `Download` | `Download/*`（`Read`、串流） | 第一個下載 | 同上；跟上傳分開，一邊塞爆不拖另一邊（維護者 2026-09-21：媒體開兩條） |
| `Subscriptions` | `Event/Subscribe`／`Push`／`DeviceChanged`（全局房間事件）**與** `Device/Subscribe`／`Push`／`CryptoState`（全局金鑰事件） | **只有訂閱命令** | 推播線，不跟資料平面共享佇列。📌 房間與金鑰暫時共用（維護者 2026-09-21：server 每台裝置預設 4 條 WS，先不動 server）；設計上金鑰該自己一條安靜的線（🚨 掉了就沒了），要分就是多一個角色 |

- ⭐ 分界是「誰會塞爆佇列」與「掉了救不救得回來」，🚫 不是照 kind：`Misc` 收各種 kind；`Device/Fetch` 走 `Misc`（它是拉窗，不是訂閱）。
- 訂閱線**綁裝置**（server 的 `Device/Subscribe` 一台裝置一條連線在收、後來的接手），所以它就是那個帳號**唯一**在收推播的連線；
  🚫 不要在 `Misc` 上訂閱。
- 一個帳號一個池；兩個帳號登在同一台 server 也各自四條（token 不同，共用會讓一個帳號塞爆另一個）。

## 2. 命令怎麼挑線：角色是**呼叫點**的屬性

`Core::client_of(account, transport, home, role)` 多一個 `role`。哪個方法走哪條線寫在 core 的呼叫點（它知道自己在做什麼），
🚫 不從 pack 的 kind 反推、🚫 不在 daemon 猜。

| core 方法 | 線 |
|---|---|
| `ping`、`media_info`、`recent`／`sync_recent`、`room_history`／`room_files`（wbf 那條）、橋、`send_event`、`device_fetch`／`items_destroy` | `Misc` |
| `upload_file`、`send_file` 的上傳半段 | `Upload` |
| `save_media`、`media_open` 的補拉 | `Download` |
| （PR 2）`device_subscription`、（第 6 階段）`Event/Subscribe` | `Subscriptions` |

`Transport::Http`（pack over HTTP，只剩 debug 用途）🚫 不進池：每次一條、用完就丟，跟現在一樣。

## 3. 生命週期：三個狀態、一條路開、一條路關

```
              第一個要用它的命令                  send/receive 回 Network，或 is_closed()
   Idle ────────────────────────────▶ Open ──────────────────────────────────────────▶ Dead
   （沒有 socket）   開：connect(Bearer) → hello                                          │
     ▲                                                                                   │
     └───────────────────────────── 下一個要用它的命令：重開再做 ◀────────────────────────┘
```

- **開**（`LinkOpener::open(account, role)`）：拿 vault 裡那個帳號的 session（`server`、`access_token`）→ `Channel::connect(WebSocket)`（Bearer 升級，**這就是登入**）
  → `hello(name, features_of(role))`。⚠️ 帳號**沒有 session**（沒登入過、或登出了）→ `Err(Usage("not logged in"))`，🚫 不開沒 token 的連線
  （server 允許未登入升級但 30 秒就關、而且我們沒有要在線上 `Session/Login`——那是另一支）。「未登入就閒置」＝池裡有這一格、沒有 socket。
- **用**：`client_of` 回一個 `PooledClient`（一條線一次一個命令，§5）。呼叫端照舊 `client.xxx().await`。
- **死**：`WsLink::is_closed()` 為 true（讀取或送出 task 走過 `shut_down`：對方關、寫失敗、**心跳沒回**）。池在**下一次**取用時看到就丟掉舊的、重開、再把命令做下去。
  📎 **心跳**（ws-receive-dispatch.md §5.1，維護者 2026-09-21）：每條線自己一個，24 秒一次、最近 20 秒有通訊就跳過、10 秒沒 `Pong` 就當死。
  所以閒著的線不會被 server 的 300 秒 idle 收掉，而對方悄悄不在了也會在半分鐘內變成 `is_closed()`——但**仍然是下一次取用才重開**，心跳不重連。
  🚨 **命令做到一半死了不重做**：錯誤原樣回呼叫端（`Network`），要不要重來是呼叫端的事（跟 1506 的原則一樣：重送是 UI 的）。
  ⭐ 這條跟維護者說的「萬一斷掉，就主動打開再執行 RPC 要的命令」一致：是**這次 RPC 開頭**發現死了就重開，不是替上一個死掉的 RPC 補做。
- **訂閱線**（`Subscriptions`）：`Idle` 到收到訂閱命令為止；死了也是 `Dead` 躺著，**下一個訂閱命令**來才重開＋重訂。訂閱的內容（訂了哪些房、`cd_seq` 在哪）
  🚫 不歸池管——池只管 socket。📌 2026-09-22 起（維護者定）開線多一步通用的 `Core::init_connection(account, role, client)`：hello 之後看角色，`Subscriptions` 就在這裡送 `Event/Subscribe`、
  起收推播的 task（`room-sync.md`）；所以「開這條線」＝「訂了」，重開就重訂。訂閱會話結束而 socket 還活著（server 送 `Error`）時池的殞死偵測看不出來，
  所以收推播的 task 收攤時自己 `close` 那格（room-sync.md §4）——「重開就重訂」在那條路才成立。`Core::open_subscriptions`／`close_subscriptions` 是開／關它的入口（還沒接 RPC）。
  金鑰那半（`Device/Subscribe`）是下一支，`init_connection` 裡多一個會話。
- **登出**（維護者 2026-09-21 定）：登出的 RPC 就是一次 HTTP `/logout`（或 fallback 到 matrix-sdk），**只有成與不成**。不成就到此為止，什麼都不動；
  成了就把這個帳號的池**直接關掉、釋放資源**（`close_all`），然後才刪本地的 `session.sealed`、`m/`…。順序：

  | 步 | 做什麼 | 之後的世界 |
  |---|---|---|
  | 1 | HTTP `/logout`（撤 token、刪裝置） | server 不再認這個 token：既有的線在下一個 message 被踢（server 每個 message 重驗），之後才開的線 hello 就被拒 |
  | 2 | `close_all`：等每一格的鎖、取出、關掉，池從註冊表拿掉 | **還在處理的命令做完才被收**（它握著鎖）；正在開的那條也是。該落地的由 cache 寫入者照常落地 |
  | 3 | 刪 `session.sealed`、`m/`、快照、current | 本地跟 server 一致；`session_of` 回 NotLoggedIn，連試都不試 |

  🚫 **池裡不存「登出了沒」**：那件事的真相只有兩份——server 的 token 表與本地的 `session.sealed`——池再存一份就是第三份（維護者 2026-09-21：「這個改法有點髯」）。
  登出與一般命令的競賽因此由 server 裁決：第 1 步之後任何新開的線都拿不到授權（hello 就是第一個 message），第 1 步之前開的由第 2 步收。
- **destroy**：同上（共用同一段）。**重登入**（session 換了）：封好新 session 之後 `close_all` 舊池（舊 token 沒撤、但那些線不該再用）。**daemon 關**：`Core` 丟掉就全關（`Drop`）。
- 🚫 **沒有背景重連、沒有退避**。第 8 階段的監督者做那個，而且做的時候用的就是這裡的 `open`／`close_all`，🚫 不另開一套。
- 📎 重開時**不重探** backend（`backends` 那格照舊）：探測的是「這台講不講 wbf」，跟這條線死沒死無關。登出才 `forget_backend_probe`（既有）。

## 4. 事件：開關線發一則，收到的每個 pack 發一則

```rust
CoreEvent::Link     { user, role: LinkRole, state: LinkState::{Opened, Closed}, reason: Option<String> }
CoreEvent::Received { user, role, kind: u8, subtype: u8, id: u64, seq: u32, route: Route }
```

- `Link`：開成功發 `Opened`；發現死了（下一次取用時）或 `close_all` 發 `Closed` 帶理由。⚠️ 不是即時的——沒有監督者在看，死了要到有人用才知道。
  `sync.state` 那個帳號層的事件維持不變（它講的是「追平了沒」，不是哪條線）。
- `Received`：`ReceivedHook` 的那一頭。**只有標頭**（kind／subtype／id／seq／路徑），🚫 不帶 meta、🚫 不帶 data——data 可能是幾 MiB 的媒體塊或密文，
  而事件是 broadcast、每條 RPC 連線都會拿到一份。要內容的（訊息、金鑰）由 PR 2／第 6 階段發**型別化**的事件（`room.message` 那種）。
  這則的用途是**讓 UI 看得到線上發生了什麼**（除錯、狀態列），維護者：「rpc 發送到 UI 的 function 裡面判斷這個包要不要過去」——
  判斷在 daemon 的推播函數（§6），池只發。
- 鉤子在讀取 task 上、表鎖之外（`ws-receive-dispatch.md` §4）；`EventSink` 是 broadcast 的 `try_send`，不會擋讀取 task。

## 5. 一條線一次一個命令

`PooledClient` 是那條線的 `WbfClient` 的 `tokio::Mutex` guard：同一條線上第二個命令等第一個做完。
原因是 `WbfClient` 的方法都是 `&mut self`（請求號計數器、hello 的結果），而底下的 `WsLink` 本身允許並行——
所以這是**上面那層**的限制，不是通道的。⭐ 有五條線之後，會排隊的只剩「同一類的兩個命令」（兩個下載、兩個 ping），可接受；
之後要讓同一條線並行，改的是 `WbfClient`（計數器變 atomic、hello 結果變 `Arc`），🚫 不是池。

## 6. daemon 那半：訂閱、推播、desync（rpc-spec §3.9、§4）

- `subscribe { events: [...], user? }`／`unsubscribe { events }`：**每條 RPC 連線一份**訂閱集合，連線關了就沒了。`"*"` 全訂。
- 每條 RPC 連線一個推播 task：`core.subscribe()` 拿 broadcast receiver → 每則 `CoreEvent` 對訂閱集合過濾（事件名 ＋ `user`）→ `seal_push`。
  🚫 不預設推任何東西（architecture-v2 §4.7）；`progress` 例外：發長工作的那條連線自動收到自己請求的 `progress`（§4）。
- 收到 `RecvError::Lagged(n)` → 送 `desync { missed: n }`（daemon-runtime §5.3）：🚫 不重播、🚫 不假裝沒事。
- 新的推播名（加進 rpc-spec §4）：`link.state`（＝`CoreEvent::Link`）、`pack.received`（＝`CoreEvent::Received`）。
  既有的 `room.message`／`sync.state`／`progress` 照 rpc-spec 對到 `CoreEvent::Message`／`SyncState`／`Progress`。

## 7. 開連線是一個接縫：`LinkOpener`

「怎麼開連線、開哪一類」抽成一個 trait，池只知道「要一條 `role` 的線」：

```rust
pub trait LinkOpener: Send + Sync {
    fn open(&self, account: &AccountDir, role: LinkRole) -> impl Future<Output = Result<WbfClient<Channel>, CoreError>> + Send;
}
```

- 正式的實作在 `Core`：session → `Channel::connect` → `hello(features_of(role))`。
- 測試的實作用 `transport::memory_pair` 起 `WsLink`：池的生命週期（要用才開、死了重開、登出全關、事件有沒有發）不用真 server 就測得到。
- `features_of(role)`：現在 `Misc`／`Upload`／`Download`／`Rooms` 都是空的；`Keys` 之後宣告 `org.wbftw.device_versions`（PR 2，宣告了就得帶 `room_version`，那時 `Event/Send` 也在 PR 2 接上）。

## 8. 測試

- `link_pool.rs` 單元（假 opener，記憶體對接）：同一角色第二次取用拿到同一條、不重開；不同角色是不同條；死了（對面丟掉 sink）下一次取用重開、`Link{Closed}` 再 `Link{Opened}`；
  沒 session 回 `Usage` 且不發事件；`close_all` 五條都關、發五則 `Closed`；**登出撞上正在 `open` 的 acquire**（oneshot 定順序）：`close_all` 等它開完、命令做完才收那條，事件是 Opened 再 Closed；
  `Received` 事件帶對的 role 與標頭。`Transport::Http` 不進池這條沒有測試（core 從不開 Http）。
- daemon：`subscribe`／`unsubscribe` 回剩下的集合；沒訂就收不到；訂了 `"*"` 全收；`user` 過濾；Lagged → `desync`；`progress` 不訂也收得到自己的。
- 真 server（`--ignored`）：`server.ping` 兩次走同一條 `Misc`（`daemon.info` 看得到線數）；`account.del` 之後五條關。

## 9. 明確不做的

- 背景重連與退避（第 8 階段）。
- `Session/Login` 走 WS（另一支；現在的登入就是 Bearer 升級）。
- 訂閱線的**內容**：金鑰的訂閱、拉、匯入是 E2EE 那支；房間的 `Event/Subscribe` codec 是第 6 階段（兩者都走 `Subscriptions`）。這支只保證那條線**開得起來、關得掉、有人收**。
- 同一條線並行（§5）。
