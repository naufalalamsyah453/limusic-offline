//! Offline downloads: resolve a track to its audio stream, pull the whole file to disk, and keep a
//! record of it so the Downloads page can list, play and delete it.
//!
//! **Why this exists.** Limusic streams everything straight from googlevideo. Nothing survives a
//! dropped connection or a flight. This module adds the one feature a music player is expected to
//! have: keep a copy on disk.
//!
//! **How the bytes are fetched** mirrors [`crate::audioproxy`] exactly, and for the same reason:
//! googlevideo throttles an open-ended streaming GET to roughly 2x realtime, but a *bounded* range
//! is served at full speed. So a download is a loop of 8 MiB `Range:` requests appended straight to
//! the output file — a 60 MB track lands in seconds instead of ~30 minutes. The resolved URL is
//! short-lived, so it is re-resolved from [`AppState::resolve`] whenever the CDN starts refusing.
//!
//! **What lands on disk.** The audio container exactly as googlevideo serves it (WebM/Opus or
//! M4A/AAC); nothing is transcoded, because mpv plays both. Tags (title, artist, album) and the
//! cover art are written with `lofty` so the file is also usable outside Limusic. The natural
//! filename is `Artist - Title.ext`, sanitised for the filesystem.
//!
//! **Where it lands.** A `Downloads` folder under the app's music directory, or the user's chosen
//! folder (`download_dir` setting). Each download is one row in `downloads`, keyed by videoId, so
//! the page lists them without walking the disk.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use futures_util::StreamExt;
use tauri::{AppHandle, Emitter, Manager};

use crate::state::AppState;

/// One bounded upstream request. Large enough to amortise the round trip and stay in the
/// full-speed regime, small enough that a cancelled download abandons little work.
const CHUNK: u64 = 8 * 1024 * 1024;

/// How many times a download re-resolves after the CDN starts refusing ranges. A googlevideo URL
/// that dies mid-download (403 on a later range) is the ordinary case this covers.
const MAX_RERESOLVES: u32 = 3;

/// The setting that overrides where downloads go. Empty/absent → `downloads_dir()` default.
const DIR_SETTING: &str = "download_dir";

/// A row in the Downloads page: what was downloaded and where it lives now.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Download {
    pub video_id: String,
    pub title: String,
    pub artists: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thumbnail: Option<String>,
    pub path: String,
    /// Bytes on disk, read back at list time so a deleted-out-of-band file reads as `0`.
    pub size: u64,
    pub created_at: i64,
    /// The track's length, seconds. `None` when YouTube did not say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_secs: Option<i64>,
}

/// Live state of one in-flight download, mirrored to the UI on every `download-progress` event.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadProgress {
    pub video_id: String,
    pub title: String,
    /// Bytes written so far, and the total the first upstream response advertised (0 if unknown).
    pub downloaded: u64,
    pub total: u64,
    /// `downloading` | `done` | `failed` | `cancelled`.
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Bytes downloaded so far per videoId, so `download_progress` can answer a page that opened
/// mid-download. Finished/failed entries are dropped once the UI has been told.
static PROGRESS: OnceLock<Mutex<HashMap<String, DownloadProgress>>> = OnceLock::new();

fn progress_map() -> &'static Mutex<HashMap<String, DownloadProgress>> {
    PROGRESS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn emit(app: &AppHandle, p: DownloadProgress) {
    if let Ok(mut map) = progress_map().lock() {
        map.insert(p.video_id.clone(), p.clone());
    }
    let _ = app.emit("download-progress", p);
}

// --- paths --------------------------------------------------------------------------------------

/// The folder downloads land in: the `download_dir` setting when it is a usable absolute path, else
/// `<app data dir>/downloads`. Created on demand.
pub fn downloads_dir(app: &AppHandle, db: &crate::db::Db) -> PathBuf {
    if let Some(dir) = db.get_setting(DIR_SETTING).filter(|d| !d.trim().is_empty()) {
        return PathBuf::from(dir);
    }
    app.path().app_data_dir().unwrap_or_else(|_| std::env::temp_dir()).join("downloads")
}

/// `Artist - Title.ext`, every filesystem-hostile run of characters collapsed to a single space.
/// Falls back to the videoId when the metadata is empty, so a file always has a name.
fn file_name(title: &str, artists: &str, video_id: &str, ext: &str) -> String {
    let sanitise = |s: &str| -> String {
        let mut out = String::with_capacity(s.len());
        let mut last_space = false;
        for c in s.chars() {
            let bad =
                matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') || c.is_control();
            let c = if bad { ' ' } else { c };
            if c == ' ' {
                if !last_space {
                    out.push(c);
                }
                last_space = true;
            } else {
                out.push(c);
                last_space = false;
            }
        }
        out.trim().trim_end_matches('.').to_string()
    };
    let a = sanitise(artists);
    let t = sanitise(title);
    let stem = match (a.is_empty(), t.is_empty()) {
        (false, false) => format!("{a} - {t}"),
        (true, false) => t,
        (false, true) => a,
        (true, true) => sanitise(video_id),
    };
    // Some filesystems cap a component near 255 bytes; leave room for " - 1.mp3".
    let stem: String = stem.chars().take(180).collect();
    format!("{stem}.{ext}")
}

/// A filename in `dir` that no other file occupies, suffixing ` - 1`, ` - 2`, … before the
/// extension. Prevents a second download of the same track from clobbering the first.
fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let path = dir.join(name);
    if !path.exists() {
        return path;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) => (s.to_string(), format!(".{e}")),
        None => (name.to_string(), String::new()),
    };
    for n in 1..10_000 {
        let candidate = dir.join(format!("{stem} - {n}{ext}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    path
}

/// Extension for a download, from the content type the CDN reports, else the itag (m4a for the
/// AAC-only itags, else webm). mpv plays either; this is about a sane name on disk.
fn extension(content_type: Option<&str>, itag: i64) -> &'static str {
    match content_type {
        Some(ct) if ct.contains("mp4") || ct.contains("m4a") || ct.contains("aac") => "m4a",
        Some(ct) if ct.contains("webm") || ct.contains("opus") => "webm",
        Some(ct) if ct.contains("mpeg") => "mp3",
        _ => match itag {
            139 | 140 | 141 | 256 | 258 => "m4a",
            _ => "webm",
        },
    }
}

// --- fetching -----------------------------------------------------------------------------------

/// A googlevideo URL that went stale mid-download. Distinct from a disk error only so the caller
/// knows a re-resolve and resume is the right recovery rather than a hard failure.
struct Refused;

/// Ask upstream for one bounded range. `Range: bytes=start-end`, plus the client headers the URL
/// was resolved under. Returns the body stream and the total size when the response carries it.
async fn range_get(
    url: &str,
    headers: &HashMap<String, String>,
    start: u64,
    end: u64,
) -> Result<(reqwest::Response, Option<u64>, Option<String>), String> {
    let mut req = crate::http::client()
        .get(url)
        .header("Range", format!("bytes={start}-{end}"))
        .header("Accept-Encoding", "identity");
    for (k, v) in headers {
        req = req.header(k, v);
    }
    let resp = req.send().await.map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("upstream answered HTTP {}", resp.status()));
    }
    let total = resp
        .headers()
        .get("content-range")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.rsplit('/').next())
        .and_then(|s| s.parse::<u64>().ok());
    let content_type =
        resp.headers().get("content-type").and_then(|v| v.to_str().ok()).map(str::to_owned);
    Ok((resp, total, content_type))
}

/// Pull the whole stream to `path`, appending bounded ranges and reporting progress. Returns the
/// bytes written and the total the CDN advertised (first time it says). `Err(Refused)` means the
/// URL went stale on a later range: the caller resolves a fresh one and calls this again with the
/// bytes already on disk, so a download resumes rather than restarts.
async fn fetch_to_file(
    app: &AppHandle,
    video_id: &str,
    title: &str,
    url: &str,
    headers: &HashMap<String, String>,
    path: &Path,
    already: u64,
    known_total: Option<u64>,
) -> Result<(u64, Option<u64>, Option<String>), Refused> {
    use std::io::Write;

    let mut file =
        std::fs::OpenOptions::new().create(true).append(true).open(path).map_err(|_| Refused)?;
    let mut done = already;
    let mut total = known_total;
    let mut content_type: Option<String> = None;
    // How large an upstream request we assume the server will honour. googlevideo caps some URLs
    // near the start; a short body there means "the file ends here" for a bounded range.
    loop {
        let end = done + CHUNK - 1;
        let (resp, resp_total, ct) = match range_get(url, headers, done, end).await {
            Ok(v) => v,
            // A refusal here is the URL having gone stale, not the disk: let the caller resolve
            // again and resume from `done`.
            Err(_) => return Err(Refused),
        };
        if let Some(t) = resp_total {
            total = Some(t);
        }
        if content_type.is_none() {
            content_type = ct;
        }
        let mut body = resp.bytes_stream();
        let mut wrote_any = false;
        while let Some(chunk) = body.next().await {
            let chunk = chunk.map_err(|_| Refused)?;
            if chunk.is_empty() {
                continue;
            }
            wrote_any = true;
            let n = chunk.len() as u64;
            file.write_all(&chunk).map_err(|_| Refused)?;
            done += n;
            emit(
                app,
                DownloadProgress {
                    video_id: video_id.to_owned(),
                    title: title.to_owned(),
                    downloaded: done,
                    total: total.unwrap_or(0),
                    state: "downloading".into(),
                    error: None,
                },
            );
        }
        let _ = file.flush();
        // The end of the file: either the total is known and reached, or this last range came back
        // short of a full chunk (a bounded request that asked for more than the file holds).
        let reached_total = total.is_some_and(|t| done >= t);
        let short = wrote_any && (done < end + 1);
        if reached_total || short || !wrote_any {
            break;
        }
    }
    let _ = file.flush();
    Ok((done, total, content_type))
}

// --- tagging ------------------------------------------------------------------------------------

/// Write title/artist/album tags and (best effort) the cover art into the finished file. Anything
/// lofty cannot parse is left alone: a download must never fail because tagging did.
fn tag(path: &Path, d: &Download, cover: Option<&[u8]>) {
    use lofty::config::WriteOptions;
    use lofty::file::TaggedFileExt;
    use lofty::probe::Probe;
    use lofty::tag::{Accessor, Tag, TagExt, TagType};

    let Ok(mut tagged) = Probe::open(path).and_then(|p| p.read()) else {
        return;
    };
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else { return };
    let tag_type = match ext {
        "m4a" => TagType::Mp4Ilst,
        "mp3" => TagType::Id3v2,
        _ => TagType::VorbisComments,
    };
    let mut tag = tagged.primary_tag_mut().cloned().unwrap_or_else(|| Tag::new(tag_type));
    tag.set_title(d.title.clone());
    tag.set_artist(d.artists.clone());
    if let Some(album) = d.album.as_deref().filter(|a| !a.is_empty()) {
        tag.set_album(album.to_owned());
    }
    if let Some(cover) = cover {
        if let Ok(picture) = lofty::picture::Picture::from_reader(&mut &cover[..]) {
            tag.push_picture(picture);
        }
    }
    let _ = tag.save_to_path(path, WriteOptions::default());
}

/// Download the cover thumbnail so it can be embedded. Best effort: a missing image just means an
/// untagged picture, never a failed download.
async fn fetch_cover(url: Option<&str>) -> Option<Vec<u8>> {
    let url = url?;
    let resp = crate::http::client().get(url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let bytes = resp.bytes().await.ok()?;
    // Guard against an error page served as 200.
    if bytes.len() < 512 || bytes.len() > 5 * 1024 * 1024 {
        return None;
    }
    Some(bytes.to_vec())
}

// --- the public entry point ---------------------------------------------------------------------

/// Download one track end-to-end: resolve, fetch, tag, record. `song` carries the metadata the UI
/// already has (title/artist/album/thumbnail), so the file is named and tagged even when the
/// resolve reply is thin. Emits `download-progress` throughout and returns the stored row.
pub async fn download(
    state: Arc<AppState>,
    song: crate::state::DownloadSeed,
) -> Result<Download, String> {
    let app = state.app.clone();
    let video_id = song.video_id.clone();
    let title = song.title.clone();

    emit(
        &app,
        DownloadProgress {
            video_id: video_id.clone(),
            title: title.clone(),
            downloaded: 0,
            total: 0,
            state: "downloading".into(),
            error: None,
        },
    );

    match download_inner(&state, &song).await {
        Ok(d) => {
            state.db.put_download(&d);
            emit(
                &app,
                DownloadProgress {
                    video_id,
                    title,
                    downloaded: d.size,
                    total: d.size,
                    state: "done".into(),
                    error: None,
                },
            );
            Ok(d)
        }
        Err(e) => {
            emit(
                &app,
                DownloadProgress {
                    video_id,
                    title,
                    downloaded: 0,
                    total: 0,
                    state: "failed".into(),
                    error: Some(e.clone()),
                },
            );
            Err(e)
        }
    }
}

async fn download_inner(
    state: &Arc<AppState>,
    song: &crate::state::DownloadSeed,
) -> Result<Download, String> {
    let video_id = &song.video_id;
    if crate::local::is_local_song(video_id) {
        return Err("this file is already on your disk".into());
    }

    let dir = downloads_dir(&state.app, &state.db);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;

    let duration_secs = song.duration_secs.unwrap_or(0);

    // Temp path: the real name depends on the extension, which depends on the content type, which
    // is only known after the first response. So write to a fixed temp file, then move it.
    let tmp = dir.join(format!(".{video_id}.part"));
    let _ = std::fs::remove_file(&tmp);

    let mut done: u64 = 0;
    let mut total: Option<u64> = None;
    let mut itag: i64 = 0;
    let mut content_type: Option<String> = None;
    let mut attempts = 0u32;

    loop {
        let data = state
            .resolve(video_id, song.is_upload, duration_secs)
            .await
            .map_err(|e| e.to_string())?;
        if itag == 0 {
            itag = data.itag;
        }
        match fetch_to_file(
            &state.app,
            video_id,
            &song.title,
            &data.stream_url,
            &data.headers,
            &tmp,
            done,
            total,
        )
        .await
        {
            Ok((n, t, ct)) => {
                done = n;
                if total.is_none() {
                    total = t;
                }
                if content_type.is_none() {
                    content_type = ct;
                }
                break;
            }
            Err(Refused) => {
                attempts += 1;
                if attempts > MAX_RERESOLVES {
                    let _ = std::fs::remove_file(&tmp);
                    return Err("the stream kept expiring before the download finished".into());
                }
                // The URL is dead; drop the cached one so the next iteration resolves fresh.
                state.forget_video_url(video_id);
            }
        }
    }

    if done == 0 {
        let _ = std::fs::remove_file(&tmp);
        return Err("the stream delivered no audio".into());
    }

    let ext = extension(content_type.as_deref(), itag);
    let name = file_name(&song.title, &song.artists, video_id, ext);
    let final_path = unique_path(&dir, &name);
    std::fs::rename(&tmp, &final_path).map_err(|e| e.to_string())?;

    let cover = fetch_cover(song.thumbnail.as_deref()).await;
    let row = Download {
        video_id: video_id.clone(),
        title: song.title.clone(),
        artists: song.artists.clone(),
        album: song.album.clone(),
        thumbnail: song.thumbnail.clone(),
        path: final_path.to_string_lossy().to_string(),
        size: std::fs::metadata(&final_path).map(|m| m.len()).unwrap_or(done),
        created_at: now_secs(),
        duration_secs: song.duration_secs,
    };
    tag(&final_path, &row, cover.as_deref());
    Ok(row)
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The Downloads page's data: every stored row whose file still exists. A row whose file was
/// deleted outside Limusic is dropped here and forgotten, so the page never offers a dead track.
pub fn list(db: &crate::db::Db) -> Vec<Download> {
    let rows = db.downloads();
    let mut live = Vec::with_capacity(rows.len());
    for mut d in rows {
        match std::fs::metadata(&d.path) {
            Ok(m) if m.is_file() => {
                d.size = m.len();
                live.push(d);
            }
            _ => {
                let _ = db.delete_download(&d.video_id);
            }
        }
    }
    live.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    live
}

/// Delete a downloaded file and its row. Missing file is not an error: the goal is "not there".
pub fn delete(db: &crate::db::Db, video_id: &str) -> Result<(), String> {
    if let Some(d) = db.download(video_id) {
        match std::fs::remove_file(&d.path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.to_string()),
        }
    }
    db.delete_download(video_id);
    Ok(())
}

/// The live progress map, for a page that mounted mid-download.
pub fn snapshot() -> Vec<DownloadProgress> {
    progress_map().lock().map(|m| m.values().cloned().collect()).unwrap_or_default()
}
