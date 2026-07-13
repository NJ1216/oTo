import { defineConfig } from 'vite';
import { resolve } from 'path';
import cargoMetaPlugin from './vite-plugin-cargo-meta.js';

export default defineConfig({
  plugins: [cargoMetaPlugin()],
  build: {
    rollupOptions: {
      input: {
        main: resolve(__dirname, 'index.html'),
        settings: resolve(__dirname, 'src/settings/settings.html'),
        about: resolve(__dirname, 'src/about/about.html'),
        licenses: resolve(__dirname, 'src/licenses.html'),
        activity: resolve(__dirname, 'src/activity/activity.html'),
        preview: resolve(__dirname, 'src/silence-preview/preview.html'),
      },
    },
  },
  server: {
    port: 1420,
  },
});
