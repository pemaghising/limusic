# Downloads and local audio

## Save songs, albums and playlists

Open a song's ⋯ menu and choose **Download for offline**, or open an album's or
playlist's ⋯ menu and choose **Download album** / **Download playlist**. Songs
download one at a time; **Library → Downloads** shows the album or playlist
progress ("song 3 of 24"), the current transfer, and a Cancel button that stops
the whole batch. Songs that are already saved are not fetched again. A playlist
loads all of its pages first, so its folder is never silently missing tracks.

Files go into a visible download folder, by default `~/Music/LiMusic`, which
**Library → Downloads → Change…** replaces for new downloads (songs already
saved stay where they are and keep playing). **Open folder** shows it in the
file manager. The layout:

```
LiMusic/
├── Albums/<Album Artist>/<Album>/<NN> - <Title>.m4a   (+ .lrc)
├── Songs/<Artist> - <Title>.m4a                        (album unknown)
└── Playlists/<Playlist>/<NN> - <Artist> - <Title>.m4a  (+ .lrc)
```

A playlist's files are hard links to the album copies where the drive allows
(no extra space), else copies; numbering follows playlist order and is padded
to the playlist's length so files sort correctly. Nothing in the folder is ever
overwritten: a name already taken gets " (2)". Removing a download deletes its
file, its playlist copies and lyrics sidecars, and any folder left empty.

Transfers are received and finished in the app's private directory and only a
complete, tagged file is moved into the music folder. Playback prefers a saved
file before looking up an online stream, keeping the song identity and loudness
metadata. Downloaded-only queues finish without fetching online
recommendations; explicit Start radio still requests them.

### Audio format

Downloads always ask for YouTube's AAC stream (usually 128 kbps), whatever the
playback quality setting, so songs are saved as `.m4a` without re-encoding.
YouTube serves it as a fragmented (DASH) MP4, which Apple Music and QuickTime
reject, so the fragments are rewritten into an ordinary M4A: the same AAC
frames, the same edit list (priming samples stay skipped), one regular sample
table. If a song has no AAC stream, the best available format (such as
WebM/Opus) is saved untagged instead. CDN range responses are checked and
joined until the declared size is complete; interrupted, invalid or cancelled
transfers leave nothing behind. The per-song limit is 512 MB.

### Tags and lyrics

Each `.m4a` carries title, artist, album, album artist, year, track number,
genre, lyrics and the album's 512 px cover. Album details come from the song's
YouTube Music album page (rows that omit the album are looked up first).
YouTube has no genre, so genre, and any year, album or track number YouTube
lacks, comes from [MusicBrainz](https://musicbrainz.org), the open CC0 music
database. Only a confident match is used: same title, same lead artist, and a
length within 7 seconds, so a remix or live take never lends its details. A
track number is taken only from the release matching the album. MusicBrainz is
queried at most once a second, as it asks of clients; lookups are best effort.

Lyrics are the ones the app shows (cached, or down the lyrics provider list).
The plain text is embedded in the file's lyrics tag, which Apple Music shows;
synced lyrics are also written as an `.lrc` file beside the song, with the
song's timing nudge applied, for players that scroll lyrics.

## Export an audio file

Choose **Export audio file…** in the same menu and choose a destination. The app
first saves an offline copy if necessary, then exports a separate file named
`Artist - Title` with the saved extension (normally `.m4a`, already tagged). The app does not transcode audio. Songs
downloaded before AAC downloads were added stay WebM: remove and download them
again to get a tagged `.m4a`.
An existing destination file is never overwritten: choose a new filename.
Removing the app's download does not delete an exported copy.

## Play your own files

Open **Library → Local** and add a folder. Existing supported audio formats,
including exported audio WebM, are scanned and played locally. Exported folders
are not watched automatically; add the folder if you want it in Local music.
The Downloads and Local tabs work without signing in.

## Verification

Focused Rust tests cover multi-range completion, truncated response cleanup,
wrong range offsets, cancellation of a stalled response, safe exports, metadata
persistence, missing/truncated local files, path boundaries and URL redaction.

```sh
MACOSX_DEPLOYMENT_TARGET=12.0 LIBRARY_PATH=/opt/local/lib \
  cargo test --release -p limusic-app --lib downloads::tests
```

Manual end-to-end checks after rebuilding: download a song, export a separate
copy, cancel a transfer, restart with networking disabled, play and seek through
Downloads, and add the exported folder to Local. Verify removing a download leaves
the exported file intact. No user library or existing download is altered by the
unit tests, which use temporary directories and a loopback HTTP server.

Validation on Intel Monterey 12.7.6:
- 8 focused download tests passed, including 15 consecutive parallel runs.
- Existing queue tests: 47 passed; local-music tests: 16 passed.
- Frontend self-checks: 25/25; Svelte check: 0 errors and 0 warnings.
- Production frontend and Intel native app builds passed.
- Isolated server-rendered Downloads component fixtures were inspected in WebKit
  in light/dark modes at 600/900 CSS pixels, with no horizontal overflow.
- Live YouTube download/export and full offline GUI flows still need manual testing
  after restarting into the new build. The active user app was not interrupted.
