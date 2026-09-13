import { defineConfig } from "vite";

// `npm run build` produces the HUD that oracle-core embeds and serves.
// `npm run build:demo` produces the static GitHub Pages demo: the same HUD with
// src/demo/entry.ts as the entry, which swaps in a scripted gateway.
export default defineConfig(({ mode }) => {
  if (mode !== "demo") return {};
  return {
    base: "/oracle-of-delphi/",
    build: { outDir: "dist-demo" },
    plugins: [{
      name: "oracle-demo-entry",
      transformIndexHtml: {
        order: "pre" as const,
        handler: (html: string) => html.replace("/src/main.ts", "/src/demo/entry.ts"),
      },
    }],
  };
});
