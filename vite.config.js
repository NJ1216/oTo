import { defineConfig } from 'vite';
import { resolve } from 'path';

export default defineConfig({
  build: {
    rollupOptions: {
      input: {
        main: resolve(__dirname, 'index.html'),
        settings: resolve(__dirname, 'src/settings/settings.html'),
        about: resolve(__dirname, 'src/about/about.html'),
        preview: resolve(__dirname, 'src/silence-preview/preview.html'),
      },
    },
  },
});
