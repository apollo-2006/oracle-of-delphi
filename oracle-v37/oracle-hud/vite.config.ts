import { defineConfig } from "vite";

const DEMO_URL = "https://apollo-2006.github.io/oracle-of-delphi/";
const DEMO_DESCRIPTION =
  "The Oracle of Delphi HUD, live in the browser with a scripted core: tool calls, the Apollo decree for irreversible actions, and spoken replies.";
const DEMO_HEAD = `<title>Oracle of Delphi · live HUD demo</title>
    <meta name="description" content="${DEMO_DESCRIPTION}">
    <meta name="theme-color" content="#04060c">
    <link rel="icon" type="image/svg+xml" href="/oracle-of-delphi/favicon.svg">
    <meta property="og:type" content="website">
    <meta property="og:site_name" content="Abir Deol">
    <meta property="og:title" content="Oracle of Delphi · live HUD demo">
    <meta property="og:description" content="${DEMO_DESCRIPTION}">
    <meta property="og:url" content="${DEMO_URL}">
    <meta property="og:image" content="${DEMO_URL}og.jpg">
    <meta property="og:image:width" content="1200">
    <meta property="og:image:height" content="630">
    <meta name="twitter:card" content="summary_large_image">`;

// `npm run build` produces the HUD that oracle-core embeds and serves.
// `npm run build:demo` produces the static GitHub Pages demo: the same HUD with
// src/demo/entry.ts as the entry, which swaps in a scripted gateway.
export default defineConfig(({ mode }) => {
  if (mode !== "demo") return {};
  return {
    base: "/oracle-of-delphi/",
    // og.jpg and the favicon live here, so the embedded HUD never ships them.
    publicDir: "demo-public",
    build: { outDir: "dist-demo" },
    plugins: [{
      name: "oracle-demo-entry",
      transformIndexHtml: {
        order: "pre" as const,
        handler: (html: string) =>
          html
            .replace("/src/main.ts", "/src/demo/entry.ts")
            .replace("<title>Oracle of Delphi</title>", DEMO_HEAD),
      },
    }],
  };
});
