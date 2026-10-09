import { expect, test } from "@playwright/test";
import { readFile, rm, mkdtemp, writeFile } from "node:fs/promises";
import { dirname } from "node:path";
import { tmpdir } from "node:os";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { createServer } from "node:http";
import { createHash } from "node:crypto";

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

test("refreshes Git status after external edits and commits without reload", async ({ browser, baseURL }) => {
  const root = await mkdtemp(`${tmpdir()}/zed-web-git-watch-`);
  const git = (...args) => promisify(execFile)("git", ["-C", root, ...args]);
  const context = await browser.newContext();
  try {
    await git("init");
    await git("config", "user.name", "Web Test");
    await git("config", "user.email", "test@example.invalid");
    await writeFile(`${root}/tracked.txt`, "original\n");
    await git("add", ".");
    await git("commit", "-m", "initial");
    await authenticate(context, baseURL);
    const page = await context.newPage();
    const statuses = [];
    const watches = [];
    page.on("websocket", socket => {
      const statusIds = new Set();
      socket.on("framesent", ({ payload }) => {
        const message = JSON.parse(payload.toString());
        if (message.method === "Fs::watch") watches.push(message.params.path);
        if (message.method === "GitRepository::status") statusIds.add(message.id);
      });
      socket.on("framereceived", ({ payload }) => {
        const message = JSON.parse(payload.toString());
        if (statusIds.delete(message.id) && !message.error) statuses.push(message.result);
      });
    });
    await openWorkspace(page, `${baseURL}/?path=${encodeURIComponent(root)}`);
    await page.keyboard.press("Control+Shift+g");
    await expect.poll(() => watches.some(path => path.endsWith("/.git"))).toBe(true);
    await expect.poll(() => statuses.length).toBeGreaterThan(0);
    statuses.length = 0;
    await writeFile(`${root}/tracked.txt`, "modified\n");
    await expect.poll(() => statuses.some(status => status.includes("tracked.txt"))).toBe(true);
    statuses.length = 0;
    await git("add", "tracked.txt");
    await git("commit", "-m", "external commit");
    await expect.poll(() => statuses.some(status => status === "")).toBe(true);
    // The response precedes GPUI's next render of the updated repository.
    await page.waitForTimeout(750);
    await page.screenshot({ path: test.info().outputPath("git-after-commit.png") });
  } finally {
    await context.close();
    await rm(root, { recursive: true, force: true });
  }
});

test("installs agent binaries over RPC without blocking filesystem requests", async ({ browser, baseURL }) => {
  const root = await mkdtemp(`${tmpdir()}/zed-web-agent-install-`);
  const body = "#!/bin/sh\necho agent-ready\n";
  const server = createServer((_request, response) => {
    setTimeout(() => response.end(body), 500);
  });
  await new Promise(resolve => server.listen(0, "127.0.0.1", resolve));
  const context = await browser.newContext();
  try {
    await authenticate(context, baseURL);
    const page = await context.newPage();
    await openWorkspace(page, baseURL);
    const responses = await page.evaluate(params => new Promise((resolve, reject) => {
      const ws = new WebSocket(`${location.protocol === "https:" ? "wss:" : "ws:"}//${location.host}/rpc`);
      const messages = [];
      const timeout = setTimeout(() => { ws.close(); reject(new Error("download RPC timed out")); }, 15000);
      ws.onopen = () => {
        ws.send(JSON.stringify({ id: 1, method: "Binary::download", params }));
        ws.send(JSON.stringify({ id: 2, method: "Fs::is_dir", params: { path: "/workspace" } }));
      };
      ws.onmessage = ({ data }) => {
        const message = JSON.parse(data);
        if (![1, 2].includes(message.id)) return;
        messages.push(message);
        if (messages.length === 2) { clearTimeout(timeout); ws.close(); resolve(messages); }
      };
    }), {
      url: `http://127.0.0.1:${server.address().port}/agent`,
      digest: createHash("sha256").update(body).digest("hex"),
      destination: `${root}/installed`, kind: "raw", file_name: "agent",
    });
    expect(responses.map(message => message.id)).toEqual([2, 1]);
    expect(responses.every(message => !message.error)).toBe(true);
    expect(await readFile(`${root}/installed/agent`, "utf8")).toBe(body);
  } finally {
    await context.close();
    await new Promise(resolve => server.close(resolve));
    await rm(root, { recursive: true, force: true });
  }
});

test("uploads browser files through the authenticated server", async ({
  browser,
  baseURL,
}) => {
  const context = await browser.newContext();
  const unauthorized = await context.request.post(`${baseURL}/upload`, {
    multipart: {
      files: {
        name: "browser-upload.txt",
        mimeType: "text/plain",
        buffer: Buffer.from("uploaded through the browser"),
      },
    },
  });
  expect(unauthorized.status()).toBe(401);

  await authenticate(context, baseURL);
  const response = await context.request.post(`${baseURL}/upload`, {
    multipart: {
      files: {
        name: "browser-upload.txt",
        mimeType: "text/plain",
        buffer: Buffer.from("uploaded through the browser"),
      },
    },
  });
  expect(response.status()).toBe(200);
  const upload = await response.json();
  expect(upload.paths).toHaveLength(1);
  expect(upload.paths[0]).toMatch(/browser-upload\.txt$/);
  expect(await readFile(upload.paths[0], "utf8")).toBe(
    "uploaded through the browser",
  );
  await rm(dirname(upload.paths[0]), { recursive: true, force: true });
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
  expect(errors.filter((error) =>
    !error.includes("WebSocket") &&
    !error.includes("ERR_INTERNET_DISCONNECTED")
  )).toEqual([]);
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

test("resizes mobile docks without re-entering entity updates", async ({
  browser,
  baseURL,
}) => {
  const context = await browser.newContext({
    viewport: { width: 412, height: 915 },
    deviceScaleFactor: 2,
    isMobile: true,
    hasTouch: true,
  });
  await authenticate(context, baseURL);
  await context.addInitScript(() => {
    localStorage.setItem("zed-web-agent-panel-open", "true");
    localStorage.setItem("zed-web-workspace-sidebar-open", "true");
  });

  const page = await context.newPage();
  const failures = [];
  page.on("console", (message) => {
    if (
      message.type() === "error" &&
      /panicked|already (?:mutably )?borrowed|already being updated|DataView|RuntimeError: unreachable/.test(
        message.text(),
      )
    ) {
      failures.push(message.text());
    }
  });
  page.on("pageerror", (error) => failures.push(error.message));

  await page.goto(baseURL);
  await expect(page).toHaveTitle(/Zed Remote . Workspace/, {
    timeout: 90_000,
  });
  await expect
    .poll(() => page.evaluate(() => self.__zedRpcConnectionState))
    .toBe("open");
  await expect(page.locator("canvas").first()).toBeVisible({ timeout: 90_000 });

  const cdp = await context.newCDPSession(page);
  for (let index = 0; index < 12; index += 1) {
    await page.mouse.move(330, 330);
    await page.mouse.wheel(0, 900);

    const startX = [3, 10, 20, 195, 386, 401][index % 6];
    await cdp.send("Input.dispatchTouchEvent", {
      type: "touchStart",
      touchPoints: [{ x: startX, y: 320, id: 1 }],
    });
    for (let step = 1; step <= 5; step += 1) {
      const deltaX = index % 2 === 0 ? step * 7 : -step * 7;
      await cdp.send("Input.dispatchTouchEvent", {
        type: "touchMove",
        touchPoints: [
          {
            x: Math.max(1, Math.min(410, startX + deltaX)),
            y: 320 + step * 16,
            id: 1,
          },
        ],
      });
    }
    await cdp.send("Input.dispatchTouchEvent", {
      type: "touchEnd",
      touchPoints: [],
    });
  }

  await expect
    .poll(() => page.evaluate(() => self.__zedRpcConnectionState))
    .toBe("open");
  expect(failures).toEqual([]);
  await context.close();
});

test("handles viewport and input bursts without GPUI borrow errors", async ({
  browser,
  baseURL,
}) => {
  const context = await browser.newContext();
  await authenticate(context, baseURL);
  const page = await context.newPage();
  const failures = [];
  page.on("console", (message) => {
    if (
      message.type() === "error" &&
      /RefCell already borrowed|already mutably borrowed|panicked|RuntimeError: unreachable/.test(
        message.text(),
      )
    ) {
      failures.push(message.text());
    }
  });
  page.on("pageerror", (error) => failures.push(error.message));
  await openWorkspace(page, baseURL);

  for (let index = 0; index < 30; index += 1) {
    await page.setViewportSize({
      width: 1100 + (index % 3) * 140,
      height: 700 + (index % 4) * 50,
    });
    await page.mouse.move(500 + (index % 5) * 20, 350);
    await page.mouse.wheel(0, 120);
    await page.keyboard.press("Escape");
  }

  await page.evaluate(() => new Promise((resolve) => {
    requestAnimationFrame(() => requestAnimationFrame(resolve));
  }));

  await expect
    .poll(() => page.evaluate(() => self.__zedRpcConnectionState))
    .toBe("open");
  expect(failures).toEqual([]);
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
