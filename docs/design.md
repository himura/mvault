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
| ミラー | `mvault mirror pull / ls / rm / gc / export` | ミラーホスト | ミラーの root、上流のトークン |
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
  - blob は sha256 で重複排除されるので、同じモデルを 2 つのホストから取っても増えるのは snapshot とメタデータだけ
- コマンドの引数では URL も受け付け、正規形に直してから使う (ページの URL を貼るだけで済むように)
  - `https://<hf host>/<org>/<repo>[/blob|resolve/<revision>/<path>]`
  - `https://<civitai host>/models/<modelId>[?modelVersionId=<versionId>]`

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
- GC も 1 つの書き込みトランザクションで行う: 参照のない blob を選ぶ → ファイルを消す → 行を消す → `COMMIT`
  - 書き込みトランザクションは直列なので、pull と GC の間に別のロックファイルは要らない
- snapshot JSON は、トランザクションの後に一時ファイルへ書いて `rename()` で置き換える

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
mvault mirror pull hf example-org/example-model [--revision <rev>] [--exclude <pattern>]...
mvault mirror pull civitai 12345@67890 [--host example.com] [--file-id <id>]... [--all-files]
```

- HuggingFace: 指定した revision のファイルを、exclude に一致するものを除いてすべて取得する
  - `--exclude` はリポジトリのルートからのパスに対する glob。`/…/` で囲むと正規表現 (例: `--exclude assets --exclude '/\.mp4$/'`)
    - `/` を含まない glob は、ファイル名と各階層のディレクトリ名に対して照合する (どの深さでも一致)。`assets` はその名前のディレクトリ以下すべて、`*.mp4` はすべての mp4
    - `/` を含む glob は、ルートからのパスと、その各階層のディレクトリに対して照合する。`*` は `/` をまたがず、`**` はまたぐ
    - 正規表現はパス全体に対する部分一致
  - ファイルを指す URL (`…/blob/<revision>/<path>`) は受け付けない。ミラーはリポジトリ単位で取得する
  - README や config.json などの非 LFS ファイルも blob として保存する
  - model info API の応答を `metadata/` に保存する
- civitai: 既定は version の primary ファイルだけ。`--file-id` で選ぶか `--all-files` で全ファイル
  - models API と model-versions API の応答を `metadata/` に保存する (説明文、trainedWords、ファイルの hash、プレビュー画像の URL など)
  - プレビュー画像そのものは取得しない
- 同じ snapshot を条件を変えて pull し直すと、取得済みファイルは和集合になる
- 途中で失敗しても、それまでに取得できたファイルは snapshot に反映して公開する
- 取り込みの手順
  1. 上流が示す sha256 の blob が既にあれば、ダウンロードせず参照だけ追加する
  2. `private/tmp/` に書きながら sha256 を計算する。途中ファイルは残し、次回は `Range` で続きを取る
     - 続きを取るのは、上流が sha256 を示すファイルだけ。示さないファイルは、上流で差し替わっても検出できないので、毎回最初から取る
  3. 上流が示す sha256 とサイズに照合する。食い違えば途中ファイルを捨ててエラー
  4. 4.2 のトランザクションで確定する
- 並行実行: 複数の pull を同時に走らせてよい
  - 同じファイルのダウンロードは `private/tmp/<key>.lock` の flock で 1 つに絞り、後から来た方は待つ。プロセスが死ねばロックは外れる
- 接続エラー、途中切断、408 / 429 / 5xx は指数バックオフで再試行する

### 4.5 その他のミラーコマンド

未実装。

- `mvault mirror ls [source]`: ミラーにある snapshot と、そのファイル・サイズ・取得状況を表示する
- `mvault mirror rm <source>[@<revision>]`: snapshot を消す (blob は GC が消す)。頻度は低い想定
- `mvault mirror gc [-n] [--min-age <期間>]`: どの snapshot からも参照されない blob を消す
  - 作成から `--min-age` (既定 1 日) 経っていないものは消さない
  - `tmp/` の途中ファイルは、ロックされておらず十分古いものだけ消す
  - DB に行がない `blobs/` 内のファイル、ファイルがない DB の行を報告する
- `mvault mirror export`: `public/snapshots/` を DB から書き直す (修復用)

### 4.6 認証

- トークンはコマンドライン引数で受け取らない (`ps` やシェル履歴に残る)
- HuggingFace: `HF_TOKEN` → `HF_TOKEN_PATH` → 公式 CLI が保存するトークンファイル
  - `huggingface.co` にだけ送る。他のホストの hub には送らない
- civitai: `CIVITAI_TOKEN` → `CIVITAI_TOKEN_FILE` (既定 `~/.config/mvault/civitai-token`) → 設定の `civitai.token_command` の出力 (未実装)
  - ミラーの `private/config.toml` の `civitai.default_host` に書いたホストにだけ送る。URL の形だけで civitai 系と判定するので、任意の URL にトークンを送らないため
- トークンは API のホストにだけ送り、CDN など別ホストのダウンロード URL やリダイレクト先には送らない
- ログにはトークンの出所だけを出し、値は出さない

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
    config.toml                     # 任意。ノード設定の上書き
  models/vae/x.safetensors          # blob への hardlink
```

- 配置は hardlink。blob と配置先は同じファイルシステムにある必要がある
  - blob が 0444 なので、アプリから同じ inode を上書きできない
  - 配置したファイルを `rm` しても blob は残る
- 「blob と同じ inode のファイル」を mvault の管理下とみなす。配置の記録ファイルは持たない
- ノードに index や DB は置かない。必要な情報は lock にある

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
- どちらの経路でも、`.mvault/tmp/` に書きながら sha256 を計算し、検証してから `blobs/` に入れる。中断したら続きから取得する

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
  { path = "split_files/upscaler/4x_*", dest = "models/upscale_models/" },
]

# リポジトリ丸ごと (ツリーを保って配置)
[[model]]
source = "hf:example-org/example-diffusers"
dest = "models/diffusers/example/"
exclude = ["assets/**"]

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

- `dest` は workspace からの相対パス。workspace の外、`.mvault/`、manifest、lock は指せない
- `files` があるとき: 書いたファイルだけを配置する
  - `path` が glob でなければ、`dest` が `/` で終わるときはその下に元のファイル名で、そうでなければ `dest` をファイル名として配置する
  - `path` が glob なら、`dest` はディレクトリで、一致したファイルをファイル名だけで置く。名前が重なればエラー
  - civitai では `path` の代わりに `file_id` で指定する (version 内でファイル名が重複しうるため)。`dest` の扱いは glob でない `path` と同じ
  - civitai の「全ファイル」という指定はない。必要なファイルを列挙する
- `files` がなく `dest` があるとき
  - HuggingFace: リポジトリ全体を、`exclude` を除いて、ツリーを保って `dest` の下に置く
  - civitai: primary ファイルを置く
- 同じ `source` と `revision` のエントリが複数あれば、読み込み時に 1 つにまとめる (`add` が末尾への追記だけで済むように)
- 同じ配置先に違う内容が来ればエラー
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
  { path = "split_files/upscaler/4x_foo.pth", size = 123, sha256 = "…" },
  { path = "split_files/vae/x_vae.safetensors", size = 456, sha256 = "…" },
]
```

- キーは `source` (正規形) と `revision`。`dest` や、どの manifest ファイルに書いたかは含まない
  - 配置先を変えても、エントリを別の toml に移しても、lock は有効
- civitai のファイルは `file_id` も持つ (`path` はファイル名)
- `files` は manifest の `files` / `exclude` / glob を展開した結果。上流が進んで一致するファイルが増えても、`update` するまで変わらない
- 上流から直接解決したファイルは、sha256 が分からないことがある (HuggingFace の API が sha256 を返すのは LFS ファイルだけ)
  - その場合は `sha256` を書かずに lock に入れ、最初の `sync` で取得したときに計算した値を書き込む
  - 一度書き込まれたら、以後は他のファイルと同じく照合に使う
  - ミラーの snapshot から解決した場合は、全ファイルの sha256 が最初から入る
- 出力は決まった順 (source、path) に並べ、差分を読みやすくする
- manifest と lock はセットで git 管理する

### 5.5 コマンド

manifest を使うコマンドは、カレントディレクトリから親へ `mvault.toml` を探す。

**`mvault init [--mirror <URL>]... [--allow-official]`**

- `mvault.toml` が無ければ作る。`.mvault/` と `.mvault/.gitignore` を作る
- `--mirror` / `--allow-official` を付けたときだけ `.mvault/config.toml` に書く。付けなければノードの設定を使う

**`mvault add <URL | 正規形> --dest <path> [--path <pattern>]... [--file-id <id>]... [--file <toml>]`**

- `--path` は HuggingFace、`--file-id` は civitai のファイル指定。URL に含まれるファイルのパスや `?fileId=` も同じ意味で読む
- モデルの情報を解決する。ミラーの snapshot を先に見て、無ければ上流の API に問い合わせる
- manifest の末尾に `[[model]]` をテキストとして追記する (既存のコメントや並びを崩さない)。追記先は既定で `mvault.toml`、`--file` で include 先を指定する
- lock に解決結果を書く
- モデルファイルのダウンロードはしない
- 同じ内容のエントリが既にあれば、警告して何もしない

**`mvault sync [-n] [--locked] [--prune] [--force]`**

1. manifest にあって lock にないエントリを、`add` と同じ手順で解決して lock に書き足す。`--locked` ではエラーにする
2. lock の各ファイルについて、`.mvault/blobs/` に無いものだけをミラーから取得する
   - どのミラーにも無ければエラー。ミラーホストで実行する `mvault mirror pull …` を表示する
   - `allow_official = true` のときは上流から取得する
3. 配置先に hardlink する
   - 既に同じ blob の hardlink なら何もしない
   - 管理下の別の内容なら差し替える (一時名で link してから `rename()`)
   - 管理下でないファイルがあれば上書きせず、衝突として報告する (`--force` で上書き)
4. `--prune`: manifest から外れた管理下のファイルを消す

- 複数の `sync` を同時に走らせてよい。同じ blob の取得は `.mvault/tmp/` の flock で 1 つに絞る
- `-n` は予定だけ表示する

**`mvault update [source]...`**

- revision (HF のブランチ名、version を省いた civitai) を解決し直して lock を更新する。配置は次の `sync` で行う

**`mvault ls [path]`**

- 配置先のファイル基準の一覧 (再帰)。パス、状態 (未取得 / 取得済み / 配置済み / 衝突)、サイズ、sha256、取得元

**`mvault status [source | URL]`**

- モデル基準の一覧。manifest の revision、lock の解決結果、ファイル数と取得済みの数、ミラーにあるかどうか

**`mvault gc [-n]`**

- `.mvault/blobs/` のうち、どこからも hardlink されていない blob (`st_nlink == 1`) を消す

## 6. テストと公開リポジトリとしての方針

- テストは実サービスに一切アクセスしない。テスト内で立てる HTTP サーバーに HuggingFace / civitai の API 応答とファイルを返させる
- コード、テスト、文書に実在のモデル ID や特定のモデル名を書かない。`example-org/example-model`、`example.com`、連番の ID を使う
- コードが名前を知っている上流は、既定のホストだけ

## 7. 実装

- Rust、単一の crate (bin + lib)
- 上流の API は HTTP で直接呼ぶ (公式クライアントライブラリには依存しない)
- 依存 crate は実装時に決める。SQLite は同梱ビルドにして、ミラーホストのシステムライブラリに依存しない

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
- civitai のプレビュー画像の保存 (`mirror pull --with-images`)
- ノードからミラーへのアップロード (ノードが上流から直接取得したものをミラーに入れる)
- ミラーとノードが同じマシンにあるときのコピー省略 (reflink / hardlink)
