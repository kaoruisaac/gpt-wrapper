// Independent OpenSSL/Node fixture. All keys are public synthetic test material.
const { createPrivateKey, createPublicKey, diffieHellman, createHash, createCipheriv } = require("node:crypto");
const { mkdirSync, writeFileSync } = require("node:fs");
const key = bytes => createPrivateKey({ key: Buffer.concat([Buffer.from("302e020100300506032b656e04220420", "hex"), bytes]), format: "der", type: "pkcs8" });
const pub = k => createPublicKey(k).export({ format: "der", type: "spki" }).subarray(-32);
const clientSecret = Buffer.from(Array.from({ length: 32 }, (_, i) => i + 1));
const deviceSecret = Buffer.from(Array.from({ length: 32 }, (_, i) => i + 33));
const client = key(clientSecret), device = key(deviceSecret);
const shared = diffieHellman({ privateKey: client, publicKey: createPublicKey(device) });
const popHash = createHash("sha256").update("24681357").digest();
const aesKey = Buffer.from(shared.map((b, i) => b ^ popHash[i]));
const iv = Buffer.from(Array.from({ length: 16 }, (_, i) => i));
const cipher = createCipheriv("aes-256-ctr", aesKey, iv);
const proof0 = cipher.update(pub(device));
const proof1 = cipher.update(pub(client));
const continuation = cipher.update(Buffer.from("synthetic STATUS request then response"));
const fixture = Object.fromEntries(Object.entries({ clientSecret, clientPublic: pub(client), devicePublic: pub(device), iv, proof0, proof1, continuation }).map(([k, v]) => [k, v.toString("hex")]));
mkdirSync("src-tauri/tests/fixtures", { recursive: true });
writeFileSync("src-tauri/tests/fixtures/security1.json", JSON.stringify(fixture, null, 2) + "\n");
