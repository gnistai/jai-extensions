# End-to-end test

Loads the built extension into Chrome for Testing (branded Chrome ignores
`--load-extension` since version 137), then:

1. saves the Replicate token through the options page and checks it is not in
   `chrome.storage`, and tests it against Replicate;
2. opens `page.html` (all four carriers and one plain image), injects the
   content script as the popup would, and checks discovery;
3. checks nothing changes before the user asks, accepts the provider notice,
   renders everything, and checks replacement mechanics, accessible names,
   the AI-rendered description, layout boxes, that the plain image is untouched,
   and that the four identical documents shared **one** provider job;
4. restores originals and checks every recorded state came back;
5. renders on `csp.html` (`img-src 'self'`) from the cache and checks the render
   is visible either in the image or through the canvas fallback.

It costs one prediction (about $0.003 with FLUX.1 schnell).

```sh
npm run build
# copy dist/extension somewhere Chrome can read, and add the test server to
# host_permissions in that copy only: "http://localhost:8766/*"
(cd test/e2e && python3 -m http.server 8766)
npx @puppeteer/browsers install chrome@stable
node test/e2e/driver.mjs <chrome> <extension-copy> http://localhost:8766 <file-with-REPLICATE_API_TOKEN> <out-dir>
```

The driver needs `puppeteer-core` and must run on the same machine as Chrome.
