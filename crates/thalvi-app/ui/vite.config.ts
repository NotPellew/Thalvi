import { defineConfig } from "vitest/config";

export default defineConfig({
  base: "./",
  build: {
    // Both webviews read `light-dark()`; lowering it breaks Neon, whose scheme comes from a variable.
    cssTarget: ["chrome123", "safari17.5"],
    rolldownOptions: { input: { settings: "settings.html" } },
  },
  test: {
    include: ["src/**/*.test.ts"],
    environment: "jsdom",
    css: { include: [/settings(?:-looks)?\.css/, /look-(?:neon|holo|gunmetal)\.css/] },
  },
});
