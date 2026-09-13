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

Input identities remain portable BLAKE3 trees. An internal local-CAS memo can
reuse those verified content digests within one operating-system boot and one
mounted filesystem instance after a metadata walk proves that every selected
entry still has the same stable platform identity and change state. The memo is
enabled only for vetted local filesystems. Unsupported or remote filesystems, a
stale record, or corruption fall back to reading the content.

`RestoreManager` compares live outputs through the canonical bundle encoder. An
equal tree is left untouched. A changed tree is replaced under prefix-aware
output locks. An immutable intent journal makes abandoned same-filesystem
staging discoverable; durable prepared and committed markers tell startup
recovery whether to discard staging, roll back the old generation, or retain
the complete new generation.

Restoration provides atomic generations across process failure: another Octa
process observes either the old output or a complete verified replacement.
Restored file data is not synchronously flushed one file at a time for immediate
power-loss durability. After a machine restart Octa validates the canonical
output tree before accepting a materialized hit and repairs an incomplete tree
from the durable CAS. This keeps large small-file restores fast without treating
filesystem metadata as cached content identity.

Local retention uses configurable maximum/high/low watermarks and a bounded
maintenance traversal. Collection expires least-recently-used action records,
then removes only bundles no longer reachable from retained actions. Access
markers are sampled so ordinary hits do not cause a metadata write every time.
