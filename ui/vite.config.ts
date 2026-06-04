import { sveltekit } from '@sveltejs/kit/vite';
import tailwindcss from '@tailwindcss/vite';
import { defineConfig } from 'vite';

// Tauri opens the dev server on a fixed port; matches src-tauri/tauri.conf.json.
const TAURI_DEV_HOST = process.env.TAURI_DEV_HOST;

export default defineConfig({
  plugins: [tailwindcss(), sveltekit()],
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    host: TAURI_DEV_HOST || false,
    hmr: TAURI_DEV_HOST ? { protocol: 'ws', host: TAURI_DEV_HOST, port: 1421 } : undefined,
    watch: {
      ignored: ['**/src-tauri/**']
    }
  }
});
