//! **Online trailer fallback** — a trailer for a movie or show page whose server has none, played
//! from a LAN trailer adapter (a self-hosted HTTP service that searches a trailer on demand and
//! streams it already muxed).
//!
//! The page offers it exactly like a Plex trailer: [`fill`] adds ONE synthetic [`Extra`] to a
//! [`Detail`] that has no playable trailer of its own, and every existing consumer — the Trailer
//! disc, the background preview, full-trailer mode, the extras shelf — reads it through
//! [`Detail::trailer`] unchanged. What makes it different is its `part`: not a PMS Part key but a
//! [`Query`] spelled under the [`PART_SCHEME`] marker. The media layer's resolve recognises that
//! marker before any PMS work (`route::online_trailer`) and builds a direct-play plan from the
//! adapter's answer instead of from `/decision`, so the item never reaches the Plex Media Decision
//! Engine, never gets a PlayQueue and is never reported to the server.
//!
//! Nothing here does I/O: building the extra is pure, and the adapter is asked only when a trailer
//! is actually started — one item at a time, on demand, nothing downloaded or stored ahead.
//!
//! **Configuration.** The adapter base URL is fixed at BUILD time: `PLX_TRAILER_ADAPTER` in the
//! build environment (e.g. `http://192.168.0.2:8765`), else [`DEFAULT_ADAPTER`]. An empty value
//! turns the fallback off entirely: no extra is added and no request is ever made.
//! `PLX_TRAILER_QUALITY` (digits, e.g. `1080` or `2160`) is passed on as the adapter's `quality`
//! parameter; absent, the adapter picks.

use super::{Detail, Extra};

/// The adapter this build asks when the build environment names none.
pub const DEFAULT_ADAPTER: &str = "http://192.168.0.2:8765";

/// The `part` marker. Not a URL scheme anything else in the app speaks, so a synthetic part can
/// never be mistaken for a PMS key (`/library/parts/…`) or opened by the media transport as-is.
pub const PART_SCHEME: &str = "plx-online-trailer:";

/// Prefix of the synthetic extra's rating key. PMS rating keys are decimal digits, so this can
/// never collide with a real one; the parent's key follows it, which keeps the key unique per item
/// (the preview's per-item negative cache is keyed on it).
pub const RK_PREFIX: &str = "plx-ot-";

/// The adapter base URL this build was configured with, without a trailing slash. Empty = off.
pub fn adapter_base() -> &'static str {
    option_env!("PLX_TRAILER_ADAPTER")
        .unwrap_or(DEFAULT_ADAPTER)
        .trim()
        .trim_end_matches('/')
}

/// Is the fallback configured at all?
pub fn enabled() -> bool {
    !adapter_base().is_empty()
}

/// The `quality` the adapter is asked for, when the build names one. Digits only; anything else is
/// ignored rather than forwarded into a query string.
pub fn quality() -> Option<u32> {
    option_env!("PLX_TRAILER_QUALITY")
        .map(str::trim)
        .filter(|q| !q.is_empty() && q.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|q| q.parse().ok())
        .filter(|q| *q > 0)
}

/// Is this rating key a synthetic online-trailer key?
pub fn is_rk(rk: &str) -> bool {
    rk.starts_with(RK_PREFIX)
}

/// Is this `part` an online-trailer marker?
pub fn is_part(part: &str) -> bool {
    part.starts_with(PART_SCHEME)
}

/// What the adapter's `type` parameter says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Movie,
    Show,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Movie => "movie",
            Kind::Show => "show",
        }
    }

    fn of(kind: &str) -> Option<Kind> {
        match kind {
            "movie" => Some(Kind::Movie),
            "show" => Some(Kind::Show),
            _ => None,
        }
    }
}

/// One trailer search: what the adapter is asked for. `year` 0 = unknown, and is then not sent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Query {
    pub kind: Kind,
    pub year: i64,
    pub title: String,
}

impl Query {
    /// The query for a detail page, or `None` for a page kind that has no trailer control
    /// (episode/season) or an item with no title to search by.
    pub fn for_item(kind: &str, title: &str, year: i64) -> Option<Query> {
        let kind = Kind::of(kind)?;
        let title = title.trim();
        if title.is_empty() {
            return None;
        }
        Some(Query { kind, year: year.max(0), title: title.to_string() })
    }

    /// `plx-online-trailer:<type>/<year>/<percent-encoded title>`.
    pub fn to_part(&self) -> String {
        format!(
            "{PART_SCHEME}{}/{}/{}",
            self.kind.as_str(),
            self.year,
            plx_plex::plex::urlenc_str(&self.title)
        )
    }

    /// The inverse of [`Query::to_part`]; `None` for anything that is not exactly that shape.
    pub fn from_part(part: &str) -> Option<Query> {
        let rest = part.strip_prefix(PART_SCHEME)?;
        let mut it = rest.splitn(3, '/');
        let kind = Kind::of(it.next()?)?;
        let year: i64 = it.next()?.parse().ok()?;
        let title = pct_decode(it.next()?)?;
        if year < 0 || title.trim().is_empty() {
            return None;
        }
        Some(Query { kind, year, title })
    }

    /// The adapter's metadata path for this query: `/api/trailer?title=…&year=…&type=…[&quality=…]`.
    pub fn api_path(&self, quality: Option<u32>) -> String {
        let mut path = format!("/api/trailer?title={}", plx_plex::plex::urlenc_str(&self.title));
        if self.year > 0 {
            path.push_str(&format!("&year={}", self.year));
        }
        path.push_str("&type=");
        path.push_str(self.kind.as_str());
        if let Some(q) = quality {
            path.push_str(&format!("&quality={q}"));
        }
        path
    }
}

/// Strict `%XX` decoding into UTF-8. `None` on a malformed escape or on bytes that are not UTF-8;
/// `+` is NOT a space here, because [`Query::to_part`] encodes a space as `%20`.
fn pct_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = b.get(i + 1..i + 3)?;
            let hi = (hex[0] as char).to_digit(16)?;
            let lo = (hex[1] as char).to_digit(16)?;
            out.push((hi * 16 + lo) as u8);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// The synthetic extra for an item. Its `title` is the boilerplate "Trailer", so the trailer
/// transport and the HUD show the item's own title under the `Trailer` kicker
/// (`screens::detail::trailer::transport_title`); codecs are left empty because only the adapter
/// knows them, and the resolve fills them from its answer.
pub fn extra_for(parent_rk: &str, query: &Query) -> Extra {
    Extra {
        rk: format!("{RK_PREFIX}{parent_rk}"),
        title: "Trailer".to_string(),
        subtype: "trailer".to_string(),
        extra_type: 1,
        part: query.to_part(),
        ..Extra::default()
    }
}

/// Add the online trailer to a movie/show detail that has no playable trailer of its own. Returns
/// whether one was added. A Plex trailer always wins: this never replaces or reorders one.
pub fn fill(d: &mut Detail) -> bool {
    if !enabled() || d.rk.is_empty() || d.trailer().is_some() {
        return false;
    }
    let Some(query) = Query::for_item(&d.kind, &d.title, d.year) else {
        return false;
    };
    let extra = extra_for(&d.rk, &query);
    d.trailer_rk = extra.rk.clone();
    d.extras.push(extra);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_part_round_trips_through_its_marker() {
        let q = Query::for_item("movie", "Dune: Part Two", 2024).unwrap();
        let part = q.to_part();
        assert_eq!(part, "plx-online-trailer:movie/2024/Dune%3A%20Part%20Two");
        assert!(is_part(&part));
        assert_eq!(Query::from_part(&part), Some(q));
    }

    #[test]
    fn non_ascii_titles_and_reserved_characters_survive_the_round_trip() {
        let q = Query::for_item("show", "Die Schöne & das Biest / Teil 2?", 0).unwrap();
        assert_eq!(Query::from_part(&q.to_part()), Some(q));
    }

    #[test]
    fn the_api_path_is_percent_encoded_and_omits_an_unknown_year() {
        let q = Query::for_item("movie", "The Batman", 2022).unwrap();
        assert_eq!(q.api_path(None), "/api/trailer?title=The%20Batman&year=2022&type=movie");
        assert_eq!(
            q.api_path(Some(2160)),
            "/api/trailer?title=The%20Batman&year=2022&type=movie&quality=2160"
        );
        let s = Query::for_item("show", "Dark", 0).unwrap();
        assert_eq!(s.api_path(None), "/api/trailer?title=Dark&type=show");
    }

    #[test]
    fn episodes_seasons_and_untitled_items_get_no_query() {
        assert!(Query::for_item("episode", "Pilot", 2017).is_none());
        assert!(Query::for_item("season", "Season 1", 2017).is_none());
        assert!(Query::for_item("movie", "   ", 2017).is_none());
    }

    #[test]
    fn malformed_markers_are_refused() {
        for bad in [
            "/library/parts/9/file.mkv",
            "plx-online-trailer:",
            "plx-online-trailer:episode/2020/X",
            "plx-online-trailer:movie/abc/X",
            "plx-online-trailer:movie/2020/%ZZ",
            "plx-online-trailer:movie/2020/%FF",
            "plx-online-trailer:movie/2020/",
        ] {
            assert_eq!(Query::from_part(bad), None, "{bad}");
        }
    }

    #[test]
    fn the_synthetic_extra_is_a_playable_trailer_with_a_non_pms_key() {
        let q = Query::for_item("movie", "Dune", 2021).unwrap();
        let e = extra_for("4711", &q);
        assert!(e.playable());
        assert!(e.is_trailer());
        assert_eq!(e.rk, "plx-ot-4711");
        assert!(is_rk(&e.rk));
        assert!(!is_rk("4711"));
        assert!(e.vcodec.is_empty() && e.acodec.is_empty());
    }
}
