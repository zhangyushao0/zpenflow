// Vite + React for Tauri 2.
//
// Tauri runs `npm run dev` (which is `vite`) before launching the Rust
// binary in dev mode, then loads the dev server URL into its WebView.
// `npm run build` produces static files in `dist/` which Tauri bundles
// into the release MSI.

import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

export default defineConfig({
    plugins: [react()],
    clearScreen: false,
    server: {
        port: 5173,
        strictPort: true,
    },
    build: {
        target: "esnext",
        // Tauri's WebView2 supports modern ES; no transpile-down needed.
        minify: "esbuild",
        sourcemap: false,
        outDir: "dist",
        emptyOutDir: true,
    },
});
