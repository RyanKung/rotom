<p align="center">
  <img src="logo.png" alt="rotom logo" width="144">
</p>

# rotom

[![CI](https://github.com/RyanKung/rotom/actions/workflows/ci.yml/badge.svg?branch=master)](https://github.com/RyanKung/rotom/actions/workflows/ci.yml)
[![Release](https://github.com/RyanKung/rotom/actions/workflows/release.yml/badge.svg?branch=master)](https://github.com/RyanKung/rotom/actions/workflows/release.yml)

Use your Codex, Grok, Kiro, or Vercel AI Gateway credentials from any tool that
speaks the OpenAI or Anthropic API.

rotom is a small local gateway. You log in once, run `rotom serve`, and point
Claude Code, the OpenAI/Anthropic SDKs, or any compatible client at the local
address.

## Demo

Claude Code running through Grok and GPT:

<p align="center">
  <img src="demos/claude-grok-4.3.gif" alt="Claude Code using grok-4.3 through rotom" width="720">
</p>
<p align="center">
  <img src="demos/claude-gpt-5.5.gif" alt="Claude Code using GPT through rotom" width="720">
</p>

## Quick Start

```bash
cargo install rotom
rotom login                                   # pick a provider, finish in browser
rotom serve --bind 127.0.0.1:14550 --api-key local-secret
```

Point Claude Code (or any Anthropic-compatible client) at the gateway:

```bash
export ANTHROPIC_BASE_URL=http://127.0.0.1:14550
export ANTHROPIC_AUTH_TOKEN=local-secret
export ANTHROPIC_MODEL="gpt-6-astra"

claude
```

That's it. Quick sanity check:

```bash
claude -p "Reply with the single word OK"
```

> Point `ANTHROPIC_BASE_URL` at the server root, **not** `/v1` — clients append
> `/v1/messages` themselves. `ANTHROPIC_AUTH_TOKEN` is your local `--api-key`,
> not an upstream token. Use a model that `/v1/models` lists (e.g. `gpt-6-astra`).

## Logging In

`rotom login` lists the providers and runs the chosen flow. Skip the prompt with
a flag:

| Provider          | Command                         |
| ----------------- | ------------------------------- |
| OpenAI/Codex      | `rotom login --provider openai` |
| Grok (xAI)        | `rotom login --provider grok`   |
| Kiro              | `rotom login --kiro`            |
| Vercel AI Gateway | `rotom login --provider vercel` |

- **Codex / Grok**: browser login, then paste the redirected
  `http://localhost:.../auth/callback?...` URL back into the terminal.
- **Kiro**: browser login via Kiro's portal callback (Google/GitHub).
- **Vercel**: paste an AI Gateway API key, or set `AI_GATEWAY_API_KEY` before
  running the login command.

Credentials are stored per provider in `~/.rotom/auth.json` (override with
`ROTOM_AUTH_FILE` or `ROTOM_HOME`). Logging in to one provider never replaces
another, and `serve` exposes every logged-in provider at once. If a daemon is
already running, restart it to pick up a new provider:

```bash
rotom daemon restart
```

## Models

```bash
rotom models                      # everything rotom exposes
rotom models --provider grok      # one provider
```

Common ids include `gpt-6-astra`, `gpt-5.6-sol`, `gpt-5.6-terra`,
`gpt-5.6-luna`, `grok-4.6`, Kiro's `claude-*` family, and Vercel AI Gateway
ids like `openai/gpt-6-astra`, `anthropic/claude-sonnet-5`, or
`typesafe-ai/jev`.

Unknown Anthropic ids like `claude-sonnet-*` are rewritten to a fallback
(default `gpt-5.5`). Override with `--model-fallback` or `ROTOM_MODEL_FALLBACK`.

## Use With SDKs

OpenAI-compatible:

```bash
curl http://127.0.0.1:14550/v1/chat/completions \
  -H 'content-type: application/json' \
  -H 'authorization: Bearer local-secret' \
  -d '{"model": "gpt-6-astra", "messages": [{"role": "user", "content": "hello"}]}'
```

Anthropic-compatible:

```bash
curl http://127.0.0.1:14550/v1/messages \
  -H 'content-type: application/json' \
  -H 'x-api-key: local-secret' \
  -H 'anthropic-version: 2023-06-01' \
  -d '{"model": "gpt-6-astra", "max_tokens": 1024, "messages": [{"role": "user", "content": "hello"}]}'
```

Vercel evaluation models:

```bash
curl http://127.0.0.1:14550/v1/evaluations \
  -H 'content-type: application/json' \
  -H 'authorization: Bearer local-secret' \
  -d '{
    "model": "typesafe-ai/jev",
    "state": "The build failed with exit code 1.",
    "questions": {
      "passed": {
        "type": "boolean",
        "instructions": "Did the build succeed?",
        "criteria": {
          "true": "exit code 0",
          "false": "any non-zero exit code"
        }
      }
    }
  }'
```

Evaluation models return typed answers and probabilities. They are not text
generation models and should not be sent to `/v1/responses`.

Grok-native text to speech:

```bash
curl http://127.0.0.1:14550/v1/tts \
  -H 'content-type: application/json' \
  -H 'authorization: Bearer local-secret' \
  -d '{"text":"你好，欢迎使用 rotom。","voice_id":"eve","language":"zh"}' \
  --output speech.mp3
```

The TTS route keeps xAI's native request shape, including `output_format`,
`speed`, `optimize_streaming_latency`, `text_normalization`, `replace`, and
`with_timestamps`. When `with_timestamps` is enabled the upstream JSON response
is returned unchanged; otherwise the response body contains the generated
audio bytes. List built-in voices with `GET /v1/tts/voices`.

For bidirectional streaming, upgrade `GET /v1/tts` to a WebSocket. Query
parameters and events use xAI's native shape:

```javascript
// npm install ws
import fs from "node:fs";
import WebSocket from "ws";

const ws = new WebSocket(
  "ws://127.0.0.1:14550/v1/tts?language=zh&voice=eve&codec=mp3",
  { headers: { Authorization: "Bearer local-secret" } },
);
const audio = [];

ws.on("open", () => {
  ws.send(JSON.stringify({ type: "text.delta", delta: "你好，欢迎使用 rotom。" }));
  ws.send(JSON.stringify({ type: "text.done" }));
});
ws.on("message", (data) => {
  const event = JSON.parse(data.toString());
  if (event.type === "audio.delta") audio.push(Buffer.from(event.delta, "base64"));
  if (event.type === "audio.done") {
    fs.writeFileSync("speech-stream.mp3", Buffer.concat(audio));
    ws.close();
  }
});
```

The gateway passes through `text.delta`, `text.done`, `text.clear`, and
`session.update`, and returns the native `audio.delta`, `audio.done`,
`audio.clear`, `session.updated`, and `error` events. The connection remains
open across multiple utterances. If local `--api-key` protection is enabled,
the WebSocket client must send that local key in the handshake; rotom replaces
it with the saved Grok credential upstream. The upstream WebSocket upgrade uses
the same HTTP client as REST and honors standard `HTTP_PROXY`, `HTTPS_PROXY`,
`ALL_PROXY`, and `NO_PROXY` environment settings.

## Running As a Service

Run a background daemon instead of `rotom serve`:

```bash
rotom daemon install --bind 127.0.0.1:14550 --api-key local-secret
rotom daemon start
rotom daemon status      # also: restart / stop / uninstall
```

macOS uses a LaunchAgent; Linux uses a systemd user unit. On Windows, use WSL.
The `--api-key` is stored in `~/.rotom/config.json` rather than embedded in the
service definition.

## Other Commands

```bash
rotom status                      # version, token expiry, auth, endpoints
rotom refresh                     # refresh saved provider credentials
rotom config                      # interactive config (~/.rotom/config.json)
rotom update                      # update to the latest release
```

`--bind` accepts a comma-separated list and CIDR selectors, e.g.
`--bind 127.0.0.1:14550,192.168.1.0/24:14550`. Token refresh and status are also
available over HTTP at `/v1/auth/refresh` and `/v1/status`.

## Supported Endpoints

OpenAI:

- `GET /v1/models`, `POST /v1/chat/completions`
- `POST /v1/responses` (+ retrieve / delete / cancel / input_items / compact /
  input_tokens)
- `POST /v1/images/generations`

Vercel Evaluation:

- `POST /v1/evaluations` for Gateway evaluation models such as
  `typesafe-ai/jev`
- Request shape: `model`, `state`, and typed `questions`

Grok Voice:

- `POST /v1/tts`, `GET /v1/tts` (WebSocket upgrade), `GET /v1/tts/voices`
- Native xAI request/event shapes and audio/JSON/WebSocket passthrough

Anthropic:

- `GET /v1/models`, `POST /v1/messages`, `POST /v1/messages/count_tokens`
- Message batches: `POST/GET /v1/messages/batches` (+ get / cancel / delete /
  results)
- `x-api-key` or `authorization: Bearer ...` auth, SSE streaming for text and
  tool use

Image generation is exposed both as `POST /v1/images/generations` and as the
Responses hosted tool `{"type":"image_generation"}`; generated images are
returned as base64.

## Provider Notes

These upstreams are not all plain model APIs, so rotom adapts requests and
quietly drops controls the upstream cannot honor.

- **Codex**: accepts `temperature` / `max_tokens` and similar fields but does
  not forward them (Codex rejects them upstream). `/v1/responses` keeps a local
  replay behavior for existing clients.
- **Grok**: uses xAI's native Responses API and forwards supported controls
  (`temperature`, `top_p`, `max_output_tokens`, `stop`, ...). Text to speech
  uses xAI's native REST and bidirectional WebSocket `/v1/tts` shapes and
  requires saved Grok credentials.
- **Kiro**: mapped to Kiro's `GenerateAssistantResponse` schema (text, tools,
  tool results, history, inline base64 images/documents). Remote image/document
  URLs are rejected, not fetched.
- **Vercel**: uses Vercel AI Gateway's OpenAI-compatible `/v1/responses`
  endpoint with `provider/model` ids for text models, and `/v1/evaluations` for
  Gateway evaluation models like `typesafe-ai/jev`. When serving, rotom fetches
  the live Gateway `/v1/models` list with the saved Vercel key and falls back to
  a small built-in list if Gateway is unavailable. Prefix a model with
  `vercel/` to force Vercel routing when another rotom provider also recognizes
  that model family; rotom strips the prefix before forwarding upstream.
## Disclaimer

rotom is an unofficial compatibility tool. It is not affiliated with, endorsed
by, or supported by OpenAI, Anthropic, xAI, Kiro, or Vercel.

You are responsible for complying with the terms and account restrictions of
your upstream provider. In particular, do not assume personal OAuth access can
be shared, resold, or exposed as a multi-user hosted service. The LGPLv3 license
does not change those upstream restrictions.

## License

Copyright (c) 2026 rotom contributors.

Licensed under the GNU Lesser General Public License v3.0 only. See [LICENSE](LICENSE).
