import { expect, test } from "@playwright/test";
import { readFile } from "node:fs/promises";

const tokenPath =
  process.env.ZED_WEB_TOKEN_PATH ?? ".zed/web-auth-token";

async function authenticate(context, baseURL) {
  const token = process.env.ZED_WEB_TOKEN ??
    (await readFile(tokenPath, "utf8")).trim();
  const response = await context.request.post(`${baseURL}/login`, {
    form: { token },
    maxRedirects: 0,
  });
  expect(response.status()).toBe(303);
}

async function openWorkspace(page, baseURL) {
  const errors = [];
  page.on("console", (message) => {
    if (message.type() === "error") errors.push(message.text());
  });
  await page.goto(baseURL);
  await expect(page).toHaveTitle(/Zed Remote . Workspace/, {
    timeout: 90_000,
  });
  await expect
    .poll(() => page.evaluate(() => self.__zedRpcConnectionState))
    .toBe("open");
  const canvas = page.locator("canvas").first();
  await expect(canvas).toBeVisible({ timeout: 90_000 });
  const bounds = await canvas.boundingBox();
  expect(bounds?.width).toBeGreaterThan(500);
  expect(bounds?.height).toBeGreaterThan(300);
  return errors;
}

test("protects the application with authentication", async ({
  browser,
  baseURL,
}) => {
  const context = await browser.newContext();
  const unauthorized = await context.request.get(baseURL, { maxRedirects: 0 });
  expect(unauthorized.status()).toBe(401);
  await authenticate(context, baseURL);
  const authorized = await context.request.get(baseURL, { maxRedirects: 0 });
  expect(authorized.status()).toBe(307);
  await context.close();
});

test("boots GPUI and reconnects after an offline transition", async ({
  browser,
  baseURL,
}) => {
  const context = await browser.newContext();
  await authenticate(context, baseURL);
  const page = await context.newPage();
  const errors = await openWorkspace(page, baseURL);

  await context.setOffline(true);
  await expect
    .poll(() => page.evaluate(() => self.__zedRpcConnectionState))
    .toBe("reconnecting");
  await context.setOffline(false);
  await expect
    .poll(() => page.evaluate(() => self.__zedRpcConnectionState))
    .toBe("open");
  expect(errors.filter((error) => !error.includes("WebSocket"))).toEqual([]);
  await context.close();
});

test("restores panel visibility after reload", async ({ browser, baseURL }) => {
  const context = await browser.newContext();
  await authenticate(context, baseURL);
  const page = await context.newPage();
  await openWorkspace(page, baseURL);

  await page.evaluate(() => {
    localStorage.setItem("zed-web-agent-panel-open", "true");
    localStorage.setItem("zed-web-workspace-sidebar-open", "true");
  });
  await page.reload();
  await expect
    .poll(() => page.evaluate(() => self.__zedRpcConnectionState))
    .toBe("open");
  await expect
    .poll(() =>
      page.evaluate(() => ({
        agent: localStorage.getItem("zed-web-agent-panel-open"),
        sidebar: localStorage.getItem("zed-web-workspace-sidebar-open"),
      })),
    )
    .toEqual({ agent: "true", sidebar: "true" });
  await context.close();
});

test("accepts pasted images exposed only through clipboard files", async ({
  browser,
  baseURL,
}) => {
  const context = await browser.newContext();
  await authenticate(context, baseURL);
  const page = await context.newPage();
  await openWorkspace(page, baseURL);

  const pasteResult = await page.evaluate(async () => {
    const input = document.querySelector("textarea");
    const transfer = new DataTransfer();
    const png = Uint8Array.from(atob(
      "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M/wHwAF/gL+XhN4WQAAAABJRU5ErkJggg==",
    ), (character) => character.charCodeAt(0));
    transfer.items.add(
      new File([png], "screenshot.png", {
        type: "image/png",
      }),
    );

    const event = new Event("paste", { bubbles: true, cancelable: true });
    Object.defineProperty(event, "clipboardData", {
      value: {
        items: { length: 0 },
        files: transfer.files,
        getData: () => "",
      },
    });
    input.dispatchEvent(event);
    await new Promise((resolve) => setTimeout(resolve, 250));
    return {
      defaultPrevented: event.defaultPrevented,
      rpcState: self.__zedRpcConnectionState,
    };
  });

  expect(pasteResult).toEqual({
    defaultPrevented: true,
    rpcState: "open",
  });
  await context.close();
});

test("offers a real new-tab link when an external popup is blocked", async ({
  browser,
  baseURL,
}) => {
  const context = await browser.newContext();
  await authenticate(context, baseURL);
  const page = await context.newPage();
  await openWorkspace(page, baseURL);

  await page.evaluate(() => {
    const open = window.open;
    window.open = () => null;
    try {
      window.__zedOpenExternalUrl("https://example.com/agent-auth");
    } finally {
      window.open = open;
    }
  });

  const prompt = page.locator("#zed-external-link-prompt");
  await expect(prompt).toBeVisible();
  const link = prompt.locator("a");
  await expect(link).toHaveAttribute("href", "https://example.com/agent-auth");
  await expect(link).toHaveAttribute("target", "_blank");
  await expect(link).toHaveAttribute("rel", "noopener noreferrer");
  await context.close();
});
