import { defineConfig } from "vite";
import solid from "vite-plugin-solid";

export default defineConfig({
  root: "tab-bar",
  base: "./",
  plugins: [solid()],
  build: {
    outDir: "../dist/tab-bar",
    emptyOutDir: true,
  },
});
