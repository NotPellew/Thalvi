import { defineConfig } from "vitest/config";

export default defineConfig({
  base: "./",
  build: {
    rolldownOptions: { input: { overlay: "overlay.html", settings: "settings.html" } },
  },
  test: {
    include: ["src/**/*.test.ts"],
    environment: "jsdom",
    css: { include: [/settings(?:-looks)?\.css/] },
  },
});
