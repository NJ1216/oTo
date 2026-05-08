export default {
  window: {
    main: 'oTo',
    settings: 'Settings',
    about: 'About',
   },
  menu: {
    settings: 'Settings',
    about: 'About',
  },
  toast: {
    done: {
      one: 'Conversion complete (1 file)',
      many: 'Conversion complete ({n} files)',
    },
    fail: {
      all: 'Conversion failed ({n}) — see DevTools for details',
      partial: '{ok} succeeded / {err} failed — see DevTools for details',
    },
    error: 'Error: {msg}',
  },
  settings: {
    title: 'Settings',
    output: {
      title: 'Output Folder',
      source: 'Same folder as source',
      desktop: 'Desktop',
      downloads: 'Downloads',
      custom: 'Custom folder',
      pick: 'Browse…',
    },
    source: {
      title: 'Source File',
      keep: 'Keep (default)',
      delete: 'Delete after conversion',
    },
    conflict: {
      title: 'Name Conflict',
      rename: 'Auto rename (default)',
      confirm: 'Ask to overwrite',
      overwrite: 'Force overwrite',
    },
    quality: {
      title: 'Quality',
      mp3: {
        label: 'MP3 Bitrate',
        128: '128 kbps',
        192: '192 kbps (default)',
        256: '256 kbps',
        320: '320 kbps',
      },
      m4a: {
        label: 'M4A Bitrate',
        96: '96 kbps',
        128: '128 kbps (default)',
        192: '192 kbps',
        256: '256 kbps',
      },
      flac: {
        label: 'FLAC Compression',
        0: '0 (fastest)',
        3: '3',
        5: '5 (default)',
        8: '8 (best compression)',
      },
    },
    parallel: {
      title: 'Parallel Conversion',
      fullPower: {
        label: 'Full Power',
        hint: 'OFF: CPU cores−1 (recommended)　ON: use all cores',
      },
    },
    formats: {
      title: 'Visible Formats',
      hint: 'Choose which formats appear in the main window (at least one required)',
    },
    mp3detail: {
      title: 'MP3 Advanced',
      sampleRate: 'Sample Rate',
      sampleRateAuto: 'Auto',
      channelMode: 'Channel',
      jointStereo: 'Joint Stereo (default)',
      stereo: 'Stereo',
      mono: 'Mono',
    },
    misc: {
      title: 'Other',
      reveal: 'Open output folder after conversion',
    },
    lang: {
      title: 'Language',
      auto: 'Follow system setting',
      ja: 'Japanese',
      en: 'English',
    },
    cancel: 'Cancel',
    save: 'Save',
  },
  about: {
    subtitle: 'Audio Converter',
    loading: 'Loading…',
    libs: 'Libraries',
    close: 'Close',
    libsSection: {
      rust: 'Rust (Backend)',
      js: 'JavaScript (Frontend)',
    },
  },
  dialog: {
    pauseMsg: 'Conversion paused',
    resume: 'Resume',
    cancelJob: 'Cancel',
  },
};
