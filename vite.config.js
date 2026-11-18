import { defineConfig } from 'vite';

const prefix = (process.env.MOCKPAD_BASE_PATH ?? '').replace(/\/+$/, '');
if (prefix.length > 128 || (prefix && !/^\/[A-Za-z0-9_-]+(?:\/[A-Za-z0-9_-]+)*$/.test(prefix))) {
  throw new Error('MOCKPAD_BASE_PATH must be a path such as /mockpad');
}

export default defineConfig({
  base: `${prefix}/`,
  root: 'web',
  build: { outDir: '../dist', emptyOutDir: true },
  resolve: { alias: [{
    find: 'monaco-editor/esm/vs/editor/editor.api.js', replacement: 'monaco-editor/editor',
  }, {
    // Monaco vendors its sanitizer; a dependency override alone does not replace it.
    find: './dompurify/dompurify.js', replacement: 'dompurify',
  }] },
  server: {
    port: 5173, strictPort: true,
    proxy: { '/api': { target: 'http://127.0.0.1:3000', ws: true } },
  },
});
