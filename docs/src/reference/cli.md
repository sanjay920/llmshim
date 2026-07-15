# CLI reference

Run `llmshim` with no subcommand, or use `llmshim help`, `--help`, or `-h`, to
print top-level help. Interactive chat starts only with `llmshim chat`.

> **Availability:** Core commands: default build · `proxy`: requires `--features proxy` · Docker commands: require Docker

## Commands

| Command | Arguments and flags | Purpose |
|---|---|---|
| `llmshim chat` | `--log <path>` | Start interactive, streaming chat |
| `llmshim proxy` | none | Start the HTTP proxy |
| `llmshim configure` | none | Prompt for four provider keys and proxy host/port |
| `llmshim set` | `<key> <value>` | Write one config value |
| `llmshim get` | `<key>` | Read one config value; keys are masked |
| `llmshim list` | none | Show masked keys and proxy settings; alias: `ls` |
| `llmshim models` | none | List registry models for configured providers |
| `llmshim path` | none | Print the config file path |
| `llmshim docker` | `<start\|stop\|status\|logs\|build>` | Manage the stock local proxy container |

Valid config keys for `set` and `get` are `openai`, `anthropic`, `gemini`,
`xai`, `proxy.host`, and `proxy.port`.

## Interactive chat

`llmshim chat` opens a model picker. Pressing Enter without a selection chooses
`anthropic/claude-sonnet-4-6`. Every answer streams, requests use
`reasoning_effort: "high"`, and reasoning text is rendered dimly before answer
text.

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

`--log <path>` appends JSONL request records. If it is absent, chat checks
`LLMSHIM_LOG`.

## Proxy

`llmshim proxy` loads file-backed keys, requires at least one configured
provider, and listens using `LLMSHIM_HOST`/`LLMSHIM_PORT` or the config file.
It accepts no command-line flags. A default-feature Cargo build prints an error;
install or build with `--features proxy`.

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
