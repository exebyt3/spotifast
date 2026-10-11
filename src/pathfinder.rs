//! Search, the playlist list, the liked songs, the saved albums, artists and
//! shows, and a show's episodes over the web player's GraphQL endpoint, with
//! the streaming session's own token. Spotify's clients ask there, and it
//! has no per-app quota, so none of them waits on the shared app's rate
//! limit.
//!
//! Each operation is a persisted one, named by a hash that Spotify moves
//! with each web player release. A stale hash is refreshed from the
//! player's own bundle; any other failure is left to the Web API.

use std::collections::BTreeMap;
use std::sync::Mutex;

use librespot_core::Session;
use serde_json::{Value, json};

use crate::http::Http;
use crate::api::models::{
    Album, Artist, ArtistRef, CursorPage, Episode, Image, Owner, Page, Playlist, ResumePoint,
    SavedAlbum, SavedShow, SavedTrack, SearchResults, Show, Track,
};
use crate::session_reads::Failure;

const ENDPOINT: &str = "https://api-partner.spotify.com/pathfinder/v1/query";
const WEB_PLAYER: &str = "https://open.spotify.com/";
/// The web player's route chunk that carries the search operations; the
/// library's are in the main bundle.
const SEARCH_CHUNK: &str = "xpui-routes-search";
/// What the web player sends the desktop page to.
const BROWSER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36";
/// What the shared app's search answers with per kind of result.
const RESULTS_PER_KIND: u32 = 20;
const STALE_QUERY: &str = "PersistedQueryNotFound";

const SEARCH: &str = "searchDesktop";
const SAVED_TRACKS: &str = "fetchLibraryTracks";
const CONTAINS: &str = "areEntitiesInLibrary";
const LIBRARY: &str = "libraryV3";
const SAVE: &str = "addToLibrary";
const UNSAVE: &str = "removeFromLibrary";
const SHOW_EPISODES: &str = "queryPodcastEpisodes";

/// The library's filters, as the web player's own sidebar names them.
const ALBUMS: &str = "Albums";
const ARTISTS: &str = "Artists";
const SHOWS: &str = "Podcasts & Shows";
/// How many followed artists are read at once before the Web API is asked
/// to page them instead.
const FOLLOWED_LIMIT: u32 = 1000;

/// The hashes of the web player build of 2026-10-10, by operation.
const KNOWN_HASHES: [(&str, &str); 7] = [
    (SHOW_EPISODES, "3539d746cf882f3909660de40b4ef472b3f5893a0761d617c282541de5d412c5"),
    (LIBRARY, "390c78e5b951029bad359785e69b07b536a509c581cbcd0aded5e5067f187455"),
    (SEARCH, "eef7cc54888d91bdd6802623477873caa3948ae173a0c34fd86827b267e94c03"),
    (SAVED_TRACKS, "087278b20b743578a6262c2b0b4bcd20d879c503cc359a2285baf083ef944240"),
    (CONTAINS, "134337999233cc6fdd6b1e6dbf94841409f04a946c5c7b744b09ba0dfe5a85ed"),
    (SAVE, "896ebcb47815681340860d121cb5d494e157e2a78d3950385cd54e0393c67148"),
    (UNSAVE, "896ebcb47815681340860d121cb5d494e157e2a78d3950385cd54e0393c67148"),
];

/// The hashes found in the web player once a known one went stale.
static FRESH_HASHES: Mutex<BTreeMap<String, String>> = Mutex::new(BTreeMap::new());

enum Reply {
    Data(Value),
    Stale,
}

/// Every kind of result for `query`, as the Web API's search shapes them.
pub async fn search(
    session: &Session,
    http: &Http,
    query: &str,
) -> Result<SearchResults, Failure> {
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
    let data = run(session, http, SEARCH, variables, false).await?;
    match data.get("searchV2") {
        Some(results) => Ok(results_of(results)),
        None => Err(Failure::Retry(anyhow::anyhow!("search answered no results"))),
    }
}

/// One page of the account's playlists, flattened out of their folders,
/// as the Web API's `/me/playlists` pages them. The answer carries no
/// song count, which the playlist's own page learns when it opens.
pub async fn my_playlists(
    session: &Session,
    http: &Http,
    offset: u32,
    limit: u32,
) -> Result<Page<Playlist>, Failure> {
    let variables = json!({
        "filters": ["Playlists"],
        "order": null,
        "textFilter": "",
        "features": ["LIKED_SONGS", "YOUR_EPISODES"],
        "limit": limit,
        "offset": offset,
        "flatten": true,
        "expandedFolders": [],
        "folderUri": null,
        "includeFoldersWhenFlattening": false,
        "withCuration": false,
    });
    let data = run(session, http, LIBRARY, variables, false).await?;
    playlist_page(&data, offset, limit)
}

fn playlist_page(data: &Value, offset: u32, limit: u32) -> Result<Page<Playlist>, Failure> {
    let library = data
        .pointer("/me/libraryV3")
        .ok_or_else(|| Failure::Retry(anyhow::anyhow!("library answered no playlists")))?;
    let entries = library["items"].as_array().map_or(&[][..], Vec::as_slice);
    // Liked songs and episodes ride along as pseudo playlists; the page's
    // own paging counts them, so the next offset still lines up.
    let items: Vec<Playlist> = entries
        .iter()
        .filter_map(|entry| playlist(&entry["item"]["data"]))
        .collect();
    let seen = offset.saturating_add(entries.len() as u32);
    let total = library["totalCount"]
        .as_u64()
        .and_then(|total| u32::try_from(total).ok())
        .unwrap_or(seen);
    Ok(Page {
        next: (seen < total).then(String::new),
        items,
        total,
        limit,
        offset,
    })
}

/// One page of the account's saved albums, newest first, as the Web API's
/// `/me/albums` pages them.
pub async fn saved_albums(
    session: &Session,
    http: &Http,
    offset: u32,
    limit: u32,
) -> Result<Page<SavedAlbum>, Failure> {
    let (entries, total) = library(session, http, ALBUMS, offset, limit).await?;
    Ok(paged(albums_in(&entries), entries.len(), total, offset, limit))
}

/// The albums among the library's entries; a pre-release or anything else
/// the Web API would not list is left out.
fn albums_in(entries: &[Value]) -> Vec<SavedAlbum> {
    entries
        .iter()
        .filter_map(|entry| {
            Some(SavedAlbum {
                added_at: added_at(entry),
                album: album(&entry["item"]["data"])?,
            })
        })
        .collect()
}

/// One page of the account's saved shows, newest first, as the Web API's
/// `/me/shows` pages them.
pub async fn saved_shows(
    session: &Session,
    http: &Http,
    offset: u32,
    limit: u32,
) -> Result<Page<SavedShow>, Failure> {
    let (entries, total) = library(session, http, SHOWS, offset, limit).await?;
    Ok(paged(shows_in(&entries), entries.len(), total, offset, limit))
}

/// The podcasts among the library's entries; audiobooks have a filter of
/// their own.
fn shows_in(entries: &[Value]) -> Vec<SavedShow> {
    entries
        .iter()
        .filter_map(|entry| {
            Some(SavedShow {
                added_at: added_at(entry),
                show: show(&entry["item"]["data"])?,
            })
        })
        .collect()
}

/// Every artist the account follows, in one answer: the Web API pages them
/// by the last artist's id, which the library's own offsets cannot stand
/// in for. A longer list is the Web API's to page.
pub async fn followed_artists(
    session: &Session,
    http: &Http,
) -> Result<CursorPage<Artist>, Failure> {
    let mut artists = Vec::new();
    let mut offset = 0;
    loop {
        let (entries, total) = library(session, http, ARTISTS, offset, 50).await?;
        if total > FOLLOWED_LIMIT {
            return Err(Failure::Retry(anyhow::anyhow!(
                "{total} followed artists are more than one answer holds"
            )));
        }
        artists.extend(artists_in(&entries));
        offset += entries.len() as u32;
        if entries.is_empty() || offset >= total {
            return Ok(CursorPage {
                total: Some(total),
                items: artists,
                next: None,
                cursors: None,
            });
        }
    }
}

fn artists_in(entries: &[Value]) -> Vec<Artist> {
    entries
        .iter()
        .filter_map(|entry| artist(&entry["item"]["data"]))
        .collect()
}

/// One page of the library's `filter` kind: the entries and how many the
/// library holds. Spotify answers a sort it does not know with an error
/// in place of the library, and the default order is asked for instead.
async fn library(
    session: &Session,
    http: &Http,
    filter: &str,
    offset: u32,
    limit: u32,
) -> Result<(Vec<Value>, u32), Failure> {
    let mut data = Value::Null;
    for order in [json!("Recently Added"), Value::Null] {
        let variables = json!({
            "filters": [filter],
            "order": order,
            "textFilter": "",
            "features": ["LIKED_SONGS", "YOUR_EPISODES"],
            "limit": limit,
            "offset": offset,
            "flatten": true,
            "expandedFolders": [],
            "folderUri": null,
            "includeFoldersWhenFlattening": false,
            "withCuration": false,
        });
        data = run(session, http, LIBRARY, variables, false).await?;
        let kind = data["me"]["libraryV3"]["__typename"].as_str().unwrap_or_default();
        if !kind.ends_with("Error") {
            break;
        }
        data = Value::Null;
    }
    let library = data
        .pointer("/me/libraryV3")
        .ok_or_else(|| Failure::Retry(anyhow::anyhow!("library answered no {filter}")))?;
    let entries = library["items"].as_array().cloned().unwrap_or_default();
    let total = library["totalCount"]
        .as_u64()
        .and_then(|total| u32::try_from(total).ok())
        .unwrap_or(offset.saturating_add(entries.len() as u32));
    Ok((entries, total))
}

fn added_at(entry: &Value) -> Option<String> {
    entry["addedAt"]["isoString"].as_str().map(str::to_string)
}

/// A page of `items` read from `seen` entries of a library that holds
/// `total`; the next page is there while entries remain.
fn paged<T>(items: Vec<T>, seen: usize, total: u32, offset: u32, limit: u32) -> Page<T> {
    let reached = offset.saturating_add(seen as u32);
    Page {
        next: (reached < total && seen > 0).then(String::new),
        items,
        total,
        limit,
        offset,
    }
}

/// One page of a show's episodes, newest first as the show's page lists
/// them, each with how far the account has listened to it.
pub async fn show_episodes(
    session: &Session,
    http: &Http,
    id: &str,
    offset: u32,
    limit: u32,
) -> Result<Page<Episode>, Failure> {
    let variables = json!({
        "uri": format!("spotify:show:{id}"),
        "offset": offset,
        "limit": limit,
        "includeEpisodeContentRatingsV2": false,
    });
    let data = run(session, http, SHOW_EPISODES, variables, false).await?;
    episodes_page(&data, offset, limit)
}

fn episodes_page(data: &Value, offset: u32, limit: u32) -> Result<Page<Episode>, Failure> {
    let retry = |reason: &str| Failure::Retry(anyhow::anyhow!("show episodes: {reason}"));
    let page = data
        .pointer("/podcastUnionV2/episodesV2")
        .filter(|page| page["__typename"] == "ContextEpisodePage")
        .ok_or_else(|| retry("no page"))?;
    let entries = page["items"].as_array().ok_or_else(|| retry("no items"))?;
    // A row left out would move every one after it, and the page's
    // numbering with it.
    let items = entries
        .iter()
        .map(|entry| episode(&entry["entity"]["data"]).ok_or_else(|| retry("unreadable row")))
        .collect::<Result<Vec<_>, _>>()?;
    let total = page["totalCount"]
        .as_u64()
        .and_then(|total| u32::try_from(total).ok())
        .unwrap_or(offset.saturating_add(items.len() as u32));
    Ok(Page {
        next: page["pagingInfo"]["nextOffset"]
            .as_u64()
            .is_some()
            .then(String::new),
        items,
        total,
        limit,
        offset,
    })
}

/// One page of the account's liked songs, newest first, as the Web API's
/// saved tracks page.
pub async fn saved_tracks(
    session: &Session,
    http: &Http,
    offset: u32,
    limit: u32,
) -> Result<Page<SavedTrack>, Failure> {
    let variables = json!({"offset": offset, "limit": limit});
    let data = run(session, http, SAVED_TRACKS, variables, false).await?;
    saved_page(&data, offset, limit)
}

fn saved_page(data: &Value, offset: u32, limit: u32) -> Result<Page<SavedTrack>, Failure> {
    let tracks = data
        .pointer("/me/library/tracks")
        .ok_or_else(|| Failure::Retry(anyhow::anyhow!("library answered no tracks")))?;
    let items: Vec<SavedTrack> = tracks["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            // A song Spotify no longer lists answers without its fields.
            let uri = entry["track"]["_uri"].as_str()?;
            Some(SavedTrack {
                added_at: entry["addedAt"]["isoString"].as_str().map(str::to_string),
                track: track_of(uri, &entry["track"]["data"])?,
            })
        })
        .collect();
    let shown = offset.saturating_add(items.len() as u32);
    let total = tracks["totalCount"]
        .as_u64()
        .and_then(|total| u32::try_from(total).ok())
        .unwrap_or(shown);
    Ok(Page {
        next: (shown < total).then(String::new),
        items,
        total,
        limit,
        offset,
    })
}

/// Whether each of `uris` is in the library, in the same order. Songs,
/// albums, shows and episodes only: the answer for any other kind is the
/// Web API's.
pub async fn contains(
    session: &Session,
    http: &Http,
    uris: &[String],
) -> Result<Vec<bool>, Failure> {
    let data = run(session, http, CONTAINS, json!({"uris": uris}), false).await?;
    saved_flags(&data, uris.len())
}

/// The `saved` flag of each of `count` entries, in the order asked.
fn saved_flags(data: &Value, count: usize) -> Result<Vec<bool>, Failure> {
    match data["lookup"].as_array() {
        Some(lookup) if lookup.len() == count => Ok(lookup
            .iter()
            .map(|entry| entry["data"]["saved"].as_bool().unwrap_or(false))
            .collect()),
        other => Err(Failure::Retry(anyhow::anyhow!(
            "library answered {} entries for {count} uris",
            other.map_or(0, Vec::len),
        ))),
    }
}

/// Adds `uris` to the library, or takes them out.
pub async fn set_saved(
    session: &Session,
    http: &Http,
    uris: &[String],
    saved: bool,
) -> Result<(), Failure> {
    let operation = if saved { SAVE } else { UNSAVE };
    let variables = json!({"libraryItemUris": uris});
    run(session, http, operation, variables, true).await?;
    Ok(())
}

/// Runs a persisted operation and returns its `data`. A stale hash is
/// refreshed from the web player and the operation asked once more.
async fn run(
    session: &Session,
    http: &Http,
    operation: &str,
    variables: Value,
    write: bool,
) -> Result<Value, Failure> {
    let known = current_hash(operation)
        .ok_or_else(|| Failure::Retry(anyhow::anyhow!("no hash for {operation}")))?;
    if let Reply::Data(data) = fetch(session, operation, &variables, &known, write).await? {
        return Ok(data);
    }
    let fresh = scrape_hash(http, operation).await?;
    FRESH_HASHES
        .lock()
        .unwrap_or_else(|lock| lock.into_inner())
        .insert(operation.to_string(), fresh.clone());
    match fetch(session, operation, &variables, &fresh, write).await? {
        Reply::Data(data) => Ok(data),
        Reply::Stale => Err(Failure::Retry(anyhow::anyhow!("{operation} hash refused"))),
    }
}

fn current_hash(operation: &str) -> Option<String> {
    let fresh = FRESH_HASHES
        .lock()
        .unwrap_or_else(|lock| lock.into_inner())
        .get(operation)
        .cloned();
    fresh.or_else(|| {
        KNOWN_HASHES
            .iter()
            .find(|(name, _)| *name == operation)
            .map(|(_, hash)| hash.to_string())
    })
}

async fn fetch(
    session: &Session,
    operation: &str,
    variables: &Value,
    hash: &str,
    write: bool,
) -> Result<Reply, Failure> {
    let token = session.login5().auth_token().await?.access_token;
    let client_token = session.spclient().client_token().await?;
    let extensions = json!({"persistedQuery": {"version": 1, "sha256Hash": hash}});
    let builder = http::Request::builder()
        .header("Authorization", format!("Bearer {token}"))
        .header("client-token", client_token)
        .header("Accept", "application/json")
        .header("App-Platform", "Win32_x86_64");
    let request = if write {
        let body = json!({
            "operationName": operation,
            "variables": variables,
            "extensions": extensions,
        });
        builder
            .method("POST")
            .uri(ENDPOINT)
            .header("Content-Type", "application/json;charset=UTF-8")
            .body(body.to_string().into_bytes().into())
    } else {
        let url = format!(
            "{ENDPOINT}?operationName={operation}&variables={}&extensions={}",
            urlencoding::encode(&variables.to_string()),
            urlencoding::encode(&extensions.to_string()),
        );
        builder.uri(url).body(Default::default())
    }
    .map_err(|error| Failure::Retry(error.into()))?;
    let body = session
        .http_client()
        .request_body(request)
        .await
        .map_err(|error| Failure::Retry(anyhow::anyhow!("{operation} request: {error}")))?;
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
            Err(Failure::Retry(anyhow::anyhow!("{operation} refused: {errors:?}")))
        };
    }
    match value.get("data") {
        Some(data) if data.is_object() => Ok(Reply::Data(data.clone())),
        _ => Err(Failure::Retry(anyhow::anyhow!("{operation} answered no data"))),
    }
}

/// The current hash of `operation`: the player's page names its main
/// bundle, which carries the library's operations, and maps route names to
/// the chunk files that carry the rest, search among them.
async fn scrape_hash(http: &Http, operation: &str) -> Result<String, Failure> {
    let retry = |reason: &str| Failure::Retry(anyhow::anyhow!("web player: {reason}"));
    let page = download(http, WEB_PLAYER).await?;
    let bundle_url = script_url(&page, "web-player/web-player.").ok_or_else(|| {
        let head: String = page.chars().take(160).collect();
        retry(&format!("no main bundle in {} bytes: {head}", page.len()))
    })?;
    let bundle = download(http, &bundle_url).await?;
    if let Some(hash) = operation_hash(&bundle, operation) {
        return Ok(hash);
    }
    let id = chunk_id(&bundle, SEARCH_CHUNK).ok_or_else(|| retry("no search chunk"))?;
    let file = chunk_file(&bundle, &id).ok_or_else(|| retry("no search chunk file"))?;
    let base = bundle_url
        .rsplit_once("web-player.")
        .map(|(base, _)| base.to_string())
        .ok_or_else(|| retry("odd bundle address"))?;
    let chunk = download(http, &format!("{base}{SEARCH_CHUNK}.{file}.js")).await?;
    operation_hash(&chunk, operation).ok_or_else(|| retry("no such operation"))
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
fn operation_hash(source: &str, operation: &str) -> Option<String> {
    ["query", "mutation"].iter().find_map(|kind| {
        let head = format!("\"{operation}\",\"{kind}\",\"");
        let start = source.find(&head)? + head.len();
        let hash = source.get(start..start + 64)?;
        hash.chars()
            .all(|c| c.is_ascii_hexdigit())
            .then(|| hash.to_string())
    })
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
    track_of(data["uri"].as_str()?, data)
}

/// A song under `uri`; the library's rows name it beside the details.
fn track_of(uri: &str, data: &Value) -> Option<Track> {
    if data["__typename"] != "Track" {
        return None;
    }
    let album = &data["albumOfTrack"];
    let number = |value: &Value| value.as_u64().and_then(|n| u32::try_from(n).ok());
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
        track_number: number(&data["trackNumber"]),
        disc_number: number(&data["discNumber"]),
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
        snapshot_id: data["revisionId"].as_str().map(str::to_string),
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
        resume_point: data["playedState"].is_object().then(|| ResumePoint {
            fully_played: data["playedState"]["state"] == "COMPLETED",
            resume_position_ms: milliseconds(&data["playedState"]["playPositionMilliseconds"]),
        }),
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
    fn liked_songs_read_as_a_web_api_page() {
        let data = json!({"me": {"library": {"tracks": {
            "totalCount": 10407,
            "items": [
                {"addedAt": {"isoString": "2026-10-10T10:12:40Z"},
                 "track": {"_uri": "spotify:track:1nBxszQgD8kHACCWMFEm5Z", "data": {
                    "__typename": "Track", "name": "Mode", "trackNumber": 1, "discNumber": 1,
                    "duration": {"totalMilliseconds": 188476},
                    "contentRating": {"label": "EXPLICIT"},
                    "artists": {"items": [{"uri": "spotify:artist:6a4D", "profile": {"name": "MBP Mayo"}}]},
                    "albumOfTrack": {"uri": "spotify:album:4JRq", "name": "Mode", "coverArt": {"sources": []}}
                 }}},
                {"addedAt": {"isoString": "2026-10-09T10:00:00Z"},
                 "track": {"_uri": "spotify:track:gone", "data": {"__typename": "NotFound"}}}
            ]
        }}}});
        let page = saved_page(&data, 100, 50).unwrap();
        assert_eq!(page.total, 10407);
        assert_eq!(page.offset, 100);
        assert_eq!(page.items.len(), 1, "a song Spotify no longer lists is left out");
        assert!(page.next.is_some(), "more remain");
        assert_eq!(
            page.items[0].added_at.as_deref(),
            Some("2026-10-10T10:12:40Z")
        );
        let song = &page.items[0].track;
        assert_eq!(song.uri, "spotify:track:1nBxszQgD8kHACCWMFEm5Z");
        assert_eq!(song.id.as_deref(), Some("1nBxszQgD8kHACCWMFEm5Z"));
        assert_eq!(song.track_number, Some(1));
        assert!(song.explicit);
        assert_eq!(song.artists[0].name, "MBP Mayo");
        assert!(saved_page(&json!({}), 0, 50).is_err());
    }

    #[test]
    fn the_playlist_list_skips_the_pseudo_playlists() {
        let data = json!({"me": {"libraryV3": {"totalCount": 132, "items": [
            {"item": {"__typename": "LibraryPseudoPlaylistResponseWrapper",
                      "data": {"__typename": "LibraryPseudoPlaylist", "name": "Liked Songs"}}},
            {"item": {"__typename": "PlaylistResponseWrapper", "data": {
                "__typename": "Playlist",
                "uri": "spotify:playlist:1IkRJXxPpLZPe4gLIdgnM1",
                "name": "favorites 2",
                "description": "",
                "revisionId": "AAACRDslneBYy+8TlromPsCXwRol9/4g",
                "images": {"items": [{"sources": [{"url": "https://mosaic.scdn.co/640/x", "width": 640, "height": 640}]}]},
                "ownerV2": {"data": {"id": "mbtoq9", "username": "mbtoq9", "name": "exebyte.", "uri": "spotify:user:mbtoq9"}}
            }}}
        ]}}});
        let page = playlist_page(&data, 50, 50).unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.total, 132);
        assert_eq!(page.offset, 50);
        assert!(page.next.is_some(), "52 of 132 seen");
        let list = &page.items[0];
        assert_eq!(list.id, "1IkRJXxPpLZPe4gLIdgnM1");
        assert_eq!(list.snapshot_id.as_deref(), Some("AAACRDslneBYy+8TlromPsCXwRol9/4g"));
        assert_eq!(list.owner.display_name.as_deref(), Some("exebyte."));
        assert_eq!(list.description, None);
        assert!(list.images[0].url.starts_with("https://mosaic.scdn.co/"));
        assert!(playlist_page(&json!({}), 0, 50).is_err());
    }

    #[test]
    fn membership_answers_in_the_order_asked() {
        let data = json!({"lookup": [
            {"data": {"__typename": "Track", "saved": true}},
            {"data": {"__typename": "Album", "saved": false}},
        ]});
        assert_eq!(saved_flags(&data, 2).unwrap(), vec![true, false]);
        assert!(saved_flags(&data, 3).is_err(), "a short answer is not trusted");
        assert!(saved_flags(&json!({}), 1).is_err());
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
            r#"("searchArtists","query","{}",null),("searchDesktop","query","{hash}",null),("addToLibrary","mutation","{hash}",null)"#,
            "7".repeat(64),
            hash = KNOWN_HASHES[0].1,
        );
        assert_eq!(
            operation_hash(&chunk, "searchDesktop").as_deref(),
            Some(KNOWN_HASHES[0].1)
        );
        assert_eq!(
            operation_hash(&chunk, "addToLibrary").as_deref(),
            Some(KNOWN_HASHES[0].1),
            "a mutation is named the same way"
        );
        assert_eq!(operation_hash(&chunk, "searchNothing"), None);
    }

    fn library_entry(kind: &str, data: Value) -> Value {
        json!({
            "addedAt": {"isoString": "2026-10-01T10:00:00Z"},
            "item": {"__typename": kind, "data": data},
        })
    }

    #[test]
    fn saved_albums_read_with_when_they_were_added() {
        let entries = vec![
            library_entry(
                "AlbumResponseWrapper",
                json!({
                    "__typename": "Album",
                    "uri": "spotify:album:AAA",
                    "name": "One",
                    "type": "EP",
                    "artists": {"items": [{"uri": "spotify:artist:XXX", "profile": {"name": "Band"}}]},
                    "coverArt": {"sources": [{"url": "https://i.scdn.co/image/a", "width": 300, "height": 300}]},
                    "date": {"year": 2020},
                }),
            ),
            library_entry("PreReleaseResponseWrapper", json!({"__typename": "PreRelease"})),
        ];
        let albums = albums_in(&entries);
        assert_eq!(albums.len(), 1, "a pre-release is not in the Web API's list");
        assert_eq!(albums[0].added_at.as_deref(), Some("2026-10-01T10:00:00Z"));
        let album = &albums[0].album;
        assert_eq!((album.id.as_str(), album.name.as_str()), ("AAA", "One"));
        assert_eq!(album.album_type.as_deref(), Some("single"), "an EP is a single");
        assert_eq!(album.artists[0].name, "Band");
        assert_eq!(album.release_date.as_deref(), Some("2020"));
    }

    #[test]
    fn saved_shows_and_followed_artists_read_by_their_kind() {
        let shows = shows_in(&[
            library_entry(
                "PodcastResponseWrapper",
                json!({
                    "__typename": "Podcast",
                    "uri": "spotify:show:SSS",
                    "name": "Talk",
                    "publisher": {"name": "Radio"},
                    "coverArt": {"sources": []},
                }),
            ),
            library_entry("AudiobookResponseWrapper", json!({"__typename": "Audiobook"})),
        ]);
        assert_eq!(shows.len(), 1);
        assert_eq!(shows[0].show.publisher, "Radio");

        let artists = artists_in(&[library_entry(
            "ArtistResponseWrapper",
            json!({
                "__typename": "Artist",
                "uri": "spotify:artist:RRR",
                "profile": {"name": "Singer"},
                "visuals": {"avatarImage": {"sources": [{"url": "https://i.scdn.co/image/r"}]}},
            }),
        )]);
        assert_eq!(artists[0].name, "Singer");
        assert_eq!(artists[0].images.len(), 1);
    }

    #[test]
    fn a_page_goes_on_while_entries_remain() {
        let page = paged(vec![1, 2], 2, 5, 0, 2);
        assert!(page.next.is_some());
        let last = paged(vec![5], 1, 5, 4, 2);
        assert!(last.next.is_none());
        let empty: Page<u8> = paged(Vec::new(), 0, 9, 4, 2);
        assert!(empty.next.is_none(), "an empty answer never loops");
    }

    fn episode_entry(name: &str, state: Value) -> Value {
        json!({"entity": {"data": {
            "__typename": "Episode",
            "uri": format!("spotify:episode:{name}"),
            "name": name,
            "description": "d",
            "duration": {"totalMilliseconds": 60_000},
            "releaseDate": {"isoString": "2026-10-01T00:00:00Z"},
            "coverArt": {"sources": []},
            "playedState": state,
        }}})
    }

    #[test]
    fn show_episodes_carry_how_far_they_were_heard() {
        let data = json!({"podcastUnionV2": {"episodesV2": {
            "__typename": "ContextEpisodePage",
            "totalCount": 3,
            "pagingInfo": {"nextOffset": 2},
            "items": [
                episode_entry("a", json!({"state": "IN_PROGRESS", "playPositionMilliseconds": 15_000})),
                episode_entry("b", json!({"state": "COMPLETED", "playPositionMilliseconds": 60_000})),
            ],
        }}});
        let page = episodes_page(&data, 0, 2).unwrap();
        assert_eq!(page.total, 3);
        assert!(page.next.is_some());
        let first = page.items[0].resume_point.as_ref().unwrap();
        assert_eq!((first.fully_played, first.resume_position_ms), (false, 15_000));
        assert!(page.items[1].resume_point.as_ref().unwrap().fully_played);
    }

    #[test]
    fn a_show_page_with_an_unreadable_row_is_left_to_the_web_api() {
        let data = json!({"podcastUnionV2": {"episodesV2": {
            "__typename": "ContextEpisodePage",
            "totalCount": 2,
            "pagingInfo": {"nextOffset": null},
            "items": [
                episode_entry("a", Value::Null),
                {"entity": {"data": {"__typename": "NotFound"}}},
            ],
        }}});
        assert!(episodes_page(&data, 0, 2).is_err());
        assert!(episodes_page(&json!({}), 0, 2).is_err());
    }
}
