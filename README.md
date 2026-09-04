# wbf-client

wbfuwunel 的 client 端：一個可以拿來寫 desktop、Android、CLI 的 Rust SDK，加上第一個用它的程式（CLI）。
目前只有設計文件與上游 matrix-rust-sdk 的 submodule，程式碼還沒開始寫。規劃看
[`docs/design/plan-v1.md`](docs/design/plan-v1.md)。

## 關聯專案

| | 在哪 | 角色 |
|---|---|---|
| **wbfuwunel** | Forgejo [`amaid/wbfuwunel`](http://ai.zooy.cc:30008/amaid/wbfuwunel)、公開鏡像 [`WhiteBirchForumTeam/wbfuwunel`](https://github.com/WhiteBirchForumTeam/wbfuwunel) | **server 端的權威。** 本 repo 對的是它，不是 Matrix 規格。它是 [`matrix-construct/tuwunel`](https://github.com/matrix-construct/tuwunel) 的 fork，分岔會持續變大，與 Matrix 規格的相容性不是它的目標；所以本 client 以 fork 為準，fork 與上游 Matrix 不一致時，照 fork。協議規格在它的 `docs/design/chunked-upload-spec.md`，黃金向量在 `docs/design/wbf-vectors.json`，本 repo 只複製規格與向量，不共用程式碼 |
| **matrix-rust-sdk** | `vendor/matrix-rust-sdk` submodule，指上游 [`matrix-org/matrix-rust-sdk`](https://github.com/matrix-org/matrix-rust-sdk) | Matrix 基礎（登入、sync、房間、E2EE）。先用上游；需要改就把 submodule 改指自己的 fork |

## 取得

```bash
git clone --recurse-submodules http://ai.zooy.cc:30008/amaid/wbf-matrix-client.git
```

已 clone 但少了 submodule：

```bash
git submodule update --init
```
