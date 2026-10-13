const fs = require("node:fs");
const path = require("node:path");

const root = path.resolve(__dirname, "..");
const version = process.argv[2];
const semver = /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-((?:0|[1-9]\d*|\d*[A-Za-z-][0-9A-Za-z-]*)(?:\.(?:0|[1-9]\d*|\d*[A-Za-z-][0-9A-Za-z-]*))*))?(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?$/;

function updateJson(file, update) {
  const original = fs.readFileSync(path.join(root, file), "utf8");
  const data = JSON.parse(original);
  update(data);
  const indent = original.match(/\n([ \t]+)"/)?.[1] ?? 2;
  const newline = original.includes("\r\n") ? "\r\n" : "\n";
  const trailingNewline = original.endsWith("\n") ? newline : "";
  return {
    file,
    content: JSON.stringify(data, null, indent).replace(/\n/g, newline) + trailingNewline,
  };
}

try {
  if (process.argv.length !== 3 || !semver.test(version ?? "")) {
    throw new Error("Usage: npm run version 0.0.3 (provide a valid semantic version)");
  }

  // Prepare every change before writing, so invalid files cause no partial update.
  const updates = [
    updateJson("package.json", (data) => {
      data.version = version;
    }),
    updateJson("src-tauri/tauri.conf.json", (data) => {
      data.version = version;
    }),
    updateJson("package-lock.json", (data) => {
      data.version = version;
      if (data.packages) {
        if (!data.packages[""]) {
          throw new Error("package-lock.json is missing the root package entry");
        }
        data.packages[""].version = version;
      }
    }),
  ];

  const cargoFile = "src-tauri/Cargo.toml";
  const cargo = fs.readFileSync(path.join(root, cargoFile), "utf8");
  const packageSection = /^\[package\][ \t]*\r?\n[\s\S]*?(?=^\[|(?![\s\S]))/m;
  const section = cargo.match(packageSection)?.[0];
  const packageVersion = /^(version[ \t]*=[ \t]*)["'][^"'\r\n]*["']/m;
  if (!section || !packageVersion.test(section)) {
    throw new Error("src-tauri/Cargo.toml is missing [package].version");
  }
  updates.push({
    file: cargoFile,
    content: cargo.replace(packageSection, () =>
      section.replace(packageVersion, (_, prefix) => `${prefix}"${version}"`),
    ),
  });

  for (const { file, content } of updates) {
    fs.writeFileSync(path.join(root, file), content, "utf8");
  }
  console.log(`Updated ${updates.map(({ file }) => file).join(", ")} to ${version}`);
} catch (error) {
  console.error(error.message);
  process.exitCode = 1;
}
