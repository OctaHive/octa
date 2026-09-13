# octa-cache-http

HTTP client adapter for Octa's versioned remote action cache and content-addressed
blob store. The crate owns authentication, connection reuse, streaming,
deadlines, retries, and a per-client circuit breaker; cache semantics remain in
`octa-cache`.

The public surface is deliberately limited to validated configuration and the
`CacheStore` adapter. Route mechanics, retry classification, and circuit state
are private implementation details rather than additional provider interfaces.
