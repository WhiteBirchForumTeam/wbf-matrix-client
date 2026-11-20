#!/usr/bin/env bash
# 驗收腳本（/docs/design/rpc-specs/wbf-cli-spec.md §8），用 rpc-cli 對著本機 wbfuwunel 跑。Windows 用 Git Bash。
#
#   WBF_PASSWORD_FILE=<檔> scripts/acceptance.sh
#
# 環境變數：WBF_SERVER（預設 http://127.0.0.1:6167）、WBF_USER（localpart，預設 alice）、WBF_PASSWORD_FILE（必要）、
# WBF_ACCEPT_SIZE_MIB（預設 200；想快一點就給小的）、WBF_ACCEPT_PROFILE（預設 release；給 debug 就不必另編一份 release）。
# 任一步失敗就 exit 非 0 並印出是哪一步。
# 執行順序與 /docs/design/rpc-specs/wbf-cli-spec.md §8 的編號不同（三種 cipher 先跑，因為 Range／續傳／串流都要它的 manifest）；
# 每一步的標題括號裡是規格的步驟號。
#
# ⚠️ 每次叫 rpc-cli 都起一支 daemon、在背景開線（/docs/design/rpc-specs/wbf-cli-spec.md §3）：太密 server 會回 429，所以每次之間緩一下。
set -euo pipefail

SERVER=${WBF_SERVER:-http://127.0.0.1:6167}
USER_LOCALPART=${WBF_USER:-alice}
PASSWORD_FILE=${WBF_PASSWORD_FILE:?set WBF_PASSWORD_FILE to a file whose content is the password}
SIZE_MIB=${WBF_ACCEPT_SIZE_MIB:-200}
RANGE_AT=150000000
RANGE_LEN=4096

PROFILE=${WBF_ACCEPT_PROFILE:-release}

repo_root=$(cd "$(dirname "$0")/.." && pwd)
case "$PROFILE" in
    release) cargo build -q --release -p wbf-daemon -p wbf-matrix-rpc-cli ;;
    debug) cargo build -q -p wbf-daemon -p wbf-matrix-rpc-cli ;;
    *) echo "WBF_ACCEPT_PROFILE is release or debug, not $PROFILE" >&2; exit 1 ;;
esac
cli="$repo_root/target/$PROFILE/wbf-matrix-rpc-cli"
[ -x "$cli" ] || cli="$cli.exe"
daemon="$repo_root/target/$PROFILE/wbf-matrix-client-daemon"
[ -x "$daemon" ] || daemon="$daemon.exe"

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
data_dir="$work/data"

# daemon 是 Windows 的程式時，JSON 裡的路徑要給它看得懂的那種（命令列參數 Git Bash 會自己轉，字串裡的不會）。
native_path() { if command -v cygpath >/dev/null; then cygpath -m "$1"; else printf '%s' "$1"; fi; }
# 每次起一支新的 daemon（--daemon）、做完關掉：續傳那條要殺掉「這一次」的 daemon，所以不用 --attach 共用一支。
rpc() { sleep 3; "$cli" --data-dir "$data_dir" --daemon "$daemon" -q "$@"; }
step() { echo; echo "== $*"; }
fail() { echo "FAIL: $*" >&2; exit 1; }
# JSON 的一個欄位（只認這個腳本用到的那幾種：字串、數字、true／false）。
field() { grep -o "\"$1\": *\(\"[^\"]*\"\|[0-9]*\|true\|false\)" | head -1 | sed 's/^[^:]*: *//; s/"//g'; }
kill_pid() { taskkill //F //PID "$1" >/dev/null 2>&1 || kill -9 "$1" 2>/dev/null || true; }

step "A (spec 1). account add, server.ping"
added=$(rpc account add --server "$SERVER" --user "$USER_LOCALPART" --password-file "$PASSWORD_FILE" --device-name acceptance)
echo "$added"
user_id=$(field user_id <<<"$added")
[ -n "$user_id" ] || fail "account add printed no user_id"
ping_out=$(rpc call server.ping)
echo "$ping_out"
grep -q '"upload"' <<<"$ping_out" || fail "ping: no upload feature"
grep -q '"download"' <<<"$ping_out" || fail "ping: no download feature"
[ -f "$data_dir/local.key" ] || fail "account add did not create local.key"
[ ! -f "$data_dir/daemon.json" ] || fail "daemon.json left behind: the daemon did not clean up"

step "generate ${SIZE_MIB} MiB random file"
big="$work/big.bin"
head -c $((SIZE_MIB * 1024 * 1024)) /dev/urandom >"$big"
size=$(stat -c %s "$big" 2>/dev/null || stat -f %z "$big")
[ "$RANGE_AT" -lt "$size" ] || RANGE_AT=$((size / 2))

# session.sealed 是封住的，讀不到 token；標準下載那一步的 curl 自己登入一次拿。
password=$(cat "$PASSWORD_FILE"); password=${password%$'\n'}; password=${password%$'\r'}
token=$(curl -sS -X POST "$SERVER/_matrix/client/v3/login" -H 'Content-Type: application/json' \
    -d "{\"type\":\"m.login.password\",\"identifier\":{\"type\":\"m.id.user\",\"user\":\"$USER_LOCALPART\"},\"password\":\"$password\",\"initial_device_display_name\":\"acceptance-curl\"}" \
    | grep -o '"access_token": *"[^"]*"' | cut -d'"' -f4)
unset password
[ -n "$token" ] || fail "curl login for the standard download failed"

# 步驟 2 與 3，一個 cipher 一次。upload.file：daemon 自己讀檔（它**不**記本機原檔，所以 get 是真的從 server 下載）。
upload_download_roundtrip() {
    local cipher=$1
    local manifest="$work/m-$cipher.json" out="$work/out-$cipher.bin"
    step "B (spec 2, $cipher). upload.file, then compare the standard download with the expected ciphertext length"
    rpc call upload.file "{\"path\":\"$(native_path "$big")\",\"cipher\":\"$cipher\",\"sha256\":true}" >"$manifest"
    local mxc chunk_size file_size chunks overhead expected_len actual_len
    mxc=$(field mxc <"$manifest")
    chunk_size=$(field chunk_size <"$manifest")
    file_size=$(field file_size <"$manifest")
    [ "$file_size" = "$size" ] || fail "manifest file_size $file_size != $size"
    chunks=$(( (file_size + chunk_size - 1) / chunk_size ))
    overhead=16; [ "$cipher" = none ] && overhead=0
    expected_len=$(( file_size + chunks * overhead ))
    # 標準下載：分塊媒體整份給（wbfuwunel 的 /docs/design/chunked-upload-spec.md §4.2）。逐 byte 的密文比對在 wbf-sdk 的 e2e 測試；這裡驗長度。
    actual_len=$(curl -sS -H "Authorization: Bearer $token" -o "$work/standard-$cipher.bin" -w '%{size_download}' \
        "$SERVER/_matrix/client/v1/media/download/${mxc#mxc://}")
    [ "$actual_len" = "$expected_len" ] || fail "standard download is $actual_len bytes, expected $expected_len"
    echo "standard download: $actual_len bytes = $chunks chunks x $overhead tag + $file_size"

    step "C (spec 3, $cipher). get and cmp"
    rpc get --manifest "$manifest" -o "$out"
    cmp "$big" "$out" || fail "get ($cipher) differs from the original"
    echo "get ($cipher): identical"
}

step "D (spec 7). three ciphers, each running spec 2 and 3"
upload_download_roundtrip chacha20-poly1305
upload_download_roundtrip aes-256-gcm
upload_download_roundtrip none
echo "all three ciphers: ok"

step "E (spec 4). get --range of $RANGE_LEN bytes at $RANGE_AT"
range_out="$work/range.bin"
rpc get --manifest "$work/m-chacha20-poly1305.json" --range "bytes=$RANGE_AT-$((RANGE_AT + RANGE_LEN - 1))" -o "$range_out"
dd if="$big" of="$work/dd.bin" bs=1 skip="$RANGE_AT" count="$RANGE_LEN" status=none
cmp "$range_out" "$work/dd.bin" || fail "range bytes differ from dd"
echo "range: identical"

step "F (spec 5). kill an upload half-way (rpc-cli and its daemon), call the same upload.file again, expect resume"
resume_src="$work/resume.bin"
cp "$big" "$resume_src"
state_file="$resume_src.wbf-upload.json"
resume_args="{\"path\":\"$(native_path "$resume_src")\",\"cipher\":\"chacha20-poly1305\",\"chunk_size\":65536,\"sha256\":true}"
sleep 3
# 直接跑 exe 不經 shell 函數：$! 才是它本人。
"$cli" --data-dir "$data_dir" --daemon "$daemon" -q call upload.file "$resume_args" >/dev/null 2>"$work/resume.log" &
cli_pid=$!
for _ in $(seq 1 600); do
    [ -f "$state_file" ] && break
    kill -0 "$cli_pid" 2>/dev/null || break
    sleep 0.05
done
sleep 1
daemon_pid=$(field pid <"$data_dir/daemon.json" 2>/dev/null || true)
kill_pid "$cli_pid"
# ⚠️ 只殺 rpc-cli 不夠：Windows 上子程序不會跟著死，daemon 會把上傳做完。
[ -n "$daemon_pid" ] && kill_pid "$daemon_pid"
wait "$cli_pid" 2>/dev/null || true
[ -f "$state_file" ] || fail "no state file after the kill (did the upload finish too fast? raise WBF_ACCEPT_SIZE_MIB)"
upload_id=$(field upload_id <"$state_file")
state_mxc=$(field mxc <"$state_file")
killed_status=$(rpc call upload.status "{\"upload_id\":$upload_id}")
echo "after kill: $killed_status"
grep -q '"finished":false' <<<"$killed_status" || fail "upload finished before the kill took effect: $killed_status"
rpc call upload.file "$resume_args" >"$work/resume.json"
# 同一個上傳接著傳（不是重傳一份新的）：mxc 跟狀態檔記的一樣。
[ "$(field mxc <"$work/resume.json")" = "$state_mxc" ] || fail "the second run uploaded a new file instead of resuming $state_mxc"
[ ! -f "$state_file" ] || fail "state file still there after Seal"
rpc get --manifest "$work/resume.json" -o "$work/resume-out.bin"
cmp "$big" "$work/resume-out.bin" || fail "resumed upload differs from the original"
echo "resume: same mxc ($state_mxc), identical"

step "G (spec 6). put - from stdin"
cat "$big" | rpc put - --name stream.bin --cipher aes-256-gcm >"$work/stream.json"
rpc get --manifest "$work/stream.json" -o "$work/stream-out.bin"
cmp "$big" "$work/stream-out.bin" || fail "stdin upload differs from the original"
echo "stream: identical"

step "logout (account.del)"
rpc call account.del "{\"user\":\"$user_id\",\"accept_history_loss\":true}" >/dev/null
rpc call account.whoami "{\"user\":\"$user_id\"}" >/dev/null 2>&1 && fail "the account is still there after account.del"
[ ! -f "$data_dir/daemon.json" ] || fail "daemon.json left behind"
echo
echo "ALL PASSED"
