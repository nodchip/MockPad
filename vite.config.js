import { defineConfig } from 'vite';

export default defineConfig({
  root: 'web',
  build: { outDir: '../dist', emptyOutDir: true },
  resolve: { alias: [{
    find: 'monaco-editor/esm/vs/editor/editor.api.js', replacement: 'monaco-editor/editor',
  }] },
  server: {
    port: 5173, strictPort: true,
    proxy: { '/api': { target: 'http://127.0.0.1:3000', ws: true } },
  },
});
