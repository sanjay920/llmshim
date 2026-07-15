# Choose a surface

Every llmshim surface reaches the same Rust translation engine. The choice is
where that engine runs and who manages its process.

> **Availability:** Rust: in-process · CLI: interactive · HTTP: separate proxy · Clients: through the proxy

| Choose | Best when | Where llmshim runs | Do you run a server? |
|---|---|---|---|
| [Rust crate](rust.md) | Your application is written in Rust | Inside your application | No |
| [CLI](cli.md) | A person wants to chat or operate llmshim from a terminal | In the `llmshim` process | No for chat; the CLI can also start the proxy |
| [HTTP proxy](proxy.md) | Any language needs a stable network boundary, or several applications share one engine | In a separate `llmshim proxy` process | Yes |
| [Language client](clients.md) | You want an idiomatic Python, TypeScript, Go, or Ruby API | Through a proxy | Python/TypeScript start one automatically; Go/Ruby do not |

## A quick decision

- **Building a Rust application?** Use the crate. It is the engine and has no
  local server hop.
- **Exploring models from a terminal?** Use `llmshim chat`. The CLI keeps the
  current conversation in memory and streams the answer.
- **Need HTTP or a shared deployment?** Run the proxy. It exposes llmshim's own
  compact API, not an OpenAI-compatible endpoint.
- **Using Python or TypeScript?** Start with the language client. Its bundled
  binary starts a local proxy on the first call. TypeScript can also connect to
  a proxy URL you provide; the current Python API always manages its own proxy.
- **Using Go or Ruby?** Start a proxy first, then connect with the pure HTTP
  client.

These are process choices, not different translation implementations. The CLI
and proxy wrap the crate, and all four language clients speak the proxy
contract. See [Two contracts, one engine](../concepts/contracts.md) for the data
shapes at each boundary.

Whichever path you choose, [configure at least one provider](configure.md)
before making a request.
