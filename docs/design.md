# mvault 設計

ML / AI モデルの保管庫 (ミラー) と、計算ノードへの配布ツール。

この文書は仕様の合意用で、実装はこれから。例に出てくる ID やホスト名はすべてダミー。

## 1. 目的と前提

- HuggingFace / civitai のモデルを手元のミラーに保存し、上流の障害や削除に備える
- 計算ノードはミラーから取得する。上流に負荷を掛けず、ローカルネットワークで速く取れる
- 同じ内容のファイルは 1 つだけ保存する (sha256 による重複排除。ZFS 等の dedup は使わない)
- ノードに置くモデルは宣言ファイルで管理し、内容を sha256 で担保する (cargo / uv に近い使い勝手)
- 既に持っている内容は書き直さない (NVMe の寿命)

前提としないもの:

- ミラーと上流の同期。モデルは頻繁に更新されないので、取得は明示的な `pull` だけ
- ノードからミラーへの書き込み、ノードからのロックファイル操作。ノードはミラーを読むだけ

## 2. 全体像

```
上流 (HuggingFace / civitai)
   │  mvault mirror pull          (ミラーホスト上で実行)
   ▼
ミラー  <mirror>/public  ──  NFS / SSHFS / HTTP で読み取り専用公開
   │  mvault sync                 (各ノードの workspace で実行)
   ▼
ノード  <workspace>/.mvault/blobs → hardlink → models/...
```

コマンドは 2 系統に分かれる。

| 系統 | コマンド | 実行場所 | 必要なもの |
|---|---|---|---|
| ミラー | `mvault mirror init / pull / ls / rm / gc / export` | ミラーホスト | ミラーの root、上流のトークン |
| ノード | `mvault init / add / sync / update / ls / status / gc` | workspace | ミラーの URL |

## 3. 取得元の表記

manifest、lock、ミラーの内部で共通に使う正規形。

| 種類 | 正規形 | 例 |
|---|---|---|
| HuggingFace | `hf:<host>:<org>/<repo>` | `hf:huggingface.co:example-org/example-model` |
| civitai | `civitai:<host>:<modelId>@<versionId>` | `civitai:example.com:12345@67890` |

- HuggingFace の `<host>:` は省略でき、省略時は `huggingface.co`
- civitai のホストは `add` が必ず書く。手で書いて省略した場合は設定の `civitai.default_host` を使う
- civitai の `@<versionId>` を省くと「解決した時点の最新版」。lock が version を固定する
- ホストごとに独立した名前空間とする。API や ID が共通の別ドメインがあっても、別名としては扱わない
  - ホストの書き方は 1 通りに正規化する (DB のキーやディレクトリ名になるため): 小文字、末尾のドットは不可、ポートは先頭ゼロなしの 10 進
  - ポートは 443 なら省く。上流には常に https で接続し、https 以外の URL は受け付けない
  - 設定ファイルに書いたホスト (`civitai.default_host`) も、読み込むときに同じ規則で正規化する。書き方の違いでトークンが送られない、ということが起きないように
  - blob は sha256 で重複排除されるので、同じモデルを 2 つのホストから取っても増えるのは snapshot とメタデータだけ
- コマンドの引数では URL も受け付け、正規形に直してから使う (ページの URL を貼るだけで済むように)
  - `https://<hf host>/<org>/<repo>[/blob|resolve/<revision>/<path>]`
  - `https://<civitai host>/models/<modelId>[?modelVersionId=<versionId>]`
  - revision に `/` を含む URL (`…/resolve/refs%2Fpr%2F1/…`) も受け付ける。percent-encode された `/` を許すのは revision の部分だけ

## 4. ミラー

### 4.1 レイアウト

```
<mirror>/
  public/                                   # これだけを NFS export / HTTP の docroot にする
    blobs/sha256/ab/abcdef...               # 内容そのもの。拡張子なし、0444
    snapshots/
      hf/<host>/<org>/<repo>/
        refs/<revision>                     # 中身は commit hash (revision は percent-encode)
        <commit>.json                       # snapshot (4.3)
      civitai/<host>/<modelId>/
        latest                              # 中身は最新として解決した versionId
        <versionId>.json
    metadata/
      hf/<host>/<org>/<repo>/<commit>.json  # model info API の応答そのまま
      civitai/<host>/<modelId>/model.json   # models API の応答そのまま
      civitai/<host>/<modelId>/<versionId>.json
  private/                                  # 公開しない
    mvault.db                               # SQLite (WAL)
    tmp/                                    # ダウンロード途中のファイルとそのロック
    config.toml                             # ミラーの設定 (任意)
```

- `<host>` にポートが付く場合、ディレクトリ名では `:` を `_` に置き換える (`hub.example.com_8443`)。`:` をファイル名に使えない環境があり、`_` はホスト名に現れない

- `public/` と `private/` は同じファイルシステムに置く (`tmp/` から `blobs/` へ `rename()` するため)
- トークンや設定は `private/` とホームディレクトリにしか置かない。`public/` をそのまま公開しても漏れない
- ノードが読むのは `public/` 以下の相対パスだけ。symlink は使わない (HTTP でも同じ契約にするため)

### 4.2 状態管理 (SQLite)

正は `private/mvault.db`。`public/snapshots/` は DB から書き出した結果で、DB から作り直せる (`mvault mirror export`)。

テーブルの概形:

| テーブル | 主な列 | 内容 |
|---|---|---|
| `blobs` | `sha256`, `size`, `created_at` | `public/blobs/` にある blob |
| `snapshots` | `id`, `source`, `revision`, `fetched_at` | HF の commit / civitai の version ごとに 1 行 |
| `snapshot_files` | `snapshot_id`, `path`, `file_id`, `size`, `sha256`, `fetched`, `is_primary` | snapshot に含まれるファイル。`fetched` が立っていれば `sha256` の blob を参照している。未取得の間の `sha256` は上流が示した値 |
| `refs` | `source`, `name`, `revision` | HF の revision 名 → commit、civitai の latest → version |

- SQLite はミラーホストのローカルでだけ開く。NFS 越しには開かない
- blob の確定と参照の追加は 1 つの書き込みトランザクションで行う: `BEGIN IMMEDIATE` → `tmp/` から `rename()` → 行の追加 → `COMMIT`
  - ファイルの `fsync` はトランザクションの前に済ませる。大きなファイルでは時間が掛かり、その間ほかの pull を待たせないため
- GC も 1 つの書き込みトランザクションで行う: 参照のない blob を選ぶ → ファイルを消す → 行を消す → `COMMIT`
  - 書き込みトランザクションは直列なので、pull と GC の間に別のロックファイルは要らない
- snapshot JSON は、トランザクションの後に一時ファイルへ書いて `rename()` で置き換える
  - 書き出しは `mirror export` と同じ 1 つの処理。取得状況を変えたコマンド (`pull`、`gc`) が、終わるときに、変えた snapshot について呼ぶ
  - コマンドが中断されると書き出されない。次の `pull` か `mirror export` で揃う

### 4.3 snapshot JSON

ノードがモデルを解決するときに読む。

```json
{
  "schema": 1,
  "source": "hf:huggingface.co:example-org/example-model",
  "revision": "0123456789abcdef0123456789abcdef01234567",
  "files": [
    { "path": "model.safetensors", "size": 123456, "sha256": "ab…", "kind": "file" },
    { "path": "assets/sample.mp4", "size": 987654, "kind": "file", "fetched": false }
  ]
}
```

- `fetched: false` は「上流にはあるがミラーは取得していない」ファイル (exclude したもの)
  - ノードは「ミラーに無い」と「ミラーが除外している」を区別して表示できる
  - sha256 は上流が教えてくれる場合 (HF の LFS ファイル、civitai) だけ入る
- civitai のファイルは `file_id` と `primary` を持ち、`path` はファイル名
- `kind` は現状 `file` のみ。将来、配置対象でない付随ファイル (プレビュー画像など) を足すための欄

### 4.4 `mvault mirror pull`

```sh
mvault mirror pull <URL | 正規形>
mvault mirror pull hf example-org/example-model [--revision <rev>] [--exclude <glob>]... [--exclude-regexp <正規表現>]...
mvault mirror pull civitai 12345@67890 [--host example.com] [--file-id <id>]... [--all-files]
```

- HuggingFace: 指定した revision のファイルを、exclude に一致するものを除いてすべて取得する
  - `--exclude` は glob、`--exclude-regexp` は正規表現。どちらかに一致したファイルを除く (例: `--exclude assets --exclude-regexp '\.mp4$'`)
    - 種別は書き方から推測せず、オプション (manifest ではキー) で決める
    - glob の意味は `.gitignore` と同じ
      - `/` を含まない glob は、ファイル名と各階層のディレクトリ名に対して照合する (どの深さでも一致)。`assets` はその名前のディレクトリ以下すべて、`*.mp4` はすべての mp4
      - 先頭か途中に `/` を含む glob は、リポジトリのルートからのパスと、その各階層のディレクトリに対して照合する。`/assets` はルート直下の `assets` だけで、`docs/assets` には一致しない
      - `*` は `/` をまたがず、`**` はまたぐ
    - 正規表現は、ルートからのパスに対する部分一致
  - ファイルを指す URL (`…/blob/<revision>/<path>`) は受け付けない。ミラーはリポジトリ単位で取得する
  - README や config.json などの非 LFS ファイルも blob として保存する
  - model info API の応答を `metadata/` に保存する
- civitai: 既定は version の primary ファイルだけ。`--file-id` で選ぶか `--all-files` で全ファイル
  - models API と model-versions API の応答を `metadata/` に保存する (説明文、trainedWords、ファイルの hash、プレビュー画像の URL など)
  - プレビュー画像そのものは取得しない
- 同じ snapshot を条件を変えて pull し直すと、取得済みファイルは和集合になる
- あるファイルの取得に失敗しても、残りのファイルは続けて取得し、失敗は最後にまとめて報告する。終了コードは 0 以外
  - 取得できたファイルは snapshot に反映して公開する
  - `refs/<revision>` と `latest` を動かすのは、選んだファイルをすべて取得できた pull だけ。失敗した pull の後も、名前は以前の (ノードが使える) snapshot を指し続ける
- 取り込みの手順
  1. 上流が示す sha256 の blob が既にあれば、ダウンロードせず参照だけ追加する
  2. `private/tmp/` にダウンロードする (4.4.1)。上流が示す sha256 とサイズを渡し、照合まで任せる
  3. 4.2 のトランザクションで確定する
- 並行実行: 複数の pull を同時に走らせてよい
  - 同じファイルのダウンロードは `private/tmp/<key>.lock` の flock で 1 つに絞り、後から来た方は待つ。プロセスが死ねばロックは外れる
  - ロックが取れたら、待っている間に他の pull が取得していないかを見直す (上流が sha256 を示さないファイルも)
  - ダウンロードが終わったら、ロックを握ったままロックファイルを消す (消し方の決まりは 4.5 の `gc`)
- 結果に出す「未取得」の数は、pull の後に snapshot で未取得のまま残っているファイルの数
- ミラーが無ければエラーにする (`mirror init` で作る)

#### 4.4.1 ダウンロード (ミラーとノードで共通)

上流からの取得、HTTP のミラーからの取得、マウントしたミラーからのコピーは、同じ手順を通る。違うのは転送元だけ。

- 途中ファイルに書きながら sha256 を計算する。途中ファイルは残し、次回は続きから取る
- 呼び出し側は、分かっていれば期待する sha256 とサイズを渡す。照合と、食い違ったときに途中ファイルを捨てるところまでをダウンロードが行う
- 始める前に、途中ファイルの sha256 が期待値と同じなら、何も要求せずに完了とする (前回、ダウンロードは終わったが確定の前に止まった場合)
- 完了の判定は、転送元が示した全長 (`Content-Length` / `Content-Range`、ファイルの長さ) まで受け取ったこと
  - 期待するサイズと全長が違えば、本文を読む前にエラーにする。全長を超える本文は書かない
  - `416` で `Content-Range: bytes */<全長>` が途中ファイルの長さと同じなら、完了とする
- 続きを取る (`Range`) 条件
  - 期待する sha256 があれば、実行をまたいで続きを取る。途中で上流が差し替わっていれば最後の照合で分かり、途中ファイルを捨てる
  - 無ければ、実行をまたいでは続けず、毎回最初から取る。差し替わっても検出できないため
  - 同じ実行の中の再試行では、最初の応答の `ETag` (無ければ `Last-Modified`) を `If-Range` で送って続きを取る。どちらも無く、期待する sha256 も無ければ、最初から取り直す
- 接続エラー、途中切断、408 / 429 / 5xx は指数バックオフで再試行する
  - 失敗の連続回数を数え直すのは、それまでに届いた最大の位置より先へ進んだときだけ。最初から取り直した分は、進んだことにしない
- 呼び出し側が渡せるヘッダは認証だけ。別のホストへのリダイレクトには付けない (4.6)

### 4.5 その他のミラーコマンド

`gc` 以外は未実装。

- `mvault mirror init`: `public/`、`private/`、DB を作る
  - 他のミラーコマンドは作らない。DB が無ければエラーにする (root を打ち間違えたときに、空のミラーを作って成功しないように)
- `mvault mirror ls [source]`: ミラーにある snapshot と、そのファイル・サイズ・取得状況を表示する
- `mvault mirror rm <source>[@<revision>]`: snapshot を消す (blob は GC が消す)。頻度は低い想定
- `mvault mirror gc [-n] [--min-age <期間>]`: どの snapshot からも参照されない blob を消す
  - 作成から `--min-age` (既定 1 日) 経っていないものは消さない
  - blob の削除は 4.2 と同じ書き込みトランザクションの中で、blob 1 つごとに行う。実行中の pull は 1 つ分待たされるだけで、消された blob を使い始めることはない
    - 途中で止まっても、消した分は DB からも消えている。ファイルを消してから commit するまでに落ちた 1 つだけ、ファイルのない行として残り、次の `gc` が下の手順で直す
  - `tmp/` の途中ファイルは、ロックされておらず 7 日より古いものだけ消す。終わったダウンロードのロックファイルも消す
    - ロックファイルはロックを握ったまま消す。ロックを取る側は、取れた後にそのファイルがまだ同じパスにあるかを確かめ、無ければ取り直す
  - DB に行がない `blobs/` 内のファイルを報告する。消さない (確定途中の pull と区別できないため)
    - blob として扱うのは、名前が digest で、その digest の位置 (`sha256/<先頭 2 桁>/<digest>`) にあるファイルだけ。それ以外のファイルも報告だけして消さない
  - ファイルがない DB の行を、それを使っていた snapshot とともに報告し、終了コードを 0 以外にする
    - 行を消し、その blob を内容としていたファイルを未取得に戻す。次の `pull` がダウンロードし直す (`-n` では報告のみ)
    - 未取得に戻した snapshot は、`gc` の最後に `public/snapshots/` へ書き出し直す
  - `-n` は何も変えない。ロックファイルも作らず、消さない
- `mvault mirror export`: `public/snapshots/` を DB から書き直す (修復用。中断された `pull` の後など)

### 4.6 認証

- トークンはコマンドライン引数で受け取らない (`ps` やシェル履歴に残る)
- HuggingFace: `HF_TOKEN` → `HF_TOKEN_PATH` → 公式 CLI が保存するトークンファイル
  - `huggingface.co` にだけ送る。他のホストの hub には送らない
- civitai: `CIVITAI_TOKEN` → `CIVITAI_TOKEN_FILE` (既定 `~/.config/mvault/civitai-token`) → 設定の `civitai.token_command` の出力 (未実装)
  - ミラーの `private/config.toml` の `civitai.default_host` に書いたホストにだけ送る。URL の形だけで civitai 系と判定するので、任意の URL にトークンを送らないため
- トークンは API のホストにだけ送り、CDN など別ホストのダウンロード URL やリダイレクト先には送らない
- ログにはトークンの出所だけを出し、値は出さない
  - pull ごとに 1 行。送らなかったときも、その旨と理由 (トークンが無い、ホストが違う) を出す
  - 401 / 403 のときは、どこにトークンを置けばよいかを添える

## 5. ノード

### 5.1 workspace のレイアウト

```
mlenv/                              # workspace
  mvault.toml                       # manifest (人が書く。git 管理)
  anima.toml, qwen.toml, ...        # include で分割した manifest
  mvault.lock                       # 解決結果 (mvault が書く。git 管理)
  .mvault/                          # ノード固有。git 管理しない (.mvault/.gitignore で自身を除外)
    blobs/sha256/ab/abcdef...       # ミラーと同じ構造、0444
    tmp/                            # ダウンロード途中
    placed.json                     # 配置の記録 (5.1.1)
    lock                            # この workspace を変えるコマンドの排他ロック (5.5)
    config.toml                     # 任意。ノード設定の上書き
  models/vae/x.safetensors          # blob への hardlink
```

- 配置は hardlink。blob と配置先は同じファイルシステムにある必要がある
  - blob が 0444 なので、ふつうの権限ではアプリから同じ inode を上書きできない
    - root (コンテナ内など) や、書く前に権限を付け直すツールは上書きできる。そのときは blob の中身も変わる
  - 配置したファイルを `rm` しても blob は残る
- 「blob と同じファイル (hardlink)」を mvault の管理下とみなす
  - 同じファイルかどうかは、Unix では dev と inode、Windows ではボリュームのシリアル番号とファイルインデックスで比べる
- ノードに index や DB は置かない。lock から作れない情報は配置の記録だけ

#### 5.1.1 配置の記録 (`.mvault/placed.json`)

`sync` が配置したファイルを覚えておく。manifest と lock は他のノードで編集され `git pull` で届くので、manifest から外れたファイルは、今の manifest と lock だけでは見つけられないため。

```json
{
  "schema": 1,
  "files": [
    { "dest": "models/vae/x.safetensors", "sha256": "ab…", "size": 456, "mtime_ns": 1700000000000000000 }
  ]
}
```

- `sync` が配置するファイル、配置済みだと確かめたファイルを書く。`size` と `mtime_ns` はそのときの blob の値
  - 書くのはリンクを張る前。張った後に書くと、その間に止まったファイルは記録が無いまま残り、manifest から外れても `prune` で消せない。先に書いて張れなかった分は、blob と同じファイルでないので、次の `sync` が記録から外す
- `size` と `mtime_ns` は blob が書き換えられていないかを見るためのもの
  - hardlink は inode を共有するので、この 2 つは配置先ではなく blob の性質。配置先のどれか 1 つが上書きされれば、blob とそのすべての配置先で変わる
  - 同じ値なら、blob の中身は sha256 のままとみなす (git の index と同じ考え方。中身は読まない)
  - 違えば blob を読み直して sha256 を確かめる。合っていれば記録の値を更新するだけ (`touch` されたなど)。合わなければ blob の破損として扱う (5.5 の `sync`)
- 記録から外すのは、`sync --prune` がファイルを消したとき、または配置先が blob と同じファイルでなくなっていたとき (ユーザが置き換えた。もう mvault のものではない)
- 削除の判断はこの記録にだけ基づく。記録に無いファイルは、blob と同じファイルでも消さない
  - 記録が無くなっても壊れるものは無い。次の `sync` が、manifest で配置しているファイルについて書き直す
  - 記録を導入する前に配置して、すでに manifest から外れていたファイルは、`prune` の対象にならない。`ls --untracked` に出る
- 読み書きは workspace の排他ロック (5.5) の中で行う。書き戻しは一時ファイルからの `rename()`
- blob を複数の workspace で共有していても、記録は workspace ごと

### 5.2 設定

`~/.config/mvault/config.toml` を基本にし、`<workspace>/.mvault/config.toml` があれば上書きする。

```toml
mirrors = ["file:///mnt/vault/public", "https://mvault.example.internal/"]   # 先頭から順に試す
allow_official = false     # true: ミラーに無いファイルを上流から直接取得する (ミラー無しの運用もこれ)
blobs = "/nvme/mvault/blobs"   # 任意。既定は <workspace>/.mvault/blobs。複数 workspace で共有するとき

[civitai]
default_host = "example.com"
```

- ミラーは URL で指定する。`file://` は NFS / SSHFS / CIFS のマウント先、`http(s)://` は `public/` を公開した Web サーバー
  - パスでも書ける。相対パスは、`.mvault/config.toml` では workspace からの相対 (`blobs` と同じ)。`init --mirror` は絶対パスに直して書く。ノードの設定 (`~/.config/…`) では相対パスはエラー
  - HTTP では、相対パスの各セグメントを percent-encode して URL にする。ミラー上のファイル名 `feature%2Fx` は、URL では `feature%252Fx`
  - 読めないミラー (落ちている、マウントされておらずディレクトリが無い) は、その実行の間は使わず、警告は 1 回だけ出す。「ミラーに無い」とは区別する
- どちらの経路でも、`tmp/` に書きながら sha256 を計算し、検証してから `blobs/` に入れる。中断したら続きから取得する (4.4.1)
- `blobs` を変えると、`tmp/` もその隣になる (同じファイルシステムに置くため)。blob を共有する workspace は、`tmp/` とその中のロックも共有する
- `blobs` と `tmp/` は、manifest がファイルを置く場所の外に置く。その中に入る配置先があればエラー

### 5.3 manifest (`mvault.toml`)

1 エントリが 1 つのモデル (HF のリポジトリと revision、または civitai の version) に対応する。

```toml
include = ["anima.toml", "qwen.toml"]    # workspace からの相対パス。glob 可。include 先からの include は不可

# リポジトリから選んだファイルを別々の場所へ
[[model]]
source = "hf:example-org/example-repackaged"
revision = "main"                         # 省略時は main
files = [
  { path = "split_files/vae/x_vae.safetensors", dest = "models/vae/" },
  { path = "split_files/diffusion_models/x.safetensors", dest = "models/diffusion_models/renamed.safetensors" },
  { glob = "split_files/upscaler/4x_*", dest = "models/upscale_models/" },
]

# リポジトリ丸ごと (ツリーを保って配置)
[[model]]
source = "hf:example-org/example-diffusers"
dest = "models/diffusers/example/"
exclude = ["assets"]
exclude_regexp = ['\.mp4$']

# civitai: version の primary ファイル
[[model]]
source = "civitai:example.com:12345@67890"
dest = "models/loras/"

# civitai: version のファイルを file id で選んで配置
[[model]]
source = "civitai:example.com:23456@78901"
files = [
  { file_id = 111, dest = "models/checkpoints/" },
  { file_id = 222, dest = "models/vae/renamed.safetensors" },
]
```

- `include` の glob は、ディレクトリが無ければ一致なしとして扱う (エラーにしない)
- `dest` は workspace からの相対パス。workspace の外、`.mvault/`、manifest (include 先も)、lock、blob の置き場所 (5.2)、git のディレクトリ (どの階層の `.git` も) は指せない
- `files` があるとき: 書いたファイルだけを配置する
  - 1 つの要素に書けるのは `path` / `glob` / `file_id` のどれか 1 つ。種別を書き方から推測しない (名前に `[` や `*` を含むファイルも選べるように)
  - `path` はリポジトリ内のパスそのもの。`dest` が `/` で終わるときはその下に元のファイル名で、そうでなければ `dest` をファイル名として配置する
  - `glob` はパターン (`*` は `/` をまたがず、`**` はまたぐ)。`dest` はディレクトリで、一致したファイルをファイル名だけで置く。名前が重なればエラー。1 つも一致しなければエラー
  - civitai では `file_id` で指定する (version 内でファイル名が重複しうるため)。`dest` の扱いは `path` と同じ
  - civitai の「全ファイル」という指定はない。必要なファイルを列挙する
- `files` がなく `dest` があるとき
  - HuggingFace: リポジトリ全体を、`exclude` (glob) か `exclude_regexp` (正規表現) に一致するものを除いて、ツリーを保って `dest` の下に置く
    - それぞれの意味は `mirror pull` の `--exclude` / `--exclude-regexp` と同じ (4.4)
  - civitai: primary ファイルを置く
- 同じ `source` と `revision` のエントリが複数あれば、読み込み時に 1 つにまとめる (`add` が末尾への追記だけで済むように)
- 同じ配置先に違うファイルが来ればエラー。あるファイルの配置先が、別のファイルの配置先のディレクトリになる場合もエラー
- `source` は正規形で書く。URL は `mvault add` が正規形に直す
- 知らないキーはエラー

### 5.4 lock (`mvault.lock`)

mvault が解決結果を書くのはこのファイルだけ。include した manifest の分もすべてここに入る。

```toml
version = 1

[[model]]
source = "hf:huggingface.co:example-org/example-repackaged"
revision = "main"
resolved = "0123456789abcdef0123456789abcdef01234567"
files = [
  { path = "README.md", size = 1234 },
  { path = "split_files/upscaler/2x_foo.pth", size = 99, sha256 = "…" },
  { path = "split_files/upscaler/4x_foo.pth", size = 123, sha256 = "…" },
  { path = "split_files/vae/x_vae.safetensors", size = 456, sha256 = "…" },
]
```

- キーは `source` (正規形) と `revision`。`dest` や、どの manifest ファイルに書いたかは含まない
  - 配置先を変えても、エントリを別の toml に移しても、lock は有効
- civitai のファイルは `file_id` も持つ (`path` はファイル名)。primary ファイルには `primary = true` が付く
- `files` は、解決した commit / version に上流が持つ全ファイルの一覧。manifest で選んでいないファイルも入る
  - どれを配置するかは、`sync` のたびに manifest の `files` / `exclude` / glob をこの一覧に当てて決める
  - そのため `files` / `exclude` / `dest` を編集しても再解決は起きない。再解決が要るのは `source` か `revision` を変えたときと `mvault update` だけ
  - 展開結果だけを書く形にしなかったのは、glob や `exclude` を書き換えたときに lock が古いかどうかを判定できないため
  - 上流が進んでファイルが増えても、`update` するまで一覧は変わらない
- 上流から直接解決したファイルは、sha256 が分からないことがある (HuggingFace の API が sha256 を返すのは LFS ファイルだけ)
  - その場合は `sha256` を書かずに lock に入れ、`sync` がミラーの snapshot から埋める。ミラーに無ければ、上流から取得したときに計算した値を書き込む
  - 一度書き込まれたら、以後は他のファイルと同じく照合に使う
  - ミラーの snapshot から解決した場合は、全ファイルの sha256 が最初から入る
- `path` は読み込むときに相対パスとして検証する (絶対パス、`..`、空のセグメント、制御文字、`.git` のセグメントはエラー)。ミラーの snapshot JSON の `path` も同じ
  - `.git` は大文字小文字を区別せず、末尾のドットや空白を除いて比べる。短い DOS 名の `git~1` も同じ。そこに置いた `config` は、その場所で git を実行したときにコマンドを実行させられる
  - lock もミラーも他から届くものなので、そこに書かれたパスで配置先が workspace の外に出ないようにする
- 出力は決まった順 (source、path) に並べ、差分を読みやすくする
- manifest と lock はセットで git 管理する

### 5.5 コマンド

manifest を使うコマンドは、カレントディレクトリから親へ `mvault.toml` を探す。

**`mvault init [--mirror <URL>]... [--allow-official]`**

- `mvault.toml` が無ければ作る。`.mvault/` と `.mvault/.gitignore` を作る
- `--mirror` / `--allow-official` を付けたときだけ `.mvault/config.toml` に書く。付けなければノードの設定を使う

**`mvault add <URL | 正規形> --dest <path> [--path <path>]... [--glob <pattern>]... [--file-id <id>]... [--file <toml>]`**

- `--path` (パスそのもの) と `--glob` (パターン) は HuggingFace、`--file-id` は civitai のファイル指定。URL に含まれるファイルのパスは `--path`、`?fileId=` は `--file-id` と同じ意味で読む
- HuggingFace は `--revision <rev>` も指定できる (URL に revision があればそれを使う)
- manifest には正規形で書く。`huggingface.co` はホストを省いて `hf:<org>/<repo>` と書く
- モデルの情報を解決する。ミラーの snapshot を先に見て、無ければ上流の API に問い合わせる
- manifest の末尾に `[[model]]` をテキストとして追記する (既存のコメントや並びを崩さない)。追記先は既定で `mvault.toml`、`--file` で include 先を指定する
- lock に解決結果を書く
- モデルファイルのダウンロードはしない
- 同じ内容のエントリが既にあれば、何も書かずにその旨を表示する
- 書き込む前に確かめる: モデルが解決できること、指定したファイルがそのモデルにあること、配置先が他のファイルとぶつからないこと。どれかが駄目なら何も書かない
  - manifest に未解決の別のエントリがあっても失敗しない。配置先の衝突は、解決済みのエントリとだけ確かめる

**`mvault sync [-n] [--locked] [--prune] [--force]`**

1. manifest にあって lock にないエントリを、`add` と同じ手順で解決して lock に書き足す。`--locked` ではエラーにする
2. 配置先と blob の状態を調べ、行う操作の一覧を作る: 取得する blob、配置するファイル、差し替えるファイル、衝突、blob の破損、(`--prune` なら) 消すファイル
   - 配置先に何も無ければ配置する。既に同じ blob の hardlink なら何もしない
   - 管理下の別の内容なら差し替える
   - 管理下でないファイルがあれば上書きせず、衝突として報告する (`--force` で上書き)。衝突があれば終了コードは 0 以外
   - 「管理下」とは、blob と同じファイル (hardlink) であること。記録にあればその blob と、無ければサイズが同じ blob と比べる
     - 差し替えても中身は blob に残るので、記録に無いファイルも差し替えてよい。消してよいかどうか (`--prune`) は記録で決める
   - blob の破損 (5.1.1): 記録と `size` か `mtime_ns` が違い、読み直した sha256 も合わない blob。報告して、その blob と配置先には手を付けない。終了コードは 0 以外
     - 中身が変わったのは blob そのものなので、同じ blob を張り直しても直らない。その blob を使う他の配置先も同じく壊れている
     - `--force`: 壊れた blob を store から外して取得し直し、配置先を張り直す。外した時点で、元の配置先は管理下でないただのファイルになる (書き換えられた中身のまま)。そのうち今回配置しないものは、消さずに記録から外す
     - blob を共有する他の workspace では、その配置先が次の `sync` で衝突として報告される
3. 一覧にある blob をミラーから取得する (配置するファイルのうち、`.mvault/blobs/` に無いものだけ)
   - ミラーは設定の順に試す。読めないミラーは警告を出して次へ進む
   - 取得した内容の sha256 が lock と違えば、そのミラーの名前を挙げてエラーにする
   - どのミラーにも無ければエラー。ミラーホストで実行する `mvault mirror pull …` を表示する
   - `allow_official = true` のときは上流から取得する
4. 配置の記録に、これから配置するファイルを書く (5.1.1)
5. 配置先に hardlink する。blob の隣の `tmp/` に一時的な link を作り、`rename()` で配置先に移す
   - 配置先は、無いか、前のファイルか、新しいファイルかのどれかで、途中の状態にならない
   - 途中で止まって残った一時 link は `gc` が消す。配置先のディレクトリには何も残さない
6. 記録にあって今回配置しないファイルを数え、`--prune` が無ければ「manifest から外れたファイルが N 個ある (`--prune` で消す)」と表示する
7. `--prune`: 記録にあって今回配置しないファイルのうち、次をすべて満たすものだけを消す
   - 記録した sha256 の blob と同じファイルである
   - その blob が破損していない
   - 満たさないものは消さずに報告する。blob と同じファイルでなくなったものは記録から外し、破損した blob のものは記録に残す (`ls` で見えるように)
   - 消した後に空になったディレクトリは消さない
   - blob は消さない。manifest に戻せば、次の `sync` は取得せずに配置し直す
8. 配置の記録を書き直す (消したファイルと、blob と同じファイルでなくなったものを外す)

- 削除はこの `--prune` と `gc` だけが行い、どちらも明示したときだけ動く。誤って消すより、消し残す方を選ぶ
- `-n` は 2. の一覧を表示して終わる。何も変えない (lock も書かない)
  - 実行は一覧に従うだけなので、`-n` の表示と実際の動作は同じ判断から出る
- workspace を変えるコマンド (`add`、`sync`、`update`) は `.mvault/lock` を排他で取り、同じ workspace では 1 つずつ動く。後から来た方は待つ
  - `mvault.lock` と配置の記録の読み書きは、このロックの中で行う
- blob を共有する別の workspace の `sync` は同時に走ってよい。同じ blob の取得は `tmp/` の flock で 1 つに絞る

`update` / `ls` / `status` は未実装。

**`mvault update [source]...`**

- revision (HF のブランチ名、version を省いた civitai) を解決し直して lock を更新する。配置は次の `sync` で行う

**`mvault ls [--untracked] [path]`**

- 配置先のファイル基準の一覧 (再帰)。パス、状態、サイズ、sha256、取得元
- `sync` の 2. と同じ調べ方をする。何も変えない
- 状態
  - manifest で配置するファイル: 未取得 / 取得済み / 配置済み / 衝突 / 破損の疑い
  - 記録にあって manifest で配置しないファイル: 外れた (`sync --prune` で消える) / 破損の疑い (消えない)
  - 「破損の疑い」は、blob の `size` か `mtime_ns` が記録と違うもの。`ls` は blob を読み直さない。確かめるのは `sync`
- `--untracked`: manifest で配置せず記録にもないファイルも出す。調べるのは、manifest か記録がファイルを置くディレクトリの中
  - blob と同じファイルなら「記録なし」(記録の導入前に配置したものなど)、そうでなければ「管理外」(ユーザが置いたもの)
  - モデルのディレクトリには学習した LoRA など管理外のファイルが普通にあるので、既定では出さない

**`mvault status [source | URL]`**

- モデル基準の一覧。manifest の revision、lock の解決結果、ファイル数と取得済みの数、ミラーにあるかどうか

**`mvault gc [-n] [--min-age <期間>]`**

- `.mvault/blobs/` のうち、どこからも hardlink されていない blob (`st_nlink == 1`) を消す
  - blob を複数の workspace で共有していても、どこかに配置されていればリンク数が 2 以上なので残る
  - 最後のリンクが外れてから `--min-age` (既定 1 日) 経っていないものは消さない。manifest から外して戻すたびに取得し直さないため
    - リンク数が変わると inode の ctime が更新されるので、blob の ctime を「最後にリンクが外れた時刻」として使う。記録は要らない
    - ctime は chmod などでも進むが、その場合は消すのが遅れるだけ
  - `sync` は取得から配置までリンク数 1 の blob を持つ。`sync` が共有ロック、`gc` が排他ロック (`.mvault/tmp/gc.lock`) を取って、その間に消さないようにする
  - 7 日より古い途中ファイルと、終わったダウンロードのロックファイルも消す。止まった `sync` が残した一時 link も消す
  - blob として扱うのは、名前が digest で、その digest の位置にあるファイルだけ (4.5 の `mirror gc` と同じ)。それ以外のファイルは報告だけして消さない
  - `-n` は何も変えない。ロックファイルも作らず、消さない。表示するのは、実際に消すものと同じ判断で選んだもの
  - リンク数を調べられない環境 (Unix 以外) ではエラーにする

## 6. テストと公開リポジトリとしての方針

- テストは実サービスに一切アクセスしない。テスト内で立てる HTTP サーバーに HuggingFace / civitai の API 応答とファイルを返させる
- コード、テスト、文書に実在のモデル ID や特定のモデル名を書かない。`example-org/example-model`、`example.com`、連番の ID を使う
- コードが名前を知っている上流は、既定のホストだけ

## 7. 実装

- Rust、単一の crate (bin + lib)
- 上流の API は HTTP で直接呼ぶ (公式クライアントライブラリには依存しない)
- 依存 crate は実装時に決める。SQLite は同梱ビルドにして、ミラーホストのシステムライブラリに依存しない
- 外から届く値は、読み込むときに検証して専用の型にする。文字列のまま持ち回らない
  - 相対パス (lock、snapshot JSON、API の応答): workspace や blob の置き場所の外を指せない型
  - ホスト名 (取得元、設定): 正規化済みの型。比較は型どうしで行う
- 調べて決める処理と、決めたことを実行する処理を分ける (`sync`、`gc`)。`-n` と `ls` は前者だけを使う

## 8. 前身 (ml-model-vault) からの移行

どちらも使い捨てのスクリプトとして提供し、本体には入れない。

- ミラー
  - `blobs/` は同じ構造なので `public/blobs/` へ移動するだけ (同じファイルシステムなら `rename()`)
  - 旧 `index/*.json` の出所と `repositories/` の symlink から、`snapshots` / `snapshot_files` / `refs` を作る
    - civitai のホストは index に記録された実際の取得元を使う
    - 旧ミラーは「取得しなかったファイル」を記録していないので、移行した snapshot には取得済みのファイルしか入らない。完全な一覧が要るものは `mirror pull` し直す (blob は再利用される)
  - 旧 `metadata/` を新しいパスへ移す
  - 最後に `mvault mirror export` で `public/snapshots/` を書き出す
- manifest
  - 旧 `[[files]]` を取得元ごとにまとめて `[[model]]` にする
  - 旧 lock の commit / version / sha256 から新しい `mvault.lock` を作る
  - 旧 `src = "sha256:…"` のエントリは対応する取得元がないので、変換せずに一覧で報告する

## 9. 将来の TODO

- glob に一致したファイルの改名 (拡張子の変更など)
- manifest の `files` での正規表現 (`{ regexp = … }`)。キーを足すだけで、形式は変わらない
- HuggingFace の LFS でないファイルを、API が返す git の blob id で照合する (今はサイズだけ)
- civitai のプレビュー画像の保存 (`mirror pull --with-images`)
- ノードからミラーへのアップロード (ノードが上流から直接取得したものをミラーに入れる)
- ミラーとノードが同じマシンにあるときのコピー省略 (reflink / hardlink)
