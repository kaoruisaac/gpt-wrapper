const assert = require("node:assert/strict");
const fs = require("node:fs");
const vm = require("node:vm");
const { stripTypeScriptTypes } = require("node:module");

// Exercise the actual General settings handlers with deterministic IPC/signals.
// Rendering is covered by frontend:build; no window or audio device is needed.
const source = fs.readFileSync("settings/src/main.tsx", "utf8");
const start = source.indexOf("function GeneralSettings() {");
const end = source.indexOf("  return (", start);
assert.ok(start >= 0 && end > start);
const code = stripTypeScriptTypes(source.slice(start, end) + `
  return { notificationSoundEnabled, notificationSoundBusy, statusKind,
    readNotificationSoundState, handleNotificationSoundChange };
}
globalThis.settings = GeneralSettings();`);

function harness(invoke) {
  const context = {
    invoke,
    createSignal(initial) {
      let value = initial;
      return [() => value, (next) => { value = next; }];
    },
    onMount() {},
    console: { error() {} },
  };
  vm.runInNewContext(code, context);
  return context.settings;
}

const change = (checked) => ({ currentTarget: { checked } });

(async () => {
  const calls = [];
  let resolveWrite;
  const settings = harness(async (command, args) => {
    calls.push({ command, args });
    if (command === "get_notification_sound_enabled") return true;
    return new Promise((resolve) => { resolveWrite = resolve; });
  });
  assert.equal(settings.notificationSoundEnabled(), true);
  assert.equal(settings.notificationSoundBusy(), true);
  await settings.handleNotificationSoundChange(change(false));
  assert.equal(calls.length, 0, "loading blocks submission");
  await settings.readNotificationSoundState();
  assert.equal(settings.notificationSoundBusy(), false);
  const pending = settings.handleNotificationSoundChange(change(false));
  assert.equal(settings.notificationSoundBusy(), true);
  assert.equal(settings.notificationSoundEnabled(), false);
  await settings.handleNotificationSoundChange(change(true));
  assert.equal(calls.length, 2, "only one write may be in flight");
  assert.equal(calls[1].command, "set_notification_sound_enabled");
  assert.equal(calls[1].args.enabled, false);
  resolveWrite(false);
  await pending;
  assert.equal(settings.notificationSoundEnabled(), false);
  assert.equal(settings.notificationSoundBusy(), false);
  assert.equal(settings.statusKind(), "success");

  let reads = 0;
  const failed = harness(async (command) => {
    if (command.startsWith("set_")) throw Error("disk failure");
    return ++reads === 1;
  });
  await failed.readNotificationSoundState();
  await failed.handleNotificationSoundChange(change(true));
  assert.equal(failed.notificationSoundEnabled(), false, "failure reloads actual saved state");
  assert.equal(failed.notificationSoundBusy(), false);
  assert.equal(failed.statusKind(), "error");

  const unavailable = harness(async () => { throw Error("IPC unavailable"); });
  await unavailable.readNotificationSoundState();
  assert.equal(unavailable.statusKind(), "error");
  await unavailable.handleNotificationSoundChange(change(false));
  assert.equal(unavailable.notificationSoundEnabled(), true, "failed resync restores previous state");
  assert.equal(unavailable.notificationSoundBusy(), false);
  console.log("notification sound settings tests passed");
})().catch((error) => { console.error(error); process.exitCode = 1; });
