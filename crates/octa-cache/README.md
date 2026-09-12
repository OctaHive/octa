# octa-cache

Filesystem engine for Octa task-result caching. It computes portable input
tree identities and creates or extracts deterministic output bundles. Local
and remote stores use the same protocol types from `octa-cache-protocol`.

The crate owns filesystem safety and bounded streaming. It intentionally does
not depend on the executor, CLI, runner, HTTP, or OctaCity.

It also contains the local implementation of the transport-independent
`CacheStore` boundary. Blob and action writes are immutable, synchronized, and
atomically published; concurrent Octa processes coordinate only around the
objects they touch. Blob bytes are verified before publication and malformed
local objects are quarantined instead of becoming hits.

`RestoreManager` compares live outputs through the canonical bundle encoder. An
equal tree is left untouched. A changed tree is replaced under prefix-aware
output locks. An immutable intent journal makes abandoned same-filesystem
staging discoverable; durable prepared and committed markers tell startup
recovery whether to discard staging, roll back the old generation, or retain
the complete new generation.

Local retention uses configurable maximum/high/low watermarks and a bounded
maintenance traversal. Collection expires least-recently-used action records,
then removes only bundles no longer reachable from retained actions. Access
markers are sampled so ordinary hits do not cause a metadata write every time.
