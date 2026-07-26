/// <reference types="vitest" />
import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";
import path from "node:path";

export default defineConfig({
  plugins: [react(), tailwindcss()],
  resolve: {
    alias: {
      "@": path.resolve(__dirname, "./src")
    }
  },
  server: {
    port: 1420,
    strictPort: true,
    watch: {
      // vite must not watch the Rust tree: on Windows, fs.watch on a
      // cargo-locked target/ dll throws EBUSY and crashes the dev server
      // whenever cargo rebuilds while vite is running.
      ignored: ["**/src-tauri/**"]
    }
  },
  clearScreen: false,
  test: {
    environment: "jsdom",
    globals: true,
    setupFiles: ["./src/test/setup.ts"],
    css: false,
    include: ["src/**/*.{test,spec}.{ts,tsx}"],
    coverage: {
      provider: "v8",
      reporter: ["text", "html"],
      include: ["src/**/*.{ts,tsx}"],
      exclude: [
        "src/**/*.{test,spec}.{ts,tsx}",
        "src/test/**",
        "src/main.tsx",
        "src/components/ui/**"
      ]
    }
  }
});
