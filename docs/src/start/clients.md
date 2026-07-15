# Language-client quickstarts

All four language clients speak the proxy's compact HTTP contract. The key
difference is whether the package starts that proxy for you.

> **Availability:** Python/TypeScript: bundled proxy, auto-start by default · Go/Ruby: pure HTTP, running proxy required

| Client | Install | Default process behavior |
|---|---|---|
| Python | `pip install llmshim` | Starts the bundled proxy on the first call |
| TypeScript | `npm install llmshim` | Starts the bundled proxy on the first call |
| Go | `go get github.com/sanjay920/llmshim/clients/go` | Connects to `http://localhost:3000` |
| Ruby | `gem install llmshim` | Connects to `http://localhost:3000` |

The auto-started Python and TypeScript proxies are reused within the current
process and stopped when it exits. TypeScript can instead connect to a proxy
you already run by setting `baseUrl`; the current Python public API always uses
its managed local proxy. Go and Ruby never spawn the Rust binary; start
[`llmshim proxy`](proxy.md) before using their default clients.

Provider keys belong to the proxy process, not to the HTTP client. Configure
them through environment variables or `llmshim configure`.

## Python

```bash
pip install llmshim
```

```python
import llmshim

response = llmshim.chat("claude-sonnet-4-6", "What is Rust?")
print(response["message"]["content"])
```

The first call starts the bundled proxy automatically.

[Python README](https://github.com/sanjay920/llmshim/blob/main/clients/python/README.md)
· [PyPI package](https://pypi.org/project/llmshim/)

## TypeScript / JavaScript

```bash
npm install llmshim
```

```typescript
import { Client } from "llmshim";

const client = new Client();
const response = await client.chat({
  model: "anthropic/claude-sonnet-4-6",
  messages: [{ role: "user", content: "What is Rust?" }],
});

console.log(response.message.content);
```

Constructing the client is synchronous. Its first request starts the bundled
proxy. Pass `baseUrl` to `new Client(...)` to use an existing server instead.

[TypeScript README](https://github.com/sanjay920/llmshim/blob/main/clients/typescript/README.md)
· [npm package](https://www.npmjs.com/package/llmshim)

## Go

```bash
go get github.com/sanjay920/llmshim/clients/go
```

```go
package main

import (
	"context"
	"fmt"
	"log"

	llmshim "github.com/sanjay920/llmshim/clients/go"
)

func main() {
	client := llmshim.New()
	response, err := client.Chat(context.Background(), llmshim.ChatRequest{
		Model: "anthropic/claude-sonnet-4-6",
		Messages: []llmshim.Message{
			{Role: "user", Content: "What is Rust?"},
		},
	})
	if err != nil {
		log.Fatal(err)
	}

	fmt.Println(response.Message.Content)
}
```

`llmshim.New()` targets `http://localhost:3000`; it does not start the proxy.

[Go README](https://github.com/sanjay920/llmshim/blob/main/clients/go/README.md)
· [Go module](https://pkg.go.dev/github.com/sanjay920/llmshim/clients/go)

## Ruby

```bash
gem install llmshim
```

```ruby
require "llmshim"

response = Llmshim.chat(
  model: "anthropic/claude-sonnet-4-6",
  messages: "What is Rust?"
)

puts response.message.content
```

The module-level client targets `LLMSHIM_BASE_URL` when set, otherwise
`http://localhost:3000`. It does not start the proxy.

[Ruby README](https://github.com/sanjay920/llmshim/blob/main/clients/ruby/README.md)
· [RubyGems package](https://rubygems.org/gems/llmshim)

These snippets cover one non-streaming request. Use the linked client README as
the canonical reference for streaming, errors, configuration, and native types
rather than relying on duplicated method lists here.
