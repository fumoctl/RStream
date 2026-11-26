# `rustmedia-media-proxy`

Origin-isolated HTTP(S) media source prober for RStream.

- Validates candidate stream URLs and `behaviorHints.proxyHeaders.request`.
- Sends a bounded 1-byte `Range: bytes=0-0` probe with manual redirect chasing (up to 8 hops).
- Forwards configured headers **only** to the initial origin (`scheme`, `host`, effective `port`), stripping them on cross-origin redirects.
- Rejects unspecified (`0.0.0.0`, `::`) and loopback (`127.0.0.0/8`, `::1`, `::ffff:127.0.0.0/104`, `localhost`) targets whenever `!allow_http` or when redirecting from a non-loopback origin (preventing SSRF against local services).
