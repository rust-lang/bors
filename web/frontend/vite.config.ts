import { defineConfig } from "vite";
import vue from "@vitejs/plugin-vue";

// Vite is used only as a bundler. The output is written into `web/assets/vue`,
// which is embedded into the bors binary and served under `/assets/vue`.
// File names contain a content hash (e.g. `queue-<hash>.js`) for cache busting.
// The backend finds the current file by its `queue-` prefix and `.js` suffix
// (see `Assets::get_queue_asset` in `src/server/mod.rs`).
export default defineConfig({
  plugins: [vue()],
  base: "/assets/vue/",
  build: {
    outDir: "../assets/vue",
    emptyOutDir: true,
    rollupOptions: {
      input: {
        queue: "src/queue.ts",
      },
      output: {
        entryFileNames: "[name]-[hash].js",
        chunkFileNames: "[name]-[hash].js",
        assetFileNames: "[name]-[hash].[ext]",
      },
    },
  },
});
