# Images and vision

Images travel inside a message's `content` array. llmshim translates recognized
image blocks into the selected provider's native representation.

> **Availability:** Rust/proxy/clients: message content blocks · CLI: file or clipboard attachment

## Use a portable image block

The most convenient input is the OpenAI Chat Completions `image_url` block:

```json
{
  "role": "user",
  "content": [
    {"type": "text", "text": "Describe this image."},
    {
      "type": "image_url",
      "image_url": {
        "url": "data:image/png;base64,iVBORw0KGgo..."
      }
    }
  ]
}
```

A data URI carries both the media type and base64 bytes. It is the portable
choice when the same request may target OpenAI, Anthropic, Gemini, or xAI.

The current translators recognize these input block forms:

| Input form | Shape |
|---|---|
| OpenAI Chat Completions | `{"type":"image_url","image_url":{"url":"..."}}` |
| OpenAI Responses | `{"type":"input_image","image_url":"..."}` |
| Anthropic Messages | `{"type":"image","source":{...}}` |

When Gemini is the target, llmshim emits Gemini's native `inline_data` part for
base64 image bytes. A raw Gemini `inline_data` part is not currently recognized
as a portable input block, so use one of the forms above at the llmshim
boundary.

## Base64 versus remote URLs

Both data URIs and plain remote URLs are accepted inside `image_url` and
`input_image` blocks:

```json
{
  "type": "image_url",
  "image_url": {"url": "https://example.com/photo.jpg"}
}
```

OpenAI, xAI, and Anthropic receive a provider-native URL image. llmshim does
not download that URL itself.

> **Gemini limitation:** the current Gemini adapter cannot send a remote image
> URL as inline image data. It replaces the image block with a text part such
> as `[Image: https://example.com/photo.jpg]`. The model receives the URL as
> text, not the image. Use a base64 data URI when targeting Gemini.

## Send through each surface

For Rust, put the content array directly in the top-level `messages` value. For
the proxy and language clients, use the same content array in the compact
request's `messages` field. Image controls do not belong in `config` or
`provider_config`.

The core does not read local paths. Applications must read and encode local
files themselves before building the request.

The CLI provides that convenience:

```text
/image ./diagram.png
```

It reads the file, builds a base64 data URI, and attaches it to the next user
message. `/paste` attempts to attach an image from the clipboard on supported
desktop environments. You can also include a readable image path directly in
the text entered at the CLI prompt.

## Generate images from Rust

Image generation is a separate non-streaming operation:

```rust,no_run
# async fn example(router: &llmshim::router::Router) -> llmshim::error::Result<()> {
let response = llmshim::images(router, &serde_json::json!({
    "model": "openai/gpt-image-1",
    "prompt": "A watercolor of a lighthouse",
    "n": 1,
    "output_format": "png"
})).await?;
for image in response.images {
    // Choose the destination using image.media_type.
    std::fs::write("lighthouse.png", image.bytes).unwrap();
}
# Ok(()) }
```

`ShimClient::images(provider, model, request)` uses an already resolved provider.
The router entry point accepts aliases and named routes. OpenAI GPT Image models
use `/images/generations`; supported controls are `n`, `size`, `quality`,
`background`, `output_format`, `output_compression`, `moderation`, and `user`.
Results contain bytes, media type, and an optional revised prompt. URL-only,
empty, filtered, and malformed outputs fail; llmshim never downloads output URLs.

The Gemini adapter uses `models/<model>:generateContent` with a user text part
and `generationConfig.responseModalities: ["TEXT", "IMAGE"]`. Supported model
IDs are `gemini-2.5-flash-image`, `gemini-3.1-flash-lite-image`,
`gemini-3.1-flash-image`, and `gemini-3-pro-image`, as listed in Google's
[image generation guide](https://ai.google.dev/gemini-api/docs/image-generation).
Use `x-gemini.imageConfig` for native `aspectRatio` and `imageSize` controls;
model-specific restrictions are enforced by the provider. Omit `n` or set it to
1: one candidate may contain multiple image parts, and Gemini does not promise
an exact image count. Other counts fail locally. Retired Imagen models are not
supported.

Images come from non-thought `inlineData` parts across the returned candidates.
Their base64 `data` and `mimeType` become decoded bytes and media type. Draft
thought images are excluded; text-only or blocked output and invalid image
parts fail with status 502. A supplied non-STOP finish reason also fails.
Gemini's `usageMetadata` is preserved as native usage, including token-modality
details. No text-model price is substituted for image-output usage.

`usage.cost_usd` prefers a reported provider bill. Otherwise, pricing uses
`cost::for_target(provider, model)` and reports `cost_source: "catalog"`.
Complete, uncached text-input and image-output modality counters are required.
Mixed text/image output, image inputs, cached usage, missing or inconsistent
counters, and uncatalogued models have null cost and `cost_source: "unknown"`.
The catalog's single output rate cannot price mixed output safely. Generation
currently has no proxy/CLI endpoint, editing, streaming, or dispatch-policy
callback surface.
