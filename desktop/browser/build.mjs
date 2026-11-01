import { build } from "esbuild";

await build({
  entryPoints: ["fixture/app.jsx"],
  bundle: true,
  format: "iife",
  platform: "browser",
  target: "chrome120",
  outfile: "dist/app.js",
  define: { "process.env.NODE_ENV": '"production"' },
  loader: { ".jsx": "jsx" },
  logLevel: "warning",
});
