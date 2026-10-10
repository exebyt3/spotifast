//! Search over the web player's GraphQL endpoint, with the streaming
//! session's own token. Spotify's clients search there, and it has no
//! per-app quota, so a search does not wait on the shared app's rate limit.
//!
//! The query is a persisted one, named by a hash that Spotify moves with
//! each web player release. A stale hash is refreshed from the player's own
//! bundle; any other failure is left to the Web API.

use std::sync::Mutex;

use librespot_core::Session;
use serde_json::{Value, json};

use crate::http::Http;
use crate::api::models::{
    Album, Artist, ArtistRef, Episode, Image, Owner, Page, Playlist, SearchResults, Show, Track,
};
use crate::session_reads::Failure;

const ENDPOINT: &str = "https://api-partner.spotify.com/pathfinder/v1/query";
const WEB_PLAYER: &str = "https://open.spotify.com/";
const SEARCH_OPERATION: &str = "searchDesktop";
/// The web player's route chunk that carries the search operations.
const SEARCH_CHUNK: &str = "xpui-routes-search";
/// The `searchDesktop` hash of the web player build of 2026-10-10.
const KNOWN_HASH: &str = "eef7cc54888d91bdd6802623477873caa3948ae173a0c34fd86827b267e94c03";
/// What the shared app's search answers with per kind of result.
const RESULTS_PER_KIND: u32 = 20;
/// What the web player sends the desktop page to.
const BROWSER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36";
const STALE_QUERY: &str = "PersistedQueryNotFound";

/// The hash found in the web player once the known one went stale.
static FRESH_HASH: Mutex<Option<String>> = Mutex::new(None);

enum Reply {
    Results(Value),
    Stale,
}

/// Every kind of result for `query`, as the Web API's search shapes them.
pub async fn search(
    session: &Session,
    http: &Http,
    query: &str,
) -> Result<SearchResults, Failure> {
    let hash = current_hash();
    let reply = fetch(session, query, &hash).await?;
    let results = match reply {
        Reply::Results(results) => results,
        Reply::Stale => {
            let fresh = scrape_hash(http).await?;
            *FRESH_HASH.lock().unwrap_or_else(|lock| lock.into_inner()) = Some(fresh.clone());
            match fetch(session, query, &fresh).await? {
                Reply::Results(results) => results,
                Reply::Stale => {
                    return Err(Failure::Retry(anyhow::anyhow!("search hash refused")));
                }
            }
        }
    };
    Ok(results_of(&results))
}

fn current_hash() -> String {
    FRESH_HASH
        .lock()
        .unwrap_or_else(|lock| lock.into_inner())
        .clone()
        .unwrap_or_else(|| KNOWN_HASH.to_string())
}

async fn fetch(session: &Session, query: &str, hash: &str) -> Result<Reply, Failure> {
    let token = session.login5().auth_token().await?.access_token;
    let client_token = session.spclient().client_token().await?;
    let variables = json!({
        "searchTerm": query,
        "offset": 0,
        "limit": RESULTS_PER_KIND,
        "numberOfTopResults": 5,
        "includeAudiobooks": false,
        "includeArtistHasConcertsField": false,
        "includePreReleases": true,
        "includeLocalConcertsField": false,
        "includeAuthors": false,
    });
    let extensions = json!({"persistedQuery": {"version": 1, "sha256Hash": hash}});
    let url = format!(
        "{ENDPOINT}?operationName={SEARCH_OPERATION}&variables={}&extensions={}",
        urlencoding::encode(&variables.to_string()),
        urlencoding::encode(&extensions.to_string()),
    );
    let request = http::Request::builder()
        .uri(url)
        .header("Authorization", format!("Bearer {token}"))
        .header("client-token", client_token)
        .header("Accept", "application/json")
        .header("App-Platform", "Win32_x86_64")
        .body(Default::default())
        .map_err(|error| Failure::Retry(error.into()))?;
    let body = session
        .http_client()
        .request_body(request)
        .await
        .map_err(|error| Failure::Retry(anyhow::anyhow!("search request: {error}")))?;
    let value: Value =
        serde_json::from_slice(&body).map_err(|error| Failure::Retry(error.into()))?;
    if let Some(errors) = value.get("errors").and_then(Value::as_array)
        && !errors.is_empty()
    {
        let stale = errors
            .iter()
            .any(|error| error["message"].as_str() == Some(STALE_QUERY));
        return if stale {
            Ok(Reply::Stale)
        } else {
            Err(Failure::Retry(anyhow::anyhow!("search refused: {errors:?}")))
        };
    }
    match value.pointer("/data/searchV2") {
        Some(results) => Ok(Reply::Results(results.clone())),
        None => Err(Failure::Retry(anyhow::anyhow!("search answered no results"))),
    }
}

/// The current `searchDesktop` hash: the player's page names its main
/// bundle, the bundle maps route names to their chunk files, and the search
/// chunk carries the operations with their hashes.
async fn scrape_hash(http: &Http) -> Result<String, Failure> {
    let retry = |reason: &str| Failure::Retry(anyhow::anyhow!("web player: {reason}"));
    let page = download(http, WEB_PLAYER).await?;
    let bundle_url = script_url(&page, "web-player/web-player.").ok_or_else(|| {
        let head: String = page.chars().take(160).collect();
        retry(&format!("no main bundle in {} bytes: {head}", page.len()))
    })?;
    let bundle = download(http, &bundle_url).await?;
    let id = chunk_id(&bundle, SEARCH_CHUNK).ok_or_else(|| retry("no search chunk"))?;
    let file = chunk_file(&bundle, &id).ok_or_else(|| retry("no search chunk file"))?;
    let base = bundle_url
        .rsplit_once("web-player.")
        .map(|(base, _)| base.to_string())
        .ok_or_else(|| retry("odd bundle address"))?;
    let chunk = download(http, &format!("{base}{SEARCH_CHUNK}.{file}.js")).await?;
    operation_hash(&chunk, SEARCH_OPERATION).ok_or_else(|| retry("no search operation"))
}

/// A page of the web player. Its address list depends on who asks: a
/// client that does not look like a desktop browser is sent the mobile
/// player's files, which carry no search chunk.
async fn download(http: &Http, url: &str) -> Result<String, Failure> {
    let retry = |error: reqwest::Error| {
        Failure::Retry(anyhow::anyhow!("{url}: {}", error.without_url()))
    };
    let client = http
        .client()
        .map_err(|error| Failure::Retry(anyhow::anyhow!(error)))?;
    client
        .get(url)
        .header(reqwest::header::USER_AGENT, BROWSER_AGENT)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(retry)?
        .text()
        .await
        .map_err(retry)
}

/// The address of the script, whichever host serves it, whose file is
/// `marker` followed by a content hash and `.js`. The page names the same
/// path again for its stylesheet.
fn script_url(page: &str, marker: &str) -> Option<String> {
    page.match_indices(marker).find_map(|(at, _)| {
        let end = at + marker.len();
        let hash = page.get(end..end + 8)?;
        let is_script = hash.chars().all(|c| c.is_ascii_hexdigit())
            && page.get(end + 8..end + 11) == Some(".js");
        let from = page[..at].rfind(['"', '\''])? + 1;
        is_script.then(|| page[from..end + 11].to_string())
    })
}

/// The webpack chunk id the bundle names `name`, as in `4406:"name"`.
fn chunk_id(bundle: &str, name: &str) -> Option<String> {
    let needle = format!("\"{name}\"");
    bundle.match_indices(&needle).find_map(|(at, _)| {
        let before = bundle[..at].strip_suffix(':')?;
        let digits: String = before
            .chars()
            .rev()
            .take_while(char::is_ascii_digit)
            .collect();
        (!digits.is_empty()).then(|| digits.chars().rev().collect())
    })
}

/// The content hash that names a chunk's file, as in `4406:"0e343fae"`. The
/// bundle holds one such map for scripts, ahead of the one for stylesheets.
fn chunk_file(bundle: &str, id: &str) -> Option<String> {
    let needle = format!("{id}:\"");
    let mut from = 0;
    while let Some(found) = bundle[from..].find(&needle) {
        let start = from + found;
        let boundary = bundle[..start]
            .chars()
            .next_back()
            .is_none_or(|before| !before.is_ascii_alphanumeric());
        let value_start = start + needle.len();
        if boundary
            && let Some(value) = bundle[value_start..].split('"').next()
            && value.len() == 8
            && value.chars().all(|c| c.is_ascii_hexdigit())
        {
            return Some(value.to_string());
        }
        from = value_start;
    }
    None
}

/// The hash of a persisted operation, as in `"searchDesktop","query","<hash>"`.
fn operation_hash(chunk: &str, operation: &str) -> Option<String> {
    let head = format!("\"{operation}\",\"query\",\"");
    let start = chunk.find(&head)? + head.len();
    let hash = chunk.get(start..start + 64)?;
    hash.chars()
        .all(|c| c.is_ascii_hexdigit())
        .then(|| hash.to_string())
}

fn results_of(results: &Value) -> SearchResults {
    SearchResults {
        tracks: page(results, "/tracksV2/items", "/tracksV2", |entry| {
            track(&entry["item"]["data"])
        }),
        artists: page(results, "/artists/items", "/artists", |entry| {
            artist(&entry["data"])
        }),
        albums: page(results, "/albumsV2/items", "/albumsV2", |entry| {
            album(&entry["data"])
        }),
        playlists: page(results, "/playlists/items", "/playlists", |entry| {
            playlist(&entry["data"])
        }),
        shows: page(results, "/podcasts/items", "/podcasts", |entry| {
            show(&entry["data"])
        }),
        episodes: page(results, "/episodes/items", "/episodes", |entry| {
            episode(&entry["data"])
        }),
    }
}

/// One kind of result as a page; `None` when the answer holds none of it.
fn page<T>(
    results: &Value,
    items: &str,
    section: &str,
    read: impl Fn(&Value) -> Option<T>,
) -> Option<Page<T>> {
    let entries = results.pointer(items)?.as_array()?;
    let items: Vec<T> = entries.iter().filter_map(read).collect();
    let total = results
        .pointer(&format!("{section}/totalCount"))
        .and_then(Value::as_u64)
        .map_or(items.len(), |total| total as usize);
    Some(Page {
        total: u32::try_from(total).unwrap_or(u32::MAX),
        limit: RESULTS_PER_KIND,
        offset: 0,
        next: None,
        items,
    })
}

fn track(data: &Value) -> Option<Track> {
    if data["__typename"] != "Track" {
        return None;
    }
    let uri = data["uri"].as_str()?;
    let album = &data["albumOfTrack"];
    Some(Track {
        id: Some(id_of(uri)),
        uri: uri.to_string(),
        name: text(&data["name"]),
        duration_ms: milliseconds(&data["duration"]["totalMilliseconds"]),
        explicit: data["contentRating"]["label"] == "EXPLICIT",
        artists: artist_refs(&data["artists"]["items"]),
        album: album["uri"].as_str().map(|album_uri| Album {
            id: id_of(album_uri),
            uri: album_uri.to_string(),
            name: text(&album["name"]),
            images: images(&album["coverArt"]["sources"]),
            ..Default::default()
        }),
        is_playable: data["playability"]["playable"].as_bool(),
        ..Default::default()
    })
}

fn artist(data: &Value) -> Option<Artist> {
    if data["__typename"] != "Artist" {
        return None;
    }
    let uri = data["uri"].as_str()?;
    Some(Artist {
        id: id_of(uri),
        uri: uri.to_string(),
        name: text(&data["profile"]["name"]),
        images: images(&data["visuals"]["avatarImage"]["sources"]),
        ..Default::default()
    })
}

fn album(data: &Value) -> Option<Album> {
    if data["__typename"] != "Album" {
        return None;
    }
    let uri = data["uri"].as_str()?;
    // The Web API files an EP under singles.
    let kind = match data["type"].as_str() {
        Some("SINGLE" | "EP") => "single",
        Some("COMPILATION") => "compilation",
        _ => "album",
    };
    Some(Album {
        id: id_of(uri),
        uri: uri.to_string(),
        name: text(&data["name"]),
        album_type: Some(kind.to_string()),
        images: images(&data["coverArt"]["sources"]),
        artists: artist_refs(&data["artists"]["items"]),
        release_date: data["date"]["year"].as_u64().map(|year| year.to_string()),
        ..Default::default()
    })
}

fn playlist(data: &Value) -> Option<Playlist> {
    if data["__typename"] != "Playlist" {
        return None;
    }
    let uri = data["uri"].as_str()?;
    let owner = &data["ownerV2"]["data"];
    let description = plain_text(&text(&data["description"]));
    Some(Playlist {
        id: id_of(uri),
        uri: uri.to_string(),
        name: text(&data["name"]),
        description: (!description.is_empty()).then_some(description),
        images: images(&data["images"]["items"][0]["sources"]),
        owner: Owner {
            id: owner["username"].as_str().map(str::to_string),
            display_name: owner["name"].as_str().map(str::to_string),
            uri: owner["uri"].as_str().map(str::to_string),
        },
        ..Default::default()
    })
}

fn show(data: &Value) -> Option<Show> {
    if data["__typename"] != "Podcast" {
        return None;
    }
    let uri = data["uri"].as_str()?;
    Some(Show {
        id: id_of(uri),
        uri: uri.to_string(),
        name: text(&data["name"]),
        publisher: text(&data["publisher"]["name"]),
        images: images(&data["coverArt"]["sources"]),
        ..Default::default()
    })
}

/// A restricted episode answers without its fields and is left out.
fn episode(data: &Value) -> Option<Episode> {
    if data["__typename"] != "Episode" {
        return None;
    }
    let uri = data["uri"].as_str()?;
    let show = &data["podcastV2"]["data"];
    Some(Episode {
        id: id_of(uri),
        uri: uri.to_string(),
        name: text(&data["name"]),
        duration_ms: milliseconds(&data["duration"]["totalMilliseconds"]),
        description: text(&data["description"]),
        images: images(&data["coverArt"]["sources"]),
        release_date: data["releaseDate"]["isoString"].as_str().map(str::to_string),
        explicit: data["contentRating"]["label"] == "EXPLICIT",
        show: show["uri"].as_str().map(|show_uri| Show {
            id: id_of(show_uri),
            uri: show_uri.to_string(),
            name: text(&show["name"]),
            images: images(&show["coverArt"]["sources"]),
            ..Default::default()
        }),
        ..Default::default()
    })
}

fn artist_refs(items: &Value) -> Vec<ArtistRef> {
    items
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|artist| {
            let uri = artist["uri"].as_str()?;
            Some(ArtistRef {
                id: Some(id_of(uri)),
                name: text(&artist["profile"]["name"]),
                uri: Some(uri.to_string()),
            })
        })
        .collect()
}

fn images(sources: &Value) -> Vec<Image> {
    sources
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|source| {
            Some(Image {
                url: source["url"].as_str()?.to_string(),
                width: source["width"].as_u64().and_then(|n| u32::try_from(n).ok()),
                height: source["height"].as_u64().and_then(|n| u32::try_from(n).ok()),
            })
        })
        .collect()
}

fn id_of(uri: &str) -> String {
    uri.rsplit(':').next().unwrap_or_default().to_string()
}

fn text(value: &Value) -> String {
    value.as_str().unwrap_or_default().to_string()
}

fn milliseconds(value: &Value) -> u32 {
    value
        .as_u64()
        .and_then(|ms| u32::try_from(ms).ok())
        .unwrap_or_default()
}

/// A description the way the Web API writes it: Spotify's mixes carry
/// links as HTML here.
fn plain_text(html: &str) -> String {
    let mut text = String::new();
    let mut in_tag = false;
    for character in html.chars() {
        match character {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => text.push(character),
            _ => {}
        }
    }
    text.replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#x27;", "'")
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answer() -> Value {
        json!({
            "tracksV2": {"totalCount": 399, "items": [{"item": {"data": {
                "__typename": "Track",
                "uri": "spotify:track:3N49gvnpF75KmNEoUROPjW",
                "name": "In Da Party",
                "duration": {"totalMilliseconds": 141250},
                "contentRating": {"label": "EXPLICIT"},
                "playability": {"playable": true},
                "artists": {"items": [{"uri": "spotify:artist:3hGJ4nHdF99Vs0gQdXz5Nw",
                    "profile": {"name": "smokedope2016"}}]},
                "albumOfTrack": {"uri": "spotify:album:30tu5IbFCw2A6y7nsbS4RC",
                    "name": "THE PEAK",
                    "coverArt": {"sources": [{"url": "https://i.scdn.co/image/a", "width": 300, "height": 300}]}}
            }}}]},
            "artists": {"totalCount": 102, "items": [{"data": {
                "__typename": "Artist",
                "uri": "spotify:artist:3hGJ4nHdF99Vs0gQdXz5Nw",
                "profile": {"name": "smokedope2016"},
                "visuals": {"avatarImage": {"sources": [{"url": "https://i.scdn.co/image/b", "width": 640, "height": 640}]}}
            }}]},
            "albumsV2": {"totalCount": 140, "items": [{"data": {
                "__typename": "Album",
                "uri": "spotify:album:30tu5IbFCw2A6y7nsbS4RC",
                "name": "THE PEAK",
                "type": "SINGLE",
                "date": {"year": 2025},
                "coverArt": {"sources": []},
                "artists": {"items": []}
            }}]},
            "playlists": {"totalCount": 201, "items": [{"data": {
                "__typename": "Playlist",
                "uri": "spotify:playlist:37i9dQZF1EIWLH4VTcMWk5",
                "name": "Mix",
                "description": "<a href=spotify:playlist:x>Fairfieldd</a>, Tom &amp; Jerry",
                "images": {"items": [{"sources": [{"url": "https://i.scdn.co/image/c", "width": null, "height": null}]}]},
                "ownerV2": {"data": {"name": "Spotify", "username": "spotify", "uri": "spotify:user:spotify"}}
            }}]},
            "podcasts": {"totalCount": 100, "items": [{"data": {
                "__typename": "Podcast",
                "uri": "spotify:show:2QytGHhXyAONulRF922g6h",
                "name": "WRLD",
                "publisher": {"name": "Iookq"},
                "coverArt": {"sources": []}
            }}]},
            "episodes": {"totalCount": 101, "items": [
                {"data": {"__typename": "RestrictedContent"}},
                {"data": {
                    "__typename": "Episode",
                    "uri": "spotify:episode:abc",
                    "name": "Interview",
                    "description": "Talk",
                    "duration": {"totalMilliseconds": 6289428},
                    "releaseDate": {"isoString": "2026-03-18T19:00:00Z"},
                    "contentRating": {"label": "NONE"},
                    "coverArt": {"sources": []},
                    "podcastV2": {"data": {"uri": "spotify:show:7H9G", "name": "The Gobcast"}}
                }}
            ]}
        })
    }

    #[test]
    fn an_answer_reads_as_web_api_results() {
        let results = results_of(&answer());

        let tracks = results.tracks.unwrap();
        assert_eq!(tracks.total, 399);
        let song = &tracks.items[0];
        assert_eq!(song.id.as_deref(), Some("3N49gvnpF75KmNEoUROPjW"));
        assert_eq!(song.duration_ms, 141_250);
        assert!(song.explicit);
        assert_eq!(song.artists[0].name, "smokedope2016");
        assert_eq!(song.album.as_ref().unwrap().name, "THE PEAK");
        assert_eq!(song.album.as_ref().unwrap().images[0].width, Some(300));

        let artists = results.artists.unwrap();
        assert_eq!(artists.items[0].id, "3hGJ4nHdF99Vs0gQdXz5Nw");
        assert_eq!(artists.items[0].images.len(), 1);

        let album = &results.albums.unwrap().items[0];
        assert_eq!(album.album_type.as_deref(), Some("single"));
        assert_eq!(album.year(), Some("2025"));

        let playlist = &results.playlists.unwrap().items[0];
        assert_eq!(playlist.description.as_deref(), Some("Fairfieldd, Tom & Jerry"));
        assert_eq!(playlist.owner.id.as_deref(), Some("spotify"));
        assert_eq!(playlist.images[0].width, None);

        let show = &results.shows.unwrap().items[0];
        assert_eq!(show.publisher, "Iookq");

        let episodes = results.episodes.unwrap();
        assert_eq!(episodes.total, 101, "the total counts the restricted one");
        assert_eq!(episodes.items.len(), 1, "a restricted episode is left out");
        assert_eq!(episodes.items[0].show.as_ref().unwrap().name, "The Gobcast");
    }

    #[test]
    fn a_kind_the_answer_lacks_is_none() {
        let results = results_of(&json!({"artists": {"items": []}}));
        assert!(results.tracks.is_none());
        assert!(results.artists.unwrap().items.is_empty());
    }

    #[test]
    fn the_hash_is_found_in_the_players_files() {
        let page = r#"<link href="https://open.spotifycdn.com/cdn/build/web-player/web-player.bd717bc2.css"/><script src="https://open.spotifycdn.com/cdn/build/web-player/web-player.dbd3aa01.js">"#;
        assert_eq!(
            script_url(page, "web-player/web-player.").as_deref(),
            Some("https://open.spotifycdn.com/cdn/build/web-player/web-player.dbd3aa01.js")
        );

        let bundle = r#"{4382:"xpui-routes-country-hub-section",4406:"xpui-routes-search",4563:"x"}
            ({4382:"96c42444",4406:"0e343fae",14406:"deadbeef"}) ({4406:"24270399"})"#;
        let id = chunk_id(bundle, SEARCH_CHUNK).unwrap();
        assert_eq!(id, "4406");
        assert_eq!(chunk_file(bundle, &id).as_deref(), Some("0e343fae"));
        assert_eq!(chunk_file(bundle, "9999"), None);

        let chunk = format!(
            r#"("searchArtists","query","{}",null),("searchDesktop","query","{KNOWN_HASH}",null)"#,
            "7".repeat(64)
        );
        assert_eq!(
            operation_hash(&chunk, "searchDesktop").as_deref(),
            Some(KNOWN_HASH)
        );
        assert_eq!(operation_hash(&chunk, "searchNothing"), None);
    }
}
