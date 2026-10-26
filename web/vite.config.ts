import { defineConfig } from "vitest/config";

export default defineConfig({
  server: {
    proxy: {
      "/session": { target: "ws://127.0.0.1:8080", ws: true },
    },
  },
  test: {
    include: ["src/**/*.test.ts"],
  },
});
