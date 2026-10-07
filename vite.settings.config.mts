import { defineConfig } from "vite";
import solid from "vite-plugin-solid";

export default defineConfig({
  root: "settings",
  base: "./",
  plugins: [solid()],
  build: {
    outDir: "../dist/settings",
    emptyOutDir: true,
  },
});
