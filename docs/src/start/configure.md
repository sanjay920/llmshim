# Configure providers

llmshim needs a key only for the provider that handles a request. Configure one
provider to begin; add others when you want to switch models.

> **Availability:** Rust: environment, optional config load · CLI/proxy: environment + config file · Clients: the proxy owns provider keys

## Option 1: environment variables

Set one or more variables in the environment of the process that runs llmshim:

```bash
export OPENAI_API_KEY=sk-...
export ANTHROPIC_API_KEY=sk-ant-...
export GEMINI_API_KEY=AIza...
export XAI_API_KEY=xai-...
```

The Rust crate reads these variables when `Router::from_env()` is constructed.
The CLI and proxy read them at startup.

## Option 2: `~/.llmshim/config.toml`

Use the interactive command to create or update the shared config file:

```bash
llmshim configure
```

It prompts for all four provider keys, the proxy host, and the proxy port. Press
Enter to keep an existing value. The resulting file is used by CLI chat and the
proxy.

You can also manage individual values:

```bash
llmshim set anthropic sk-ant-...
llmshim get anthropic
llmshim list
```

Valid key names are `openai`, `anthropic`, `gemini`, `xai`, `proxy.host`, and
`proxy.port`. `get` and `list` mask stored API keys. Be aware that a value passed
to `set` may remain in your shell history; the interactive command avoids that
shell-history exposure.

## Precedence: environment wins

The CLI and proxy call `llmshim::env::load_all()`. It reads
`~/.llmshim/config.toml`, but fills only variables that are not already set:

```text
environment variable  >  config file  >  not configured
```

This makes a shell, container, or deployment secret override the local file
without editing it.

`Router::from_env()` does not load the file on its own. A Rust application that
wants the same behavior must call `llmshim::env::load_all()` first. See
[Environment variables versus `config.toml`](../concepts/routing.md#environment-variables-versus-configtoml).

## Check what is available

```bash
llmshim models
```

The command lists registry entries only for providers whose keys are available
after applying the precedence above. Runtime discovery is the canonical way to
see the current curated model list.

Go and Ruby clients never receive provider keys directly; they connect to a
proxy that owns them. Python and TypeScript auto-started proxies inherit the
current environment and use the same config-file loading behavior.
