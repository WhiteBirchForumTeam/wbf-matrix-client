# 房間金鑰的備份：server 一份、本地一份（維護者 2026-09-09 定）

## 1 為什麼要有這一章

在這一章之前，房間金鑰（Megolm inbound session）**只活在一個地方**：帳號目錄底下的 `m/crypto.db`（那時還叫 `matrix/`）。
整個 repo 沒有任何一行碰 `/room_keys`、backup、recovery。這代表：

- 換一台機器、重灌、`logout`，**歷史訊息永久解不開**。事件本身還在 server 上，但沒有鑰匙。
- vault-and-keys.md §1.1 與 local-cache-db.md §1 那條「store 開不了就刪掉 `matrix/` 重新 `login`，store 只是裝置狀態」**把這件事寫成了正常操作**。
  「只是裝置狀態」對 `state.db` 成立，對 `crypto.db` 不成立 —— 它裡面是解開全部歷史的唯一鑰匙。
- 光有 recovery key 沒有用。recovery key 只是解開 SSSS 拿到 backup 的解密金鑰；**如果沒有人把房間金鑰上傳上去，備份是空的**。
  有意義的是房間金鑰本身（維護者 2026-09-09 在別的專案踩過同一個坑）。

唯一的緩衝是 `cache.db` 存的是**解密後的明文**（local-cache-db.md §2），所以本機歷史不會馬上消失。但它是快取（local-cache-db.md §1：可以整個丟掉），而且新裝置拿不到。
**快取不是備份。** 這一章講的是備份。

## 2 兩份備份，分工不同

| | server 端（標準 Matrix key backup） | 本地端（我們自己的加密金鑰池） |
|---|---|---|
| 存哪 | homeserver 的 `/room_keys`（`m.megolm_backup.v1.curve25519-aes-sha2`） | 帳號目錄的 `k/`（§4） |
| 防什麼 | 這台機器整個沒了（有 recovery key 之後才真的做得到，見 §3） | 意外：`crypto.db` 壞掉、`matrix/` 被刪掉重 `login`、server 端資料沒了 |
| 加密 | backup 的 curve25519 公鑰加密，私鑰在 crypto store（設了 recovery key 之後才進 SSSS） | 第五把子金鑰（`local.key` 導出，vault-and-keys.md §1） |
| 寫入時機 | 上游的背景 task，靠 sync 觸發；**CLI 靠 `key-backup upload` 追平**（§6） | **命令觸發**：`key-backup save`，`upload` 時順手一起（§5） |
| 開關 | 預設開，可以在 conf 關掉（`SERVER_BACKUP=off`，wbf-cli-spec.md §10） | 預設開，可以關（`LOCAL_ROOM_KEYS=off`） |
| 生命週期 | 跟帳號走，`logout` 不動它 | **跟這台機器上的這個帳號走：`logout` 連它一起刪**（維護者 2026-09-09，§7） |
| 互通性 | 有：Element 之類的 client 用同一份 | 沒有：只有這個 client 讀得懂 |

兩份都是 best effort 的**副本**，權威永遠是 crypto store。任何一份讀壞就當作沒有（fail closed），不要拿壞掉的金鑰去覆蓋 store。

## 3 server 端：標準 Matrix key backup

fork server（wbfuwunel）已經有完整實作（`src/api/client/backup/`、`src/service/key_backups/`），不需要 server 改任何東西。

- `ClientBuilder` 上開 `EncryptionSettings { auto_enable_backups: true, auto_enable_cross_signing: true, backup_download_strategy: AfterDecryptionFailure }`。
  `auto_enable_backups` 的意思是：`login` 之後如果 server 上沒有 backup version 就建一個，並開始上傳。
- **`login` 就開始 backup，recovery key 延後**（維護者 2026-09-09 定）。要知道這個組合的實際含意：

  > `auto_enable_backups` 建 version 時，backup 的**私鑰存在本地 crypto store**，沒有進 SSSS。
  > 所以在使用者顯式產生 recovery key 之前，server 上那份備份**換一台機器也解不開** ——
  > 它防的是「本機 crypto.db 壞掉」，不是「換裝置」。

  這句話要出現在警告裡（警告的原文在 wbf-cli-spec.md §3.6），不能只說「你還沒設 recovery key」。
- **recovery key 不自動印**（維護者定）。顯式入口是 `key-backup recovery`（wbf-cli-spec.md §3.6）：走上游的 `recovery().enable()`，
  把 recovery key 印**一次**並說明拿不回來（只能 reset）。同一個命令會把它封進 `<data dir>/r/`（§8）——
  🚫 但仍然不進 conf、不進 log，而且那份保管**不算「使用者擁有」**（同一台機器，一起被拿走就一起沒了）。
- 關掉（`SERVER_BACKUP=off`）就是 `auto_enable_backups: false` 且不跑上傳；**已經在 server 上的 version 不動、不刪**
  （刪 server 端備份是不可逆的，要顯式命令，見 §9）。

## 4 本地端：一個全量快照檔（2026-09-09 實作時改的）

放在**帳號層**（維護者 2026-09-09 定）：

```
a/<b58>_<b58>/k/
  snapshot        上游 export_room_keys 倒出來的全量加密快照
  snapshot.tmp    寫入時的暫存檔，寫完 rename 成 snapshot
```

> ⚠️ **這一節 2026-09-09 實作時改過**。原本定的是「一房一檔、逐把 append 的 `WBFRK1` 格式，
> 每個命令結束前比對後只寫新的那幾筆」。做不到，因為**上游只給全量匯出**：
> `Encryption::export_room_keys(path, passphrase, predicate)` 直接把金鑰寫成一個加密檔，
> 而拿逐把金鑰要 `Client::olm_machine()`，那是 `pub(crate)`（只有 `olm_machine_for_testing()` 露出來，
> 名字就是契約，🚫 不碰）。
>
> 改成全量快照之後**反而簡單得多**，而且原本那套 append 格式的理由都消失了：
>
> | 原本要解決的 | 全量快照為什麼不需要 |
> |---|---|
> | 去重（同一把 session 存很多次） | 每次覆蓋整份，本來就沒有重複 |
> | 檔案愈積愈長 | 快照大小 ＝ 金鑰總量，不隨備份次數長 |
> | 自訂的 `WBFRK1` 格式與它的 nonce／序號 | 不用了，格式是上游的 |
> | 尾巴壞掉要截斷 | 先寫 `.tmp` 再 rename，要嘛舊的完整、要嘛新的完整 |
>
> 代價只有一個：拿不到「只寫增量」，每次要跑一輪 PBKDF2 500,000（約半秒）。所以它是**命令觸發**的（§5）。

- **passphrase 是 vault 第五把子金鑰的 base64**（`BLAKE3 derive_key("wbf-matrix-client room key backup v1", master)`），
  不是使用者打的字——所以「passphrase 太弱被暴力破」在這裡不存在。
- **🚫 不再包一層我們自己的 AEAD**：上游的匯出格式本身就是加密的，而它的 passphrase 已經是 vault 保護的，
  多包一層不增加任何安全性，只多一個要維護的格式（全域 CLAUDE.md A2）。
- **金鑰不進我們的記憶體**：上游直接寫檔、直接讀檔，我們只給路徑與 passphrase。
- 一房一檔也做得到（`predicate` 可以按房過濾），但那會變成「房間數 × 半秒」。**維護者當初說的
  「以 server & 房間為識別碼」在這裡沒有照做**，理由就是這個；要改回一房一檔的話只是把 predicate
  換成迴圈，格式不用動。

## 5 什麼時候寫

**命令觸發，不是每個命令都做**——每次一輪 PBKDF2 500,000（約半秒），掛在 `read` 這種命令上太貴。

| 時機 | 做什麼 |
|---|---|
| `key-backup save` | 手動存一份 |
| `key-backup upload` | 推完 server 那份**順手也存本地這份**：兩份備份的用途不同（§2），但沒有理由讓使用者記得跑兩個命令 |
| `logout` 的閘門擋下來時 | 訊息告訴他有 `key-backup save` 這條路（也老實說 logout 會連它一起刪） |

⚠️ 所以本地這份**不是「一定不會漏」**：兩次 `save` 之間拿到的金鑰，只在 crypto store 與（有跑 upload 的話）
server 那份裡。原本的設計把它寫成「同步寫、不能漏」，那是建立在「拿得到逐把金鑰」的假設上，而那個假設是錯的。
真正的保險仍然是 §3 的 server 端 backup 加 recovery key。

## 6 server 那份怎麼追平：`key-backup upload`

維護者 2026-09-09 定：**不在每個命令結束前等上傳**（那會讓每個命令慢），改成一個獨立命令手動跑。

- `key-backup upload` 走上游的 `backups().wait_for_steady_state()`，印上傳進度與結果，完成才 exit。
- 代價老實寫：**server 那份會長期落後**，落後多少由使用者跑不跑這個命令決定。
  ⚠️ 🚫 這個代價**不是**靠本地那份補起來的——本地那份同樣是命令觸發的（§5），兩者會一起落後。
  真正的保險是 §3 的 server 端 backup 加 recovery key。
- `key-backup status` 印這幾個欄位（🚫 不印任何金鑰內容）：
  `server_backup_exists`、`uploading_locally`、`recovery_enabled`、`recovery_state`、
  `local_snapshot`、`local_snapshot_bytes`、`local_snapshot_saved_at`。
  ⚠️ 逐把的計數印不出來：上游只給整包匯出，沒有「crypto store 裡有幾把」這種問法（§4）。

## 7 誰刪 `k/`：意外留門，離開就清乾淨

分界是**這次是意外還是有意的**（維護者 2026-09-09 定）：

| 情形 | `m/` | `k/` | 怎麼把歷史找回來 |
|---|---|---|---|
| **意外**：store 壞掉、金鑰對不上，照 vault-and-keys.md §1.1 的指示手動刪 `matrix/` 重新 `login` | 被刪 | **留著** | 重 `login` 後 `key-backup import` 把快照餵回新的 crypto store |
| **有意**：`logout`／`account del <user>`（同一件事，wbf-cli-spec.md §3.1） | 被刪（Matrix logout 讓裝置失效，留著會擋下一次 `login`） | **一起刪** | 靠 server 那份加 recovery key（所以有閘門，見下） |
| **有意**：`account destroy <user>` | 被刪（它包含 `del`） | **一起刪** | 同上。它多做的是資料層：這個帳號在 `cache.db` 裡**獨有**的紀錄（別人也持有的不動） |

📎 **to-device 的水位（`cd_seq`）住在 `m/` 裡面，所以這張表的每一列它都自動跟著對**
（維護者 2026-09-12）：`m/` 被刪 → 水位一起沒 → 下次從頭拉。🚫 不需要有人記得另外去清它，
理由在 [to-device-client.md](../keys/to-device-client.md) local-cache-db.md §2.1。

為什麼 `logout`（即 `account del`）連著刪（維護者 2026-09-09）：它在心智上是「我離開這台機器」，
留一個能解開全部歷史的檔案在磁碟上是驚嚇，而且跟「crypto store 一定會被刪」不一致。

### `logout` 的閘門：不是正面認得救得回來，就不准走

⚠️ 直接刪有一個連鎖：在還沒有 recovery key 的預設狀態下，**server backup 的私鑰是存在 crypto store 裡的**（§3），
而 `logout` 正要刪掉 crypto store。所以「crypto store 沒了 ＋ room-keys 也刪了 ＋ server 那份解不開」＝ 歷史三份全滅。

所以 `logout` 前面加一道閘門，寫成**正面認得**的形式：

```
准走（照常 logout，k/ 一起刪）  ⟸  ① server 上有 backup ＆ recovery().state() == Enabled
                                          ＆ ② 這台機器保管著這個帳號的 recovery key（§8）
其他任何狀態                            ⟹  exit 1，要 --accept-history-loss 才走
```

⚠️ **兩關都要過，而且第二關才是真的**（維護者 2026-09-09）：`RecoveryState::Enabled` 的上游定義是
「secret storage is set up and we have all the secrets locally」——它說得出 SSSS 設好了，
**說不出那串 recovery key 在誰手上**。跑過 `key-backup recovery`、印出來、沒抄就關掉終端的人，
第一關照樣過。第二關看的是 `<data dir>/r/` 有沒有封著這個帳號的 key，
而那個目錄 `logout` 不碰——所以刪完 `m/` 與 `k/` 之後它還在，歷史真的救得回來。

🚫 **不問使用者手打 recovery key**（維護者 2026-09-09 定）：既然我們自己就保管著，問他等於刁難。

「其他任何狀態」包含：沒有 recovery key、`SERVER_BACKUP=off`（使用者自己關掉的，那本地這份就是唯一一份）、
`RecoveryState` 是 `Unknown`／`Incomplete`、問不到 server。🚫 不寫成「沒有 recovery key 才擋」——
那樣新增一種狀態就默默放行；要壞就壞在「多擋一次」那一邊。

擋下來的時候印的訊息要直接給下一步（原文在 wbf-cli-spec.md §3.6）：先跑 `key-backup recovery` 產生 recovery key，
server 那份就變成換裝置也解得開的備份，再 `logout` 就沒有損失。

📎 副作用（好的）：這讓「recovery key 延後」不會被無限期延後 —— **延到第一次 `logout` 為止**。
本地池的定位因此也清楚了：它是**線上那份還沒真的可攜之前的中繼**，不是永久保險。

## 8 recovery key 存哪：獨立的資料夾，`logout` 不碰（維護者 2026-09-09 定）

```
<data dir>/r/<b58>_<b58>            檔名是 `recovery-key@bob:matrix.org` 加密後的樣子
```

**為什麼不放在帳號目錄底下**：`logout`／`account del` 要把帳號目錄整個清乾淨
（`session.sealed`、`m/`、`k/`），而 recovery key 正好是**清完之後唯一回得去的路**。
放在一起就會一起被刪，那等於 server 上的備份也沒了——閘門（§7）就變成在檢查一個馬上要被自己刪掉的東西。

| 誰 | 對 recovery key 做什麼 |
|---|---|
| `key-backup recovery` | 產生、印出來一次、**封進這裡** |
| `logout`／`account del` | **不碰**——這是它跟帳號目錄分開放的全部理由 |
| `account destroy` | **一起摧毀**（那個命令的語意就是「什麼都不留」）。⚠️ 之後 server 上那份備份永遠解不開 |
| `recovery list` | 列出這台機器保管著誰的（只解**檔名**，🚫 不解內容） |
| `recovery show <user>` | 印出某一個（會印秘密，跟 `key-backup recovery` 一樣） |
| `key-backup restore` | 拿它**恢復這台裝置**——重新 `login` 之後必跑，見下 |

- **檔名跟其他兩層一樣加密**（`DirScope::Recovery`，第六把子金鑰）：外面看不出這台機器保管著誰的 key。
- **內容用第三把子金鑰封**（跟 `session.sealed` 同一把，AAD 不同所以密文換不過去）。
- 目錄 0700。
- ⚠️ **重新 `login` 之後要跑 `key-backup restore`**（2026-09-09 對真 server 驗證時發現）：
  `logout` 之後再 `login` 是**新裝置**，它的 crypto store 沒有 SSSS 的 secrets，
  `RecoveryState` 會是 `Incomplete`、server 上那份備份解不開。保管著 recovery key 不會自動生效，
  要有人拿它去 `recovery().recover()`。
- ⚠️ 老實說它的邊界：這是**方便性的保管**，不是「使用者擁有」的證明——它跟 crypto store 在同一台
  機器上，整台被拿走就一起沒了。真正換裝置時仍然要使用者手上有那串字，所以 `key-backup recovery`
  印出來時還是會叫他寫下來。

## 9 明確不做的 / 還開著的

- 🚫 不自己發明備份格式上傳到 fork server（走 pack 通道）：標準路徑已經可用，自訂等於放棄互通又要 server 改。
- 🚫 `key-backup` 不做「刪掉 server 上的 backup version」：不可逆，而且會讓其他裝置的備份一起失效。要刪去別的 client 刪。
- 還開著：本地金鑰池要不要配額或壓縮（append-only 會一直長）。一筆約 200 byte，一萬把也才 2 MB，第一版不管。
- 還開著：UI 那版怎麼呈現 recovery key（CLI 只印一次就算了，UI 要有「我存好了」的確認流程）。

