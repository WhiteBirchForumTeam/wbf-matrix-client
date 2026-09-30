# 連線池：一個帳號五條線，各司其職；daemon 解鎖／登入後全開，背景看著、死了重開

> 維護者 2026-09-21 定的形狀（/docs/design/overview/architecture-v2.md §5.1.1 的落地版）；2026-09-29 定：房間與金鑰的訂閱**各自一條**（§1）、訂閱**總是由 daemon 搞定**（§3.1）。
> 收包這層（`WsLink` 能收任何 pack、每個收到的 pack 過 `ReceivedHook`）在 `/docs/design/daemon/ws-receive-dispatch.md`。這份文件講的是**誰擁有那些 link、什麼時候開關、命令怎麼挑線**。
> 實作在 `crates/wbf-core/src/link_pool.rs`（池）、`link_keeper.rs`（「該開的都開著嗎」的鉤子）與 `crates/wbf-daemon`（觸發、背景迴圈、推播、desync）。

## 0. 一句話

每個登入的 wbf 帳號一個池，池裡**五條線**，一條一個用途。命令進來先看它是哪一類、丟到那條線；線還沒開就開、開了就用、
發現死了就重開再做。**daemon 在 `vault.unlock`、`account.add` 成功之後把五條都開起來，常駐時每 15 秒看一次，被關掉的重開**（§3.1）。
兩條訂閱線開的時候就訂了，所以「開著」＝「在收」。每條線開關都發事件；每個收到的 pack 經鉤子變成 core 的事件，daemon 的推播函數決定要不要送 UI。

## 1. 五條線

| 角色 `LinkRole` | 只做 | 為什麼單獨一條 |
|---|---|---|
| `Misc` | 一問一答：`Hello`／`Ping`、`Info`、橋、`Event/Send`、`Recent`（拉窗） | 一問一答的東西不該排在別人的長佇列後面 |
| `Upload` | `Upload/*` | 資料平面，長時間高頻寫，最會塞爆佇列——只能塞爆自己 |
| `Download` | `Download/*`（`Read`、串流） | 同上；跟上傳分開，一邊塞爆不拖另一邊（維護者 2026-09-21：媒體開兩條） |
| `Rooms` | `Event/Subscribe`／`Push`／`DeviceChanged`（全局房間事件） | 推播線，不跟資料平面共享佇列；量大但可重拉 |
| `Keys` | `Device/Subscribe`／`Push`／`CryptoState`（全局金鑰事件），以及 `Device/Fetch`／`ItemsDestroy`（拉、銷毀：server 只讓持有裝置佇列的那條連線銷毀，維護者 2026-09-24 同意） | 🚨 **掉了就沒了**：它必須有一條安靜的線，不跟任何大流量共享佇列（/docs/design/overview/architecture-v2.md §5.1.1）；跟 `Rooms` 分開靠的是 server 每台裝置給 8 條 WS（server #85） |

- ⭐ 分界是「誰會塞爆佇列」與「掉了救不救得回來」，🚫 不是照 kind：`Misc` 收各種 kind。例外是 `Device/Fetch`／`ItemsDestroy`：它們是拉窗、不是訂閱，
  但 server 只讓**持有這台裝置佇列的那條連線**銷毀（/docs/design/keys/to-device-client.md §8 實跑補的），所以跟著金鑰的訂閱走：收金鑰的 task 用線時跟池 `reuse` 那一格（/docs/design/keys/key-sync.md §1）。
- `Keys` **綁裝置**（server 的 `Device/Subscribe` 一台裝置一條連線在收、後來的接手），所以它就是那個帳號**唯一**在收金鑰的連線；🚫 不要在別條線上訂。
  `Rooms` 沒有這個限制（server 的房間 topic 是 `Occupancy::Many`），但一個帳號也只訂一次。
- 一個帳號一個池；兩個帳號登在同一台 server 也各自五條（token 不同，共用會讓一個帳號塞爆另一個）。server 的上限是每台裝置 8 條、每個來源位址預設 40 條（超過回 1403）。
  常駐五條之後的容量：**同一台機器（同一個位址）最多 8 個常駐的 wbf 帳號**，第 9 個開始會有線開不起來。

## 2. 命令怎麼挑線：角色是**呼叫點**的屬性

`Core::client_of(account, transport, home, role)` 多一個 `role`。哪個方法走哪條線寫在 core 的呼叫點（它知道自己在做什麼），
🚫 不從 pack 的 kind 反推、🚫 不在 daemon 猜。

| core 方法 | 線 |
|---|---|
| `ping`、`media_info`、`recent`／`sync_recent`、`room_history`／`room_files`（wbf 那條）、橋、`send_event` | `Misc` |
| `upload_file`、`send_file` 的上傳半段 | `Upload` |
| `save_media`、`media_open` 的補拉 | `Download` |
| `init_connection` 的 `Event/Subscribe`（沒有命令用它，鉤子開） | `Rooms` |
| `init_connection` 的 `Device/Subscribe`、收金鑰 task 的 `Device/Fetch`／`ItemsDestroy`（`reuse`）、登出前的 `Device/Unsubscribe` | `Keys` |

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
  📎 **心跳**（/docs/design/daemon/ws-receive-dispatch.md §5.1，維護者 2026-09-21）：每條線自己一個，24 秒一次、最近 20 秒有通訊就跳過、10 秒沒 `Pong` 就當死。
  所以閒著的線不會被 server 的 300 秒 idle 收掉，而對方悄悄不在了也會在半分鐘內變成 `is_closed()`——心跳本身不重連：重開的是下一次取用，或背景看線迴圈的下一輪（§3.1）。
  🚨 **命令做到一半死了不重做**：錯誤原樣回呼叫端（`Network`），要不要重來是呼叫端的事（跟 1506 的原則一樣：重送是 UI 的）。
  ⭐ 這條跟維護者說的「萬一斷掉，就主動打開再執行 RPC 要的命令」一致：是**這次 RPC 開頭**發現死了就重開，不是替上一個死掉的 RPC 補做。
- **訂閱線**（`Rooms`、`Keys`）：沒有命令會「用」它們，所以開它們的是 §3.1 的鉤子。訂閱的內容（`cd_seq` 在哪、收了什麼）🚫 不歸池管——池只管 socket。
  開線多一步通用的 `Core::init_connection(account, role, client)`（2026-09-22 維護者定）：hello 之後看角色，`Rooms` 就送 `Event/Subscribe`、起收推播的 task（`/docs/design/rooms/room-sync.md`），
  `Keys` 就 `Device/Subscribe` → 追平 → 起收金鑰的 task（`/docs/design/keys/key-sync.md`）；所以「開這條線」＝「訂了」，重開就重訂。
  訂閱會話結束而 socket 還活著（server 送 `Error`，例如金鑰被同一裝置後來的連線接手的 1505）時池的殞死偵測看不出來，
  所以兩個 task 收攤時都自己 `close` 那格（/docs/design/rooms/room-sync.md §4、/docs/design/keys/key-sync.md §1）——鉤子下一輪看到它不在，才會重開、重訂。
- **登出**（維護者 2026-09-21 定；完整順序在 /docs/design/daemon/account-session.md §4）：登出的 RPC 就是一次 HTTP `/logout`（一般 Matrix 帳號走 Client 的），**只有成與不成**。不成就到此為止，什麼都不動；
  成了就把這個帳號的池**直接關掉、釋放資源**（`close_all`），然後才刪本地的 `session.sealed`、`m/`…。池這邊看到的是：
  - `/logout` 成了：server 不再認這個 token。既有的線在下一個 message 被踢（server 每個 message 重驗），之後才開的線 hello 就被拒。
  - `close_all`：等每一格的鎖、取出、關掉，池從註冊表拿掉。**還在處理的命令做完才被收**（它握著鎖）；正在開的那條也是。該落地的由 cache 寫入者照常落地。

  🚫 **池裡不存「登出了沒」**：那件事的真相只有兩份——server 的 token 表與本地的 `session.sealed`——池再存一份就是第三份（維護者 2026-09-21：「這個改法有點髯」）。
  登出與一般命令的競賽因此由 server 裁決：`/logout` 之後任何新開的線都拿不到授權（hello 就是第一個 message），之前開的由 `close_all` 收。
- **destroy**：同上（共用同一段）。**重登入**（session 換了）：封好新 session 之後 `close_all` 舊池（舊 token 沒撤、但那些線不該再用）。**daemon 關**：`Core` 丟掉就全關（`Drop`）。
- 背景重開見 §3.1：它用的就是這裡的 `acquire`（經 `ensure_open`），🚫 不另開一套。
- 📎 重開時**不重探** backend：wbf 帳號的 backend 登入時就記在 session（`session.backend`，/docs/design/daemon/account-session.md §2），跟這條線死沒死無關。登出也不忘掉探測結果（探測以 server 為鍵、不帶 token）；`forget_backend_probe` 沒有正式呼叫點，留給監督者重連。

### 3.1 「該開的線都開著嗎」：鉤子與背景迴圈（維護者 2026-09-29）

> 「訂閱連線這件事總是由 daemon 搞定。」「daemon 起來後應該再起一個極輕量的 thread，在背景檢查所有帳號的所有連線池，如果哪個被 close，應該嘗試再開；
> 這裡可能需要判斷該帳號是否正在被登出，或是該帳號正在被登入，或是 daemon 是否正在被 shutdown。」

**鉤子**是 core 的一支 `Core::ensure_links()`（`link_keeper.rs`），看一次、補一次：

1. vault 還鎖著 → 什麼都不做。
2. 已經有一輪在跑（`Core::ensuring_links` 旗；例如迴圈那輪還沒完又來一次 `vault.unlock`）→ 這次跳過（`skipped_already_running`），🚫 不疊第二輪。
   資料目錄的帳號生命週期鎖（`account_lock`）在別人手上 ＝ 有**登入、登出或摧毀**正在進行 → **整輪跳過**（`skipped_busy`），下一輪再看。
   拿到就馬上放：🚫 不握著它開線（開線要幾秒，server 不在時更久；握著會把使用者的登入／登出擋成 `AccountBusy`）。
   之後才開始的登出由封池擋（`pool_of_account` 回 `AccountBusy`）；之後才開始的登入會自己 `close_links` 舊 session 的線，下一輪用新 session 重開。
3. 一個一個帳號：沒登入的跳過；登出中的跳過（封池）；不是 wbf 的跳過——判準只看登入時記下的 `session.backend == Some(WbfSdk)`（跟 `init_keys`／`olm_engine_of` 同一條），
   🚫 不探 server（維護者 2026-09-29 選的：探測沒有逾時、失敗不記，每 15 秒一輪會對一般 Matrix 帳號一直敲門、server 黑洞時卡住整輪）。
   代價：沒記 backend 的 session（舊版封的、`--token` 接的，/docs/design/daemon/account-session.md §2）不會自動開線，重新登入一次就有。
4. 五個角色照 `LinkRole::ALL` 的順序 `LinkPool::ensure_open`：開著的不動、**有命令正在用的算開著**（🚫 不排在一個長下載後面等）、沒開或死了的開一條（死的先發 `closed`）。
   開不起來：發 `Note`、記進 `failed`、繼續下一條（一條不擋其他條、其他帳號）。

**誰叫它**（daemon）：

| 什麼時候 | 怎麼叫 |
|---|---|
| `vault.unlock` 成功（起 daemon 之後的第一步，所以這就是「daemon 起來後」） | 背景 spawn 一次，🚫 不擋那個 RPC 的回應；正在關機就不叫。已經解鎖再叫一次 `vault.unlock` 也會觸發——等於手動要它馬上看一次 |
| `account.add` 成功 | 同上 |
| 常駐期間 | `Handle::keep_links_open`：`RpcServer::run` 起的背景迴圈，每 15 秒一輪；那一輪有開不起來的就把間隔加倍（上限 5 分鐘），一輪全順就回到 15 秒。`daemon.shutdown` 一廣播就停（睡到一半也停），`run` 結束也 abort 它 |

- conf 的 `TRANSPORT = http` 是上限（/docs/design/overview/architecture-v2.md §5.1）：一律 HTTP，🚫 不開 WS 線，觸發與迴圈都不跑。
- 被 server 關掉的訂閱（`Rooms` 的訂閱會話結束、`Keys` 被接手的 1505）：task 關掉那格 → 下一輪（最多 15 秒）重開、重訂。
  ⚠️ 1505 是「同一台**裝置**後來的連線接手」，裝置 id 只有這個資料目錄的 session 有，而資料目錄同時只有一個 daemon 能寫（/docs/design/overview/architecture-v2.md §0.2 的寫入鎖）——
  所以不會跟別人互踢。要是真有兩個程式拿同一個 session（例如資料目錄被整份複製到另一台），兩邊會每一輪互相接手一次；那是複製資料目錄的錯，這裡🚫 不替它設計。
- 心跳（§3「死」）讓死掉的線在半分鐘內變成 `is_closed()`，所以線死後大約一分鐘內會被重開；心跳本身仍然不重連。
- 🚫 還不是第 8 階段的監督者：不做 task panic 收攤、不重探 backend（`forget_backend_probe` 仍然沒有呼叫點）。

## 4. 事件：開關線發一則，收到的每個 pack 發一則

```rust
CoreEvent::Link     { user, role: LinkRole, state: LinkState::{Opened, Closed}, reason: Option<String> }
CoreEvent::Received { user, role, kind: u8, subtype: u8, id: u64, seq: u32, route: Route }
```

- `Link`：開成功發 `Opened`；發現死了（下一次取用、或背景迴圈下一輪看到時）、`close`／`close_all` 發 `Closed` 帶理由。⚠️ 不是即時的——死了大約一分鐘內才被看到（心跳＋迴圈間隔，§3.1）。
  `sync.state` 那個帳號層的事件維持不變（它講的是「追平了沒」，不是哪條線）。
- `Received`：`ReceivedHook` 的那一頭。**只有標頭**（kind／subtype／id／seq／路徑），🚫 不帶 meta、🚫 不帶 data——data 可能是幾 MiB 的媒體塊或密文，
  而事件是 broadcast、每條 RPC 連線都會拿到一份。要內容的（訊息、金鑰）走**型別化**的事件（`room.message`、`keys.state` 那種）。
  這則的用途是**讓 UI 看得到線上發生了什麼**（除錯、狀態列），維護者：「rpc 發送到 UI 的 function 裡面判斷這個包要不要過去」——
  判斷在 daemon 的推播函數（§6），池只發。
- 鉤子在讀取 task 上、表鎖之外（/docs/design/daemon/ws-receive-dispatch.md §4）；`EventSink` 是 broadcast 的 `try_send`，不會擋讀取 task。

## 5. 一條線一次一個命令

`PooledClient` 是那條線的 `WbfClient` 的 `tokio::Mutex` guard：同一條線上第二個命令等第一個做完。
原因是 `WbfClient` 的方法都是 `&mut self`（請求號計數器、hello 的結果），而底下的 `WsLink` 本身允許並行——
所以這是**上面那層**的限制，不是通道的。⭐ 有五條線之後，會排隊的只剩「同一類的兩個命令」（兩個下載、兩個 ping），可接受；
之後要讓同一條線並行，改的是 `WbfClient`（計數器變 atomic、hello 結果變 `Arc`），🚫 不是池。

## 6. daemon 那半：訂閱、推播、desync（/docs/design/rpc-specs/rpc-spec.md §3.9、§4）

- `subscribe { events: [...], user? }`／`unsubscribe { events }`：**每條 RPC 連線一份**訂閱集合，連線關了就沒了。`"*"` 全訂。
- 每條 RPC 連線一個推播 task：`core.subscribe()` 拿 broadcast receiver → 每則 `CoreEvent` 對訂閱集合過濾（事件名 ＋ `user`）→ `seal_push`。
  🚫 不預設推任何東西（/docs/design/rpc-specs/local-interface.md §7）；`progress` 例外：發長工作的那條連線自動收到自己請求的 `progress`（§4）。
- 收到 `RecvError::Lagged(n)` → 送 `desync { missed: n }`（/docs/design/daemon/daemon-runtime.md §5.3）：🚫 不重播、🚫 不假裝沒事。
- 維護者 2026-09-29：「`sync.open`、`sync.close` 應該是指是否要推到 RPC UI 端的一個 flag。訂閱連線這件事總是由 daemon 搞定。」
  那個 flag 就是這裡的 `subscribe`／`unsubscribe`（例如 `subscribe { events: ["room.message", "keys.state"] }`），所以🚫 沒有另外的 `sync.open`／`sync.close`：
  UI 訂不訂只決定它收不收得到推播，daemon 對上游的訂閱照跑（§3.1）。
- 新的推播名（加進 /docs/design/rpc-specs/rpc-spec.md §4）：`link.state`（＝`CoreEvent::Link`）、`pack.received`（＝`CoreEvent::Received`）。
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
- `features_of(role)`：`Misc`（加密訊息從這條送）與 `Rooms`（收 `DeviceChanged`）宣告 `org.wbftw.device_versions`，其他三條空的（/docs/design/keys/e2ee-rpc.md §2）。⚠️ 同一條線重 `hello` 會蓋掉宣告，重 hello 的呼叫點要帶 `features_of` 回的那份。

## 8. 測試

- `link_pool.rs` 單元（假 opener，記憶體對接）：同一角色第二次取用拿到同一條、不重開；不同角色是不同條；死了（對面丟掉 sink）下一次取用重開、`Link{Closed}` 再 `Link{Opened}`；
  沒 session 回 `Usage` 且不發事件；`close_all` 五條都關、發五則 `Closed`；**登出撞上正在 `open` 的 acquire**（oneshot 定順序）：`close_all` 等它開完、命令做完才收那條，事件是 Opened 再 Closed；
  `Received` 事件帶對的 role 與標頭；`ensure_open`：沒開的開、開著的不動、有命令在用的算開著而且不等、死了的重開。`Transport::Http` 不進池這條沒有測試（core 從不開 Http）。
- `link_keeper.rs` 單元（server 是一個沒人聽的位址，所以「開」一定失敗——看的是**試了哪幾條**）：鎖著什麼都不做；登入的 wbf 帳號五條照順序都試、每條開不起來講一聲；
  已經有一輪在跑就跳過、跑完放旗子；生命週期鎖在別人手上整輪跳過、放手後照常；開著的 `Misc` 不動、死掉的 `Upload` 先發 `closed` 再重開；
  沒記 `backend: WbfSdk` 的 session 與沒登入的帳號一條都不開、而且🚫 沒探 server（探測註冊表是空的）。
- `room_sync.rs`／`key_sync.rs`：兩個 task 的訂閱會話結束（socket 還活著）都關自己那格、發 `closed`、🚫 不自己重訂。
- daemon：`subscribe`／`unsubscribe` 回剩下的集合；沒訂就收不到；訂了 `"*"` 全收；`user` 過濾；Lagged → `desync`；`progress` 不訂也收得到自己的；
  看線的迴圈 `daemon.shutdown` 一廣播就停、`TRANSPORT = http` 根本不跑。
- 真 server（`--ignored`）：`account.add` 之後五條在背景開起來（`daemon.info` 的線數到 5）、`server.ping` 走開著的 `Misc`（還是 5）；daemon 重開、`vault.unlock` 之後又是 5；
  `account.del` 之後五條關。core 的兩條（房間、金鑰）用 `ensure_links` 開線。

## 9. 明確不做的

- 第 8 階段監督者的其他部分：task panic 收攤、重連時重探 backend。背景看線（§3.1）是固定間隔＋失敗加倍，🚫 沒有更細的退避。
- `Session/Login` 走 WS（另一支；現在的登入就是 Bearer 升級）。
- 同一條線並行（§5）。
