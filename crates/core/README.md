# RustMedia core

`rustmedia_core::Core::open(data_dir)` performs atomic SQLite migrations and installs built-in Cinemeta, Kitsu, and OpenSubtitles v3 descriptors without making network requests. `Core` is cloneable, `Send + Sync`; `execute(Value)` is async and must run on a Tokio runtime. It returns `anyhow::Result<Value>`; transport errors deliberately omit configured URLs. No SQLite guard crosses an await.

## JSON protocol

Requests are objects with `op` and the fields below. Unknown response/stream fields are retained unless explicitly noted. Types and IDs are raw Stremio strings, not inferred from names.

| Operation | Request fields | Response |
|---|---|---|
| `addons` | — | `{addons:[manifest + manifest_url]}` |
| `install_addon` | `url`, optional `allow_http` | Manifest descriptor directly |
| `remove_addon` | `url` | `{removed:bool}` |
| `refresh_addons` | — | `{addons:[...],results:[{addon,error?}]}` |
| `catalog` | `addon_url,type,id,extra?:object,force?:bool` | `{metas:[],cached:bool,...}` |
| `meta` | `addon_url,type,id,force?:bool` | `{meta:object,cached:bool,...}` |
| `streams_targets` | `type,id,addon_url?` | `{targets:[{addon_name,addon_url}]}` |
| `streams` | `type,id,addon_url?,force?:bool` | `{groups:[{addon_url,addon_name,streams:[raw objects],error?}]}` |
| `subtitles` | `type,id,extra?:object,addon_url?` | `{subtitles:[raw objects],errors:[]}` |
| `subtitle_file` | `url,allow_http?:bool` | `{url,format,cached:bool,cache_path?:string}` |
| `search` | `query,extra?:object` | `{groups:[{addon_name,addon_url,catalog_id,type,metas,cached?,stale?,error?}]}` |
| `library` | — | `{items:[metadata + state]}` for saved/history/feedback rows |
| `item` | `type,id,addon_url` | `{item:(metadata + state)\|null}` |
| `history` | `limit?` | `{items:[metadata + state + seen]}` |
| `watched_videos` | `type,id` | `{video_ids:[string]}` |
| `set_library` | `type,id,addon_url,saved,meta?` | `{item:metadata + state}` |
| `progress` | `type,id,addon_url,position_ms,duration_ms,video_id?,watched_ms?,watched_add_ms?,meta?` | `{item:metadata + state}` |
| `mark_watched` | `type,id,addon_url,watched,meta?` | `{item:metadata + state}` |
| `mark_videos` | `type,id,addon_url,watched,video_ids,meta?` | `{item:metadata + state}` |
| `feedback` | `type,id,addon_url,value` (-1, 0 or 1), `meta?` | `{item:metadata + state}` |
| `forget` | `type,id,addon_url` | `{forgotten:key}` |
| `sync_changes` | `since?` | `{media:[...],playback:[...],now:int}` |
| `sync_apply` | `media:[],playback:[]` | `{applied:int}` |

## Addon transport and storage

- HTTPS by default; `stremio://` becomes HTTPS. HTTP requires loopback or explicit `allow_http:true` during install. Redirects from non-loopback origins to loopback (`127.0.0.0/8`, `::1`, `localhost`) or unspecified (`0.0.0.0`, `::`) addresses are rejected.
- At most eight HTTP requests per `Core` run concurrently. Connect timeout is 5s; request timeout is 12s; redirects are capped at three; responses at 8 MiB.
- Catalog default TTL is 15 minutes; metadata default is 6 hours. `Cache-Control` `no-store`, `private`, `no-cache`, `max-age` (24-hour cap), and `Age` are enforced conservatively.
- Resource cache is bounded to 500 entries / 64 MiB of raw bodies. Streams are cached only in volatile memory (60s default, 180s cap, 64 entries / 16 MiB) and never persisted to SQLite.
- Subtitle files (`.ass`, `.ssa`, `.vtt`, `.srt`) are validated as non-empty, non-HTML text (UTF-8, UTF-16, or Latin-1) and cached atomically under `subtitles/` within a 64 MiB / 30-day budget.

## Verification

```sh
cargo test -p rustmedia-core
cargo clippy -p rustmedia-core --all-targets -- -D warnings
```
