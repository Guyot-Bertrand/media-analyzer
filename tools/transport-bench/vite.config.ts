import { defineConfig } from "vite";

// Page unique servie par Tauri (frontendDist = dist) ; port fixe pour `tauri dev`.
export default defineConfig({
  clearScreen: false,
  server: { port: 5173, strictPort: true },
  build: { target: "es2022" },
});
