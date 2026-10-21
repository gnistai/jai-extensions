# JAI tools — reference viewer, encoder and TypeScript library

Reference tools for [JAI (Just AI Images)](https://jai.gnist.ai/), an open
standard for images stored as a description instead of pixels. This repository
holds four layers, each depending only on the ones above it:

| Layer | Where | What |
|---|---|---|
| Library | `src/jai/` | Format logic only: no network, no rendering |
| Engines | `src/providers/`, `src/renderers/`, `src/encoder/` | UI-free and environment-neutral: Replicate and JAI Studio clients, renderers, encoder loop |
| Apps | `src/extension/`, `bin/jai.mjs`, `studio/` | The Chrome extension (viewer and encode page), the command line, and JAI Studio |
| Tests | `test/` | Unit tests with mocked providers, the spec's conformance suite, end to end in Chrome |

In detail:

- **`src/jai/`** — a dependency-free TypeScript library for JAI: parsing and
  canonical serialisation of JAI text, `.jai` files, `data:image/jai` URLs,
  HTML discovery, parameter interpretation and validation. It passes the
  specification's full conformance suite.
- **`src/renderers/`** — a provider-independent renderer interface and two
  adapters: Replicate (`replicate.ts`) and JAI Studio's local FLUX.1 schnell
  (`local.ts`).
- **`src/encoder/`** — the image encoder engine: image to JAI by analysis by
  synthesis, with pluggable analyst, optimiser and renderer, hard budgets and
  per-model refinements (`prompt/<target>`).
- **`src/extension/`** — a Chrome Manifest V3 extension that finds JAI images on
  web pages and renders them, when you ask, with JAI Studio on your own computer
  or your own Replicate account, and an **encode page** that turns an image into
  JAI.
- **`bin/jai.mjs`** — a command-line app: `jai encode`, `jai render`,
  `jai validate`. With `--local` it uses JAI Studio's models instead of Replicate.
- **`studio/`** — **JAI Studio**, one `jai` executable (Windows and Linux) that is a
  command line, a desktop viewer and encoder, a tray service, and a local model
  server: Qwen3.8 27B (llama.cpp) and FLUX.1 schnell (stable-diffusion.cpp) on your own GPU,
  behind OpenAI-style `/v1/chat/completions` and `/v1/images/generations` on 127.0.0.1. It
  runs the same TypeScript library and encoder inside an embedded JavaScript engine. See
  [`studio/README.md`](studio/README.md); the design and contracts are in
  [`studio/DESIGN.md`](studio/DESIGN.md).

The [specification and its conformance fixtures](https://github.com/gnistai/jai)
are authoritative; this extension demonstrates them and must not be read as
the definition of the format.

## What it does

1. Click the toolbar button on a page. The extension scans it for images that
   explicitly carry JAI: the attribute form (`data-jai`, where the image's `alt`
   is the prompt unless `data-jai` gives one), the picture form
   (`<source type="image/jai">`), or the src form (a `.jai` file or
   `data:image/jai` URL in `src`). `.jai` files are fetched only when you open
   an image's controls or render it. An `alt` without `data-jai` is never read
   as JAI. Ordinary images are never touched.
2. Each JAI image gets a small **JAI** control. It opens a panel with the
   parsed description, diagnostics, the canonical text, and a choice of renderer.
3. **Render** sends the description to the renderer chosen in the options, then
   swaps the render in while keeping the layout box. **Show original** switches
   back at any time. With **JAI Studio** the description goes to Studio's server
   on this computer and nothing leaves it; Studio renders every document with
   FLUX.1 schnell, and a `mode=transform` document from its text. With
   **Replicate** the description (and, for `mode=transform`, the picture shown
   on the page, if the page itself is allowed to read it) goes to Replicate
   under your account.

Per site you can choose *render when I ask* (the default), *render visible JAI
images automatically* (needs permission for that site, and respects your page
and daily limits), or *ignore JAI here*.

## Encoding images

The encoder turns an existing image into a JAI document that your chosen models
reproduce as closely as possible. It follows the specification's informative
section [Encoding images](https://jai.gnist.ai/spec/#image-encoding):

1. A vision model (GPT-5.6 luna by default, Gemini 2.5 Flash as an option)
   measures the original: its main subjects with bounding boxes, the prominent
   people with their boxes and visible traits (apparent age and gender, skin
   tone, hair, eyes, the shape of the face, clothing, pose), the horizon's height and shape, the
   camera, the light and the medium. Palette and lightness come from the
   pixels.
2. It writes several first descriptions. Every candidate is rendered with every
   target model and two fixed seeds.
3. Each render is measured the same way, and its fitness is computed from the
   differences: positions, sizes, people, horizon, palette and lightness.
   People weigh by how tall they appear in the frame; each visible trait that
   differs adds to a person's error (a gender swap or a large change of skin
   tone alone counts three quarters), and someone of another kind costs as
   much as someone missing: the aim is the same kind of person, not the same
   face. A candidate that names a group or likens someone to a named person is
   never rendered. Nothing is
   judged by feel, so scores are stable and comparable across runs. A
   candidate's fitness blends its mean and its worst per-model score.
4. An optimiser (by default the same model; `--optimiser sonnet` uses Claude
   Sonnet 5) sees the original's measurements and the best earlier candidates
   with their measured differences, largest first, and writes the next
   candidates. This repeats until a target fitness, a plateau or the budget.
5. Models that still lag get short `prompt/<family>` refinements written the
   same way. Independent restarts (`--restarts`) can share the budget; the
   best by fitness wins.

With JAI Studio (chosen in the extension's options, or the CLI's `--local`),
Qwen3.8 27B describes, measures and writes the candidates, FLUX.1 schnell is
the only target model, and the run costs nothing.

From the command line (reads `REPLICATE_API_TOKEN`; with `--local`, JAI
Studio's token from its data folder):

```sh
npm install && npm run build   # Node 20.9 or later
node bin/jai.mjs encode photo.jpg --models flux-schnell,sdxl --budget 0.60
# writes photo.jai and photo.jai.report/ (report.json and an HTML contact sheet)
node bin/jai.mjs render photo.jai --model sdxl --out check.png
node bin/jai.mjs validate "A castle;prompt/flux=at dusk"
```

A document argument that begins with `data:image/jai` followed by `,` or `;`
(ignoring case and surrounding spaces) is a `data:image/jai` URL, read as its
text body; `validate` also reports the URL's own advisories, such as
`data-url-parameter` for `;base64`. Otherwise one line ending in `.jai` is a
`.jai` file, read as the file form only (one gzip member of JSON; nothing else
is tried), and any other argument, prose that begins with "Data:" included, is
JAI text. A document that cannot be read (`malformed-container` or
`invalid-json` for a `.jai` file, `undecodable` for a `data:` URL, a
`data:image/jai` URL that carries no document, or a `.jai` file that exists but
cannot be read) makes `validate` and `render` exit with 1: `validate` prints
only its diagnostics, and `render` sends nothing. Usage errors, such as a
missing `.jai` file, exit with 2. `render` renders at the document's `aspect`,
else at its `preview`'s ratio, else at the model's default; it never fetches a
`source`, so a `mode=transform` document renders as `generate` and is reported
with `original-not-readable`.

In the extension, open **Encode** from the popup or right-click an image and
choose **Encode this image as JAI**. The page runs the same engine, on the
renderer chosen in the options. Every provider call goes through the background
worker, which holds the tokens and accepts calls only from the encode page
itself, and caps concurrent calls. With JAI Studio it passes on only Studio's
chat and image endpoints, their known fields and `data:` images, and the run is
free. With Replicate it rebuilds each input from an allow-list of fields per
model, accepts images only as re-encoded `data:` images or Replicate output
URLs, and enforces the run's spending limit, fixed when the run starts.

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
distribution. Open the extension's options and set up a renderer: paste JAI
Studio's token (**Copy** next to the token in Studio's Server tab, or **Copy
server token** in its tray menu) and press **Connect**, or paste a Replicate API
token (`r8_…`, from <https://replicate.com/account/api-tokens>) and enable the
models you want. The extension finds JAI Studio on this computer at ports 5240
to 5249.

## Privacy, security and cost

- **Nothing happens without you.** No image is rendered until you click Render
  (with Replicate, after you allow its provider notice for the site), or
  explicitly turn on automatic rendering for that site.
- **Your tokens stay in the extension.** They are stored in the extension's
  private IndexedDB (not `chrome.storage`, which in-page extension code can
  read) and used only by the background worker. The Replicate token is sent
  only to `api.replicate.com`; JAI Studio's only to the Studio server on
  `127.0.0.1` that has first proved it holds the same token.
- **Renders go only to the renderer you chose.** The extension sends JAI Studio
  on this computer the mapped parameters, and Replicate the mapped parameters
  plus, for `mode=transform`, the locally re-encoded image bytes. It never sends
  the page URL, title, cookies or other page content, and nothing goes through any Gnist
  server.
- **Documents cannot take control.** A document's `model` value only selects
  among the models you enabled. No key can make the extension download weights
  or workflows, change safety settings, pick an endpoint, or exceed your limits.
- **Pixels are only used when the page can read them.** For `mode=transform`, an
  image is sent only if the page itself could read it (same-origin or CORS);
  cross-origin images are never fetched with your cookies.
- **Costs are bounded.** Rendering with Replicate is paid per image on your
  account; JAI Studio costs nothing. Either way the extension caps renders per
  page, per day and in parallel, caches
  results, shares one job between identical images, and stops the job when you
  cancel or leave the page (JAI Studio finishes a render that has already
  started).
- **Encoding sends your image to JAI Studio or Replicate.** The encode page and
  the CLI send a copy, at most 1024 pixels on its longest side and without
  metadata, to the vision model that describes and measures it and writes the
  next candidates (the analyst and the optimiser). With JAI Studio (chosen in
  the extension's options, or the CLI's `--local`) that is Qwen3.8 27B in
  Studio's server on your own computer, and nothing leaves it. With Replicate it
  is the vision models on Replicate; the encode page sends nothing until you
  consent and start, and the spending limit is enforced before every call.
- **Bandwidth.** The browser has usually already downloaded the original image
  before the extension sees the page, so rendering does not save bandwidth.

## Supported models

| Model | Use | Notes |
|---|---|---|
| FLUX.1 schnell | text to image | fast, about $0.003 per image; default |
| FLUX.1 dev | text to image, image to image | `strength` maps to `prompt_strength` |
| FLUX.1 Kontext pro | image editing | best for `mode=transform`; no strength control (reported) |
| Stable Diffusion XL | text to image, image to image | native negative prompt for `avoid`; style terms first |

JAI Studio renders every document with FLUX.1 schnell on your own GPU, at no
cost.

How JAI parameters map to each model (aspect snapping, negative prompts or
folding, strength polarity, seeds) lives in
[`src/renderers/replicate.ts`](src/renderers/replicate.ts) (Replicate) and
[`src/renderers/local.ts`](src/renderers/local.ts) (JAI Studio). Every parameter the
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
about $0.03. `test/e2e/studio.mjs` does the same with JAI Studio as the
renderer, against `test/fake-studio.mjs` at no cost (connect, render, a
transform document, a 70 s reply, a refused token, a stopped Studio, a tiny
encode), or with `--real` against the JAI Studio running on this computer. See
[`test/e2e/README.md`](test/e2e/README.md).

### Adding a renderer

Implement the `Renderer` interface in `src/renderers/types.ts`: list the models
you support, map a `RenderIntent` to your provider's input, return image bytes,
the exact prompt sent and renderer diagnostics. `src/renderers/local.ts` (JAI Studio's
local FLUX.1 schnell) is an example of a second renderer behind the same interface.

## Licence

MIT. See [LICENSE](LICENSE). The JAI specification itself is CC0.
