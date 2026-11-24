# CLI quickstart

Use the CLI for an interactive, streaming conversation from a terminal. It can
also configure llmshim and start the HTTP proxy.

> **Availability:** Rust: wrapped by the CLI · CLI: interactive workflow · HTTP: available through `llmshim proxy` · Clients: separate

## 1. Install the binary

On macOS with Homebrew:

```bash
brew install sanjay920/tap/llmshim
```

Or install from source. The `proxy` feature keeps the proxy command available:

```bash
cargo install llmshim --features proxy
```

Running `llmshim` with no subcommand prints the command overview.

## 2. Configure a provider

```bash
llmshim configure
```

You can use environment variables instead. See
[Configure providers](configure.md) for precedence and the four supported key
names.

## 3. Start chatting

```bash
llmshim chat
```

Choose a model at the prompt, then enter a message. The CLI streams each answer
as it arrives. It requests `reasoning_effort: "high"` on every turn; reasoning
returned by the provider is shown in dim gray before the answer.

The CLI keeps conversation history in memory for this process. Switch the next
turn to another provider without clearing that history:

```text
/model
```

Use `/clear` when you want a new conversation.

## Attach an image

Attach a local image before the next message:

```text
/image ./diagram.png
```

The CLI reads the file, converts it to a base64 data URI, and adds it to the
next user message. `/paste` attempts to attach an image from the system
clipboard on supported desktop environments.

## Common operational tasks

```bash
llmshim models               # models for configured providers
llmshim set proxy.port 8080  # update one config value
llmshim get proxy.port       # read one config value
llmshim list                 # show masked keys and proxy settings
llmshim proxy                # start the HTTP API
```

This page covers the normal workflow. The exhaustive command and interactive
slash-command list belongs in the [CLI reference](../reference/cli.md).

