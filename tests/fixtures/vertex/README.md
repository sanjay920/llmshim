# Vertex offline fixtures

These sanitized wire examples are recorded from the public protocol descriptions below, with
synthetic text, project names, identifiers and token counts. They are not cloud-call captures.
The SSE examples retain the documented event ordering; tool arguments are split deliberately.
No account, bearer token, real service-account key or cloud call is involved.

- Gemini request/response, function calls, thoughts, cache and usage:
  https://cloud.google.com/vertex-ai/generative-ai/docs/model-reference/inference
- Response and usage fields:
  https://cloud.google.com/vertex-ai/generative-ai/docs/reference/rest/v1/GenerateContentResponse
- Google error envelopes:
  https://cloud.google.com/vertex-ai/generative-ai/docs/model-reference/api-errors
- Anthropic Vertex path, version and model removal:
  https://github.com/anthropics/anthropic-sdk-python/blob/main/src/anthropic/lib/vertex/_client.py
- Messages SSE framing, cumulative usage and exceptions:
  https://platform.claude.com/docs/en/api/streaming
- Service-account JWT exchange:
  https://developers.google.com/identity/protocols/oauth2/service-account

Protocol facts and synthetic examples are supplied under this repository's MIT OR Apache-2.0
license. No provider SDK source is copied. Tests construct binary/token envelopes in memory.

`synthetic-key.pem` is a freshly generated, unregistered RSA test key. Its matching public key
is stored in base64 DER PKCS#1 format; tests verify the JWT signature independently with it.
Both belong only to these offline fixtures and have never identified any cloud account.
