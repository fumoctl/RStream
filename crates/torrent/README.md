# Isolated torrent adapter

`stremio_torrent::TorrentEngine` is `Send + Sync`. `new(PathBuf)` creates no runtime or network tasks. Call its async methods from a **multithread Tokio runtime**.

## Contract

- `prepare(Value) -> anyhow::Result<Value>` accepts `{ "infoHash": "40 hexadecimal characters", "fileIdx": 0, "sources": ["tracker:https://...", "tracker:udp://..."] }`.
- `download_resumable(job_id, stream, dest_path, episode_hint, on_progress, stop_token, keep_partial_on_stop) -> anyhow::Result<u64>` resolves metadata under the active `VpnPolicy`, downloads the selected file (`fileIdx`, `episode_hint` video match, or largest video file), preserves verified partial pieces across pauses/network errors/VPN drops, and atomically places the completed file at `dest_path`.
- `VpnPolicy { killswitch, bind_interface }` and `list_interfaces()` enforce interface binding and VPN liveness across Linux (`SO_BINDTODEVICE`), macOS (`IP_BOUND_IF`/`IPV6_BOUND_IF`), Windows (`IP_UNICAST_IF`/`IPV6_UNICAST_IF` + per-lease CSPRNG-authenticated RFC 1929 loopback `socks5h://` + bound UDP tracker harvester), and Android (`android_setsocknetwork` + immediate `NetworkCallback` drop cancellation + per-lease CSPRNG-authenticated RFC 1929 loopback `socks5h://` + bound UDP tracker harvester), with a 250ms watcher that cancels active leases and stops the session if the VPN/interface drops.
- No UPnP port forwarding, no persistent DHT.
