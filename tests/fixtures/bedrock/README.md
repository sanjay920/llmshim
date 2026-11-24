# Bedrock protocol fixtures

The chat, Converse JSON and EventStream hex files are synthetic protocol recordings constructed
from the official request/event schemas below. They contain no real conversations or credentials.
Hex encodes complete wire frames including independent CRC32 values. The text/tool stream uses
two input fragments; the exception stream starts successfully before a modeled throttle error.
The fixtures are static: tests never generate their own expected frames or contact a service.

- https://docs.aws.amazon.com/bedrock/latest/APIReference/API_runtime_Converse.html
- https://docs.aws.amazon.com/bedrock/latest/APIReference/API_runtime_ConverseStream.html
- https://docs.aws.amazon.com/bedrock/latest/userguide/prompt-caching.html
- https://smithy.io/2.0/aws/amazon-eventstream.html

`signing/` contains unmodified request, context and signature fixtures from the AWS-published
`aws-c-auth` SigV4 test suite, retrieved 2026-10-15. SPDX-License-Identifier: Apache-2.0.
Source directory and license:

- https://github.com/awslabs/aws-c-auth/tree/main/tests/aws-signing-test-suite/v4
- https://github.com/awslabs/aws-c-auth/blob/main/LICENSE

Each context supplies the clock, region, service, synthetic credentials and payload-header
setting. Expected signatures are the published values, never calculated by the code under test.
