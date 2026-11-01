//! Genre, and the release details YouTube leaves out, from MusicBrainz: the open (CC0) music
//! database most taggers use. YouTube Music has no genre at all, and its rows often lack the year
//! or track number. Only a confident match is used: the same title, the same lead artist, and a
//! length within a few seconds. A downloaded file is better with no genre than with another
//! song's.
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::sync::Mutex;

const ROOT: &str = "https://musicbrainz.org/ws/2";
/// MusicBrainz asks every client to say who it is, and blocks anonymous ones.
const UA: &str =
    concat!("Limusic v", env!("CARGO_PKG_VERSION"), " (https://github.com/SimoHypers/limusic)");

#[derive(Debug, Default, PartialEq)]
pub struct Found {
    pub genre: Option<String>,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub year: Option<u32>,
    /// Only from a release whose title is the album being tagged: a compilation numbers the
    /// same recording differently.
    pub track: Option<(u32, Option<u32>)>,
}

/// One request a second, MusicBrainz's limit for every client; an album download is many songs.
async fn get(url: &str) -> Option<Value> {
    static LAST: Mutex<Option<Instant>> = Mutex::const_new(None);
    let mut last = LAST.lock().await;
    if let Some(at) = *last {
        let wait = Duration::from_millis(1100).saturating_sub(at.elapsed());
        tokio::time::sleep(wait).await;
    }
    *last = Some(Instant::now());
    let response = crate::http::client()
        .get(url)
        .header(reqwest::header::USER_AGENT, UA)
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?;
    response.json().await.ok()
}

/// Lowercase letters and digits only, with a "(feat. …)" / "[Official …]" tail dropped, so
/// "Pal Pal (with Talwiinder)" and "pal pal" compare equal.
fn simple(text: &str) -> String {
    let head = text.split(['(', '[']).next().unwrap_or(text);
    let head = head.split(" feat").next().unwrap_or(head);
    head.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

/// The first name in a YouTube artist line ("A, B & C" → "A").
fn lead_artist(artists: &str) -> &str {
    artists.split([',', '&']).next().unwrap_or(artists).trim()
}

fn quoted(text: &str) -> String {
    // Lucene syntax: escape the characters that would end or alter the phrase.
    let escaped: String = text
        .chars()
        .flat_map(|c| match c {
            '"' | '\\' => vec!['\\', c],
            _ => vec![c],
        })
        .collect();
    format!("\"{escaped}\"")
}

fn year(date: &str) -> Option<u32> {
    date.get(..4)?.parse().ok().filter(|y| (1900..=2100).contains(y))
}

/// Pick the recording and release from a search answer. Pure, so it is tested on fixtures.
fn choose(
    answer: &Value,
    title: &str,
    artists: &str,
    album: Option<&str>,
    length_ms: Option<u64>,
) -> Option<(Found, Option<String>, Option<String>)> {
    let (want_title, want_artist) = (simple(title), simple(lead_artist(artists)));
    let want_album = album.map(simple);
    let candidates: Vec<&Value> = answer["recordings"]
        .as_array()?
        .iter()
        .filter(|r| {
            let credit = r["artist-credit"][0]["name"].as_str().unwrap_or("");
            r["score"].as_u64().unwrap_or(0) >= 90
                && simple(r["title"].as_str().unwrap_or("")) == want_title
                && !want_artist.is_empty()
                && simple(credit) == want_artist
        })
        .collect();
    // A length that agrees beats an unknown one; one that disagrees is another version (a remix,
    // a live take, the radio edit) and never counts.
    let length = |r: &Value| match (length_ms, r["length"].as_u64()) {
        (Some(a), Some(b)) if a.abs_diff(b) <= 7_000 => Some(true),
        (Some(_), Some(_)) => Some(false),
        _ => None,
    };
    let recording = candidates
        .iter()
        .find(|r| length(r) == Some(true))
        .or_else(|| candidates.iter().find(|r| length(r).is_none()))?;
    let releases = recording["releases"].as_array().cloned().unwrap_or_default();
    let official = |r: &&Value| r["status"].as_str().is_none_or(|s| s == "Official");
    let same_album = |r: &&Value| {
        want_album.as_ref().is_some_and(|a| simple(r["title"].as_str().unwrap_or("")) == *a)
    };
    let release = releases.iter().filter(official).find(same_album).or_else(|| {
        // No album to match: the earliest official album/EP/single is the original release.
        let mut picks: Vec<&Value> = releases
            .iter()
            .filter(official)
            .filter(|r| {
                let kind = r["release-group"]["primary-type"].as_str().unwrap_or("");
                let secondary = r["release-group"]["secondary-types"].as_array();
                ["Album", "EP", "Single"].contains(&kind) && secondary.is_none_or(|s| s.is_empty())
            })
            .collect();
        picks.sort_by_key(|r| r["date"].as_str().unwrap_or("9999").to_owned());
        picks.into_iter().next()
    });
    let mut found = Found::default();
    let mut group = None;
    if let Some(release) = release {
        found.album = release["title"].as_str().map(str::to_owned);
        found.album_artist = release["artist-credit"][0]["name"]
            .as_str()
            .or(recording["artist-credit"][0]["name"].as_str())
            .map(str::to_owned);
        found.year = release["date"].as_str().and_then(year);
        if same_album(&release) {
            let media = &release["media"][0];
            let number = media["track"][0]["number"].as_str().and_then(|n| n.parse().ok());
            let total = media["track-count"].as_u64().map(|n| n as u32);
            found.track = number.map(|n| (n, total));
        }
        group = release["release-group"]["id"].as_str().map(str::to_owned);
    }
    let artist = recording["artist-credit"][0]["artist"]["id"].as_str().map(str::to_owned);
    Some((found, group, artist))
}

/// The most-voted genre, title-cased ("hip hop" → "Hip Hop").
fn top_genre(answer: &Value) -> Option<String> {
    let genres = answer["genres"].as_array()?;
    let best = genres.iter().max_by_key(|g| g["count"].as_u64().unwrap_or(0))?;
    let name = best["name"].as_str()?;
    Some(
        name.split(' ')
            .map(|w| {
                let mut c = w.chars();
                c.next().map(|f| f.to_uppercase().chain(c).collect::<String>()).unwrap_or_default()
            })
            .collect::<Vec<_>>()
            .join(" "),
    )
}

pub async fn lookup(
    title: &str,
    artists: &str,
    album: Option<&str>,
    length_ms: Option<u64>,
) -> Option<Found> {
    // The album is matched in `choose`, not in the query: YouTube's album title can differ from
    // MusicBrainz's, and the song should still be found.
    let query = format!(
        "recording:{} AND artist:{}",
        quoted(title.split(['(', '[']).next().unwrap_or(title).trim()),
        quoted(lead_artist(artists))
    );
    let url = format!("{ROOT}/recording?fmt=json&limit=10&query={}", urlencoding::encode(&query));
    let answer = get(&url).await?;
    let (mut found, group, artist) = choose(&answer, title, artists, album, length_ms)?;
    // Genres are voted on the release group, and more often on the artist.
    if let Some(id) = group {
        found.genre = get(&format!("{ROOT}/release-group/{id}?fmt=json&inc=genres"))
            .await
            .as_ref()
            .and_then(top_genre);
    }
    if found.genre.is_none() {
        if let Some(id) = artist {
            found.genre = get(&format!("{ROOT}/artist/{id}?fmt=json&inc=genres"))
                .await
                .as_ref()
                .and_then(top_genre);
        }
    }
    Some(found)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn answer() -> Value {
        json!({ "recordings": [
            { "score": 100, "title": "Pal Pal", "length": 208000,
              "artist-credit": [{ "name": "Afusic", "artist": { "id": "artist-1" } }],
              "releases": [
                { "title": "Bollywood Hits 2025", "status": "Official", "date": "2025-06-01",
                  "release-group": { "id": "rg-comp", "primary-type": "Album", "secondary-types": ["Compilation"] },
                  "media": [{ "track-count": 40, "track": [{ "number": "17" }] }] },
                { "title": "Pal Pal", "status": "Official", "date": "2025-01-10",
                  "artist-credit": [{ "name": "Afusic" }],
                  "release-group": { "id": "rg-single", "primary-type": "Single" },
                  "media": [{ "track-count": 1, "track": [{ "number": "1" }] }] }
              ] },
            { "score": 95, "title": "Pal Pal", "length": 180000,
              "artist-credit": [{ "name": "Someone Else", "artist": { "id": "artist-2" } }] }
        ] })
    }

    #[test]
    fn a_confident_match_fills_the_release_and_its_track_number() {
        let (found, group, artist) = choose(
            &answer(),
            "Pal Pal (with Talwiinder)",
            "Afusic, Talwiinder & AliSoomroMusic",
            Some("Pal Pal"),
            Some(208_500),
        )
        .unwrap();
        assert_eq!(found.album.as_deref(), Some("Pal Pal"));
        assert_eq!(found.album_artist.as_deref(), Some("Afusic"));
        assert_eq!((found.year, found.track), (Some(2025), Some((1, Some(1)))));
        assert_eq!((group.as_deref(), artist.as_deref()), (Some("rg-single"), Some("artist-1")));
    }

    #[test]
    fn without_an_album_the_original_release_wins_but_numbers_are_not_guessed() {
        let (found, ..) = choose(&answer(), "Pal Pal", "Afusic", None, None).unwrap();
        assert_eq!(found.album.as_deref(), Some("Pal Pal"));
        assert_eq!(found.track, None);
    }

    #[test]
    fn a_matching_length_beats_an_unknown_one() {
        let mut answer = answer();
        let first = answer["recordings"][0].clone();
        let mut unknown = first.clone();
        unknown["length"] = Value::Null;
        unknown["releases"] = json!([]);
        answer["recordings"] = json!([unknown, first]);
        let (found, ..) = choose(&answer, "Pal Pal", "Afusic", None, Some(208_000)).unwrap();
        assert_eq!(found.album.as_deref(), Some("Pal Pal"));
    }

    #[test]
    fn another_artist_or_length_is_no_match() {
        assert!(choose(&answer(), "Pal Pal", "Talwiinder", None, None).is_none());
        assert!(choose(&answer(), "Pal Pal", "Afusic", None, Some(240_000)).is_none());
        assert!(choose(&answer(), "Other Song", "Afusic", None, None).is_none());
    }

    #[test]
    fn the_most_voted_genre_is_title_cased() {
        let genres =
            json!({ "genres": [{ "name": "pop", "count": 2 }, { "name": "hip hop", "count": 5 }] });
        assert_eq!(top_genre(&genres).as_deref(), Some("Hip Hop"));
        assert_eq!(top_genre(&json!({ "genres": [] })), None);
    }
}
