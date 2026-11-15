# Strata

[English](README.md)

Strata は、Rust と GPUI で構築された、macOS および Windows 向けの軽量で高速なネイティブ Git クライアントです（Linux はベストエフォートで対応しています）。リポジトリの読み取りは gitoxide（`gix`）で直接行い、書き込み操作はすべて `git` CLI を経由するため、フック、コミット署名、認証情報ヘルパー、Git LFS などがターミナル上の `git` と同じように動作します。UI は英語と日本語に対応しています。

<!-- Screenshot placeholder: スクリーンショットが用意できたらここに追加します -->

## 開発状況

Strata は個人開発のオープンソースプロジェクトとして公開しており、現在は初期段階（`0.x`）です。荒削りな部分や破壊的変更が残っている場合があります。

## 主な機能

### 履歴とコミットグラフ
- 仮想化されたコミット一覧とキャンバス描画によるコミットグラフ、ローカル／リモートブランチおよびタグのバッジ、`HEAD` の強調表示、相対日時・絶対日時の切り替えに対応しています。
- バックグラウンドでの段階的な履歴読み込みと、大規模リポジトリ向けの commit-graph 生成提案を備えています。
- コミット詳細（変更ファイル一覧と増減行数）、複数コミット選択時の差分表示（範囲内の変更 / 2 コミット間の比較）、および 2 ドット（`A..B`）・3 ドット（`A...B`）の比較ビューを利用できます。
- シャロークローンの境界表示と、ワンクリックでの全履歴フェッチに対応しています。

### 差分表示・Blame・ツリーブラウザー
- ユニファイド表示とサイドバイサイド表示、行内の単語単位ハイライト、tree-sitter によるシンタックスハイライト（Rust / JavaScript / TypeScript / TSX / Python / Go / JSON / TOML / YAML / CSS / HTML / Bash / C / C++ / Markdown）、空白文字の表示、行の折り返し、コンテキスト行数の調整、空白の無視（`-w`）、省略行の展開およびファイル全体表示に対応しています。
- 画像の差分プレビュー（PNG / JPEG / GIF / WebP / BMP / ICO、およびテキスト表示と切り替え可能な SVG）を、並べて表示・スワイプ・オニオンスキンの 3 モードで比較できます。
- リネーム追跡付きのファイル履歴、行単位の Blame、任意のコミット時点のツリーブラウザーを備えています。

### ワーキングツリー・ステージング・コンフリクト解決
- アンステージ／ステージ済み／コンフリクトのファイル一覧を、複数ファイル選択やキーボード操作で扱えます。
- 元のファイルのバイト列・文字コードを保持したパッチ生成により、ファイル単位・ハンク単位・行単位でのステージ、アンステージ、変更の破棄を行えます。
- コミットおよび Amend に対応し、サマリー文字数のガイドライン警告や、HTTP / SSH 認証プロンプトを受け取る内蔵 `--askpass` ブリッジを備えています。
- ウィンドウ全体を使う 3-way マージエディタ（Ours / Theirs / Base / 編集可能な Result）を搭載し、コンフリクトごとの採用選択、手動編集、コンフリクトマーカー残存チェック、外部マージツールの起動が可能です。
- マージ、リベース、チェリーピック、リバート、`git am`、`git bisect` が進行中の場合にバナーを表示し、続行や中止・リセットを行えます。

### ブランチ・リモート・スタッシュ・ワークツリー・サブモジュール
- クイックブランチスイッチャー、ブランチの作成、リネーム（リモートブランチ名の変更を含む）、削除、上流ブランチの設定、ブランチ／タグ単位の絞り込みに対応しています。
- フェッチ（自動プルーンやバックグラウンドの自動フェッチ設定を含む）、プル、プッシュ、新規ブランチの公開、force-with-lease の確認ダイアログを備えています。
- スタッシュの作成（全変更ファイルまたは選択ファイル、未追跡ファイルの含有オプション付き）、適用、ポップ、削除、履歴上の位置表示を行えます。
- リンクトワークツリー（追加、タブ／ウィンドウで開く、削除、prune）およびサブモジュール（初期化と更新、一括更新、タブ／ウィンドウで開く、親リポジトリへの戻りナビゲーション）に対応しています。
- リポジトリのクローン（ブランチ指定、シャロー深度、サブモジュールの再帰クローン）と新規リポジトリの作成を行えます。

### 履歴の編集と Reflog による取り消し
- 単一・複数コミットのチェリーピック、リバート、ソフト／ミックス／ハードリセット、指定コミット・ブランチへのリベース（autostash 対応）を実行できます。
- インタラクティブな「履歴を編集」ダイアログ（pick / reword / squash / fixup / drop / 並び替え）と、個別コミットに対するメッセージ変更・スカッシュ・フィックスアップ・削除・上下移動を備えています。
- Reflog ビューアから、直前の操作を 1 ステップで取り消す機能や指定時点への復元を行えます。

### 検索・LFS・署名検証・外部ツール連携
- サマリー・作成者・ハッシュによる絞り込みに加え、履歴全体のコンテンツ検索（`-S` 部分文字列 / `-G` 正規表現、パス絞り込みおよび commit-graph Bloom フィルタによる高速化）に対応しています。
- Git LFS（ポインター検出、オブジェクトのダウンロード、追跡設定、ファイルのロック／アンロック）に対応しています。
- コミット署名（`OpenPGP` / `SSH` / `X.509`）のステータス表示とバックグラウンド検証を行います。
- オプションの GitHub CLI（`gh`）連携により、ブランチ上のプルリクエスト状態（Open / Draft / Merged / Closed、レビュー状態、チェック結果）の表示、PR チェックアウトダイアログ、GitHub 上でファイル・コミット・PR を開く操作、PR 作成（`gh pr create --web`）を利用できます。
- オプションの `ghq` 連携により、リポジトリピッカーでの一覧表示やクローン先パスの解決を行えます。
- 各レイヤー（local / worktree / global / system / XDG / 環境変数 / コマンドライン）を横断する Git 設定ビューア＆エディタを備え、`include` / `includeIf` の確認や、書き込み前のコマンド・差分プレビューが可能です。

### ワークスペース・カスタマイズ・埋め込みターミナル
- 複数タブ・複数ウィンドウ、セッション復元、最近開いたリポジトリピッカー、ライト／ダーク／システム連動テーマ、UI フォント・コードフォントのカスタマイズ、日英 UI 切り替え（`システム` は OS の言語に追従）に対応しています。
- コマンドパレット、`keymap.json` およびキーボード設定画面によるキーバインド変更、`commands.json` によるカスタムコマンド定義（`${repo}` / `${branch}` / `${commit}` / `${file}` / `${remote}` 変数展開付き）を行えます。
- リポジトリタブごとに埋め込み PTY ターミナルパネル（`libghostty-vt` と `portable-pty` による `strata-term`、Cargo の `terminal` feature で有効化）を利用できます。
- 日本語およびレガシー文字コードに対応しています。UTF-8、UTF-16LE/BE（BOM 付き）、`.gitattributes` の `working-tree-encoding`、自動判定（`Shift_JIS`、`EUC-JP`、`ISO-2022-JP` など）を用いて表示用にデコードしつつ、差分計算・パッチ適用・書き込み時には元のバイト列をそのまま維持します。

## インストール（GitHub Releases）

ビルド済みバイナリは GitHub Releases から配布しています。

### 実行に必要なツール
- **`git`（`2.30.0` 以上）** が `PATH` に通っている必要があります（すべての書き込み操作で使用します）。
- 任意: `gh`（GitHub CLI）、`git-lfs`、`ghq`

### macOS
GitHub Releases から `.app` バンドル（macOS 14.0 以降対応）をダウンロードして展開し、`Strata.app` を `/Applications` に移動します。

配布ビルドはアドホック署名のみで**公証（notarization）を受けていません**。初回起動時は `Strata.app` を右クリックして**「開く」**を選ぶか、次のコマンドで quarantine 属性を解除してください。

```sh
xattr -dr com.apple.quarantine /Applications/Strata.app
```

### Windows（x86_64）
`.zip` アーカイブをダウンロードして展開し、`strata.exe` を実行します。

配布ビルドは**署名されていません**。Windows SmartScreen の警告が表示された場合は、**「詳細情報」→「実行」**を選択してください。

### Linux（ベストエフォート）
リリース用の tarball をダウンロードして展開します。実行にはホスト側のシステムに以下の共有ライブラリとフォントが必要です。
- ウィンドウシステム・入力: `libxkbcommon`、Wayland（`libwayland-client`）および／または X11（`libX11`、`libxcb`、`libXcursor`、`libXi`、`libXrandr`、`libXrender`）
- グラフィックス・オーディオ: `vulkan-loader`（`libvulkan`）および動作する Vulkan ドライバ、`libGL`、`alsa-lib`
- テキスト描画・TLS・C++ ランタイム: `fontconfig`、`freetype`、`openssl`（`libssl`）、`libstdc++`、およびシステムフォント（日本語テキストを表示する場合は Noto Sans CJK JP などの CJK フォント）

## ソースからのビルド

ビルドには Rust（[rust-toolchain.toml](rust-toolchain.toml) で指定された stable チャネル）と `git` が必要です。デフォルトで有効になっている埋め込みターミナル（`libghostty-vt`）をビルドする場合は、**Zig `0.17.1`** も必要になります。埋め込みターミナルを無効にして Zig なしでビルドするには、`--no-default-features` を指定してください。

```sh
# macOS（Xcode および Metal Toolchain をインストール済みの場合）
cargo run --release -p strata-app

# Windows（「Developer PowerShell for VS」から実行）
cargo build --release -p strata-app

# Linux（Nix dev shell を利用する場合）
direnv allow && direnv exec . cargo run --release -p strata-app

# 埋め込みターミナルなしでビルドする場合（Zig 不要）
cargo build --release -p strata-app --no-default-features
```

各プラットフォームの詳細な手順、Linux の依存パッケージ一覧、Windows のセットアップ、Nix dev shell の仕様については [docs/building.md](docs/building.md) を参照してください。

## 関連ドキュメント

- [設計ドキュメント（日本語）](docs/design/README.md)
- [ビルド手順（英語）](docs/building.md)
- [パッケージング手順（英語）](packaging/README.md)

## ライセンス

以下のいずれかのライセンス（デュアルライセンス）の下で提供されています。

- MIT License（[LICENSE-MIT](LICENSE-MIT)）
- Apache License, Version 2.0（[LICENSE-APACHE](LICENSE-APACHE)）

サードパーティライセンス表示（Third-party notices）はリリースアーカイブに同梱されます。
