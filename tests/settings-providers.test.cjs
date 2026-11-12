const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const test = require("node:test");
const { stripTypeScriptTypes } = require("node:module");

const load = () => {
  const source = fs.readFileSync(path.join(__dirname, "../settings/src/providers.ts"), "utf8");
  const js = stripTypeScriptTypes(source);
  return import(`data:text/javascript;base64,${Buffer.from(js).toString("base64")}`);
};

test("Settings lists G and C with their own built-in home pages", async () => {
  const { PROVIDER_CHOICES, providerChoice, emptyProjectSettings } = await load();
  assert.deepEqual(PROVIDER_CHOICES.map((p) => [p.id, p.letter]), [["chatgpt", "G"], ["claude", "C"]]);
  assert.equal(providerChoice("claude").home, "https://claude.ai");
  const empty = emptyProjectSettings();
  assert.deepEqual(Object.keys(empty), ["chatgpt", "claude"]);
  assert.notEqual(empty.chatgpt, empty.claude);
});

test("bookmark URLs stay on their own provider", async () => {
  const { bookmarkUrlError } = await load();
  assert.equal(bookmarkUrlError("claude", "https://claude.ai/project/1"), "");
  for (const rejected of ["https://chatgpt.com/g/1", "http://claude.ai/", "https://claude.ai:444/", "nope"]) {
    assert.notEqual(bookmarkUrlError("claude", rejected), "", rejected);
  }
  assert.equal(bookmarkUrlError("chatgpt", "https://chatgpt.com/g/g-1"), "");
  assert.equal(bookmarkUrlError("chatgpt", "https://example.com/odd"), "", "existing G URL rules are kept");
  assert.match(bookmarkUrlError("chatgpt", "https://claude.ai/new"), /Claude/);
  assert.match(bookmarkUrlError("chatgpt", "https://www.claude.ai/"), /Claude/);
  assert.notEqual(bookmarkUrlError("chatgpt", "javascript:alert(1)"), "");
});

test("Settings pages send the edited provider explicitly", () => {
  const bookmarks = fs.readFileSync(path.join(__dirname, "../settings/src/ProjectBookmarks.tsx"), "utf8");
  assert.match(bookmarks, /invoke<ProjectSettings>\(command, \{ provider: provider\(\), \.\.\.args \}\)/);
  const microphone = fs.readFileSync(path.join(__dirname, "../settings/src/MicrophoneSettings.tsx"), "utf8");
  assert.match(microphone, /"list_microphone_inputs", \{\s*provider: source, requestAccess,/);
  assert.match(microphone, /"set_preferred_microphone", \{\s*provider: provider\(\), preference: next,/);
});
