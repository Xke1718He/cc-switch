import path from "node:path";
import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import { codeInspectorPlugin } from "code-inspector-plugin";

export default defineConfig(({ command, mode }) => {
  const webMode = mode === "web" || process.env.CC_SWITCH_WEB === "1";
  const webAliases = webMode
    ? {
        "@tauri-apps/api/core": path.resolve(
          __dirname,
          "./src/lib/web-shims/tauri-core.ts",
        ),
        "@tauri-apps/api/event": path.resolve(
          __dirname,
          "./src/lib/web-shims/tauri-event.ts",
        ),
        "@tauri-apps/api/window": path.resolve(
          __dirname,
          "./src/lib/web-shims/tauri-window.ts",
        ),
        "@tauri-apps/api/path": path.resolve(
          __dirname,
          "./src/lib/web-shims/tauri-path.ts",
        ),
        "@tauri-apps/api/app": path.resolve(
          __dirname,
          "./src/lib/web-shims/tauri-app.ts",
        ),
        "@tauri-apps/plugin-dialog": path.resolve(
          __dirname,
          "./src/lib/web-shims/tauri-dialog.ts",
        ),
        "@tauri-apps/plugin-process": path.resolve(
          __dirname,
          "./src/lib/web-shims/tauri-process.ts",
        ),
        "@tauri-apps/plugin-updater": path.resolve(
          __dirname,
          "./src/lib/web-shims/tauri-updater.ts",
        ),
        "@tauri-apps/plugin-log": path.resolve(
          __dirname,
          "./src/lib/web-shims/tauri-log.ts",
        ),
      }
    : {};

  return {
    root: "src",
    plugins: [
      command === "serve" &&
        codeInspectorPlugin({
          bundler: "vite",
        }),
      react(),
    ].filter(Boolean),
    base: "./",
    build: {
      outDir: "../dist",
      emptyOutDir: true,
    },
    server: {
      port: 3000,
      strictPort: true,
      proxy: webMode
        ? {
            "/api": "http://127.0.0.1:31235",
          }
        : undefined,
    },
    resolve: {
      alias: {
        "@": path.resolve(__dirname, "./src"),
        ...webAliases,
      },
    },
    clearScreen: false,
    envPrefix: ["VITE_", "TAURI_"],
  };
});
