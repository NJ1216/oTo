export default {
  menu: {
    settings: '設定',
    about: 'バージョン情報',
  },
  toast: {
    done: {
      one: '変換完了 (1ファイル)',
      many: '変換完了 ({n}ファイル)',
    },
    fail: {
      all: '変換失敗 ({n}件) — 詳細はDevToolsで確認',
      partial: '{ok}件成功 / {err}件失敗 — DevToolsで詳細確認',
    },
    error: 'エラー: {msg}',
  },
  settings: {
    title: '設定',
    output: {
      title: '出力先フォルダ',
      source: '元ファイルと同じフォルダ',
      desktop: 'デスクトップ',
      downloads: 'ダウンロード',
      custom: '指定フォルダ',
      pick: '選択…',
    },
    source: {
      title: '元ファイルの扱い',
      keep: '保持（デフォルト）',
      delete: '変換後に削除',
    },
    conflict: {
      title: '同名ファイルの競合処理',
      rename: '自動リネーム（デフォルト）',
      confirm: '上書き確認',
      overwrite: '強制上書き',
    },
    quality: {
      title: '変換品質',
      mp3: {
        label: 'MP3 ビットレート',
        128: '128 kbps',
        192: '192 kbps（デフォルト）',
        256: '256 kbps',
        320: '320 kbps',
      },
      m4a: {
        label: 'M4A ビットレート',
        96: '96 kbps',
        128: '128 kbps（デフォルト）',
        192: '192 kbps',
        256: '256 kbps',
      },
      flac: {
        label: 'FLAC 圧縮レベル',
        0: '0（最速）',
        3: '3',
        5: '5（デフォルト）',
        8: '8（最高圧縮）',
      },
    },
    parallel: {
      title: '並列変換',
      count: '同時実行数',
    },
    misc: {
      title: 'その他',
      reveal: '変換完了後に出力先を開く',
    },
    lang: {
      title: '言語',
      auto: 'システム設定に従う',
    },
    cancel: 'キャンセル',
    save: '保存',
  },
  about: {
    subtitle: '音声変換ツール',
    loading: '読み込み中…',
    libs: '使用ライブラリ',
    close: '閉じる',
  },
  dialog: {
    pauseMsg: '変換を一時停止しました',
    resume: '続ける',
    cancelJob: '中止する',
  },
};
