# Octa cache HTTP protocol v1

This protocol exposes two immutable namespaces: an action cache maps a
namespace and action digest to replay metadata, while the content-addressed
store (CAS) holds encoded output bundles. Octa clients always publish a blob
before the action that references it. Services must never overwrite either
object.

Every request and response carries `X-Octa-Cache-Protocol: 1`. Requests use
`Authorization: Bearer <token>`; the token is loaded from a bounded private file
and is never placed in Octafile, runner JSON, command-line arguments, events, or
logs. Production endpoints must use HTTPS and must not redirect requests.

## Routes

The base URL may contain a path prefix. Route path values use lowercase digest
algorithm and hexadecimal names.

| Operation | Method and path | Success |
| --- | --- | --- |
| GetActionResult | `GET v1/actions/{algorithm}/{hash}/{size}?namespace=...` | `200` JSON or `404` miss |
| FindMissingBlobs | `POST v1/blobs/missing` | `200` JSON |
| ReadBlob | `GET v1/blobs/{algorithm}/{hash}/{expanded}/{encoding}/{encoded}/{entries}` | `200` encoded byte stream |
| WriteBlobIfAbsent | `PUT` to the ReadBlob path with `If-None-Match: *` | `201` created, `204` identical, `409` conflict |
| WriteActionIfAbsent | `PUT` to the GetActionResult path with `If-None-Match: *` | `201` created, `204` identical, `409` conflict |

JSON uses `application/vnd.octa.cache.v1+json`; blob bodies use
`application/vnd.octa.cache.blob`. `FindMissingBlobsRequestV1`,
`FindMissingBlobsResponseV1`, and `WriteActionRequestV1` are defined by
`octa-cache-protocol`. The crate also ships
`remote-cache-v1.schema.json` plus golden request and response documents.
Servers must reject unknown fields and enforce the same list, string,
result-metadata, and blob bounds as the shared types. Write routes require
`If-None-Match: *`; an action publication must bind the same namespace and
action digest in its URL and JSON envelope.

## Integrity and concurrency

Blob identity is the BLAKE3 digest and size of the canonical uncompressed
bundle. Encoding, encoded length, expanded length, and entry count identify the
exact transferable representation. The client bounds the encoded response,
then local publication and restore verify the expanded digest and filesystem
contract. Neither HTTP status, ETag, nor TLS replaces content verification.

Concurrent equal writes are idempotent. A service returns `409` when an action
key is already bound to different metadata; Octa reports this as a
nondeterministic result. An action must not become readable until all referenced
blobs are readable. A generic HTTP `412` proves only that the precondition did
not hold; it does not prove that the resident value is identical. Version one
therefore treats `412` as a protocol failure rather than a cache hit.

## Failure behavior

Clients retry connection and response-body failures, timeouts, `408`, `425`,
`429`, and selected `5xx` responses with bounded full-jitter exponential delay.
A numeric `Retry-After` value is honored up to the configured maximum. One
per-operation deadline covers waiting for a transfer slot, request staging,
every retry delay, and the complete response body. Repeated failures open a
run-scoped circuit breaker; a new job or CLI invocation gets fresh state.

Operator values are bounded before any request starts: the whole-operation
deadline is at most 10 minutes, concurrency at most 256 transfers, retries at
most 10, retry delay at most 60 seconds, and circuit-open time at most one hour.
Connection-pool idle time and TCP keepalive are named process-local tuning
constants rather than job-controlled policy.

Remote misses and failures fall back to normal task execution. Remote write
failure leaves the verified local result usable. Cancellation is never changed
into a miss. Metadata lookup never downloads a bundle: the executor first
checks job-specific limits, then the layered store copies and verifies the blob
in local CAS during restore. Only a successfully restored result is published
as a local action, so later reads are local and cannot observe a partial
generation.

Downloads are staged privately before their reader is exposed. This deliberate
extra disk pass allows an interrupted response body to be retried without ever
feeding a partial stream to the bundle decoder. Uploads are staged for the same
reason: each retry needs a fresh stream without assuming the caller's reader is
seekable.

## Credential files

The client rejects empty, oversized, non-regular, replaced, or symbolic-link
token files. On Unix the token must be owned by the effective process user and
must grant no group or other permissions. On Windows, ownership and ACL
provisioning remain the responsibility of the service installer; the client
still verifies the regular-file identity before and after opening it. Token
contents are redacted from debug output and never enter URLs or logs.
Private deployments may provide one bounded PEM CA file; disabling TLS
verification is not supported.
