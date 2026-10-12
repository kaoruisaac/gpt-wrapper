const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");

const html = fs.readFileSync(
  path.join(__dirname, "../settings/public/tab-completion-notifications.html"),
  "utf8"
);
const script = html.match(/<script>([\s\S]*?)<\/script>/)?.[1];
assert.ok(script, "notification script must be embedded in the packaged HTML");
assert.match(html, /\.card\s*\{[^}]*opacity:\s*0\.8/);
assert.match(html, /\.card\.faded\s*\{[^}]*opacity:\s*0\.2/);

class Element {
  constructor(tag) {
    this.tag = tag;
    this.children = [];
    this.dataset = {};
    this.classes = new Set();
    this.classList = {
      toggle: (name, shouldHave) => {
        if (shouldHave) this.classes.add(name);
        else this.classes.delete(name);
      }
    };
    this.listeners = {};
    this.attributes = {};
    this.parent = null;
  }
  setAttribute(key, value) { this.attributes[key] = value; }
  addEventListener(name, callback) { this.listeners[name] = callback; }
  append(...nodes) { nodes.forEach((node) => this.insertBefore(node, null)); }
  insertBefore(node, before) {
    if (node.parent) {
      const siblings = node.parent.children;
      siblings.splice(siblings.indexOf(node), 1);
    }
    node.parent = this;
    const index = before ? this.children.indexOf(before) : -1;
    if (index < 0) this.children.push(node);
    else this.children.splice(index, 0, node);
  }
  remove() {
    if (!this.parent) return;
    const siblings = this.parent.children;
    siblings.splice(siblings.indexOf(this), 1);
    this.parent = null;
  }
}

const container = new Element("main");
const invokes = [];
const window = {
  __TAURI_INTERNALS__: {
    invoke: (command, args) => {
      invokes.push({ command, args });
      return Promise.resolve();
    }
  }
};
const document = {
  getElementById: (id) => id === "cards" ? container : null,
  createElement: (name) => new Element(name),
};
vm.runInNewContext(script, { window, document, console });

const render = window.__GPT_TAB_COMPLETIONS__.render;
const hostile = "</script><img src=x onerror=alert(1)>";
const a = { tabId: 1, generation: 17, title: hostile, faded: false };
const b = { tabId: 2, generation: 18, title: "Same", faded: false };
render([a]);
assert.equal(container.children[0].children[1].textContent, hostile);
assert.equal(container.children[0].children[1].innerHTML, undefined);
assert.equal(container.children[0].classes.has("faded"), false);
render([b, a]);
assert.deepEqual(container.children.map((card) => card.dataset.key), ["2:18", "1:17"]);
const firstNode = container.children[0];
render([{ ...b, faded: true }, { ...a, faded: true }]);
assert.equal(container.children[0], firstNode, "fade must not replace or reorder existing cards");
assert.equal(firstNode.classes.has("faded"), true);
const close = container.children[0].children[2];
close.listeners.click();
assert.equal(invokes.length, 1);
assert.equal(invokes[0].command, "dismiss_tab_completion_notification");
assert.equal(invokes[0].args.tabId, 2);
assert.equal(invokes[0].args.generation, 18);
render([{ ...a, faded: true }]);
assert.deepEqual(container.children.map((card) => card.dataset.key), ["1:17"]);
render([]);
assert.equal(container.children.length, 0);
console.log("tab completion notification UI tests passed");
