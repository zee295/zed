import { defineConfig } from "@playwright/test";

export default defineConfig({
  testDir: "./tests",
  timeout: 120_000,
  expect: { timeout: 30_000 },
  fullyParallel: false,
  use: {
    baseURL: process.env.ZED_WEB_URL ?? "http://127.0.0.1:8090",
    browserName: process.env.ZED_WEB_TEST_BROWSER ?? "chromium",
    headless: true,
    viewport: { width: 1440, height: 900 },
    launchOptions: {
      args: process.env.ZED_WEB_TEST_BROWSER === "firefox" ? [] : ["--enable-unsafe-webgpu", "--use-angle=metal"],
    },
  },
});
