#!/usr/bin/env bash
# 驗收腳本（CLI 規格 §8、plan-v1 §4），對著本機 wbfuwunel 跑。Windows 用 Git Bash。
#
#   WBF_PASSWORD_FILE=<檔> scripts/acceptance.sh
#
# 環境變數：WBF_SERVER（預設 http://127.0.0.1:6167）、WBF_USER（預設 alice）、WBF_PASSWORD_FILE（必要）、
# WBF_ACCEPT_SIZE_MIB（預設 200；想快一點就給小的）。
# 任一步失敗就 exit 非 0 並印出是哪一步。
# 執行順序與規格 §8 的編號不同（三種 cipher 先跑，因為 seek／續傳／串流都要它的 manifest）；
# 每一步的標題括號裡是規格的步驟號。
set -euo pipefail

SERVER=${WBF_SERVER:-http://127.0.0.1:6167}
USER_ID=${WBF_USER:-alice}
PASSWORD_FILE=${WBF_PASSWORD_FILE:?set WBF_PASSWORD_FILE to a file whose content is the password}
SIZE_MIB=${WBF_ACCEPT_SIZE_MIB:-200}
SEEK_AT=150000000
SEEK_LEN=4096

repo_root=$(cd "$(dirname "$0")/.." && pwd)
cargo build -q --release -p wbf-cli
cli="$repo_root/target/release/wbf-cli"
[ -x "$cli" ] || cli="$cli.exe"

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
data_dir="$work/data"
wbf() { "$cli" --server "$SERVER" --data-dir "$data_dir" "$@"; }

step() { echo; echo "== $*"; }
fail() { echo "FAIL: $*" >&2; exit 1; }

step "A (spec 1). login, ping"
wbf login --user "$USER_ID" --password-file "$PASSWORD_FILE" >/dev/null
ping_out=$(wbf ping)
echo "$ping_out"
grep -q '"upload"' <<<"$ping_out" || fail "ping: no upload feature"
grep -q '"download"' <<<"$ping_out" || fail "ping: no download feature"

step "generate ${SIZE_MIB} MiB random file"
big="$work/big.bin"
head -c $((SIZE_MIB * 1024 * 1024)) /dev/urandom >"$big"
size=$(stat -c %s "$big" 2>/dev/null || stat -f %z "$big")
[ "$SEEK_AT" -lt "$size" ] || SEEK_AT=$((size / 2))

# session.sealed 是封住的，讀不到 token；標準下載那一步的 curl 自己登入一次拿。
password=$(cat "$PASSWORD_FILE"); password=${password%$'\r'}
token=$(curl -sS -X POST "$SERVER/_matrix/client/v3/login" -H 'Content-Type: application/json' \
    -d "{\"type\":\"m.login.password\",\"identifier\":{\"type\":\"m.id.user\",\"user\":\"$USER_ID\"},\"password\":\"$password\",\"initial_device_display_name\":\"acceptance-curl\"}" \
    | grep -o '"access_token": *"[^"]*"' | cut -d'"' -f4)
unset password
[ -n "$token" ] || fail "curl login for the standard download failed"
[ -f "$data_dir/local.key" ] || fail "login did not create local.key"
[ -f "$data_dir/session.sealed" ] || fail "login did not create session.sealed"
grep -q '"access_token"' "$data_dir/session.sealed" && fail "session.sealed holds the token in plaintext"

# 步驟 2 與 3，一個 cipher 一次。
upload_download_roundtrip() {
    local cipher=$1
    local manifest="$work/m-$cipher.json" out="$work/out-$cipher.bin"
    step "B (spec 2, $cipher). upload, then compare the standard download with the expected ciphertext length"
    wbf --quiet upload "$big" --cipher "$cipher" --sha256 --manifest "$manifest" >/dev/null
    local mxc chunk_size file_size chunks overhead expected_len actual_len
    mxc=$(grep -o '"mxc": *"[^"]*"' "$manifest" | cut -d'"' -f4)
    chunk_size=$(grep -o '"chunk_size": *[0-9]*' "$manifest" | grep -o '[0-9]*$')
    file_size=$(grep -o '"file_size": *[0-9]*' "$manifest" | grep -o '[0-9]*$')
    [ "$file_size" = "$size" ] || fail "manifest file_size $file_size != $size"
    chunks=$(( (file_size + chunk_size - 1) / chunk_size ))
    overhead=16; [ "$cipher" = none ] && overhead=0
    expected_len=$(( file_size + chunks * overhead ))
    # 標準下載：分塊媒體整份給（線上規格 §4.2）。逐 byte 的密文比對在 wbf-sdk 的 e2e 測試；這裡驗長度。
    actual_len=$(curl -sS -H "Authorization: Bearer $token" -o "$work/standard-$cipher.bin" -w '%{size_download}' \
        "$SERVER/_matrix/client/v1/media/download/${mxc#mxc://}")
    [ "$actual_len" = "$expected_len" ] || fail "standard download is $actual_len bytes, expected $expected_len"
    echo "standard download: $actual_len bytes = $chunks chunks x $overhead tag + $file_size"

    step "C (spec 3, $cipher). download and cmp"
    wbf --quiet download --manifest "$manifest" -o "$out"
    cmp "$big" "$out" || fail "download ($cipher) differs from the original"
    echo "download ($cipher): identical"
    echo "$manifest"
}

step "D (spec 7). three ciphers, each running spec 2 and 3"
manifest_chacha=$(upload_download_roundtrip chacha20-poly1305 | tail -1)
upload_download_roundtrip aes-256-gcm >/dev/null
upload_download_roundtrip none >/dev/null
echo "all three ciphers: ok"

step "E (spec 4). seek --at $SEEK_AT --len $SEEK_LEN reads exactly one chunk"
seek_out="$work/seek.bin"
seek_summary=$(wbf --quiet seek --manifest "$manifest_chacha" --at "$SEEK_AT" --len "$SEEK_LEN" 2>&1 >"$seek_out")
echo "$seek_summary"
dd if="$big" of="$work/dd.bin" bs=1 skip="$SEEK_AT" count="$SEEK_LEN" status=none
cmp "$seek_out" "$work/dd.bin" || fail "seek bytes differ from dd"
grep -qE '"chunks_read":\[[0-9]+\]' <<<"$seek_summary" || fail "seek read more than one chunk: $seek_summary"
echo "seek: identical, one chunk"

step "F (spec 5). kill an upload half-way, run the same command again, expect resume"
resume_src="$work/resume.bin"
cp "$big" "$resume_src"
log="$work/resume.log"
# HTTP 通道一塊一個請求，慢到夠我們在中途殺掉。直接跑 exe 不經 shell 函數：$! 才是它本人，kill 才殺得到。
"$cli" --server "$SERVER" --data-dir "$data_dir" --transport http upload "$resume_src" \
    --cipher chacha20-poly1305 --chunk-size 65536 --manifest "$work/resume.json" 2>"$log" >/dev/null &
upload_pid=$!
for _ in $(seq 1 600); do
    grep -q 'chunk 20/' "$log" 2>/dev/null && break
    kill -0 "$upload_pid" 2>/dev/null || break
    sleep 0.05
done
kill -9 "$upload_pid" 2>/dev/null || true
wait "$upload_pid" 2>/dev/null || true
[ -f "$resume_src.wbf-upload.json" ] || fail "no state file after the kill (did the upload finish too fast?)"
upload_id=$(grep -o '"upload_id": *[0-9]*' "$resume_src.wbf-upload.json" | grep -o '[0-9]*$')
killed_status=$(wbf status "$upload_id")
echo "after kill: $killed_status"
grep -q '"finished":false' <<<"$killed_status" || fail "upload finished before the kill took effect: $killed_status"
resume_log=$(wbf upload "$resume_src" --cipher chacha20-poly1305 --chunk-size 65536 --sha256 --manifest "$work/resume.json" 2>&1 >/dev/null)
grep -q 'resume from chunk' <<<"$resume_log" || fail "second run did not resume: $resume_log"
echo "$resume_log" | grep 'resume from chunk'
[ ! -f "$resume_src.wbf-upload.json" ] || fail "state file still there after Seal"
wbf --quiet download --manifest "$work/resume.json" -o "$work/resume-out.bin"
cmp "$big" "$work/resume-out.bin" || fail "resumed upload differs from the original"
echo "resume: identical"

step "G (spec 6). stream upload from stdin"
cat "$big" | wbf --quiet upload --stream --cipher aes-256-gcm --link wifi --name stream.bin --manifest "$work/stream.json" >/dev/null
wbf --quiet download --manifest "$work/stream.json" -o "$work/stream-out.bin"
cmp "$big" "$work/stream-out.bin" || fail "stream upload differs from the original"
echo "stream: identical"

step "logout"
wbf logout >/dev/null
wbf whoami >/dev/null 2>&1 && fail "token still valid after logout"
echo
echo "ALL PASSED"
