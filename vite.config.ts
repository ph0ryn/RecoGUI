import react from "@vitejs/plugin-react";
import { defineConfig, type ServerOptions } from "vite";

const host = process.env.TAURI_DEV_HOST;
const server: ServerOptions = {
  host: false,
  port: 1420,
  strictPort: true,
  watch: {
    ignored: ["**/src-tauri/**"],
  },
};

if (host !== undefined) {
  server.hmr = {
    host,
    port: 1421,
    protocol: "ws",
  };
  server.host = host;
}

export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server,
});
