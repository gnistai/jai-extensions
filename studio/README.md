# JAI Studio — the `jai` app, CLI and local model server

JAI Studio is one executable, `jai` (`jai.exe` on Windows). It is:

- the JAI command-line tool: `validate`, `render` and `encode`, plus model and server commands;
- a desktop app for viewing `.jai` files and encoding images into `.jai` documents;
- a tray service that keeps a local model server running;
- a local model server on `127.0.0.1:5240` with two OpenAI-style endpoints:
  LLM chat completions (`qwen3.8-27b`) and image generations (`flux-schnell`).

Behind the server, Studio runs two engines and loads one at a time on the GPU:
llama.cpp's `llama-server` with Qwen3.8 27B (UD-IQ3_XXS plus its vision projector),
and stable-diffusion.cpp's `sd-server` with FLUX.1 schnell. Engines and models are
downloaded on first run, after you click **Download models** (or run
`jai models download`). Nothing is downloaded before that, and nothing leaves the PC
afterwards: no account, no telemetry.

[`DESIGN.md`](DESIGN.md) is the contract between the parts; this page is the user and
developer guide.

## Quick start (Windows)

1. Run `jai.exe`. It is not code-signed yet, so the first run may stop at SmartScreen's
   "Windows protected your PC": choose **More info**, then **Run anyway**. With Smart App
   Control turned on (Windows 11), Windows blocks unsigned programs and offers no way past;
   it can only be turned off in Windows Security. The window opens on **Models**, which lists
   every file with its size, source and licence, the total to download and the free disk space.
2. Click **Download models**. Files that LM Studio already has (same name and size, in its
   models folder or the downloads folder set in its settings) are checked against their
   SHA-256 and hard-linked instead of downloaded. A file on another disk, or one that other
   accounts on the PC can change, is copied instead (the Models tab says so). Untick
   "Reuse files found on this PC" to download everything. Downloads resume after a
   cancel or a lost connection.
3. Open a `.jai` file in **View** (drop it, click Open, or run `jai photo.jai`), or turn an
   image into a `.jai` document in **Encode**.

Closing the window keeps Studio in the tray (Settings: "Keep running in the tray when the
window is closed"). Quit from the tray menu stops the engines and the server.

Requirements:
- Windows 10 or 11 x64 with the Microsoft Edge WebView2 Runtime. Windows 11 includes it; on
  Windows 10 install the Evergreen runtime from
  <https://developer.microsoft.com/microsoft-edge/webview2/>. The tray mode needs it too (it
  keeps a hidden window), so without it Studio says so and exits.
- Disk: the CUDA set is a 31.8 GB download (models 30.3 GB, engines and CUDA runtimes 1.5 GB).
  The download starts only with that much free plus 1 GiB to spare, so plan on about 33 GB
  free; the engine archives grow by roughly a third when they are unpacked, then are deleted.
  The Vulkan and CPU sets are about 1.4 GB smaller.
- The defaults are set for an NVIDIA GPU with 16 GB of VRAM. `backend: auto` picks the CUDA builds
  on an NVIDIA GPU of the Turing generation or newer (GTX 16 and RTX 20 series and later) with
  driver 580 or newer, and the Vulkan builds otherwise; the Server tab says why it did not pick
  CUDA. `backend` in the settings can force `cuda`, `vulkan` or `cpu`.
- With less VRAM, edit `config.json` (see Settings and data): set `llm.mtp` to false (it takes
  about 2 GB), lower `llm.ctx` (for example to 8192), and if the LLM still does not fit, set
  `llm.gpuLayers` to a number of layers instead of `"all"` (the rest run on the CPU, more
  slowly). FLUX needs no setting: it keeps 3 GB of VRAM free by itself (below).
- The CUDA engines need the Visual C++ runtime 14.40 or newer; the Models tab offers to install
  it when it is missing.

## Command line

```
jai                                   GUI (window + tray + server)
jai <path.jai>  |  jai open <path>    GUI viewer on that file (forwarded to a running instance)
jai tray                              start in the tray only (start at login uses this)
jai serve [--host H] [--port P] [--allow-lan]      headless server in the foreground; Ctrl+C stops
jai models status | download [--yes] [--no-reuse] [--only <ids,…>] | verify | path
jai generate "<prompt>" [--size WxH] [--seed N] [--steps N] [--n N] [--format png|jpeg|webp] [--out FILE]
jai chat "<prompt>" [--image FILE]… [--system TEXT] [--think] [--max-tokens N] [--json]
jai render <file.jai|data:image/jai,…|text> [--seed S] [--out FILE]
jai validate <file.jai|data:image/jai,…|text>
jai encode <image> [--generations N=8] [--population N=4] [--seeds 1,2] [--restarts N=1] [--goal F=0.9]
                   [--scale F=1] [--think off|propose|all] [--out F.jai] [--report DIR] [--force]
jai status                            server and engine status
jai token [--regenerate]              print the server URL and token, for clients of the HTTP API
jai licenses                          print the licences of the open-source software in jai
jai --version | --help
```

- `generate`, `chat`, `render` and `encode` use a shared server (the GUI's, the tray's or
  `jai serve`). When none runs, the command starts a temporary server in its own process
  and stops the engines when it ends. That server is private to the command (a random
  port and a token of its own), so a second command started meanwhile waits for the GPU
  ("Waiting for the GPU…"; Ctrl+C stops waiting) rather than coming to depend on a server
  that is about to stop. To share the loaded models between commands, keep JAI Studio, its
  tray or `jai serve` running. `status` reports the shared server, or the local install and
  GPU when there is none.
- A document argument that begins with `data:image/jai` followed by `,` or `;` (ignoring
  case and surrounding spaces) is a `data:image/jai` URL, read as its text body. Otherwise
  one line ending in `.jai` is a `.jai` file, read as the file form only (one gzip member of
  JSON; nothing else is tried); a missing one is a usage error. Any other argument, prose
  that begins with "Data:" and other `data:` URLs included, is JAI text. The Node CLI reads
  document arguments the same way.
- `validate` prints the canonical text and any diagnostics, for a `data:image/jai` URL its
  own advisories too (such as `data-url-parameter` for `;base64`). Exit code 1 means the
  document cannot be read: `malformed-container` or `invalid-json` for a `.jai` file,
  `undecodable` for a `data:` URL, or a `data:image/jai` URL that carries no document (no
  `,`, or a `;base64` body that is not base64). For a fatal code `validate` prints only the
  decoder's diagnostics (and a URL's advisories), with no canonical text; for a URL that
  carries no document it says so. `render` refuses an unreadable document the same way,
  before it looks for a server.
- `render` renders at the document's `aspect`, else at its `preview`'s ratio, else square.
  `render --seed` takes 1 to 20 digits and replaces the document's seed; `render --out`
  takes a `.png`, `.jpg` or `.jpeg` file name (`generate` also writes WebP).
- `encode` writes `<image>.jai` and a report folder (`<out>.report`: `target.jpg`,
  `renders/`, `report.json` and an `index.html` contact sheet). It checks before it starts
  that the output can be written; if the `.jai` still cannot be written at the end, the
  result text is printed and kept in `report.json`. Ctrl+C stops it and keeps the best
  result so far.
- `models download` asks before it starts unless `--yes` is given; without a terminal,
  `--yes` is required. It lists the licence of each component first.
- A mistyped subcommand is a usage error with a suggestion (`jai stauts` suggests
  `status`), not a document to open.
- Text that comes from documents, models or engines is printed with its control characters
  shown as escapes (`\x1b`), so it cannot take over the terminal. `chat` writes the model's
  answer exactly when its output is redirected.
- Exit codes: 0 success (also when the reader of a pipe stops early, as `| head` does),
  1 failure, 2 usage error.

The Node CLI (`bin/jai.mjs`) can use the same server with `--local`:
`node bin/jai.mjs encode photo.png --local` finds it through `server.json` and the token
file, so no Replicate token is needed, and sends the token only after the server there has
proved that it holds it (the `/health` proof below). It needs a shared server (JAI Studio,
its tray or `jai serve`).

The JAI browser extension renders and encodes with JAI Studio once you paste the token into its
options and press **Connect**: copy it with **Copy** next to the token in the Server tab, or with
**Copy server token** in the tray menu. The extension finds Studio on `127.0.0.1` at ports 5240
to 5249 and sends the token only to the one that proves it holds it, so a Studio set to another
port, or bound only to a LAN address, is not found. With "Keep running in the tray when the
window is closed" (on by default), closing the window keeps Studio serving the extension from the
tray; "Start in the tray when I sign in" (Windows) has it ready after a restart.

Other programs use the HTTP API below with the URL and token that `jai token` prints, for
example any OpenAI-compatible client pointed at `http://127.0.0.1:5240/v1`.

## HTTP API

Base URL `http://127.0.0.1:5240` (5241 to 5249 when 5240 is busy; `jai token` prints the
URL in use). The server also listens on `[::1]` at the same port, so `http://localhost:5240`
works whichever address `localhost` resolves to. Every request follows these rules:

| Rule | Detail |
|---|---|
| Token | Everything except `GET /health` needs `Authorization: Bearer <token>`; `jai token` prints it. Otherwise 401 `unauthorized`. |
| Host | `Host` must be `127.0.0.1:<port>`, `localhost:<port>` or `[::1]:<port>`, plus the bound address when it is another one (a LAN address with `allowLan`, or another loopback address). Otherwise 403 `bad_host`; this blocks DNS rebinding. |
| Connections | A request head must arrive within 30 s, and a keep-alive connection idle for 30 s is closed. At most 256 connections are served at once; more wait. |
| Origin | When present it must start with `chrome-extension://`, `moz-extension://` or `safari-web-extension://`. Otherwise 403 `bad_origin`. Studio sends no CORS headers, so web pages cannot use it. |
| Body size | 64 MiB for `/v1/chat/completions`, 1 MiB for everything else (413 `body_too_large`). |

Errors use OpenAI's shape:

```json
{"error": {"message": "…", "type": "invalid_request_error|auth_error|server_error|unavailable_error", "code": "…"}}
```

When the models are not installed, model requests answer 409 `models_missing`: "Download
the models first: open JAI Studio or run `jai models download`."

### `GET /health`

No token needed. `{"status":"ok","name":"jai","version":"0.1.0"}`. With
`?nonce=<16 to 128 base64url characters>` it also returns `proof`, the base64url HMAC-SHA256
of `jai-health:<port>:<nonce>` under the token: a client can check that it found the real
server before it sends the token. That answer also gives the server's `pid`.

### `GET /v1/models`

```json
{"object":"list","data":[
  {"id":"qwen3.8-27b","object":"model","owned_by":"jai","kind":"llm","installed":true},
  {"id":"flux-schnell","object":"model","owned_by":"jai","kind":"image","installed":true,"aliases":["black-forest-labs/flux-schnell"]}]}
```

### `GET /v1/status`

```json
{"version":"0.1.0","installed":true,"backend":"cuda","imageBackend":"cuda",
 "gpu":{"name":"NVIDIA GeForce RTX 5070 Ti","vramUsedMb":1024,"vramTotalMb":16303},
 "engines":{"loaded":"llm","state":"ready","active":0,"waitingLlm":0,"waitingImage":0,
            "mtp":true,"lastError":null,"swaps":2,"sinceMs":1792232605675}}
```

`gpu` is null without an NVIDIA GPU. `backend` is the LLM engine's build and
`imageBackend` the image engine's (Linux has no CUDA build of stable-diffusion.cpp).

### `POST /v1/chat/completions`

An OpenAI chat-completions body, checked and forwarded to llama-server:

- `messages` is required (400 `missing_messages`; not an array or empty: `invalid_messages`).
  Content is a string or a list of `text` and `image_url` parts.
- Images must be `data:image/(png|jpeg|gif|bmp);base64,…` URLs that decode in full as the
  declared type, at most 40 MB (counted as three quarters of the base64 length) and 64
  megapixels each; remote URLs, file paths and WebP are refused (400 `invalid_image`). At most
  8 images per request (400 `too_many_images`). Studio decodes each image and encodes it again
  before the engine sees it, with its EXIF orientation applied: a JPEG as a JPEG, the others as
  PNG (a GIF's first frame).
- `model` is absent, `qwen3.8-27b` or `default` (else 400 `model_not_found`); `n` is absent or 1.
- `max_tokens` and `max_completion_tokens` are clamped to 8192, which is also the default.
- `reasoning_effort` is `none`, `low`, `medium`, `high` or `xhigh`.
- Forwarded unchanged: `stream`, `chat_template_kwargs`, `response_format` (including
  JSON schemas), `temperature`, `top_p`, `top_k`, `presence_penalty`, `seed`, `stop`.
  Other fields are dropped.
- `stream: true` answers with server-sent events. The response's `model` is `qwen3.8-27b`.
- Qwen thinks before answering unless told not to: send
  `"chat_template_kwargs": {"enable_thinking": false}` for a direct answer.

```sh
curl http://127.0.0.1:5240/v1/chat/completions -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"messages":[{"role":"user","content":"Describe a lighthouse at dusk in one sentence."}],
       "chat_template_kwargs":{"enable_thinking":false}}'
```

Closing the connection gives up the GPU wait and aborts the request upstream.

### `POST /v1/images/generations`

```json
{"prompt":"a lighthouse at dusk","model":"flux-schnell","n":1,"size":"1344x768",
 "seed":1,"steps":4,"output_format":"png","output_compression":90,"response_format":"b64_json"}
```

| Field | Rule |
|---|---|
| `prompt` | 1 to 8000 characters; `<sd_cpp_extra_args` is refused (400 `invalid_prompt`). |
| `model` | Absent, `flux-schnell` or `black-forest-labs/flux-schnell` (else 400 `model_not_found`). |
| `size`, or `width` and `height` | Each side a multiple of 64 from 256 to 2048, at most 2,359,296 pixels; default 1024x1024 (400 `invalid_size`). |
| `steps` | 1 to 16; default 4 (`image.steps` in the settings). |
| `n` | 1 to 4. Image *i* uses seed `seed + i`. |
| `seed` | 0 to 4294967295; drawn at random when absent, and reported per image. |
| `output_format` | `png` (default), `jpeg` or `webp`; `output_compression` 0 to 100, default 90. |
| `response_format` | `b64_json` only (`url` is 400 `unsupported_response_format`). |

```json
{"created":1792232605,"model":"flux-schnell","width":1344,"height":768,"steps":4,"output_format":"png",
 "data":[{"b64_json":"…","seed":1}],"timings":{"queueMs":5210,"generateMs":3480}}
```

`queueMs` includes waiting for the GPU and loading the engine. A request takes at most
10 minutes (504 `timeout`); closing the connection cancels a queued job.

### Error codes

| Status | Code | Meaning |
|---|---|---|
| 400 | `invalid_json`, `invalid_body`, `invalid_value`, `missing_messages`, `invalid_messages`, `invalid_content`, `unsupported_content`, `invalid_image`, `too_many_images`, `invalid_prompt`, `invalid_size`, `unsupported_response_format`, `model_not_found`, `invalid_nonce` | The request breaks a rule above; the message says which. |
| 401 | `unauthorized` | Missing or wrong token. |
| 403 | `bad_host`, `bad_origin` | Host or Origin rule. |
| 404 | `not_found` | Unknown endpoint. |
| 405 | `method_not_allowed` | Wrong method for the endpoint. |
| 409 | `models_missing` | Download the models first. |
| 413 | `body_too_large` | Over the body limit. |
| 4xx | llama-server's own type, e.g. `exceed_context_size_error`; else `upstream_rejected` | The engine refused the request (`upstream_rejected` when its answer names no code, as sd-server's do). |
| 500 | `generation_failed`, `internal_error` | The engine or Studio failed. |
| 502 | `upstream_error` | The engine stopped answering or answered wrongly. |
| 503 | `engine_start_failed`, `engine_busy`, `shutting_down`, `cancelled` | The engine could not start (the message has the reason; `logs/llm.log` or `logs/image.log` has its output), is overloaded, or Studio is quitting. |
| 504 | `timeout` | An image took longer than 10 minutes. |

### Scheduling

Only one engine is loaded at a time. Requests for the loaded engine are served in order;
a request for the other engine waits until the current one has no more waiting requests
(or has made it wait 120 s), then the engines swap. The encoder keeps up to four renders and
four inspections in flight, each kind's queue filled until that kind of work runs out for the
generation, so a generation costs two swaps and no request waits for a whole generation. After 15 idle minutes the engine is
unloaded (`idleUnloadMinutes`, 0 keeps it loaded); **Unload models** in the tray or the
Server tab frees the GPU at once. When the LLM fails to start with multi-token prediction,
Studio retries once without it and reports `mtp: false` for the rest of the session.

The LLM is loaded whole into VRAM (`gpuLayers: "all"`), so on CUDA each LLM start first reads
the free VRAM. The desktop takes its share too, more with several or 4K monitors. On Windows
a model that does not fit does not fail: the driver moves the excess into shared system memory,
and the LLM gets many times slower. So, with the default 20k context:

| Free VRAM | The LLM starts with | Measured on an RTX 5070 Ti |
|---|---|---|
| 13.6 GB or more | MTP and the default f16 KV cache | 13.1 GB, about 75–95 tok/s |
| 13.1 to 13.6 GB | MTP and a q8_0 KV cache (unless `llm.extraArgs` sets the cache type) | 12.6 GB, about 80 tok/s |
| 12.7 to 13.1 GB | no MTP, for this start only | 12.2 GB, about 56 tok/s |
| less | no MTP, with a warning in `jai.log` that it may spill into shared memory | |

The check runs at every LLM start, so MTP comes back once VRAM is free again (after closing a
game, say). The figures include a 0.5 GB margin and grow with `llm.ctx`.

FLUX keeps 3 GB of VRAM free for the desktop and other programs (sd-server's `--max-vram -3`):
its weights wait in RAM and move to the GPU as far as that allows. On a 16 GB card a render at
1216x832 takes about 6.3 s; without the reserve it would fail as soon as the desktop and other
programs took more than about 2 GB.

## Settings and data

| Where | What |
|---|---|
| Windows | `%LOCALAPPDATA%\JAI` |
| Linux | `$XDG_DATA_HOME/jai`, or `~/.local/share/jai` |
| `JAI_HOME` | overrides both |

The folder holds the server token and the engines Studio runs, so it must be private to you,
wherever it is. A folder Studio creates is made private (Windows: an access list for you,
SYSTEM and Administrators only, inherited by everything inside; Linux: mode 0700). An existing
folder that other accounts can open is made private when it holds only Studio's files;
otherwise Studio refuses to use it and says why. So let Studio create the folder: point
`JAI_HOME` at a folder that does not exist yet on an NTFS drive, such as `D:\JAI` (FAT and
exFAT drives have no permissions at all). A folder made by hand under a drive root inherits
"Authenticated Users: Modify" on Windows, which would let every account on the PC use your
token and replace the engines. `models`, `engines` and `downloads` may be links to another
disk (to keep the 30 GB elsewhere), but not to a folder that other accounts can change.

The folder holds `config.json` (settings), `token`, `server.json` (the shared server, for
discovery), `gpu.lock`, `models/`, `engines/`, `downloads/` (resumable `.part` files),
`logs/` (`jai.log`, `llm.log`, `image.log`), `cache/renders/` (viewer renders),
`cache/no-ffmpeg/` (an empty folder, see Security) and `encodings/` (GUI encode jobs, each
with its renders, report and result).

`config.json`, every field optional:

```json
{
  "version": 1,
  "server": { "host": "127.0.0.1", "port": 5240, "allowLan": false },
  "idleUnloadMinutes": 15,
  "backend": "auto",
  "llm": { "ctx": 20480, "gpuLayers": "all", "mtp": true, "parallel": 1, "imageMaxTokens": 1536, "extraArgs": [] },
  "image": { "steps": 4, "extraArgs": [] },
  "app": { "closeToTray": true, "startAtLogin": false, "fileAssociation": false }
}
```

Saving keeps fields Studio does not know. A `config.json` with a syntax error loads as the
defaults (the Server tab says so), and the next save renames it to
`config.json.invalid-<time>` first, so a hand edit is not lost.

`llm.extraArgs` and `image.extraArgs` are appended to the engine command lines, to tune an
engine (threads, cache types, sd-server's memory options). Flags that set what Studio sets or
would open the engine up are refused, as `--flag value` or `--flag=value`, and the settings
then fail with the flag named: for llama-server `-m --model -mm --mmproj --host --port
--api-key --api-key-file --webui --path --media-path --tools --tools-runtime
--slot-save-path --log-file --video-ffmpeg-dir --rpc` and the download flags (`-hf`,
`--hf-repo`, `--hf-file`, `--model-url`, `--mmproj-url`, `--docker-repo` and their short
forms); for sd-server `-m --model --diffusion-model --vae --clip_l --t5xxl -l --listen-ip
--listen-port --serve-html-path --lora-model-dir --embd-dir -o --output --preview-path`. The
settings form has no field for them, and saving it keeps what `config.json` holds. Other
environment variables: `JAI_PORT` (overrides `server.port` for that run; the Server tab shows
and saves `config.json`, so the override is never saved) and `JAI_LOG` (a tracing filter such
as `debug` or `jai_server=trace`). The settings that restart the server or the models (port,
backend, engine settings) cannot be changed while an encode runs or the models are answering
requests; the Server tab says so and saves nothing. A new idle-unload time applies at once. `logs/jai.log` starts afresh past 10 MB
(the old one is kept as `jai.log.1`).

Opt-in Windows integration (Server tab settings): "Open .jai files with JAI Studio" writes
the file association under `HKCU\Software\Classes`, and "Start in the tray when I sign in"
writes `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`. Turning them off removes the
entries. No administrator rights are needed. The UI offers these two options on Windows
only. One key is always written, because Windows shows no notifications from an unpackaged
program without it: `HKCU\Software\Classes\AppUserModelId\ai.gnist.jai` (the name and icon of
Studio's notifications; the icon is `cache\notification-icon.png` in the data folder).

## Security

- The server binds 127.0.0.1, and [::1] on the same port, so no other program can listen where
  `localhost` may lead. Binding another address needs `--allow-lan` (or `allowLan`), still
  requires the token, and the UI shows a warning.
- The token (`jai_` + 32 random bytes, base64url) lives in the data folder, which is private
  to you (see Settings and data): on Unix the file is also mode 0600. `jai token
  --regenerate` or **Make a new token** in the Server tab replaces it; clients that use it
  must then be updated. A `jai` command's temporary server uses a token of its own that never
  leaves the process. Clients find the server through `server.json` (the browser extension,
  which cannot read it, asks ports 5240 to 5249) and send it the token only after it has proved
  that it holds the token (`/health?nonce=…`), so a program that took the port of a server that
  crashed never sees it. The extension stores its copy in its own IndexedDB and uses it only in
  its background worker; web pages and content scripts never see it.
- The Host allow-list and the Origin rule keep web pages out; there are no CORS headers.
  Request heads must arrive within 30 s and at most 256 connections are served at once.
- The engines listen on a random loopback address and port (`127.x.y.z`), so web pages cannot
  find them by scanning 127.0.0.1 or by DNS rebinding, and Studio checks that the engine
  process itself owns that address before it sends anything there. llama-server gets a random
  API key through its environment, never on its command line. It is never handed an image its
  own decoder cannot read (Studio re-encodes them), so it has no reason to run an `ffmpeg`
  from PATH, and its ffmpeg folder is set to an empty one besides. Engine `extraArgs` cannot
  rebind, re-key or open up an engine. Engines run in a Job object (Windows) or with a
  parent-death signal (Linux), so they die with Studio.
- Known gap: the pinned sd-server has no authentication. Web pages cannot reach it, but while
  the image engine is loaded (during renders, then until it is unloaded after
  `idleUnloadMinutes`), another account signed in to the same PC could find its address and
  submit, read or cancel image jobs on it. On a PC you share with other accounts, use a short
  `idleUnloadMinutes` or **Unload models** after use.
- Downloads use pinned HTTPS URLs (exact commits and releases) and are checked against
  their SHA-256 before use. Archives are unpacked with path checks (no absolute paths, no
  `..`). Nothing downloaded is executed except the engines, and the Visual C++
  redistributable: Studio fetches it over HTTPS only from Microsoft into a new private
  folder, and starts it only after you confirm and only when its Authenticode signature is
  valid and names Microsoft Corporation.
- Image inputs are limited to 40 MB and 64 megapixels. Chat images must be `data:` URLs,
  so llama-server is never asked to fetch anything, and Studio decodes and re-encodes each one
  before the engine sees it.
- Embedded JavaScript (the JAI library running in QuickJS) can only reach the local
  server's origin, and is stopped when it runs for 30 s without a pause, so a document crafted
  to be slow cannot hold a validate, render or encode.
- No telemetry, and nothing that identifies you in any request; downloads send only
  `User-Agent: jai-studio/<version>`.

## What gets downloaded

| Component | Source (pinned) | Size | Licence |
|---|---|---|---|
| Qwen3.8 27B, UD-IQ3_XXS | unsloth/Qwen3.8-27B-GGUF @4ca7207 | 10.9 GB | Apache-2.0 |
| Qwen3.8 27B vision projector, F16 | unsloth/Qwen3.8-27B-GGUF @4ca7207 | 928 MB | Apache-2.0 |
| FLUX.1 schnell, Q8_0 | leejet/FLUX.1-schnell-gguf @c7f665d | 12.8 GB | Apache-2.0 |
| FLUX autoencoder | second-state/FLUX.1-schnell-GGUF @8c45a2b | 335 MB | Apache-2.0 |
| CLIP-L text encoder | comfyanonymous/flux_text_encoders @6af2a98 | 246 MB | MIT |
| T5-XXL text encoder, Q8_0 | city96/t5-v1_1-xxl-encoder-gguf @005a6ea | 5.1 GB | Apache-2.0 |
| llama.cpp b11669 (CUDA 13.4, Vulkan or CPU build) | ggml-org/llama.cpp | 20 to 155 MB | MIT |
| stable-diffusion.cpp master-970-2f016e8 (CUDA 12, Vulkan or CPU build) | leejet/stable-diffusion.cpp | 18 to 338 MB | MIT |
| CUDA runtimes for the CUDA builds | the same releases | 424 MB and 563 MB | NVIDIA CUDA EULA (redistributable) |

Only the engine builds for your OS and backend are downloaded. [`catalog.json`](catalog.json)
holds the exact URLs, sizes and SHA-256 values. `jai models verify` re-checks the installed
files.

## Building

Run `npm run build` in `jai-tools` first: the Rust build embeds `dist/studio/host.mjs`
(the JAI library and encoder, bundled for QuickJS) and `dist/studio/ui/` (the webview UI).

| Target | Command |
|---|---|
| Windows `jai.exe` from Linux or WSL | `studio/scripts/build-windows.sh` (cargo-xwin, LLVM tools on `PATH`); writes `dist/studio/jai.exe`, and with cargo-about also `dist/studio/THIRD-PARTY-NOTICES.txt`, embedded in the exe |
| Linux, headless CLI and server | `cargo build --release -p jai --no-default-features` |
| Linux, with the GUI | `cargo build --release -p jai` (needs `libwebkit2gtk-4.1-dev` and `libayatana-appindicator3-dev`) |

A build that is given to anyone must carry the licence notices of what it is built from:
`studio/scripts/notices.sh <target> <file>` writes them (it needs cargo-about:
`cargo install cargo-about --locked --features cli`), and building with
`JAI_THIRD_PARTY_NOTICES=<file>` embeds them, so `jai licenses` prints them. CI does both and
ships the file next to the executables.

Run cargo in `studio/`. Tests:

```sh
npm test                                   # TypeScript unit tests and the spec conformance suite
cargo test --workspace --exclude jai       # core, engines, server, JS host (conformance inside QuickJS)
cargo test -p jai --no-default-features    # the app's CLI and helpers, and the executable
```

The Rust tests use fake engines and local mock servers; they need no GPU and no network.
Windows test binaries cross-built in WSL (`cargo xwin test --target x86_64-pc-windows-msvc
--workspace --no-run`) also run from WSL, each in its crate's folder, once they are told
where things are in Windows terms: `JAI_SPEC_DIR` (the spec repository), `JAI_TOOLS_DIR`
(jai-tools) and `JAI_TEST_EXE` (the cross-built debug `jai.exe`), passed through `WSLENV`.
`studio/scripts/e2e-windows.sh` runs the GPU end-to-end checks with the real `jai.exe`.

Layout: `crates/jai-core` (paths, config, token, catalog, downloader, LM Studio reuse, GPU
detection), `crates/jai-engines` (engine processes, launch commands, scheduler),
`crates/jai-server` (HTTP server, auth, discovery), `crates/jai-js` (the QuickJS host with
web APIs and image operations) and `app/` (the `jai` binary: CLI, Tauri GUI, tray, Windows
integration). The UI is plain TypeScript in `src/studio/ui/`; opened in a normal browser it
runs against an in-memory mock (`?installed=1` starts with the models installed).

## Licence

JAI Studio is MIT-licensed, like the rest of jai-tools. It is built from many open-source
Rust packages (Tauri, tokio, QuickJS through rquickjs, rustls and more), whose licences and
copyright notices `jai licenses` prints, the Server tab shows under "Open-source licences",
and `THIRD-PARTY-NOTICES.txt` next to each released executable contains. The models and engines it downloads are not part of it and keep their
own licences, listed above, shown in the Models tab and by `jai models status` and `jai
models download`.
