import { defineConfig } from "vitest/config";

export default defineConfig({
  server: {
    proxy: {
      "/session": { target: "ws://127.0.0.1:8080", ws: true },
      "/sessions": { target: "http://127.0.0.1:8080" },
      "/learnings": { target: "http://127.0.0.1:8080" },
      "/voices": { target: "http://127.0.0.1:8080" },
      "/voice": { target: "http://127.0.0.1:8080" },
    },
  },
  test: {
    include: ["src/**/*.test.ts", "src/**/*.test.js"],
  },
});
