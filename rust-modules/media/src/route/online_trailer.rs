//! **The online trailer's resolve** — the media half of `plx_data::metadata::online_trailer`.
//!
//! A play or preview request whose `part` is an online-trailer marker never reaches the PMS half
//! of `plan::build_stream`: no `/decision`, no PlayQueue, no `fetch_playing_item`, no transcode.
//! Instead the resolve worker asks the configured LAN trailer adapter ONCE for this one title
//! (`GET /api/trailer?title=…&year=…&type=…`), and its answer becomes a plain direct-play [`Plan`]:
//! the adapter's `stream_url`, fed through the same bundled demuxer and Starfish pipeline as any
//! Plex direct play (`ff.rs` over the plaintext `stream.rs` socket, which follows redirects and
//! reads chunked bodies of unknown length).
//!
//! **Privacy and scope.** The request carries no credential header and no token; the adapter's
//! origin is the build's configured one and nothing else is dialled: a `stream_url` naming another
//! origin is refused rather than followed. The movie title is never written to the event log.
//!
//! **What is NOT written to the server.** The landing installs the synthetic rating key
//! (`online_trailer::RK_PREFIX…`) as `cur_rk`; `route::online_trailer_session` reads that, and the
//! timeline reporter and the stop scrobble both refuse such a session, exactly as they refuse a
//! preview. A full-screen online trailer therefore never creates progress or a watched state.
//!
//! **Failure is a failed resolve, nothing more.** No answer, a non-2xx (the adapter's "no German
//! trailer found"), an unparsable body or a codec the pipeline cannot be fed all land as a plan
//! with no URL: the player read-out offers Retry, and a preview records the item's negative fact
//! (`preview::note_refused_direct`) so the dwell does not ask the adapter again for the same page
//! in this session. The process-wide preview breaker is not touched by a resolve failure.

use plx_data::metadata::online_trailer::{self, Query};
use std::time::{Duration, Instant};

use super::{Plan, ResolveEnv};

/// The whole resolve's budget, both attempts included. The adapter searches on demand, so its
/// first answer can take a while; the plaintext transport's own 15 s inactivity bound still applies
/// to each attempt.
const RESOLVE_BUDGET: Duration = Duration::from_secs(40);

/// Attempts on a transport failure or deadline. A second attempt is worth it because the
/// adapter's search usually completes (and is cached there) even when its first answer was late;
/// an HTTP answer — a 404 "nothing found" included — is never retried.
const ATTEMPTS: u32 = 2;

/// Build the plan for an online-trailer `part`. Blocking; runs on the resolve worker only.
pub(super) fn plan(part: &str, env: &ResolveEnv) -> Plan {
    let base = Plan {
        sid: env.sid,
        direct_play_mode: env.direct_play_mode,
        // Early failures carry no negative claim about the decoder.
        source_decodable: true,
        ..Default::default()
    };
    let Some(query) = Query::from_part(part) else {
        crate::player::log("online-trailer: malformed marker; nothing to resolve");
        return base;
    };
    let adapter = online_trailer::adapter_base();
    let Some(origin) = plx_plex::plex::Origin::parse(adapter).filter(|_| !adapter.is_empty()) else {
        crate::player::log("online-trailer: no usable adapter address configured");
        return base;
    };
    let path = query.api_path(online_trailer::quality());
    let Some(body) = fetch(&origin, &path, query.kind) else {
        return base;
    };
    match Answer::parse(&body, adapter) {
        Ok(a) => {
            crate::player::log(&format!(
                "online-trailer: {} → {}x{} v={} a={} lang={} (direct play from adapter)",
                query.kind.as_str(),
                a.width,
                a.height,
                a.vcodec,
                a.acodec,
                a.audio_language
            ));
            Plan {
                url: a.url,
                vcodec: a.vcodec.clone(),
                acodec: a.acodec.clone(),
                src_vcodec: a.vcodec,
                src_acodec: a.acodec,
                fps: a.fps,
                src_fps: a.fps,
                src_measure: (0, a.width, a.height),
                ..base
            }
        }
        Err(why) => {
            crate::player::log(&format!("online-trailer: adapter answer refused — {why}"));
            base
        }
    }
}

/// One adapter request, retried once on a transport failure or deadline. `Some(body)` only for a
/// 2xx answer.
fn fetch(
    origin: &plx_plex::plex::Origin,
    path: &str,
    kind: online_trailer::Kind,
) -> Option<Vec<u8>> {
    let deadline = Instant::now() + RESOLVE_BUDGET;
    for attempt in 1..=ATTEMPTS {
        if Instant::now() >= deadline {
            break;
        }
        match plx_plex::http::request_until_outcome(
            origin,
            path,
            plx_plex::http::Method::Get,
            &[plx_plex::http::ACCEPT_JSON],
            deadline,
            None,
        ) {
            plx_plex::http::RequestOutcome::Response(reply) if reply.ok() => {
                return Some(reply.body);
            }
            plx_plex::http::RequestOutcome::Response(reply) => {
                crate::player::log(&format!(
                    "online-trailer: adapter {} answered HTTP {} for a {} — no trailer",
                    origin.log_form(),
                    reply.status,
                    kind.as_str()
                ));
                return None;
            }
            plx_plex::http::RequestOutcome::Deadline => {
                crate::player::log(&format!(
                    "online-trailer: adapter {} did not answer in time (attempt {attempt}/{ATTEMPTS})",
                    origin.log_form()
                ));
            }
            plx_plex::http::RequestOutcome::Transport(_) => {
                crate::player::log(&format!(
                    "online-trailer: adapter {} unreachable (attempt {attempt}/{ATTEMPTS})",
                    origin.log_form()
                ));
            }
        }
    }
    None
}

/// The parts of the adapter's `/api/trailer` answer the plan uses, normalised to the codec
/// spellings the Load payload expects (`h264`/`hevc`/`vp9`/`av1`, `aac`/`ac3`/`eac3`).
#[derive(Debug, PartialEq)]
struct Answer {
    url: String,
    vcodec: String,
    acodec: String,
    width: i64,
    height: i64,
    fps: f64,
    audio_language: String,
}

impl Answer {
    /// Lenient about which fields are present (the adapter is not this project's), strict about the
    /// two that decide whether the pipeline can be fed: the stream's ORIGIN and its CODECS.
    fn parse(body: &[u8], adapter: &str) -> Result<Answer, String> {
        let v: serde_json::Value =
            serde_json::from_slice(body).map_err(|e| format!("not JSON ({e})"))?;
        let s = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").trim().to_string();
        let n = |k: &str| v.get(k).and_then(|x| x.as_i64()).unwrap_or(0).max(0);
        let stream = s("stream_url");
        if stream.is_empty() {
            return Err("no stream_url".into());
        }
        let url = stream_url_on_adapter(&stream, adapter)
            .ok_or_else(|| "stream_url is not on the configured adapter".to_string())?;
        let raw_v = s("vcodec");
        let vcodec = video_codec(&raw_v).ok_or_else(|| format!("video codec {raw_v:?} cannot be fed"))?;
        let raw_a = [s("acodec"), s("audio_codec")].into_iter().find(|c| !c.is_empty()).unwrap_or_default();
        let acodec = audio_codec(&raw_a).ok_or_else(|| format!("audio codec {raw_a:?} cannot be fed"))?;
        let fps = v.get("fps").and_then(|x| x.as_f64()).filter(|f| f.is_finite() && *f > 0.0).unwrap_or(0.0);
        let audio_language = [s("audio_language"), s("language")].into_iter().find(|c| !c.is_empty()).unwrap_or_default();
        // A language code only: never let a free-text field into the log line.
        let audio_language = if audio_language.len() <= 8 && audio_language.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
            audio_language
        } else {
            String::new()
        };
        Ok(Answer { url, vcodec, acodec, width: n("width"), height: n("height"), fps, audio_language })
    }
}

/// The stream URL, made absolute against the adapter and held to its origin. `None` for any other
/// origin (including an https one: the adapter is a plaintext LAN service, and following it
/// elsewhere is exactly the network reach this feature must not have).
fn stream_url_on_adapter(stream: &str, adapter: &str) -> Option<String> {
    let adapter_origin = plx_plex::plex::Origin::parse(adapter)?;
    if stream.starts_with('/') && !stream.starts_with("//") {
        return Some(format!("{}{stream}", adapter_origin.base()));
    }
    if !(stream.starts_with("http://") || stream.starts_with("https://")) {
        return None;
    }
    let origin = plx_plex::plex::Origin::parse(stream)?;
    (origin == adapter_origin).then(|| stream.to_string())
}

/// RFC 6381 / FFmpeg / yt-dlp spellings → the Load payload's. Empty is the adapter's default mux,
/// H.264. `None` = the pipeline has no framing for it.
fn video_codec(raw: &str) -> Option<String> {
    let c = raw.to_ascii_lowercase();
    let c = c.as_str();
    let name = if c.is_empty() || c.starts_with("avc") || c.starts_with("h264") {
        "h264"
    } else if c.starts_with("hvc1") || c.starts_with("hev1") || c.starts_with("hevc") || c.starts_with("h265") {
        "hevc"
    } else if c.starts_with("vp09") || c.starts_with("vp9") {
        "vp9"
    } else if c.starts_with("av01") || c == "av1" {
        "av1"
    } else {
        return None;
    };
    Some(name.to_string())
}

/// Audio: AAC, AC-3 and E-AC-3 are what the feed pipeline takes from a web source. Opus and Vorbis
/// (YouTube's WebM audio) are refused here so the failure is a readable log line instead of a
/// silent or refused Load. Empty is the adapter's default mux, AAC.
fn audio_codec(raw: &str) -> Option<String> {
    let c = raw.to_ascii_lowercase();
    let c = c.as_str();
    let name = if c.is_empty() || c.starts_with("mp4a") || c.starts_with("aac") {
        "aac"
    } else if c == "ac-3" || c == "ac3" {
        "ac3"
    } else if c == "ec-3" || c == "eac3" || c == "e-ac-3" {
        "eac3"
    } else {
        return None;
    };
    Some(name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ADAPTER: &str = "http://192.168.0.2:8765";

    #[test]
    fn the_documented_answer_becomes_an_h264_aac_direct_play() {
        let body = br#"{"title":"DUNE - Offizieller Main Trailer #2 Deutsch HD German (2021)",
            "language":"de","audio_language":"de","audio_verified":true,"height":1080,
            "mode":"mux","vcodec":"avc1.640028",
            "stream_url":"http://192.168.0.2:8765/stream/abc","youtube_url":"https://www.youtube.com/watch?v=x",
            "expires_in_seconds":356}"#;
        let a = Answer::parse(body, ADAPTER).unwrap();
        assert_eq!(a.url, "http://192.168.0.2:8765/stream/abc");
        assert_eq!((a.vcodec.as_str(), a.acodec.as_str()), ("h264", "aac"));
        assert_eq!(a.height, 1080);
        assert_eq!(a.audio_language, "de");
    }

    #[test]
    fn a_relative_stream_url_is_joined_to_the_adapter() {
        let a = Answer::parse(br#"{"stream_url":"/stream/abc","vcodec":"hev1.2.4.L153"}"#, ADAPTER).unwrap();
        assert_eq!(a.url, "http://192.168.0.2:8765/stream/abc");
        assert_eq!(a.vcodec, "hevc");
    }

    #[test]
    fn a_stream_on_another_origin_is_refused() {
        for url in [
            "http://10.0.0.9:8765/stream/abc",
            "http://192.168.0.2:9999/stream/abc",
            "https://rr1.googlevideo.com/videoplayback?x",
            "//evil.example/stream",
            "ftp://192.168.0.2:8765/x",
        ] {
            let body = format!(r#"{{"stream_url":"{url}"}}"#);
            assert!(Answer::parse(body.as_bytes(), ADAPTER).is_err(), "{url}");
        }
    }

    #[test]
    fn missing_stream_or_unfeedable_codecs_are_refused() {
        assert!(Answer::parse(br#"{"error":"no german trailer"}"#, ADAPTER).is_err());
        assert!(Answer::parse(br#"not json"#, ADAPTER).is_err());
        assert!(Answer::parse(br#"{"stream_url":"/s","acodec":"opus"}"#, ADAPTER).is_err());
        assert!(Answer::parse(br#"{"stream_url":"/s","vcodec":"mpeg4"}"#, ADAPTER).is_err());
    }

    #[test]
    fn codec_spellings_normalise() {
        assert_eq!(video_codec("vp09.00.51.08").as_deref(), Some("vp9"));
        assert_eq!(video_codec("av01.0.13M.10").as_deref(), Some("av1"));
        assert_eq!(video_codec("H264").as_deref(), Some("h264"));
        assert_eq!(audio_codec("mp4a.40.2").as_deref(), Some("aac"));
        assert_eq!(audio_codec("ec-3").as_deref(), Some("eac3"));
        assert_eq!(audio_codec("").as_deref(), Some("aac"));
        assert_eq!(audio_codec("vorbis"), None);
    }

    #[test]
    fn a_free_text_language_never_reaches_the_log() {
        let a = Answer::parse(br#"{"stream_url":"/s","audio_language":"Deutsch (Original) mit Titel"}"#, ADAPTER).unwrap();
        assert!(a.audio_language.is_empty());
    }
}
