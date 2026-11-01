//! Persistent, explicitly requested offline audio. Unlike mpv's packet cache, completed files
//! survive restarts and are preferred before any network resolution. Metadata stays in SQLite;
//! a row is published only after the complete audio has been atomically renamed into place.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use innertube::{AlbumPage, AudioQuality, SongItem};
use serde::{Deserialize, Serialize};
use tauri::{Emitter, Manager, State};
use tokio::io::AsyncWriteExt;
use tokio::sync::{Mutex, Notify};

use crate::{db::Db, orchestrator::PlaybackData, state::AppState};

const MAX_AUDIO: u64 = 512 * 1024 * 1024;
const RANGE_SIZE: u64 = 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SavedDownload {
    pub song: SongItem,
    /// The file's absolute path in the music folder, built from tags and never supplied by the
    /// caller. Downloads from before the music folder hold a basename in the private directory.
    pub file: String,
    pub bytes: u64,
    pub created_at: i64,
    pub loudness_db: Option<f64>,
    /// Absolute paths of the same song in playlist folders, removed with it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub copies: Vec<String>,
}

#[derive(Clone, Serialize)]
struct Progress<'a> {
    video_id: &'a str,
    received: u64,
    total: Option<u64>,
}

fn gate() -> &'static Mutex<()> {
    static GATE: OnceLock<Mutex<()>> = OnceLock::new();
    GATE.get_or_init(|| Mutex::new(()))
}

#[derive(Default)]
struct Cancellation {
    flag: AtomicBool,
    notify: Notify,
}
impl Cancellation {
    async fn cancelled(&self) {
        if !self.flag.load(Ordering::SeqCst) {
            self.notify.notified().await;
        }
    }
}
fn active() -> &'static std::sync::Mutex<Option<(String, Arc<Cancellation>)>> {
    static ACTIVE: OnceLock<std::sync::Mutex<Option<(String, Arc<Cancellation>)>>> =
        OnceLock::new();
    ACTIVE.get_or_init(|| std::sync::Mutex::new(None))
}
struct TransferNotice {
    app: tauri::AppHandle,
    id: String,
}
impl Drop for TransferNotice {
    fn drop(&mut self) {
        *active().lock().unwrap() = None;
        let _ = self.app.emit("download-finished", &self.id);
    }
}
#[tauri::command]
pub fn cancel_download(video_id: String) {
    if let Some((id, cancel)) = active().lock().unwrap().as_ref() {
        if id == &video_id {
            cancel.flag.store(true, Ordering::SeqCst);
            cancel.notify.notify_one();
        }
    }
}

fn directory(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    Ok(app.path().app_data_dir().map_err(|e| e.to_string())?.join("downloads"))
}

const FOLDER_SETTING: &str = "download_folder";

/// The visible music folder downloads are filed into: chosen in Library → Downloads, by default
/// `~/Music/LiMusic` (the platform's audio directory).
fn library_root(state: &AppState) -> Result<PathBuf, String> {
    if let Some(dir) = state.db.get_setting(FOLDER_SETTING).filter(|d| Path::new(d).is_absolute()) {
        return Ok(dir.into());
    }
    let music = state.app.path().audio_dir().or_else(|_| state.app.path().home_dir());
    Ok(music.map_err(|e| e.to_string())?.join("LiMusic"))
}

#[tauri::command]
pub fn download_folder(state: State<'_, Arc<AppState>>) -> Result<String, String> {
    Ok(library_root(&state)?.to_string_lossy().into_owned())
}

/// New downloads go here; ones already saved stay where they are and keep playing.
#[tauri::command]
pub fn set_download_folder(
    state: State<'_, Arc<AppState>>,
    path: String,
) -> Result<String, String> {
    let dir = Path::new(&path);
    if !dir.is_absolute() {
        return Err("Choose a folder.".into());
    }
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    state.db.set_setting(FOLDER_SETTING, &path);
    Ok(path)
}

#[tauri::command]
pub fn open_download_folder(state: State<'_, Arc<AppState>>) -> Result<(), String> {
    let dir = library_root(&state)?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    #[cfg(target_os = "macos")]
    let program = "open";
    #[cfg(target_os = "windows")]
    let program = "explorer";
    #[cfg(target_os = "linux")]
    let program = "xdg-open";
    std::process::Command::new(program).arg(&dir).spawn().map(|_| ()).map_err(|e| e.to_string())
}

/// One path component from free text: no separators or characters Windows and macOS reject, no
/// leading dot (a hidden file), no reserved Windows device name, bounded to 120 bytes.
fn component(text: &str, fallback: &str) -> String {
    let cleaned: String = text
        .chars()
        .map(|c| if c.is_control() || r#"<>:"/\|?*"#.contains(c) { '_' } else { c })
        .collect();
    let mut out = String::new();
    for c in cleaned.trim().trim_start_matches('.').chars() {
        if out.len() + c.len_utf8() > 120 {
            break;
        }
        out.push(c);
    }
    let out = out.trim_end_matches(['.', ' ']).to_owned();
    let stem = out.split('.').next().unwrap_or("").to_ascii_uppercase();
    let reserved = ["CON", "PRN", "AUX", "NUL"].contains(&stem.as_str())
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.as_bytes()[3].is_ascii_digit());
    match () {
        _ if out.is_empty() => fallback.to_owned(),
        _ if reserved => format!("_{out}"),
        _ => out,
    }
}

/// Where a song is filed, relative to the music folder: `Albums/<Album Artist>/<Album>/<NN> -
/// <Title>` when its album is known, else `Songs/<Artist> - <Title>`.
fn song_path(song: &SongItem, details: &AlbumDetails, extension: &str) -> PathBuf {
    let present = |s: Option<&str>| s.filter(|s| !s.trim().is_empty()).map(str::to_owned);
    let album = present(details.album.as_deref()).or_else(|| present(song.album.as_deref()));
    match album {
        Some(album) => {
            let artist = present(details.album_artist.as_deref()).unwrap_or(song.artists.clone());
            let name = match details.track {
                Some((n, _)) => format!("{n:02} - {}", song.title),
                None => song.title.clone(),
            };
            PathBuf::from("Albums")
                .join(component(&artist, "Unknown Artist"))
                .join(component(&album, "Unknown Album"))
                .join(format!("{}.{extension}", component(&name, "Untitled")))
        }
        None => PathBuf::from("Songs").join(format!(
            "{}.{extension}",
            component(&format!("{} - {}", song.artists, song.title), "Untitled")
        )),
    }
}

/// `path`, or `name (2).ext`, `name (3).ext`… when something is already there: a download never
/// replaces a file in the user's music folder.
fn unique(path: PathBuf) -> PathBuf {
    if !path.exists() {
        return path;
    }
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("Audio").to_owned();
    let extension = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_owned();
    (2..)
        .map(|n| path.with_file_name(format!("{stem} ({n}).{extension}")))
        .find(|p| !p.exists())
        .expect("unbounded")
}

/// Move a finished file into the music folder; a copy when it is on another drive.
fn place(from: &Path, to: &Path) -> std::io::Result<()> {
    if let Some(dir) = to.parent() {
        std::fs::create_dir_all(dir)?;
    }
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    std::fs::copy(from, to).inspect_err(|_| {
        let _ = std::fs::remove_file(to);
    })?;
    std::fs::remove_file(from)
}

/// Put the song in a playlist's folder too. A hard link where the drive allows (no extra space,
/// and it still looks like an ordinary file everywhere), else a copy. Answers whether it added
/// one: a playlist downloaded again keeps the copy it already has.
fn add_copy(
    saved: &mut SavedDownload,
    source: &Path,
    slot: &PlaylistSlot,
    library: &Path,
) -> Result<bool, String> {
    let dir = library.join("Playlists").join(component(&slot.name, "Playlist"));
    if saved.copies.iter().map(Path::new).any(|c| c.parent() == Some(dir.as_path()) && c.exists()) {
        return Ok(false);
    }
    let width = slot.total.to_string().len().max(2);
    let extension = source.extension().and_then(|e| e.to_str()).unwrap_or("m4a");
    let name = format!("{:0width$} - {} - {}", slot.position, saved.song.artists, saved.song.title);
    let target = unique(dir.join(format!("{}.{extension}", component(&name, "Untitled"))));
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let link = |from: &Path, to: &Path| {
        std::fs::hard_link(from, to).or_else(|_| std::fs::copy(from, to).map(|_| ()))
    };
    link(source, &target).map_err(|e| e.to_string())?;
    let lyrics = source.with_extension("lrc");
    if lyrics.exists() {
        let _ = link(&lyrics, &target.with_extension("lrc"));
    }
    saved.copies.push(target.to_string_lossy().into_owned());
    Ok(true)
}

/// The lyrics the app shows for this song (cached, or down the provider list), best effort.
async fn song_lyrics(state: &AppState, song: &SongItem) -> Option<crate::lyrics::Lyrics> {
    let ms = crate::state::parse_duration_ms(song.duration.as_deref());
    let request = crate::lyrics::LyricsRequest {
        video_id: song.video_id.clone(),
        title: song.title.clone(),
        artists: song.artists.clone(),
        album: song.album.clone(),
        duration: (ms > 0).then_some(ms as f64 / 1000.0),
    };
    let fetched = crate::lyrics::get_lyrics(state, request, None);
    let lyrics = tokio::time::timeout(Duration::from_secs(20), fetched).await.ok()?.ok()??;
    (!lyrics.instrumental && lyrics.lines.iter().any(|l| !l.text.trim().is_empty()))
        .then_some(lyrics)
}

/// The words alone, for the file's lyrics tag (what Apple Music and most players display).
fn plain_lyrics(lyrics: &crate::lyrics::Lyrics) -> String {
    lyrics.lines.iter().map(|l| l.text.trim()).collect::<Vec<_>>().join("\n")
}

/// Synced lyrics as an LRC sidecar, which players that scroll lyrics pick up by file name. The
/// song's timing nudge is applied, as the app's own view does.
fn lrc(lyrics: &crate::lyrics::Lyrics) -> Option<String> {
    if !lyrics.synced {
        return None;
    }
    let mut out = String::new();
    for line in &lyrics.lines {
        let Some(at) = line.time_ms else { continue };
        let at = (at as i64 + lyrics.offset_ms).max(0);
        let (minutes, seconds, centis) = (at / 60_000, at / 1000 % 60, at / 10 % 100);
        out.push_str(&format!("[{minutes:02}:{seconds:02}.{centis:02}]{}\n", line.text.trim()));
    }
    (!out.is_empty()).then_some(out)
}

pub fn allow_paths(app: &tauri::AppHandle) {
    if let Ok(dir) = directory(app) {
        let _ = app.asset_protocol_scope().allow_directory(dir, true);
    }
}

fn valid_id(id: &str) -> bool {
    id.len() == 11 && id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}

fn owned_path(root: &Path, file: &str) -> Option<PathBuf> {
    // Path components also reject Windows separators when this database moves between OSes.
    if file.is_empty() || file.contains(['/', '\\', ':', '\0']) || file == "." || file == ".." {
        return None;
    }
    let path = root.join(file);
    // Reject symlinks escaping the private downloads directory.
    if path.exists() && !path.canonicalize().ok()?.starts_with(root.canonicalize().ok()?) {
        return None;
    }
    Some(path)
}

fn ready(root: &Path, saved: &SavedDownload) -> Option<PathBuf> {
    let path = if Path::new(&saved.file).is_absolute() {
        PathBuf::from(&saved.file)
    } else {
        owned_path(root, &saved.file)?
    };
    let meta = path.metadata().ok()?;
    (saved.bytes > 0 && meta.is_file() && meta.len() == saved.bytes).then_some(path)
}

pub fn playback(app: &tauri::AppHandle, db: &Db, id: &str) -> Option<PlaybackData> {
    let saved = db.offline_download(id)?;
    let path = ready(&directory(app).ok()?, &saved)?;
    let mut data = crate::local::playback_data(id, path.to_str()?).ok()?;
    data.loudness_db = saved.loudness_db;
    data.is_video = Some(false);
    data.stream_client = "offline".into();
    Some(data)
}

#[tauri::command]
pub fn list_downloads(state: State<'_, Arc<AppState>>) -> Result<Vec<SavedDownload>, String> {
    let root = directory(&state.app)?;
    Ok(state.db.offline_downloads()?.into_iter().filter(|s| ready(&root, s).is_some()).collect())
}

fn http_error(error: reqwest::Error) -> String {
    error.without_url().to_string()
}

fn audio_extension(content_type: &str, itag: i64) -> Result<&'static str, String> {
    match content_type.split(';').next().unwrap_or("").trim().to_ascii_lowercase().as_str() {
        "audio/mp4" | "video/mp4" => Ok("m4a"),
        "audio/webm" | "video/webm" => Ok("webm"),
        "audio/mpeg" => Ok("mp3"),
        "audio/ogg" | "application/ogg" => Ok("ogg"),
        "audio/aac" => Ok("aac"),
        "application/octet-stream" | "" => match itag {
            139..=141 => Ok("m4a"),
            249..=251 => Ok("webm"),
            _ => Err("The server did not identify a supported audio format.".into()),
        },
        _ => Err("The server returned something other than a supported audio stream.".into()),
    }
}

/// Strictly validate partial responses: never publish a CDN's first chunk as a whole song.
fn parse_range(value: &str, offset: u64) -> Result<(u64, u64), String> {
    let (span, total) = value
        .strip_prefix("bytes ")
        .and_then(|s| s.split_once('/'))
        .ok_or("Invalid audio range response.")?;
    let (start, end) = span.split_once('-').ok_or("Invalid audio range response.")?;
    let start = start.parse::<u64>().map_err(|_| "Invalid audio range start.")?;
    let end = end.parse::<u64>().map_err(|_| "Invalid audio range end.")?;
    let total = total.parse::<u64>().map_err(|_| "Invalid audio size.")?;
    if start != offset || end < start || end >= total || total > MAX_AUDIO {
        return Err("The server returned an incomplete or oversized audio range.".into());
    }
    Ok((end - start + 1, total))
}

/// Remove partial files on network failure, cancellation, disk errors, or validation errors.
struct Partial {
    path: PathBuf,
    keep: bool,
}
impl Drop for Partial {
    fn drop(&mut self) {
        if !self.keep {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

async fn receive_audio(
    client: &reqwest::Client,
    stream: &PlaybackData,
    root: &Path,
    id: &str,
    cancel: &Cancellation,
    mut progress: impl FnMut(u64, Option<u64>),
) -> Result<(String, u64), String> {
    tokio::fs::create_dir_all(root).await.map_err(|e| e.to_string())?;
    let partial = root.join(format!("{id}.part"));
    if partial.exists() {
        tokio::fs::remove_file(&partial).await.map_err(|e| e.to_string())?;
    }
    let guard = Partial { path: partial.clone(), keep: false };
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&partial)
        .await
        .map_err(|e| e.to_string())?;
    let downloaded: Result<(&str, u64), String> = async {
        let mut received = 0u64;
        let mut known_total = None;
        let mut extension = None;
        loop {
            let end = received.saturating_add(RANGE_SIZE - 1);
            let mut request = client.get(&stream.stream_url).timeout(Duration::from_secs(120));
            for (name, value) in &stream.headers {
                request = request.header(name, value);
            }
            let sending = request
                .header(reqwest::header::ACCEPT_ENCODING, "identity")
                .header(reqwest::header::RANGE, format!("bytes={received}-{end}"))
                .send();
            let mut response = tokio::select! {
                _ = cancel.cancelled() => return Err("Download cancelled.".into()),
                response = sending => response.map_err(http_error)?.error_for_status().map_err(http_error)?,
            };
            if extension.is_none() {
                extension = Some(audio_extension(
                    response
                        .headers()
                        .get(reqwest::header::CONTENT_TYPE)
                        .and_then(|h| h.to_str().ok())
                        .unwrap_or(""),
                    stream.itag,
                )?);
            }
            let (expected, total) = if response.status() == reqwest::StatusCode::PARTIAL_CONTENT {
                let range = response
                    .headers()
                    .get(reqwest::header::CONTENT_RANGE)
                    .and_then(|h| h.to_str().ok())
                    .ok_or("Audio response omitted its range.")?;
                let (length, total) = parse_range(range, received)?;
                (Some(length), Some(total))
            } else if response.status() == reqwest::StatusCode::OK && received == 0 {
                (response.content_length(), response.content_length())
            } else {
                return Err("The audio server did not honor the requested range.".into());
            };
            if total.is_some_and(|n| n == 0 || n > MAX_AUDIO)
                || (known_total.is_some() && known_total != total)
            {
                return Err("The audio size changed or exceeds the 512 MB per-song limit.".into());
            }
            known_total = total;
            let before = received;
            loop {
                let next = tokio::select! {
                    _ = cancel.cancelled() => return Err("Download cancelled.".into()),
                    next = response.chunk() => next.map_err(http_error)?,
                };
                let Some(chunk) = next else { break };

                received += chunk.len() as u64;
                if received > MAX_AUDIO || expected.is_some_and(|n| received - before > n) {
                    return Err("The audio response exceeded its expected size.".into());
                }
                file.write_all(&chunk).await.map_err(|e| e.to_string())?;
                progress(received, known_total);
            }
            if received == before || expected.is_some_and(|n| received - before != n) {
                return Err("The download ended before the audio was complete. Try again.".into());
            }
            if known_total.map_or(true, |total| received == total) {
                break;
            }
        }
        Ok((extension.ok_or("Missing audio format.")?, received))
    }
    .await;
    // Drain Tokio's pending disk write before closing/removing a cancelled transfer (Windows
    // cannot unlink an open file). Preserve the original transfer error if flushing also fails.
    let flushed = file.flush().await.map_err(|e| e.to_string());
    let synced = if downloaded.is_ok() && flushed.is_ok() {
        file.sync_all().await.map_err(|e| e.to_string())
    } else {
        Ok(())
    };
    drop(file);
    let (extension, received) = downloaded?;
    flushed?;
    synced?;
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).map_err(|e| e.to_string())?.as_nanos();
    let name = format!("{id}-{stamp}.{}", extension);
    tokio::fs::rename(&partial, root.join(&name)).await.map_err(|e| e.to_string())?;
    drop(guard);
    Ok((name, received))
}

/// Where a playlist download puts its copy: `Playlists/<name>/<position> - Artist - Title`.
#[derive(Clone, Debug, Deserialize)]
pub struct PlaylistSlot {
    pub name: String,
    pub position: u32,
    pub total: u32,
}

#[tauri::command]
pub async fn download_song(
    state: State<'_, Arc<AppState>>,
    mut song: SongItem,
    playlist: Option<PlaylistSlot>,
) -> Result<SavedDownload, String> {
    let _lock = gate().try_lock().map_err(|_| "Another download is still running.")?;
    if !valid_id(&song.video_id) {
        return Err("This item is not a downloadable song.".into());
    }
    let root = directory(&state.app)?;
    let library = library_root(&state)?;
    if let Some(mut saved) = state.db.offline_download(&song.video_id) {
        if let Some(path) = ready(&root, &saved) {
            // Already on disk: a playlist download only adds this playlist's copy.
            if let Some(slot) = &playlist {
                if add_copy(&mut saved, &path, slot, &library)? {
                    state.db.put_offline_download(&saved)?;
                    let _ = state.app.emit("downloads-changed", ());
                }
            }
            return Ok(saved);
        }
    }
    let cancel = Arc::new(Cancellation::default());
    *active().lock().unwrap() = Some((song.video_id.clone(), cancel.clone()));
    let _notice = TransferNotice { app: state.app.clone(), id: song.video_id.clone() };
    let _ = state.app.emit("download-started", &song.video_id);
    let disabled = state.disabled_clients();
    // AAC regardless of the playback quality setting: it saves as a standard .m4a that other
    // players and tag editors understand, without re-encoding YouTube's audio.
    let resolving =
        state.orchestrator.resolve(&song.video_id, song.is_upload, AudioQuality::Aac, &disabled);
    let stream = tokio::select! {
        _ = cancel.cancelled() => return Err("Download cancelled.".into()),
        result = resolving => result.map_err(|e| e.to_string())?,
    };
    let mut last = Instant::now() - Duration::from_secs(1);
    // Received and finished in the private directory; only a complete, tagged file reaches the
    // music folder.
    let (staged, _) = receive_audio(
        crate::http::client(),
        &stream,
        &root,
        &song.video_id,
        &cancel,
        |received, total| {
            if last.elapsed() >= Duration::from_millis(200) || total == Some(received) {
                let _ = state.app.emit(
                    "download-progress",
                    Progress { video_id: &song.video_id, received, total },
                );
                last = Instant::now();
            }
        },
    )
    .await?;
    let mut audio_guard = Partial { path: root.join(&staged), keep: false };
    let mut details = tokio::select! {
        _ = cancel.cancelled() => return Err("Download cancelled.".into()),
        details = album_details(&state, &song) => details,
    };
    tokio::select! {
        _ = cancel.cancelled() => return Err("Download cancelled.".into()),
        () = enrich(&song, &mut details) => (),
    }
    song.album = song.album.take().or_else(|| details.album.clone());
    song.album_id = song.album_id.take().or_else(|| details.album_id.clone());
    // The album's own square cover over a row thumbnail (a music video's is 16:9), and 512px
    // rather than the 120px most rows carry. Kept privately for the app's own rows.
    let source = details.cover.as_deref().or(song.thumbnail.as_deref());
    let cover = if let Some(url) = source.map(crate::media::cover_url) {
        tokio::select! {
            _ = cancel.cancelled() => return Err("Download cancelled.".into()),
            result = save_cover(&url, &root, &staged) => result,
        }
    } else {
        None
    };
    let mut cover_guard = cover.as_ref().map(|p| Partial { path: p.clone(), keep: false });
    let lyrics = tokio::select! {
        _ = cancel.cancelled() => return Err("Download cancelled.".into()),
        lyrics = song_lyrics(&state, &song) => lyrics,
    };
    if staged.ends_with(".m4a") {
        let (path, tagged, info, art) =
            (root.join(&staged), song.clone(), details.clone(), cover.clone());
        let text = lyrics.as_ref().map(plain_lyrics);
        let written = tokio::task::spawn_blocking(move || {
            finish_m4a(&path, &tagged, &info, art.as_deref(), text.as_deref())
        })
        .await;
        match written {
            Ok(Ok(())) => (),
            // Untagged audio still plays; keep it rather than failing the download.
            Ok(Err(error)) => tracing::warn!(%error, "could not tag downloaded audio"),
            Err(error) => tracing::warn!(%error, "tagging task failed"),
        }
    }
    let extension = Path::new(&staged).extension().and_then(|e| e.to_str()).unwrap_or("m4a");
    let destination = unique(library.join(song_path(&song, &details, extension)));
    let (from, to) = (root.join(&staged), destination.clone());
    tokio::task::spawn_blocking(move || place(&from, &to))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("Could not save into the download folder: {e}"))?;
    audio_guard.path = destination.clone();
    let mut lrc_guard = None;
    if let Some(text) = lyrics.as_ref().and_then(lrc) {
        let path = destination.with_extension("lrc");
        if tokio::fs::write(&path, text).await.is_ok() {
            lrc_guard = Some(Partial { path, keep: false });
        }
    }
    let bytes = tokio::fs::metadata(&destination).await.map_err(|e| e.to_string())?.len();
    if let Some(path) = cover {
        song.thumbnail = Some(path.to_string_lossy().into_owned());
    }
    let mut saved = SavedDownload {
        song,
        file: destination.to_string_lossy().into_owned(),
        bytes,
        created_at: crate::db::now_secs(),
        loudness_db: stream.loudness_db,
        copies: Vec::new(),
    };
    if let Some(slot) = &playlist {
        if let Err(error) = add_copy(&mut saved, &destination, slot, &library) {
            tracing::warn!(%error, "could not add the playlist copy");
        }
    }
    if let Err(error) = state.db.put_offline_download(&saved) {
        remove_copies(&saved);
        return Err(error);
    }
    audio_guard.keep = true;
    for guard in [cover_guard.as_mut(), lrc_guard.as_mut()].into_iter().flatten() {
        guard.keep = true;
    }
    allow_paths(&state.app);
    let _ = state.app.emit("downloads-changed", ());
    Ok(saved)
}

/// Delete a file this download put in the music folder, with its lyrics sidecar, and any folders
/// that leaves empty. Missing files are fine: the user may have tidied up already.
fn remove_placed(path: &Path) -> Result<(), String> {
    for file in [path.to_path_buf(), path.with_extension("lrc")] {
        match std::fs::remove_file(&file) {
            Ok(()) => (),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.to_string()),
        }
    }
    // Up to two levels (a playlist folder, or an album folder and its artist folder), never the
    // top-level Albums/Playlists/Songs folders or anything above them.
    let mut dir = path.parent();
    for _ in 0..2 {
        let Some(d) = dir else { break };
        let name = d.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if ["Albums", "Playlists", "Songs"].contains(&name) {
            break;
        }
        let entries: Vec<_> = std::fs::read_dir(d)
            .map(|r| r.flatten().map(|e| e.file_name()).collect())
            .unwrap_or_default();
        // Finder leaves a .DS_Store in any folder it has shown.
        if entries.iter().any(|n| n != ".DS_Store") {
            break;
        }
        let _ = std::fs::remove_file(d.join(".DS_Store"));
        if std::fs::remove_dir(d).is_err() {
            break;
        }
        dir = d.parent();
    }
    Ok(())
}

fn remove_copies(saved: &SavedDownload) {
    for copy in &saved.copies {
        let _ = remove_placed(Path::new(copy));
    }
}

#[tauri::command]
pub async fn remove_download(
    state: State<'_, Arc<AppState>>,
    video_id: String,
) -> Result<(), String> {
    let _lock = gate().try_lock().map_err(|_| "Wait for the current download to finish.")?;
    if let Some(saved) = state.db.offline_download(&video_id) {
        let root = directory(&state.app)?;
        if Path::new(&saved.file).is_absolute() {
            remove_placed(Path::new(&saved.file))?;
        } else if let Some(path) = owned_path(&root, &saved.file) {
            match tokio::fs::remove_file(path).await {
                Ok(()) => (),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
                Err(e) => return Err(e.to_string()),
            }
        }
        remove_copies(&saved);
        if let Some(cover) = saved.song.thumbnail.as_deref().map(Path::new) {
            if cover.parent() == Some(root.as_path())
                && cover
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with(&video_id))
            {
                if let Some(path) =
                    cover.file_name().and_then(|n| n.to_str()).and_then(|n| owned_path(&root, n))
                {
                    let _ = tokio::fs::remove_file(path).await;
                }
            }
        }
        state.db.delete_offline_download(&video_id)?;
    }
    let _ = state.app.emit("downloads-changed", ());
    Ok(())
}

#[tauri::command]
pub async fn export_download(
    state: State<'_, Arc<AppState>>,
    video_id: String,
    path: String,
) -> Result<(), String> {
    let saved = state.db.offline_download(&video_id).ok_or("Download this song first.")?;
    let source = ready(&directory(&state.app)?, &saved)
        .ok_or("The downloaded file is missing. Download it again.")?;
    copy_export(&source, Path::new(&path)).await
}

async fn copy_export(source: &Path, path: &Path) -> Result<(), String> {
    let mut input = tokio::fs::File::open(source).await.map_err(|e| e.to_string())?;
    // Never truncate an existing user file, even if the UI accidentally supplies the source path.
    let mut output =
        tokio::fs::OpenOptions::new().write(true).create_new(true).open(&path).await.map_err(
            |e| format!("Could not create the export file (choose a new filename): {e}"),
        )?;
    let result = async {
        tokio::io::copy(&mut input, &mut output).await.map_err(|e| e.to_string())?;
        output.flush().await.map_err(|e| e.to_string())
    }
    .await;
    drop(output);
    if result.is_err() {
        let _ = tokio::fs::remove_file(path).await;
    }
    result
}

/// What the album page adds to a song row: list rows often carry no album at all, and never the
/// album artist, year or track number.
#[derive(Clone, Default)]
struct AlbumDetails {
    album: Option<String>,
    album_id: Option<String>,
    album_artist: Option<String>,
    year: Option<u32>,
    /// Position on the album, and the album's length when the page listed every track.
    track: Option<(u32, Option<u32>)>,
    cover: Option<String>,
    /// From MusicBrainz; YouTube has none.
    genre: Option<String>,
}

/// Fill what YouTube left out from MusicBrainz. YouTube's own album details win where both
/// have one: they describe the release this exact audio came from.
async fn enrich(song: &SongItem, details: &mut AlbumDetails) {
    let ms = crate::state::parse_duration_ms(song.duration.as_deref());
    let lookup = crate::musicbrainz::lookup(
        &song.title,
        &song.artists,
        details.album.as_deref(),
        (ms > 0).then_some(ms as u64),
    );
    let Ok(Some(found)) = tokio::time::timeout(Duration::from_secs(30), lookup).await else {
        return;
    };
    details.genre = found.genre;
    details.year = details.year.or(found.year);
    if details.album.is_none() {
        details.album = found.album;
        details.album_artist = details.album_artist.take().or(found.album_artist);
    }
    details.track = details.track.or(found.track);
}

/// Best effort, bounded lookups: a slow or failed one only means fewer tags.
async fn album_details(state: &AppState, song: &SongItem) -> AlbumDetails {
    let mut details = AlbumDetails {
        album: song.album.clone(),
        album_id: song.album_id.clone(),
        ..Default::default()
    };
    let Some(client) = state.clients.get(innertube::METADATA_CLIENT) else { return details };
    let limit = Duration::from_secs(15);
    if details.album_id.is_none() {
        // `/next` answers with the song's own row, which names its album (#309).
        let next = tokio::time::timeout(limit, state.it.next(client, Some(&song.video_id), None));
        if let Ok(Ok(next)) = next.await {
            if let Some(row) = next.items.into_iter().find(|i| i.video_id == song.video_id) {
                details.album = details.album.or(row.album);
                details.album_id = row.album_id;
            }
        }
    }
    let Some(id) = details.album_id.clone() else { return details };
    // A whole album downloads track by track; its page is fetched once, not once per track.
    static PAGES: OnceLock<std::sync::Mutex<HashMap<String, Arc<AlbumPage>>>> = OnceLock::new();
    let pages = PAGES.get_or_init(Default::default);
    let cached = pages.lock().unwrap().get(&id).cloned();
    let page = match cached {
        Some(page) => page,
        None => {
            let Ok(Ok(page)) = tokio::time::timeout(limit, state.it.album(client, &id)).await
            else {
                return details;
            };
            let page = Arc::new(page);
            let mut pages = pages.lock().unwrap();
            if pages.len() >= 32 {
                pages.clear();
            }
            pages.insert(id, page.clone());
            page
        }
    };
    let total = page.continuation.is_none().then_some(page.items.len() as u32);
    details.track = page
        .items
        .iter()
        .position(|i| {
            i.video_id == song.video_id || i.counterpart.as_deref() == Some(&song.video_id)
        })
        .map(|n| (n as u32 + 1, total));
    details.year = page.subtitle.as_deref().and_then(year);
    details.album = page.title.clone().or(details.album);
    details.album_artist = page.artist.clone();
    details.cover = page.thumbnail.clone();
    details
}

/// The release year in an album subtitle such as "Album • 2026".
fn year(subtitle: &str) -> Option<u32> {
    subtitle
        .split(|c: char| !c.is_ascii_digit())
        .filter(|w| w.len() == 4)
        .filter_map(|w| w.parse().ok())
        .find(|y| (1900..=2100).contains(y))
}

/// Make the saved file an ordinary, tagged M4A. Defragmenting is what lets Apple Music and
/// QuickTime open it; if the layout is unexpected the original (which mpv plays) is kept.
fn finish_m4a(
    path: &Path,
    song: &SongItem,
    details: &AlbumDetails,
    cover: Option<&Path>,
    lyrics: Option<&str>,
) -> Result<(), String> {
    let data = std::fs::read(path).map_err(|e| e.to_string())?;
    if crate::m4a::is_fragmented(&data) {
        match crate::m4a::defragment(&data) {
            Ok(plain) => {
                drop(data);
                let temp = Partial { path: path.with_extension("m4a.tmp"), keep: false };
                let mut file = std::fs::File::create(&temp.path).map_err(|e| e.to_string())?;
                std::io::Write::write_all(&mut file, &plain).map_err(|e| e.to_string())?;
                file.sync_all().map_err(|e| e.to_string())?;
                drop(file);
                std::fs::rename(&temp.path, path).map_err(|e| e.to_string())?;
            }
            Err(error) => tracing::warn!(%error, "kept fragmented download"),
        }
    }
    write_tags(path, song, details, cover, lyrics)
}

/// Embed title, artist, album, album artist, year, track number, genre, lyrics and cover so the file (and any
/// export of it) shows up properly in other players. YouTube has no genre to give.
fn write_tags(
    path: &Path,
    song: &SongItem,
    details: &AlbumDetails,
    cover: Option<&Path>,
    lyrics: Option<&str>,
) -> Result<(), String> {
    use lofty::config::WriteOptions;
    use lofty::picture::{MimeType, Picture, PictureType};
    use lofty::tag::{Accessor, ItemKey, Tag, TagExt, TagType};
    let mut tag = Tag::new(TagType::Mp4Ilst);
    tag.set_title(song.title.clone());
    tag.set_artist(song.artists.clone());
    let present = |s: &Option<String>| s.clone().filter(|s| !s.trim().is_empty());
    if let Some(album) = present(&details.album).or_else(|| present(&song.album)) {
        tag.set_album(album);
    }
    if let Some(artist) = present(&details.album_artist) {
        tag.insert_text(ItemKey::AlbumArtist, artist);
    }
    if let Some(year) = details.year {
        tag.set_year(year);
    }
    if let Some(genre) = present(&details.genre) {
        tag.set_genre(genre);
    }
    if let Some((track, total)) = details.track {
        tag.set_track(track);
        if let Some(total) = total {
            tag.set_track_total(total);
        }
    }
    if let Some(text) = lyrics.filter(|t| !t.trim().is_empty()) {
        tag.insert_text(ItemKey::Lyrics, text.to_owned());
    }
    // MP4 cover art only carries JPEG or PNG; a WebP cover stays a sidecar for the app's own UI.
    if let Some(data) = cover.and_then(|p| std::fs::read(p).ok()) {
        let mime = if data.starts_with(&[0xff, 0xd8, 0xff]) {
            Some(MimeType::Jpeg)
        } else if data.starts_with(b"\x89PNG\r\n\x1a\n") {
            Some(MimeType::Png)
        } else {
            None
        };
        if mime.is_some() {
            tag.push_picture(Picture::new_unchecked(PictureType::CoverFront, mime, None, data));
        }
    }
    tag.save_to_path(path, WriteOptions::default()).map_err(|e| e.to_string())
}

// Artwork is best effort, capped, and restricted to bitmap formats the UI already supports.
async fn save_cover(url: &str, root: &Path, audio_name: &str) -> Option<PathBuf> {
    let mut response = crate::http::client()
        .get(url)
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?;
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        if bytes.len() + chunk.len() > 3 * 1024 * 1024 {
            return None;
        }
        bytes.extend_from_slice(&chunk);
    }
    let ext = if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        "jpg"
    } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        "png"
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        "webp"
    } else {
        return None;
    };
    let path = root.join(format!("{audio_name}.{ext}"));
    tokio::fs::write(&path, bytes).await.ok()?;
    Some(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;
    use tokio::net::TcpListener;

    static NEXT_TEST_DIR: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    struct TestDir(PathBuf);
    impl TestDir {
        fn new() -> Self {
            let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
            let next = NEXT_TEST_DIR.fetch_add(1, Ordering::SeqCst);
            let dir = std::env::temp_dir()
                .join(format!("limusic-download-test-{}-{stamp}-{next}", std::process::id()));
            std::fs::create_dir(&dir).unwrap();
            Self(dir)
        }
        fn empty(&self) -> bool {
            std::fs::read_dir(&self.0).unwrap().next().is_none()
        }
    }
    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    /// A real AAC file from macOS's own encoder, tagged and read back the way Local and other
    /// players would see it.
    #[cfg(target_os = "macos")]
    #[test]
    fn m4a_downloads_get_album_tags_and_cover() {
        use lofty::file::TaggedFileExt;
        use lofty::tag::Accessor;
        let dir = TestDir::new();
        let (wav, m4a, cover) =
            (dir.0.join("in.wav"), dir.0.join("song.m4a"), dir.0.join("song.m4a.jpg"));
        let samples = 44_100u32;
        // One second of 16-bit mono silence.
        let mut data = b"RIFF".to_vec();
        data.extend_from_slice(&(36 + samples * 2).to_le_bytes());
        data.extend_from_slice(b"WAVE");
        data.extend_from_slice(b"fmt \x10\0\0\0\x01\0\x01\0");
        data.extend_from_slice(&44_100u32.to_le_bytes());
        data.extend_from_slice(&88_200u32.to_le_bytes());
        data.extend_from_slice(b"\x02\0\x10\0data");
        data.extend_from_slice(&(samples * 2).to_le_bytes());
        data.resize(data.len() + samples as usize * 2, 0);
        std::fs::write(&wav, data).unwrap();
        let encoded = std::process::Command::new("afconvert")
            .args(["-f", "m4af", "-d", "aac"])
            .arg(&wav)
            .arg(&m4a)
            .status()
            .unwrap();
        assert!(encoded.success());
        std::fs::write(&cover, [0xff, 0xd8, 0xff, 0xe0, 1, 2, 3]).unwrap();
        let song: SongItem = serde_json::from_value(serde_json::json!({
            "video_id": "abcdefghijk", "title": "Café Song", "artists": "Artist A, Artist B",
            "album": "The Album", "is_video": false, "is_upload": false, "explicit": false
        }))
        .unwrap();
        let details = AlbumDetails {
            album: Some("The Album".into()),
            album_artist: Some("Artist A".into()),
            year: Some(2024),
            track: Some((3, Some(12))),
            genre: Some("Hip Hop".into()),
            ..Default::default()
        };
        finish_m4a(&m4a, &song, &details, Some(&cover), Some("First line\nSecond line")).unwrap();
        let file = lofty::read_from_path(&m4a).unwrap();
        let tag = file.primary_tag().unwrap();
        assert_eq!(tag.title().as_deref(), Some("Café Song"));
        assert_eq!(tag.artist().as_deref(), Some("Artist A, Artist B"));
        assert_eq!(tag.album().as_deref(), Some("The Album"));
        assert_eq!(tag.get_string(&lofty::tag::ItemKey::AlbumArtist), Some("Artist A"));
        assert_eq!((tag.year(), tag.track(), tag.track_total()), (Some(2024), Some(3), Some(12)));
        assert_eq!(tag.genre().as_deref(), Some("Hip Hop"));
        assert_eq!(tag.get_string(&lofty::tag::ItemKey::Lyrics), Some("First line\nSecond line"));
        assert_eq!(tag.pictures()[0].data(), &[0xff, 0xd8, 0xff, 0xe0, 1, 2, 3]);
    }
    #[test]
    fn folder_names_are_safe_on_every_platform() {
        assert_eq!(component("AC/DC: Live?", "x"), "AC_DC_ Live_");
        assert_eq!(component("..hidden. ", "x"), "hidden");
        assert_eq!(component("  ", "Unknown"), "Unknown");
        assert_eq!(component("con", "x"), "_con");
        assert_eq!(component("COM1.txt", "x"), "_COM1.txt");
        assert!(component(&"日本".repeat(100), "x").len() <= 120);
    }

    #[test]
    fn songs_are_filed_by_album_artist_and_album() {
        let song: SongItem = serde_json::from_value(serde_json::json!({
            "video_id": "abcdefghijk", "title": "Pal Pal", "artists": "Afusic, Talwiinder",
            "is_video": false, "is_upload": false, "explicit": false
        }))
        .unwrap();
        let details = AlbumDetails {
            album: Some("Pal Pal".into()),
            album_artist: Some("Afusic".into()),
            track: Some((3, Some(9))),
            ..Default::default()
        };
        assert_eq!(
            song_path(&song, &details, "m4a"),
            Path::new("Albums/Afusic/Pal Pal/03 - Pal Pal.m4a")
        );
        assert_eq!(
            song_path(&song, &AlbumDetails::default(), "webm"),
            Path::new("Songs/Afusic, Talwiinder - Pal Pal.webm")
        );
    }

    #[test]
    fn playlist_copies_are_linked_once_and_removed_with_empty_folders() {
        let dir = TestDir::new();
        let source = dir.0.join("Albums/A/B/01 - Song.m4a");
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        std::fs::write(&source, b"audio").unwrap();
        std::fs::write(source.with_extension("lrc"), b"[00:01.00]hi").unwrap();
        let mut saved = SavedDownload {
            song: SongItem { title: "Song".into(), artists: "A".into(), ..Default::default() },
            file: source.to_string_lossy().into_owned(),
            bytes: 5,
            created_at: 1,
            loudness_db: None,
            copies: Vec::new(),
        };
        let slot = PlaylistSlot { name: "Road/Trip".into(), position: 7, total: 120 };
        assert!(add_copy(&mut saved, &source, &slot, &dir.0).unwrap());
        assert!(!add_copy(&mut saved, &source, &slot, &dir.0).unwrap());
        let copy = dir.0.join("Playlists/Road_Trip/007 - A - Song.m4a");
        assert_eq!(saved.copies, [copy.to_string_lossy()]);
        assert_eq!(std::fs::read(&copy).unwrap(), b"audio");
        assert!(copy.with_extension("lrc").exists());
        // A different song with the same name never overwrites the first.
        assert_eq!(unique(copy.clone()), dir.0.join("Playlists/Road_Trip/007 - A - Song (2).m4a"));

        remove_copies(&saved);
        remove_placed(&source).unwrap();
        assert!(!dir.0.join("Playlists/Road_Trip").exists());
        assert!(dir.0.join("Playlists").exists());
        assert!(!dir.0.join("Albums/A").exists());
        assert!(dir.0.join("Albums").exists());
    }

    #[test]
    fn synced_lyrics_become_an_lrc_with_the_timing_nudge() {
        let lyrics = crate::lyrics::Lyrics {
            synced: true,
            offset_ms: 500,
            lines: vec![
                crate::lyrics::LyricLine::simple(Some(1_000), "One".into()),
                crate::lyrics::LyricLine::simple(Some(65_430), " Two ".into()),
            ],
            ..Default::default()
        };
        assert_eq!(lrc(&lyrics).unwrap(), "[00:01.50]One\n[01:05.93]Two\n");
        assert_eq!(plain_lyrics(&lyrics), "One\nTwo");
        assert!(lrc(&crate::lyrics::Lyrics { synced: false, ..lyrics }).is_none());
    }

    #[test]
    fn album_year_comes_from_the_subtitle() {
        assert_eq!(year("Album • 2026"), Some(2026));
        assert_eq!(year("EP • 1999 • 4 songs"), Some(1999));
        assert_eq!(year("Single • 12345"), None);
    }
    fn client() -> reqwest::Client {
        reqwest::Client::builder().no_proxy().build().unwrap()
    }
    fn stream(url: String) -> PlaybackData {
        PlaybackData {
            video_id: "abcdefghijk".into(),
            stream_url: url,
            itag: 140,
            headers: Default::default(),
            expires_in_seconds: 60,
            loudness_db: None,
            playback_ping: None,
            title: None,
            artists: None,
            duration: None,
            thumbnail: None,
            is_video: None,
            is_episode: None,
            stream_client: "test".into(),
        }
    }
    async fn server(responses: Vec<(&'static str, &'static str)>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            for (expected_range, response) in responses {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut header = Vec::new();
                let mut byte = [0u8; 1];
                while !header.ends_with(b"\r\n\r\n") {
                    if socket.read(&mut byte).await.unwrap() == 0 {
                        return;
                    }
                    header.push(byte[0]);
                }
                let header = String::from_utf8_lossy(&header).to_lowercase();
                assert!(header.contains(expected_range), "{header}");
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });
        format!("http://{address}/audio?secret=never-display")
    }

    #[tokio::test]
    async fn joins_capped_ranges_before_publishing_a_file() {
        let dir = TestDir::new();
        let url = server(vec![
            ("range: bytes=0-", "HTTP/1.1 206 Partial Content\r\nContent-Type: audio/mp4\r\nContent-Range: bytes 0-2/6\r\nContent-Length: 3\r\nConnection: close\r\n\r\nabc"),
            ("range: bytes=3-", "HTTP/1.1 206 Partial Content\r\nContent-Type: audio/mp4\r\nContent-Range: bytes 3-5/6\r\nContent-Length: 3\r\nConnection: close\r\n\r\ndef"),
        ]).await;
        let (file, bytes) = receive_audio(
            &client(),
            &stream(url),
            &dir.0,
            "abcdefghijk",
            &Cancellation::default(),
            |_, _| {},
        )
        .await
        .unwrap();
        assert_eq!(bytes, 6);
        assert_eq!(std::fs::read(dir.0.join(&file)).unwrap(), b"abcdef");
        assert!(!dir.0.join("abcdefghijk.part").exists());
        assert!(file.ends_with(".m4a"));
    }

    #[tokio::test]
    async fn truncated_responses_leave_no_download_or_partial() {
        let dir = TestDir::new();
        let url = server(vec![("range: bytes=0-", "HTTP/1.1 200 OK\r\nContent-Type: audio/mp4\r\nContent-Length: 6\r\nConnection: close\r\n\r\nabc")]).await;
        assert!(receive_audio(
            &client(),
            &stream(url),
            &dir.0,
            "abcdefghijk",
            &Cancellation::default(),
            |_, _| {}
        )
        .await
        .is_err());
        assert!(dir.empty());
    }

    #[tokio::test]
    async fn wrong_range_offsets_cannot_duplicate_bytes() {
        let dir = TestDir::new();
        let url = server(vec![("range: bytes=0-", "HTTP/1.1 206 Partial Content\r\nContent-Type: audio/mp4\r\nContent-Range: bytes 3-5/6\r\nContent-Length: 3\r\nConnection: close\r\n\r\ndef")]).await;
        assert!(receive_audio(
            &client(),
            &stream(url),
            &dir.0,
            "abcdefghijk",
            &Cancellation::default(),
            |_, _| {}
        )
        .await
        .is_err());
        assert!(dir.empty());
    }

    #[tokio::test]
    async fn cancelling_a_stalled_body_cleans_up_immediately() {
        let dir = TestDir::new();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/audio", listener.local_addr().unwrap());
        let serving = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut header = Vec::new();
            let mut byte = [0u8; 1];
            while !header.ends_with(b"\r\n\r\n") {
                socket.read_exact(&mut byte).await.unwrap();
                header.push(byte[0]);
            }
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: audio/mp4\r\nContent-Length: 6\r\n\r\nabc",
                )
                .await
                .unwrap();
            std::future::pending::<()>().await;
        });
        let cancel = Arc::new(Cancellation::default());
        let cancel_worker = cancel.clone();
        let root = dir.0.clone();
        let (sent, received) = tokio::sync::oneshot::channel();
        let mut sent = Some(sent);
        let worker = tokio::spawn(async move {
            receive_audio(
                &client(),
                &stream(url),
                &root,
                "abcdefghijk",
                &cancel_worker,
                |bytes, _| {
                    if bytes > 0 {
                        if let Some(sent) = sent.take() {
                            let _ = sent.send(());
                        }
                    }
                },
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(3), received).await.unwrap().unwrap();
        cancel.flag.store(true, Ordering::SeqCst);
        cancel.notify.notify_one();
        let result = tokio::time::timeout(Duration::from_secs(2), worker).await.unwrap().unwrap();
        assert_eq!(result.unwrap_err(), "Download cancelled.");
        assert!(dir.empty());
        serving.abort();
    }

    #[tokio::test]
    async fn export_does_not_overwrite_existing_files_or_its_source() {
        let dir = TestDir::new();
        let source = dir.0.join("song.m4a");
        let target = dir.0.join("export.m4a");
        std::fs::write(&source, b"audio").unwrap();
        std::fs::write(&target, b"user file").unwrap();
        assert!(copy_export(&source, &target).await.is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"user file");
        assert!(copy_export(&source, &source).await.is_err());
        assert_eq!(std::fs::read(&source).unwrap(), b"audio");
        let new = dir.0.join("new.m4a");
        copy_export(&source, &new).await.unwrap();
        assert_eq!(std::fs::read(new).unwrap(), b"audio");
    }

    #[test]
    fn metadata_survives_restart_and_missing_or_truncated_files_are_not_ready() {
        let dir = TestDir::new();
        let database = dir.0.join("library.sqlite");
        let audio = dir.0.join("audio.m4a");
        std::fs::write(&audio, b"abcd").unwrap();
        let saved = SavedDownload {
            song: SongItem {
                video_id: "abcdefghijk".into(),
                title: "A song".into(),
                artists: "An artist".into(),
                ..Default::default()
            },
            file: "audio.m4a".into(),
            bytes: 4,
            created_at: 1,
            loudness_db: Some(-3.0),
            copies: Vec::new(),
        };
        let db = Db::open(&database).unwrap();
        db.put_offline_download(&saved).unwrap();
        drop(db);
        let db = Db::open(&database).unwrap();
        let loaded = db.offline_download("abcdefghijk").unwrap();
        assert_eq!(loaded.song, saved.song);
        assert!(ready(&dir.0, &loaded).is_some());
        std::fs::write(&audio, b"ab").unwrap();
        assert!(ready(&dir.0, &loaded).is_none());
        std::fs::remove_file(&audio).unwrap();
        assert!(ready(&dir.0, &loaded).is_none());
        db.delete_offline_download("abcdefghijk").unwrap();
        assert!(db.offline_downloads().unwrap().is_empty());
    }

    #[test]
    fn paths_formats_and_sizes_are_validated() {
        let dir = TestDir::new();
        for path in ["../outside", "/etc/passwd", "C:\\outside", ".", ".."] {
            assert!(owned_path(&dir.0, path).is_none());
        }
        assert!(!valid_id("../outside"));
        assert!(valid_id("abcdEFG_12-"));
        assert!(audio_extension("text/html", 140).is_err());
        assert_eq!(audio_extension("audio/webm; codecs=opus", 251).unwrap(), "webm");
        assert!(parse_range("bytes 0-0/0", 0).is_err());
        assert!(parse_range("bytes 0-0/9999999999", 0).is_err());
        #[cfg(unix)]
        {
            let outside = TestDir::new();
            std::fs::write(outside.0.join("secret"), b"not audio").unwrap();
            std::os::unix::fs::symlink(outside.0.join("secret"), dir.0.join("escape")).unwrap();
            assert!(owned_path(&dir.0, "escape").is_none());
        }
    }

    #[tokio::test]
    async fn errors_do_not_expose_signed_stream_urls() {
        let dir = TestDir::new();
        let url = server(vec![(
            "range: bytes=0-",
            "HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        )])
        .await;
        let error = receive_audio(
            &client(),
            &stream(url),
            &dir.0,
            "abcdefghijk",
            &Cancellation::default(),
            |_, _| {},
        )
        .await
        .unwrap_err();
        assert!(!error.contains("never-display") && !error.contains("http://"), "{error}");
        assert!(dir.empty());
    }
}
