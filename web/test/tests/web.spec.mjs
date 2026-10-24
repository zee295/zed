import { expect, test } from "@playwright/test";
import { readFile, rm, mkdtemp, writeFile } from "node:fs/promises";
import { dirname } from "node:path";
import { tmpdir } from "node:os";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { createServer } from "node:http";
import { createHash, createPublicKey, publicEncrypt } from "node:crypto";

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
  const ready = page.waitForEvent("console", {
    predicate: (message) => message.text().includes("zed_web_workspace: docks ready"),
    timeout: 90_000,
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
  await ready;
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

test("keeps user settings subscribed and reloads profile and permission edits", async ({ browser, baseURL }) => {
  const context = await browser.newContext();
  let settingsPath;
  let original;
  try {
    await authenticate(context, baseURL);
    const page = await context.newPage();
    const loads = [];
    const removed = new Set();
    let watchId;
    page.on("websocket", socket => {
      const watches = new Set();
      const loadIds = new Set();
      socket.on("framesent", ({ payload }) => {
        const message = JSON.parse(payload.toString());
        if (message.method === "Fs::watch" && message.params.path.endsWith("/settings.json")) {
          settingsPath = message.params.path;
          watches.add(message.id);
        }
        if (message.method === "Fs::unwatch") removed.add(message.params.subscription_id);
        if (message.method === "Fs::load_bytes" && message.params.path.endsWith("/settings.json")) loadIds.add(message.id);
      });
      socket.on("framereceived", ({ payload }) => {
        const message = JSON.parse(payload.toString());
        if (watches.delete(message.id)) watchId = message.result?.subscription_id;
        if (loadIds.delete(message.id) && !message.error) loads.push(Buffer.from(message.result, "base64").toString("utf8"));
      });
    });
    await openWorkspace(page, baseURL);
    await expect.poll(() => watchId).toBeDefined();
    expect(removed.has(watchId)).toBe(false);
    original = await readFile(settingsPath, "utf8").catch(error => {
      if (error.code === "ENOENT") return null;
      throw error;
    });
    const edited = JSON.stringify({ agent: {
      profiles: { "browser-watch-test": { name: "Browser Watch Test", tools: {} } },
      tool_permissions: { default: "allow" },
    }});
    await writeFile(settingsPath, edited);
    await expect.poll(() => loads.includes(edited)).toBe(true);
    expect(removed.has(watchId)).toBe(false);
    const primary = await page.evaluate(() => /Mac/.test(navigator.platform) ? "Meta" : "Control");
    await page.keyboard.press(`${primary}+Shift+p`);
    await page.keyboard.type("agent: manage profiles");
    await page.keyboard.press("Enter");
    await page.waitForTimeout(750);
    await page.screenshot({ path: test.info().outputPath("updated-profiles.png") });
    await page.mouse.click(600, 273);
    await page.keyboard.type("Browser Created");
    await page.keyboard.press("Enter");
    await expect.poll(async () => {
      const saved = JSON.parse(await readFile(settingsPath, "utf8"));
      return saved.agent?.profiles?.["browser-created"]?.name;
    }).toBe("Browser Created");
    await page.keyboard.press("Escape");
    await page.waitForTimeout(500);
    await page.screenshot({ path: test.info().outputPath("created-profiles.png") });
  } finally {
    if (settingsPath && original !== undefined) {
      if (original === null) await rm(settingsPath, { force: true });
      else await writeFile(settingsPath, original);
    }
    await context.close();
  }
});

test("continues Markdown lists in the agent composer", async ({ browser, baseURL }) => {
  const context = await browser.newContext();
  try {
    await authenticate(context, baseURL);
    const page = await context.newPage();
    let agentReady = false;
    page.on("console", message => {
      if (message.text().includes("agent panel attached")) agentReady = true;
    });
    await openWorkspace(page, `${baseURL}/?path=${encodeURIComponent(dirname(dirname(tokenPath)))}`);
    await expect.poll(() => agentReady).toBe(true);
    const primary = await page.evaluate(() => /Mac/.test(navigator.platform) ? "Meta" : "Control");
    await page.keyboard.press(`${primary}+Shift+p`);
    await page.keyboard.type("agent: toggle focus");
    await page.keyboard.press("Enter");
    await page.waitForTimeout(500);
    await page.keyboard.press(`${primary}+n`);
    await page.waitForTimeout(500);
    await page.screenshot({ path: test.info().outputPath("composer-start.png") });
    const input = page.locator("textarea").first();
    await page.keyboard.type("- first");
    await page.keyboard.press("Shift+Enter");
    await page.keyboard.type("second");
    await expect.poll(() => input.inputValue()).toContain("- first\n- second");
    await page.keyboard.press(`${primary}+a`);
    await page.keyboard.type("1. first");
    await page.keyboard.press("Shift+Enter");
    await page.keyboard.type("second");
    await expect.poll(() => input.inputValue()).toContain("1. first\n2. second");
    await page.keyboard.press(`${primary}+a`);
    await page.keyboard.type("- [x] first");
    await page.keyboard.press("Shift+Enter");
    await page.keyboard.type("second");
    await expect.poll(() => input.inputValue()).toContain("- [x] first\n- [ ] second");
    await page.screenshot({ path: test.info().outputPath("composer-lists.png") });
  } finally {
    await context.close();
  }
});

test("persists provider credentials only on the authenticated server", async ({ browser, baseURL }) => {
  const context = await browser.newContext();
  const url = `https://provider-test.invalid/${Date.now()}`;
  const password = [...Buffer.from("fake-provider-refresh-token")];
  const request = operation => ({ data: { operation, url, username: "Bearer", password } });
  try {
    const unauthorized = await context.request.post(`${baseURL}/credentials`, request("read"));
    expect(unauthorized.status()).toBe(401);
    await authenticate(context, baseURL);
    const saved = await context.request.post(`${baseURL}/credentials`, request("write"));
    expect(saved.status()).toBe(200);
    expect(saved.headers()["cache-control"]).toBe("no-store");
    const fresh = await browser.newContext();
    try {
      await authenticate(fresh, baseURL);
      const read = await fresh.request.post(`${baseURL}/credentials`, request("read"));
      expect(await read.json()).toEqual(["Bearer", password]);
      const crossOrigin = await fresh.request.post(`${baseURL}/credentials`, {
        ...request("read"), headers: { Origin: "https://untrusted.example" },
      });
      expect(crossOrigin.status()).toBe(403);
    } finally { await fresh.close(); }
  } finally {
    await context.request.post(`${baseURL}/credentials`, request("delete"));
    await context.close();
  }
});

test("opens Zed account sign-in from the native command action", async ({ browser, baseURL }) => {
  const context = await browser.newContext();
  try {
    await authenticate(context, baseURL);
    const existing = await context.request.post(`${baseURL}/credentials`, {
      data: { operation: "read", url: "https://zed.dev" },
    });
    test.skip(await existing.json() !== null, "Preserve an existing Zed account session");
    const page = await context.newPage();
    const errors = await openWorkspace(page, baseURL);
    await page.evaluate(() => { window.open = () => null; });
    const primary = await page.evaluate(() => /Mac/.test(navigator.platform) ? "Meta" : "Control");
    await page.keyboard.press(`${primary}+Shift+p`);
    await page.keyboard.type("client: sign in");
    await page.keyboard.press("Enter");
    const dialog = page.getByRole("dialog", { name: "Complete provider sign-in" });
    await expect(dialog).toBeVisible();
    const authorization = new URL(await dialog.getByRole("link", { name: "Open Sign In" }).getAttribute("href"));
    expect(authorization.origin).toBe("https://zed.dev");
    expect(authorization.pathname).toBe("/native_app_signin");
    expect(authorization.searchParams.get("native_app_public_key")).toBeTruthy();
    await page.screenshot({ path: test.info().outputPath("zed-sign-in.png") });
    await dialog.getByRole("button", { name: "Cancel" }).click();
    await expect(dialog).not.toBeVisible();
    expect(errors.filter(error => /panicked|RefCell|unreachable/.test(error))).toEqual([]);
  } finally { await context.close(); }
});

test("completes native Zed account sign-in and restores it after reload", async ({ browser, baseURL }) => {
  const context = await browser.newContext();
  let stored;
  let restored = false;
  try {
    await authenticate(context, baseURL);
    await context.route("**/credentials", async route => {
      const request = route.request().postDataJSON();
      if (request.url !== "https://zed.dev") return route.fallback();
      if (request.operation === "write") stored = request;
      if (request.operation === "read" && stored) restored = true;
      await route.fulfill({ json: request.operation === "read" && stored ? [stored.username, stored.password] : null });
    });
    await context.route("**/proxy/zed-cloud/**", route => route.fulfill({ json: {} }));
    const page = await context.newPage();
    const errors = await openWorkspace(page, baseURL);
    await page.evaluate(() => { window.open = () => null; });
    const primary = await page.evaluate(() => /Mac/.test(navigator.platform) ? "Meta" : "Control");
    await page.keyboard.press(`${primary}+Shift+p`);
    await page.keyboard.type("client: sign in");
    await page.keyboard.press("Enter");
    const dialog = page.getByRole("dialog", { name: "Complete provider sign-in" });
    await expect(dialog).toBeVisible();
    const authorization = new URL(await dialog.getByRole("link", { name: "Open Sign In" }).getAttribute("href"));
    const key = createPublicKey({
      key: Buffer.from(authorization.searchParams.get("native_app_public_key"), "base64url"),
      format: "der", type: "pkcs1",
    });
    const encrypted = publicEncrypt({ key, oaepHash: "sha256" }, Buffer.from("fake-zed-token"))
      .toString("base64").replaceAll("+", "-").replaceAll("/", "_");
    const callback = new URL(`http://127.0.0.1:${authorization.searchParams.get("native_app_port")}/`);
    callback.searchParams.set("user_id", "123456");
    callback.searchParams.set("access_token", encrypted);
    await dialog.getByRole("textbox", { name: "Sign-in callback URL" }).fill(callback.href);
    await dialog.getByRole("button", { name: "Continue" }).click();
    await expect.poll(() => stored?.username).toBe("123456");
    expect(Buffer.from(stored.password).toString("utf8")).toBe("fake-zed-token");
    await openWorkspace(page, baseURL);
    await expect.poll(() => restored).toBe(true);
    await expect(dialog).not.toBeVisible();
    expect(errors.filter(error => /panicked|RefCell|unreachable|not supported|not available in the browser/.test(error))).toEqual([]);
  } finally { await context.close(); }
});

for (const provider of [
  { name: "ChatGPT", host: "auth.openai.com", proxy: "openai-auth", slot: "https://chatgpt.com/backend-api/codex", scroll: 760 },
  { name: "SuperGrok", host: "auth.x.ai", proxy: "xai-auth", slot: "https://auth.x.ai/zed-supergrok", scroll: 4000 },
]) {
  test(`completes native ${provider.name} provider sign-in with a mocked token exchange`, async ({ browser, baseURL }) => {
    const context = await browser.newContext();
    let stored;
    let tokenRequest;
    try {
      await authenticate(context, baseURL);
      await context.route("**/credentials", async route => {
        const request = route.request().postDataJSON();
        if (request.url !== provider.slot) return route.fallback();
        if (request.operation === "write") stored = request;
        await route.fulfill({ json: null });
      });
      await context.route(`**/proxy/${provider.proxy}/**`, async route => {
        tokenRequest = new URLSearchParams(route.request().postData());
        await route.fulfill({ json: {
          access_token: "fake-access-token", refresh_token: "fake-refresh-token",
          expires_in: 3600, email: "test@example.invalid",
        } });
      });
      await context.route("**/proxy/chatgpt/**", route => route.fulfill({ json: { models: [] } }));
      const page = await context.newPage();
      const errors = await openWorkspace(page, baseURL);
      await page.evaluate(() => { window.open = () => null; });
      const primary = await page.evaluate(() => /Mac/.test(navigator.platform) ? "Meta" : "Control");
      await page.keyboard.press(`${primary}+,`);
      await page.waitForTimeout(400);
      await page.mouse.click(310, 448);
      await page.waitForTimeout(250);
      await page.mouse.click(1096, 552);
      await page.waitForTimeout(300);
      await page.mouse.move(950, 570);
      await page.mouse.wheel(0, provider.scroll);
      await page.waitForTimeout(400);
      await page.mouse.click(1114, 590);
      const dialog = page.getByRole("dialog", { name: "Complete provider sign-in" });
      await expect(dialog).toBeVisible();
      const authorization = new URL(await dialog.getByRole("link", { name: "Open Sign In" }).getAttribute("href"));
      expect(authorization.hostname).toBe(provider.host);
      const callback = new URL(authorization.searchParams.get("redirect_uri"));
      callback.searchParams.set("code", "test-code");
      callback.searchParams.set("state", authorization.searchParams.get("state"));
      await dialog.getByRole("textbox", { name: "Sign-in callback URL" }).fill(callback.href);
      await dialog.getByRole("button", { name: "Continue" }).click();
      await expect.poll(() => ({
        username: stored?.username,
        panics: errors.filter(error => /panicked|RefCell|unreachable/.test(error)),
      })).toEqual({ username: "Bearer", panics: [] });
      const saved = JSON.parse(Buffer.from(stored.password).toString("utf8"));
      expect(saved.access_token).toBe("fake-access-token");
      expect(saved.refresh_token).toBe("fake-refresh-token");
      expect(tokenRequest.get("code")).toBe("test-code");
      expect(createHash("sha256").update(tokenRequest.get("code_verifier")).digest("base64url"))
        .toBe(authorization.searchParams.get("code_challenge"));
      await page.evaluate(() => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve))));
      expect(errors.filter(error => /panicked|RefCell|unreachable|not supported|not available in the browser/.test(error))).toEqual([]);
      await page.screenshot({ path: test.info().outputPath(`${provider.name.toLowerCase()}-signed-in.png`) });
    } finally { await context.close(); }
  });
}

test("validates subscription and Zed sign-in callbacks before completing", async ({ browser, baseURL }) => {
  const context = await browser.newContext();
  try {
    await authenticate(context, baseURL);
    const page = await context.newPage();
    const errors = await openWorkspace(page, baseURL);
    await page.setViewportSize({ width: 320, height: 650 });
    for (const [redirect, authorization, query] of [
      ["http://localhost:1455/auth/callback", "https://auth.openai.com/oauth/authorize?state=chatgpt-state", "code=test&state=chatgpt-state"],
      ["http://127.0.0.1:56121/callback", "https://auth.x.ai/oauth2/authorize?state=grok-state", "code=test&state=grok-state"],
      ["http://127.0.0.1:49153/", "https://zed.dev/native_app_signin?native_app_port=49153", "user_id=123&access_token=encrypted-test"],
    ]) {
      await page.evaluate(({ redirect, authorization }) => {
        self.__zedOAuthTestResult = null;
        self.__zedWaitOAuthCallback(redirect).then(
          query => { self.__zedOAuthTestResult = { query }; },
          error => { self.__zedOAuthTestResult = { error: error.message }; },
        );
        const url = new URL(authorization);
        if (!url.searchParams.has("native_app_port")) url.searchParams.set("redirect_uri", redirect);
        const open = window.open;
        try { window.open = () => null; self.__zedOpenExternalUrl(url.href); }
        finally { window.open = open; }
      }, { redirect, authorization });
      const dialog = page.getByRole("dialog", { name: "Complete provider sign-in" });
      await expect(dialog).toBeVisible();
      const bounds = await dialog.boundingBox();
      expect(bounds.x).toBeGreaterThanOrEqual(0);
      expect(bounds.x + bounds.width).toBeLessThanOrEqual(320);
      await expect(dialog.getByRole("button", { name: "Continue" })).toBeInViewport();
      const input = dialog.getByRole("textbox", { name: "Sign-in callback URL" });
      await input.fill("http://untrusted.example/callback?code=stolen&state=wrong");
      await dialog.getByRole("button", { name: "Continue" }).click();
      await expect(dialog.getByRole("alert")).toContainText("callback URL for this sign-in");
      expect(await page.evaluate(() => self.__zedOAuthTestResult)).toBeNull();
      if (authorization.includes("state=")) {
        await input.fill(`${redirect}?code=test&state=wrong`);
        await dialog.getByRole("button", { name: "Continue" }).click();
        await expect(dialog.getByRole("alert")).toContainText("state does not match");
      }
      await input.fill(`${redirect}?${query}`);
      await dialog.getByRole("button", { name: "Continue" }).click();
      await expect(dialog).not.toBeVisible();
      await expect.poll(() => page.evaluate(() => self.__zedOAuthTestResult)).toEqual({ query });
    }
    await page.evaluate(() => {
      self.__zedOAuthTestResult = null;
      self.__zedWaitOAuthCallback("http://localhost:1455/auth/callback").catch(error => {
        self.__zedOAuthTestResult = { error: error.message };
      });
      self.__zedCancelOAuthCallback("http://localhost:1455/auth/callback");
    });
    await expect.poll(() => page.evaluate(() => self.__zedOAuthTestResult?.error)).toBe("Sign-in canceled");
    expect(errors).toEqual([]);
  } finally { await context.close(); }
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
  expect(errors).toEqual([]);

  await context.setOffline(true);
  await expect
    .poll(() => page.evaluate(() => self.__zedRpcConnectionState))
    .toBe("reconnecting");
  await context.setOffline(false);
  await expect
    .poll(() => page.evaluate(() => self.__zedRpcConnectionState))
    .toBe("open");
  // Background SQL can still be in flight when the browser goes offline.
  expect(errors.filter((error) =>
    !error.includes("WebSocket") &&
    !error.includes("ERR_INTERNET_DISCONNECTED") &&
    !(error.includes("NetworkError: Failed to execute 'send' on 'XMLHttpRequest'") &&
      error.includes(`${baseURL}/sql`))
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
