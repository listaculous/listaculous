import { defineConfig } from 'vite';
import { svelte } from '@sveltejs/vite-plugin-svelte';

export default defineConfig({
  plugins: [
    svelte(),
    {
      // In dev, serve the docs entry at /docs like the Rust server does.
      name: 'docs-route',
      configureServer(server) {
        server.middlewares.use((req, _res, next) => {
          if (req.url === '/docs' || req.url?.startsWith('/docs?')) req.url = '/docs.html' + req.url.slice(5);
          next();
        });
      },
    },
  ],
  build: {
    // Scalar (docs page only) is a large bundle; the admin app stays small.
    chunkSizeWarningLimit: 4000,
    rollupOptions: {
      // The admin app and the Scalar-powered API docs are separate pages.
      input: { main: 'index.html', docs: 'docs.html' },
    },
  },
  server: {
    proxy: {
      '/api': 'http://localhost:8080',
      '/subscribe': 'http://localhost:8080',
    },
  },
});
