# JAI tools — reference viewer, encoder and TypeScript library

Reference tools for [JAI (Just AI Images)](https://jai.gnist.ai/), an open
standard for images stored as a description instead of pixels. This repository
holds four layers, each depending only on the ones above it:

| Layer | Where | What |
|---|---|---|
| Library | `src/jai/` | Format logic only: no network, no rendering |
| Engines | `src/providers/`, `src/renderers/`, `src/encoder/` | UI-free and environment-neutral: Replicate client, renderer, encoder loop |
| Apps | `src/extension/`, `bin/jai.mjs` | The Chrome extension (viewer and encode page) and the command line |
| Tests | `test/` | Unit tests with mocked providers, the spec's conformance suite, end to end in Chrome |

In detail:

- **`src/jai/`** — a dependency-free TypeScript library for JAI: parsing,
  canonical serialisation, JSON and `.jai` files, `data:` URLs, HTML
  discovery, parameter interpretation and validation. It passes the
  specification's full conformance suite.
- **`src/renderers/`** — a provider-independent renderer interface and the
  Replicate adapter.
- **`src/encoder/`** — the image encoder engine: image to JAI by analysis by
  synthesis, with pluggable analyst, scorer and renderer, hard budgets and
  per-model refinements (`prompt/<target>`).
- **`src/extension/`** — a Chrome Manifest V3 extension that finds JAI images on
  web pages and renders them with your own Replicate account, when you ask, and
  an **encode page** that turns an image into JAI.
- **`bin/jai.mjs`** — a command-line app: `jai encode`, `jai render`,
  `jai validate`.

The [specification and its conformance fixtures](https://github.com/gnistai/jai)
are authoritative; this extension demonstrates them and must not be read as
the definition of the format.

## What it does

1. Click the toolbar button on a page. The extension scans it for images that
   explicitly carry JAI: `<picture><source type="image/jai">`, `data-jai`,
   or `data:image/jai` in `src`. `alt` is never read as JAI: it stays the
   screen-reader text.
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

## Encoding images

The encoder turns an existing image into a JAI document that your chosen models
reproduce as closely as possible. It follows the specification's informative
section [Encoding images](https://jai.gnist.ai/spec/#image-encoding):

1. A vision model (Gemini 2.5 Flash by default, or Claude Sonnet 4.5) writes
   several first descriptions.
2. Every candidate is rendered with every target model, with fixed seeds.
3. Each render is scored: CLIP image-embedding similarity, a coarse colour and
   layout distance between thumbnails, and the vision model's own rating. The
   current best is rated again in every call, because the ratings are noisy.
   A candidate's fitness blends its mean and its worst per-model score, so the
   core description cannot sacrifice one model.
4. The vision model lists the differences between the best renders and the
   original and writes revised descriptions. This repeats for a number of
   generations or until scores stop improving.
5. For models that still lag, it tries short `prompt/<family>` refinements and
   keeps one only if it raises that model's score. It skips models whose
   prompt window the core already fills, and models that share a family with
   another target model.
6. It stops at the spending limit, which it enforces from price estimates
   before every call. The result is the best document plus a report.

From the command line (reads `REPLICATE_API_TOKEN`):

```sh
npm install && npm run build   # Node 20.9 or later
node bin/jai.mjs encode photo.jpg --models flux-schnell,sdxl --budget 0.50
# writes photo.jai and photo.jai.report/ (report.json and an HTML contact sheet)
node bin/jai.mjs render photo.jai --model sdxl --out check.webp
node bin/jai.mjs validate "jai:A castle;prompt/flux=at dusk"
```

In the extension, open **Encode** from the popup or right-click an image and
choose **Encode this image as JAI**. The page runs the same engine. Every
provider call goes through the background worker, which holds the token and
accepts calls only from the encode page itself. It rebuilds each input from an
allow-list of fields per model and accepts images only as re-encoded `data:`
images or Replicate output URLs. It also enforces the run's spending limit,
fixed when the run starts, and caps concurrent calls.

A typical run for two models, four candidates and six generations costs
roughly $0.20 to $0.40 and takes five to eight minutes. See the
[Earthrise example](https://jai.gnist.ai/examples/#encoding).

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
- **Encoding sends your image to Replicate.** The encode page and the CLI send
  a copy, at most 1024 pixels on its longest side and without metadata, to the
  vision model and to CLIP on Replicate. Nothing is sent until you consent and
  start, and the spending limit is enforced before every call.
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
de-duplication and a strict-CSP page, checks that content scripts cannot use
the encoder's port, and runs a tiny real encode on the encode page. It costs
about $0.03. See [`test/e2e/README.md`](test/e2e/README.md).

### Adding a renderer

Implement the `Renderer` interface in `src/renderers/types.ts`: list the models
you support, map a `RenderIntent` to your provider's input, return image bytes,
the exact prompt sent and renderer diagnostics. A local ComfyUI or diffusers
renderer would fit the same interface without any change to the format.

## Licence

MIT. See [LICENSE](LICENSE). The JAI specification itself is CC0.
