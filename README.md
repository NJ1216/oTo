# oTo — Audio Converter

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
[![日本語](https://img.shields.io/badge/Language-Japanese-blue)](#oto--audio-converter)
[![English](https://img.shields.io/badge/Language-English-blue)](#english)

macOS / Windows向けのシンプルなオーディオ変換アプリです。ファイルをドラッグ＆ドロップして、音声・動画から必要な音声形式へ変換できます。

![App Workflow (Jp)](img/app-workflow-ja.jpg)

## アプリ名の由来

**oTo**（オト）は、日本語の「**音**」という言葉の響きと、データの変換（mp3 **to** wavなど）や橋渡しを意味する「**to**」を組み合わせた名前です。

## 主な機能

- wav、mp3、m4a、flac、ogg、opus、wma、aiff、alacなどの音声ファイルに対応
- mp4、mov、mkv、m4v、aviなどの動画から音声を抽出
- mp3、aac、opus、flac、alacへのエンコードと、wav、aiffへのデコード
- ファイルやフォルダのドラッグ＆ドロップ変換
- ファイル先頭・末尾の無音トリミング
- 変換ジョブの一時停止、再開、キャンセル
- 出力先、同名ファイルの処理、音質、変換後の元ファイル処理を設定可能
- 日本語・英語UI

## ダウンロード

最新版は[GitHub Releases](https://github.com/NJ1216/oTo/releases/latest)からダウンロードできます。

Windowsで初回起動時に警告が表示される場合は、配布元がこのリポジトリであることを確認してから実行してください。

## FFmpegの準備

oToの使用にはFFmpegが必要です。FFmpegはアプリに同梱されていないため、先にインストールしてください。

### macOS

[Homebrew](https://brew.sh/index_ja)をインストール後、ターミナルで次を実行します。

```bash
brew install ffmpeg
```

### Windows

ターミナルで次を実行します。

```powershell
winget install FFmpeg
```

wingetを利用できない場合は、[FFmpeg公式ダウンロードページ](https://ffmpeg.org/download.html)から取得し、展開先の `bin` フォルダを環境変数 `PATH` に追加してください。

## 使い方

1. oToを起動します。
2. ENCODEまたはDECODEを選び、出力形式を指定します。
3. ファイルまたはフォルダをウィンドウへドラッグ＆ドロップします。
4. 変換が完了するまで待ちます。

設定画面では、出力先、音質、無音トリミング、同名ファイルが存在する場合の処理などを変更できます。変換中にEscキーを押すと、処理を一時停止または中止できます。

## スクリーンショット

![Main Window (ENCODE)](img/main_window1.png)
![Main Window (DECODE)](img/main_window2.png)

### メインメニュー

<img src="img/settings-menu.png" alt="メイン画面の設定メニュー" width="300">

メイン画面のメニューから、無音トリミングとアクティビティ表示のオン／オフ、設定画面、バージョン情報へアクセスできます。

### 設定画面

<img src="img/settings-window.png" alt="設定画面" width="300">

出力先フォルダ、変換後の元ファイルの扱い、同名ファイルの処理、各形式の品質をまとめて設定できます。表示するエンコード／デコード形式も選択できます。

### 無音トリミングの詳細設定

<img src="img/silence-trimming-preview.png" alt="無音トリミングの詳細設定" width="500">

音声波形を確認しながら、検出レベルと最短無音時間を調整できます。削除される範囲と保持される音声を色分けして表示し、適用前にプレビューできます。

## ソースからビルドする

自分の環境でoToをビルドする場合は、先に次のツールを用意してください。

- [Node.js](https://nodejs.org/) 22以降
- [pnpm](https://pnpm.io/installation)
- [Rust](https://www.rust-lang.org/tools/install) stable
- OSごとの[Tauri 2 prerequisites](https://v2.tauri.app/start/prerequisites/)

リポジトリを取得し、依存関係をインストールしてからproduction buildを実行します。

```bash
git clone https://github.com/NJ1216/oTo.git
cd oTo
pnpm install --frozen-lockfile
pnpm tauri build
```

生成物は `src-tauri/target/release/` および、その配下の `bundle/` に出力されます。アプリの実行時には、前述の手順で別途インストールしたFFmpegが必要です。

## ライセンス

[MIT License](LICENSE)

Copyright © 2026 NJ

第三者ライセンス通知は、アプリ内の「バージョン情報」から確認できます。

## English

oTo is a simple audio converter for macOS and Windows. Drag and drop files or folders to convert audio and extract audio tracks from video files.

![App Workflow (En)](img/app-workflow-en.jpg)

### Application name

**oTo** combines the sound of the Japanese word **音** (*oto*, meaning “sound”) with **to**, representing both format conversion—such as mp3 **to** wav—and the idea of connecting or bridging.

### Features

- Supports common audio formats including wav, mp3, m4a, flac, ogg, opus, wma, aiff, and alac
- Extracts audio from mp4, mov, mkv, m4v, avi, and other video formats
- Encodes to mp3, aac, opus, flac, and alac; decodes to wav and aiff
- Drag-and-drop conversion of files and folders
- Optional trimming of leading and trailing silence
- Pause, resume, and cancel conversion jobs
- Configurable output folder, quality, file-conflict behavior, and source-file handling
- Japanese and English interface

### Download

Download the latest version from [GitHub Releases](https://github.com/NJ1216/oTo/releases/latest).

On Windows, an initial launch warning may appear. Confirm that the application came from this repository before running it.

### Install FFmpeg

oTo requires FFmpeg, which is not bundled with the application.

On macOS with [Homebrew](https://brew.sh):

```bash
brew install ffmpeg
```

On Windows:

```powershell
winget install FFmpeg
```

If winget is unavailable, download FFmpeg from the [official download page](https://ffmpeg.org/download.html) and add its `bin` directory to `PATH`.

### Usage

1. Launch oTo.
2. Select ENCODE or DECODE and choose an output format.
3. Drag files or folders onto the window.
4. Wait for conversion to finish.

Use Settings to configure the output folder, quality, silence trimming, and file-conflict behavior. Press Esc during conversion to pause or cancel the job.

### Screenshots

#### Main menu

<img src="img/settings-menu.png" alt="Settings menu on the main window" width="300">

The main menu provides quick access to silence trimming, activity display, application settings, and version information.

#### Settings

<img src="img/settings-window.png" alt="Settings window" width="300">

Configure the output folder, source-file handling, filename conflicts, and quality for each format. You can also choose which encode and decode formats appear in the main window.

#### Silence trimming details

<img src="img/silence-trimming-preview.png" alt="Detailed silence-trimming view" width="500">

Adjust the detection level and minimum silence duration while viewing the waveform. Removed and retained regions are color-coded, and the result can be previewed before conversion.

### Build from source

To build oTo locally, install the following prerequisites first:

- [Node.js](https://nodejs.org/) 22 or later
- [pnpm](https://pnpm.io/installation)
- [Rust](https://www.rust-lang.org/tools/install) stable
- The [Tauri 2 prerequisites](https://v2.tauri.app/start/prerequisites/) for your operating system

Clone the repository, install locked dependencies, and create a production build:

```bash
git clone https://github.com/NJ1216/oTo.git
cd oTo
pnpm install --frozen-lockfile
pnpm tauri build
```

Build outputs are written to `src-tauri/target/release/` and its `bundle/` directory. Running the application also requires a separately installed FFmpeg, as described above.

### License

[MIT License](LICENSE)

Copyright © 2026 NJ

Third-party license notices are available from the About window in the application.
