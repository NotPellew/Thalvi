// Sweep the rendered Settings controls and editors at the supported window sizes.

import { createServer } from "vite";
import { chromium, type Page } from "playwright-core";
import { measureSettingsBoxes } from "./settings-boxes.ts";

declare const process: { exitCode: number | undefined };

const viewports = [
  { width: 400, height: 680 },
  { width: 640, height: 850 },
  { width: 780, height: 680 },
  { width: 1280, height: 900 },
];
const editors = [
  ["Add a streaming server", "Edit Realtime transcription (WebSocket)", "realtime"],
  ["Add an after-you-stop server", "Edit LLM Gateway", "open_ai"],
  ["Add an on-device model", "Edit On this computer", "on_device"],
] as const;
const controls = "button, input:not([type=hidden]), select, textarea, summary, a[href], label, legend, .what > span, .trow > .phrase, [role=button], [role=switch], [role=tab], [tabindex]:not([tabindex='-1'])";

async function check(page: Page, state: string): Promise<void> {
  const { width, height } = await page.evaluate(() => ({ width: innerWidth, height: innerHeight }));
  const { boxes, overflow } = await page.locator(controls).evaluateAll(measureSettingsBoxes, true);
  const failures = boxes.flatMap(({ label, x, y, w, h, problems }) =>
    problems.map((problem) => `${label} ${JSON.stringify({ x, y, w, h })}: ${problem}`));
  const heading = `${String(width)}×${String(height)} ${state}`;
  if (failures.length || overflow.length || boxes.length === 0) {
    console.error(`${heading}\n  ${[...failures, ...overflow, ...(boxes.length === 0 ? ["no controls found"] : [])].join("\n  ")}`);
    process.exitCode = 1;
  } else {
    console.log(`${heading}: ${String(boxes.length)} controls and labels passed`);
  }
}

async function sweep(page: Page, url: string): Promise<void> {
  await page.clock.setFixedTime(new Date("2026-11-17T12:00:00Z"));
  await page.goto(url);
  const nav = page.getByRole("navigation", { name: "Settings pages" });
  const buttons = nav.getByRole("button");
  await buttons.first().waitFor();
  await page.evaluate(() => document.fonts.ready.then(() => undefined));
  const details = page.locator('details[data-fold="chat/writing/endpoint"]');
  for (const button of await buttons.all()) {
    const name = (await button.innerText()).trim();
    await button.click();
    await page.getByRole("region", { name, exact: true }).waitFor();
    if (name === "Chat model" && await details.evaluate((node: HTMLDetailsElement) => node.open)) {
      await details.locator("summary").click();
    }
    await check(page, name);
    if (name === "Recognition") {
      for (const [add, edit, kind] of editors) {
        const editor = page.locator(`[data-kind="${kind}"] .editor`);
        for (const opener of [add, edit]) {
          await page.getByRole("button", { name: opener, exact: true }).click();
          await editor.getByRole("heading").waitFor();
          if (kind === "on_device") await editor.locator(".model:not([hidden])").waitFor();
          await check(page, `${name} > ${opener}`);
          if (kind === "on_device" && opener === edit) {
            await editor.getByText("Details · your own model folder", { exact: true }).click();
            await editor.getByRole("textbox", { name: "Your own model folder" }).waitFor();
            await check(page, `${name} > ${opener} > Your own model folder`);
          }
          await editor.getByRole("button", { name: "Cancel", exact: true }).click();
        }
      }
    }
    if (name === "Chat model") {
      await details.locator("summary").click();
      await details.getByRole("textbox", { name: "Writing endpoint address" }).waitFor();
      await check(page, `${name} > Details`);
    }
  }
}

const server = await createServer({ server: { host: "127.0.0.1", port: 0 }, logLevel: "silent" });
try {
  await server.listen();
  const base = server.resolvedUrls?.local[0];
  if (base === undefined) throw new Error("Settings dev server has no local URL");
  const browser = await chromium.launch();
  try {
    for (const viewport of viewports) {
      const page = await browser.newPage({ viewport, locale: "en-US", timezoneId: "UTC" });
      page.setDefaultTimeout(5000);
      try {
        await sweep(page, `${base}settings.html?fake&at=0&look=gunmetal`);
      } catch (error) {
        throw new Error(`Settings layout failed at ${String(viewport.width)}×${String(viewport.height)}`, { cause: error });
      } finally {
        await page.close();
      }
    }
  } finally {
    await browser.close();
  }
} finally {
  await server.close();
}
