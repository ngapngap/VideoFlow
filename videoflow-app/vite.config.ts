import { defineConfig } from "vite";
import { svelte } from "@sveltejs/vite-plugin-svelte";

export default defineConfig({
  plugins: [svelte()],
  clearScreen: false,
  // Bắt buộc cho Tauri: asset phải là đường dẫn tương đối, không phải /assets
  base: "./",
  server: {
    port: 1420,
    strictPort: true,
  },
  envPrefix: ["VITE_", "TAURI_ENV_*"],
  build: {
    target: "chrome105",
    minify: "esbuild",
    sourcemap: false,
    rollupOptions: {
      output: {
        manualChunks(id) {
          if (id.includes("node_modules")) {
            return "vendor";
          }
          if (id.includes("/src/screens/Settings.svelte")) {
            return "screen-settings";
          }
          if (id.includes("/src/screens/Rows.svelte")) {
            return "screen-rows";
          }
        },
      },
    },
  },
});
