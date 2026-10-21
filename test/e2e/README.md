# End-to-end test

Loads the built extension into Chrome for Testing (branded Chrome ignores
`--load-extension` since version 137), then:

1. saves the Replicate token through the options page and checks it is not in
   `chrome.storage`, and tests it against Replicate;
2. opens `page.html` (all three carriers, including a `.jai` file in `src`, a
   `data-jai` whose `alt` is the prompt and one with its own prompt, JAI text in
   an `alt` without `data-jai` that must be ignored, and one plain image),
   injects the content script as the popup would, and checks discovery, that
   the `.jai` file is not fetched before use, and that the privileged fetch
   returns a cross-origin `.jai` resource but declines any other path on the
   same host;
3. checks nothing changes before the user asks, accepts the provider notice,
   renders everything, and checks replacement mechanics, accessible names,
   the AI-rendered description, layout boxes, that the plain image is untouched,
   and that the five identical documents shared **one** provider job;
   checks that a sized box with a different `aspect` is not stretched and that
   the flux render received only the `prompt/flux` refinement;
4. restores originals and checks every recorded state came back;
5. renders on `csp.html` (`img-src 'self'`) from the cache and checks the render
   is visible either in the image or through the canvas fallback;
6. checks that a content script cannot use the encoder's provider port;
7. opens the encode page, checks that it needs consent, and runs a tiny real
   encode (one generation of two candidates, each rendered with FLUX.1 schnell
   at two seeds and measured by GPT-5.6 luna; limit $0.10).

It costs about $0.03 on a fresh profile: two FLUX.1 schnell renders for the
viewer; for the encoder, GPT-5.6 luna calls and four FLUX.1 schnell renders
(two candidates by two seeds).

```sh
npm run build
# copy dist/extension somewhere Chrome can read, and add the test server under
# both host names to host_permissions in that copy only:
# "http://localhost:8766/*", "http://127.0.0.1:8766/*" (the second one is the
# cross-origin host for the privileged-fetch check)
(cd test/e2e && python3 -m http.server 8766)
npx @puppeteer/browsers install chrome@stable
node test/e2e/driver.mjs <chrome> <extension-copy> http://localhost:8766 <file-with-REPLICATE_API_TOKEN> <out-dir>
```

The driver needs `puppeteer-core` and must run on the same machine as Chrome.

Use a fresh profile directory after changing extension code: Chrome keeps running a
cached copy of the background worker in a reused profile, even after a version bump,
so tests silently exercise old code. A fresh profile re-renders (a few cents).

## With JAI Studio as the renderer

`studio.mjs` runs the viewer and the encode page against JAI Studio instead of
Replicate. By default it starts `test/fake-studio.mjs` on Studio's first free
port (5240 to 5249), at no cost, and checks connecting from the options page
(a token no Studio holds is refused), the render of `page.html` and a transform document, a 70 s reply
(the background worker must stay alive with no debugger attached, and stop once
idle afterwards), a refused token after the proof, a stopped Studio, the
encoder's port, a tiny encode and Remove. With `--real` it uses the JAI Studio
running on this computer, reading its token from Studio's data folder, in a
visible window; it skips the fake-only checks.

```sh
npm run build
# copy dist/extension, and add to host_permissions in that copy only:
# "http://127.0.0.1/*" (JAI Studio) and "http://localhost:8767/*" (the test pages,
# which the script serves itself)
node test/e2e/studio.mjs <chrome> <extension-copy> test/e2e <out-dir> [--real]
```
