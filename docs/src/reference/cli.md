# CLI reference

Run `llmshim` with no subcommand, or use `llmshim help`, `--help`, or `-h`, to
print top-level help. Interactive chat starts only with `llmshim chat`.

> **Availability:** Core commands: default build · `proxy`: requires `--features proxy` · Docker commands: require Docker

## Commands

| Command | Arguments and flags | Purpose |
|---|---|---|
| `llmshim chat` | `--log <path>` | Start interactive, streaming chat |
| `llmshim proxy` | `--host <IP>`, `--port <PORT>` | Start the HTTP proxy |
| `llmshim gateway` | `--host <IP>`, `--port <PORT>` | Start the priority-queue gateway |
| `llmshim configure` | none | Prompt for four provider keys and proxy host/port |
| `llmshim login chatgpt` | `--status` | Sign in via device code, or inspect the local cache |
| `llmshim logout chatgpt` | none | Remove the selected local ChatGPT OAuth cache |
| `llmshim set` | `<key> <value>` | Write one config value |
| `llmshim get` | `<key>` | Read one config value; keys are masked |
| `llmshim list` | none | Show masked keys and proxy settings; alias: `ls` |
| `llmshim models` | `--all`, `--json`, `--refresh` | List registry models for configured providers |
| `llmshim path` | none | Print the config file path |
| `llmshim docker` | `<start\|stop\|status\|logs\|build>` | Manage the stock local proxy container |

Valid config keys for `set` and `get` are `openai`, `anthropic`, `gemini`,
`xai`, `proxy.host`, and `proxy.port`.

## Interactive chat

The model picker shares the curated catalog used by `llmshim models` and the
server's `/v1/models` endpoint. An exact historical model ID with recorded
metadata can still be selected explicitly, but is omitted from the picker.

`llmshim chat` opens a model picker. Pressing Enter without a selection chooses
`openai/gpt-6-astra` (the first advertised entry). Every answer streams, requests use
`reasoning_effort: "high"`, and reasoning text is rendered dimly before answer
text.

Chat output, provider errors, pasted-text echoes, and plain-text catalog listings
render terminal control characters as visible escapes. Newlines, tabs, and
ordinary Unicode remain readable. This display protection preserves the original
conversation content and JSON data; provider text cannot supply terminal commands
such as clipboard updates or cursor movement.

The chat process owns and resends its current history. Switching models changes
the next route without clearing that history.

| Interactive command | Action |
|---|---|
| `/model` | Open the model picker |
| `/model <number-or-query>` | Select by list number or first partial ID/label match |
| `/models` or `/model list` | Show the model list |
| `/clear` | Clear conversation history |
| `/history` | Show the number of messages in history |
| `/image <path>` | Attach an image to the next user turn |
| `/paste` | Attach an image from the clipboard |
| `/help` or `/h` | Show interactive help |
| `/quit`, `/exit`, or `/q` | Exit |

Existing image paths can also appear inline in a prompt. In an interactive
terminal, Ctrl-V pastes an image when the platform clipboard integration can
read one; otherwise it pastes text.

After `llmshim login chatgpt`, choose a ChatGPT entry in the picker or enter
one of the four supported IDs: `chatgpt/gpt-6-astra`, `chatgpt/gpt-5.6-sol`,
`chatgpt/gpt-5.6-terra`, or `chatgpt/gpt-5.6-luna`. `/model` uses the same list.

`--log <path>` appends JSONL request records. If it is absent, chat checks
`LLMSHIM_LOG`.

## Proxy

`llmshim proxy` loads file-backed keys, requires at least one configured
provider, and listens using `--host`/`--port`, then `LLMSHIM_HOST`/`LLMSHIM_PORT`,
then saved configuration, in that order. The host must be an IPv4 or IPv6 address.
`--port 0` asks the OS for an available port; the startup banner prints that port.
The gateway accepts the same options. A default-feature Cargo build prints an
error for these server commands; build with `--features proxy` or `--features gateway`.

Every subcommand accepts `--help` without starting a server, loading credentials,
or entering an interactive prompt. Unknown arguments, missing option values, and
invalid ports exit with status 2. Bind failures exit with a readable diagnostic
and status 1 instead of a panic.

See [HTTP API](../proxy/http-api.md) and
[Deploy the proxy safely](../proxy/deployment.md).

## Docker helper

| Command | Flags | Action |
|---|---|---|
| `llmshim docker build` | none | Build image `llmshim` from the current directory |
| `llmshim docker start` | `--port <port>` or `-p <port>` | Start container `llmshim-proxy`; host port defaults to configured proxy port |
| `llmshim docker stop` | none | Stop and remove the managed container |
| `llmshim docker status` | none | Inspect container state and port mapping |
| `llmshim docker logs` | `--follow` or `-f` | Follow logs; without the flag, show the last 50 lines |

`docker start` passes configured provider keys into the container and maps the
chosen host port to container port 3000. The helper manages only the fixed
image and container names above; it is not a deployment orchestrator.
