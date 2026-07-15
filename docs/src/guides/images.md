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
