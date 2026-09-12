# octa-cache-protocol

`octa-cache-protocol` defines the stable, versioned identities exchanged by
Octa's local cache and the future OctaCity cache service.

The crate intentionally contains no filesystem, executor, Tokio, HTTP, or
compression implementation. `ActionDescriptorV1::digest` uses a tagged,
length-prefixed binary encoding with the `octa.action.v1` domain. JSON is only
the wire and diagnostic representation and is never hashed as an action key.

Compatibility rules:

- adding or changing a key-relevant field requires a new action key format;
- changing result JSON incompatibly requires a new result version and schema;
- digests retain their algorithm tag and source byte size;
- relative paths always use `/` and reject host-specific or escaping forms;
- plugin order is canonicalized, while command argument order is semantic.

`ActionResultV1.output_bundle` is omitted for a cacheable action with no
filesystem outputs. Such a result may still replay bounded stdout and public
structured outputs, but it cannot carry artifact or report registrations because
there is no restored filesystem bundle containing their paths.

Published schemas live in `schema/`. Golden documents in `fixtures/` are
validated by the crate tests and are suitable for implementations on Linux,
macOS, and Windows.

JSON Schema describes the portable wire shape and algorithm roles. Consumers
must still run the semantic `validate` method after deserialization: aggregate
metadata byte limits, duplicate plugin names, and relationships between blob
sizes cannot all be expressed by ordinary JSON Schema. The custom
`x-maxUtf8Bytes` annotations preserve byte-based limits where JSON Schema's
standard `maxLength` counts Unicode code points.
