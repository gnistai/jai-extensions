# JAI Viewer — reference browser extension

The reference viewer for [JAI (Just AI Images)](https://gnistai.github.io/jai/), an
open standard for images stored as a description instead of pixels. This
repository holds:

- **`src/jai/`** — a dependency-free TypeScript library for JAI: parsing,
  canonical serialisation, JSON and `.jai` files, `data:` URLs, HTML
  discovery, parameter interpretation and validation. It passes the
  specification's full conformance suite.
- **`src/renderers/`** — a provider-independent renderer interface and the
  Replicate adapter.
- **`src/extension/`** — a Chrome Manifest V3 extension that finds JAI images on
  web pages and renders them with your own Replicate account, when you ask.

The [specification and its conformance fixtures](https://github.com/gnistai/jai)
are authoritative; this extension demonstrates them and must not be read as
the definition of the format.

## What it does

1. Click the toolbar button on a page. The extension scans it for images that
   explicitly carry JAI: `<picture><source type="image/jai">`, `data-jai`,
   `data:image/jai` in `src`, or the deprecated `jai:` prefix in `alt`.
   Ordinary images are never touched.
2. Each JAI image gets a small **JAI** control. It opens a panel with the raw and
   parsed description, diagnostics, the canonical form, and a choice of renderer.
3. **Render** sends the description (and, for `mode=transform`, the picture shown
   on the page, if the page itself is allowed to read it) to Replicate, then
   swaps the render in while keeping the layout box. **Show original** switches
   back at any time.

Per site you can choose *render when I ask* (the default), *render visible JAI
images automatically* (needs permission for that site, and respects your page
and daily limits), or *ignore JAI here*.

## Install

From source:

```sh
npm install
npm run build          # writes dist/extension/
```

Then open `chrome://extensions`, enable **Developer mode**, choose **Load
unpacked**, and select `dist/extension`. `npm run package` writes a zip for
distribution. Open the extension's options, paste a Replicate API token
(`r8_…`, from <https://replicate.com/account/api-tokens>), and enable the models
you want.

## Privacy, security and cost

- **Nothing happens without you.** No image is rendered until you allow the
  provider notice for a site and click Render, or explicitly turn on automatic
  rendering for that site.
- **Your token stays in the extension.** It is stored in the extension's private
  IndexedDB (not `chrome.storage`, which in-page extension code can read), used
  only by the background worker, and sent only to `api.replicate.com`.
- **Only Replicate sees your renders.** The extension sends the mapped
  parameters and locally re-encoded image bytes. It never sends the page URL,
  title, cookies or other page content, and nothing goes through any Gnist
  server.
- **Documents cannot take control.** A document's `model` value only selects
  among the models you enabled. No key can make the extension download weights
  or workflows, change safety settings, pick an endpoint, or exceed your limits.
- **Pixels are only used when the page can read them.** For `mode=transform`, an
  image is sent only if the page itself could read it (same-origin or CORS);
  cross-origin images are never fetched with your cookies.
- **Costs are bounded.** Rendering is paid per image on your Replicate account.
  The extension caps renders per page, per day and in parallel, caches
  results, shares one job between identical images, and cancels provider jobs
  when you cancel or leave the page.
- **Bandwidth.** The browser has usually already downloaded the original image
  before the extension sees the page, so rendering does not save bandwidth.

## Supported models

| Model | Use | Notes |
|---|---|---|
| FLUX.1 schnell | text to image | fast, about $0.003 per image; default |
| FLUX.1 dev | text to image, image to image | `strength` maps to `prompt_strength` |
| FLUX.1 Kontext pro | image editing | best for `mode=transform`; no strength control (reported) |
| Stable Diffusion XL | text to image, image to image | native negative prompt for `avoid`; style terms first |

How JAI parameters map to each model (aspect snapping, negative prompts or
folding, strength polarity, seeds) lives in
[`src/renderers/replicate.ts`](src/renderers/replicate.ts). Every parameter the
model could not honour as written is reported with the specification's
renderer diagnostic codes, and the panel shows the exact text sent.

## Development

```sh
npm run typecheck      # strict TypeScript
npm test               # build, unit tests, then the spec's conformance suite against dist/jai.mjs
npm run conformance    # conformance only; uses ../jai, $JAI_SPEC_DIR, or clones gnistai/jai
```

`test/e2e/` contains an end-to-end test that loads the built extension into
Chrome for Testing, saves a token through the options page, renders a test
page with all four carriers, and checks replacement, accessibility, restore,
de-duplication and a strict-CSP page. It renders one real image. See
[`test/e2e/README.md`](test/e2e/README.md).

### Adding a renderer

Implement the `Renderer` interface in `src/renderers/types.ts`: list the models
you support, map a `RenderIntent` to your provider's input, return image bytes,
the exact prompt sent and renderer diagnostics. A local ComfyUI or diffusers
renderer would fit the same interface without any change to the format.

## Licence

MIT. See [LICENSE](LICENSE). The JAI specification itself is CC0.
