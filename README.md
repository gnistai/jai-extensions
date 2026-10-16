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
   explicitly carry JAI: the attribute form (`alt` as the description plus
   `data-jai`), the picture form (`<source type="image/jai">`), or the src
   form (a `.jai` file or `data:image/jai` URL in `src`). `.jai` files are
   fetched only when you open an image's controls or render it. A plain `alt`
   is never read as JAI.
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

1. A vision model (GPT-5.6 luna by default, Gemini 2.5 Flash as an option)
   measures the original: its main subjects with bounding boxes, the horizon's
   height and shape, the camera, the light and the medium. Palette and
   lightness come from the pixels.
2. It writes several first descriptions. Every candidate is rendered with every
   target model and two fixed seeds.
3. Each render is measured the same way, and its fitness is computed from the
   differences: positions, sizes, horizon, palette and lightness. Nothing is
   judged by feel, so scores are stable and comparable across runs. A
   candidate's fitness blends its mean and its worst per-model score.
4. An optimiser (by default the same model; `--optimiser sonnet` uses Claude
   Sonnet 5) sees the original's measurements and the best earlier candidates
   with their measured differences, largest first, and writes the next
   candidates. This repeats until a target fitness, a plateau or the budget.
5. Models that still lag get short `prompt/<family>` refinements written the
   same way. Independent restarts (`--restarts`) can share the budget; the
   best by fitness wins.

From the command line (reads `REPLICATE_API_TOKEN`):

```sh
npm install && npm run build   # Node 20.9 or later
node bin/jai.mjs encode photo.jpg --models flux-schnell,sdxl --budget 0.60
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

A run for one model with six candidates per generation and two seeds costs
about $0.25 and takes five to seven minutes; on Earthrise it climbed from
fitness 0.54 to 0.72 by itself. Jointly optimising one core for FLUX.1 schnell
and SDXL does not converge yet (SDXL responds too erratically), so encode for
one model and let other models render the core. See the
[examples](https://jai.gnist.ai/examples/#encoding).

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
page with all three carriers (including a `.jai` file in `src`), and checks replacement, accessibility, restore,
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
