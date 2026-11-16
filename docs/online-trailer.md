# Online trailer fallback (LAN trailer adapter)

A movie or show page whose server has no playable trailer gets one from a self-hosted LAN trailer
adapter instead. The adapter searches a trailer on demand for one title and streams it already
muxed; nothing is downloaded, analysed or stored ahead of time.

## How it fits

- **Data** — `rust-modules/data/src/metadata/online_trailer.rs`. After the extras fetch,
  `fetch_full` calls `online_trailer::fill`: when `Detail::trailer()` is `None` on a movie/show page,
  one synthetic `Extra` is appended (rk `plx-ot-<parent rk>`, subtype `trailer`, `part` =
  `plx-online-trailer:<type>/<year>/<pct title>`). A Plex trailer always wins. Every existing
  consumer — Trailer disc, background preview, full-trailer mode, extras shelf — reads it through
  `Detail::trailer()` unchanged.
- **Resolve** — `rust-modules/media/src/route/online_trailer.rs`. `build_stream` recognises the
  marker before any PMS work and asks the adapter once (`GET /api/trailer?title&year&type[&quality]`)
  on the resolve worker. The answer's `stream_url` (held to the adapter's origin) becomes a plain
  direct-play `Plan`; codecs come from `vcodec`/`acodec` (default H.264/AAC). No `/decision`, no
  PlayQueue, no PMS fetch, no token.
- **No account writes** — `route::online_trailer_session` (installed `cur_rk` carries the synthetic
  prefix) makes `begin_timeline_reporting` and `scrobble_stop` refuse the session; `fresh_play` is
  false for the marker, so no remembered choice is written back.
- **Failure** — an unreachable adapter, a non-2xx, a body without `stream_url` or an unfeedable codec
  (Opus/Vorbis) lands as a plan with no URL: the player read-out with Retry, or for a preview the
  item's negative fact (`preview::note_refused_direct`), so the dwell does not ask again for that page
  this session. The process-wide preview breaker is not touched by a resolve failure.

## Configuration (build time)

| Variable | Meaning |
| --- | --- |
| `PLX_TRAILER_ADAPTER` | Adapter base URL, e.g. `http://192.168.0.2:8765`. Default `http://192.168.0.2:8765`. Empty disables the fallback. |
| `PLX_TRAILER_QUALITY` | Optional `quality` parameter (digits, e.g. `1080`, `2160`). Empty = adapter default. |

`.github/workflows/online-trailer-ipk.yml` builds the `com.beb.plxnative.debug` package with both as
inputs (`FLAVOR=debug RELEASE=1`: separate app id, no dev triggers).

## Stream requirements (not verified on a television)

The adapter stream goes through the bundled FFmpeg (`matroska`, `mov`, `mpegts` demuxers) over the
plaintext `stream.rs` socket: redirects and chunked bodies of unknown length are supported. Video
H.264/HEVC (VP9/AV1 only where the set's decoder table lists them), audio AAC/AC-3/E-AC-3. A live mux
with no `Content-Length` and no duration in its header plays, but the playbar has no length and a
seek reopen with `Range` fails unless the adapter serves ranges. Each request must answer within the
plaintext transport's 15 s inactivity bound.
