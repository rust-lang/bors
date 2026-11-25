import { defineConfig } from "vite";
import vue from "@vitejs/plugin-vue";

// Vite is used only as a bundler. The output is written into `web/assets/vue`,
// which is embedded into the bors binary and served under `/assets/vue`.
// File names are fixed (no content hashes), so that Askama templates can
// reference them directly without having to parse a Vite manifest.
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
        entryFileNames: "[name].js",
        chunkFileNames: "[name].js",
        assetFileNames: "[name].[ext]",
      },
    },
  },
});
