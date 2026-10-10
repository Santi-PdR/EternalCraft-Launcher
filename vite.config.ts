import { svelte } from '@sveltejs/vite-plugin-svelte';
import { defineConfig } from 'vite';

const platform = (globalThis as { process?: { env?: Record<string, string | undefined> } }).process?.env?.TAURI_ENV_PLATFORM;

export default defineConfig({
  plugins: [svelte()],
  clearScreen: false,
  server: { strictPort: true, port: 1420 },
  envPrefix: ['VITE_', 'TAURI_ENV_*'],
  build: { target: platform === 'windows' ? 'chrome105' : 'safari13' },
});
