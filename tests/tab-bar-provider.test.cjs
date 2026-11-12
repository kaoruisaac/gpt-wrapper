const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const test = require("node:test");
const { stripTypeScriptTypes } = require("node:module");

// provider.ts is a dependency-free ES module; strip its types and load it as ESM.
const load = () => {
  const source = fs.readFileSync(path.join(__dirname, "../tab-bar/src/provider.ts"), "utf8");
  const js = stripTypeScriptTypes(source);
  return import(`data:text/javascript;base64,${Buffer.from(js).toString("base64")}`);
};

test("provider IDs map to fixed G / C letters in order", async () => {
  const { PROVIDERS, providerInfo, isProviderId } = await load();
  assert.deepEqual(PROVIDERS.map((p) => [p.id, p.letter]), [["chatgpt", "G"], ["claude", "C"]]);
  assert.equal(providerInfo("claude").name, "Claude");
  assert.equal(isProviderId("claude"), true);
  assert.equal(isProviderId("G"), false);
});

test("switcher disables both buttons while a switch or command is pending", async () => {
  const { switcherButtonState } = await load();
  assert.deepEqual(switcherButtonState("chatgpt", "chatgpt", null, false),
    { pressed: true, pending: false, disabled: false });
  assert.deepEqual(switcherButtonState("claude", "chatgpt", null, false),
    { pressed: false, pending: false, disabled: false });
  assert.deepEqual(switcherButtonState("claude", "chatgpt", "claude", true),
    { pressed: false, pending: true, disabled: true });
  assert.equal(switcherButtonState("chatgpt", "chatgpt", "claude", true).disabled, true);
  assert.equal(switcherButtonState("claude", "chatgpt", null, true).disabled, true);
});

test("provider changes are detected for drag cancellation", async () => {
  const { providerChanged } = await load();
  assert.equal(providerChanged({ activeProvider: "chatgpt" }, { activeProvider: "claude" }), true);
  assert.equal(providerChanged({ activeProvider: "claude" }, { activeProvider: "claude" }), false);
});

test("Rust rejection reasons are shown verbatim, with a fallback", async () => {
  const { errorMessage } = await load();
  assert.equal(errorMessage("正在使用 PTT 聽寫，放開後再切換平台。"), "正在使用 PTT 聽寫，放開後再切換平台。");
  assert.equal(errorMessage(new Error("boom")), "boom");
  assert.equal(errorMessage(undefined), "操作失敗，請再試一次。");
});

test("Tab Bar renders the switcher before the tab strip and calls the host command", () => {
  const source = fs.readFileSync(path.join(__dirname, "../tab-bar/src/main.tsx"), "utf8");
  const switcher = source.indexOf('class="gpt-wrapper-provider-switcher"');
  const strip = source.indexOf('class="gpt-wrapper-tabs"');
  assert.ok(switcher > 0 && strip > switcher, "switcher must be the leftmost element");
  assert.match(source, /invoke<TabSnapshot>\("switch_provider", \{ provider \}\)/);
  // Both providers' bookmark menus name the provider explicitly.
  assert.match(source, /"show_project_bookmark_menu", \{\s*provider,/);
  assert.match(source, /"create_project_tab", \{ provider, projectId \}/);
  assert.doesNotMatch(source, /"ChatGPT tabs"/, "tab strip label must follow the provider");
});
