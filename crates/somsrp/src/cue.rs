//! CUE sheets: one audio file (a CD ripped whole) cut into tracks by
//! start offsets. Parsing turns the sheet's text into [`ParsedCue`];
//! [`ParsedCue::sheet_for`] turns one of its files into the track list
//! Som is told about, and the `find_*` helpers locate the audio file and
//! the album picture next to the sheet.

use somsrv::protocol::{CueSheet, CueTrack};
use std::path::{Path, PathBuf};

/// A CUE sheet frame is 1/75 of a second.
const FRAMES_PER_SECOND: u64 = 75;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct ParsedCue {
    pub album: String,
    pub performer: String,
    pub year: Option<u32>,
    pub files: Vec<CueFile>,
}

/// One `FILE` of a sheet and the tracks that start in it.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct CueFile {
    pub name: String,
    pub tracks: Vec<CueRawTrack>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct CueRawTrack {
    pub number: u32,
    pub title: String,
    pub performer: String,
    /// `INDEX 01` - where the track itself starts.
    pub start_ms: Option<u64>,
}

/// Decodes a sheet's bytes: a byte order mark decides, otherwise valid
/// UTF-8 is taken as it is and anything else is run through a charset
/// guess (old rips are mostly in a Windows code page).
pub fn decode_text(bytes: &[u8]) -> String {
    if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return String::from_utf8_lossy(rest).into_owned();
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        return encoding_rs::UTF_16LE.decode_without_bom_handling(rest).0.into_owned();
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        return encoding_rs::UTF_16BE.decode_without_bom_handling(rest).0.into_owned();
    }
    if let Ok(text) = std::str::from_utf8(bytes) {
        return text.to_string();
    }
    let mut detector = chardetng::EncodingDetector::new();
    detector.feed(bytes, true);
    detector.guess(None, true).decode_without_bom_handling(bytes).0.into_owned()
}

/// `MM:SS:FF` as milliseconds.
fn parse_time(text: &str) -> Option<u64> {
    let mut parts = text.trim().split(':');
    let minutes: u64 = parts.next()?.trim().parse().ok()?;
    let seconds: u64 = parts.next()?.trim().parse().ok()?;
    let frames: u64 = parts.next()?.trim().parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((minutes * 60 + seconds) * 1000 + frames * 1000 / FRAMES_PER_SECOND)
}

/// The command word of a line and its argument, with the quotes of a
/// quoted argument removed.
fn split_command(line: &str) -> Option<(String, &str)> {
    let line = line.trim();
    let (command, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
    if command.is_empty() {
        return None;
    }
    Some((command.to_ascii_uppercase(), rest.trim()))
}

/// The first quoted string of `text`, or the whole of it when unquoted.
fn unquote(text: &str) -> String {
    let text = text.trim();
    match text.strip_prefix('"') {
        Some(rest) => rest.split('"').next().unwrap_or("").to_string(),
        None => text.to_string(),
    }
}

/// `FILE "name" WAVE` -> `name` (the type word after it is ignored).
fn parse_file_name(rest: &str) -> String {
    let rest = rest.trim();
    match rest.strip_prefix('"') {
        Some(inner) => inner.split('"').next().unwrap_or("").to_string(),
        None => rest.rsplit_once(char::is_whitespace).map(|(name, _)| name).unwrap_or(rest).trim().to_string(),
    }
}

pub fn parse(text: &str) -> ParsedCue {
    let mut cue = ParsedCue::default();
    let mut in_track = false;
    for line in text.lines() {
        let line = line.trim_start_matches('\u{feff}');
        let Some((command, rest)) = split_command(line) else {
            continue;
        };
        match command.as_str() {
            "FILE" => {
                cue.files.push(CueFile { name: parse_file_name(rest), tracks: Vec::new() });
                in_track = false;
            },
            "TRACK" => {
                let number = rest.split_whitespace().next().and_then(|n| n.parse().ok()).unwrap_or(0);
                if let Some(file) = cue.files.last_mut() {
                    file.tracks.push(CueRawTrack { number, ..Default::default() });
                    in_track = true;
                }
            },
            "TITLE" => {
                let title = unquote(rest);
                match (in_track, cue.files.last_mut().and_then(|file| file.tracks.last_mut())) {
                    (true, Some(track)) => track.title = title,
                    _ => cue.album = title,
                }
            },
            "PERFORMER" => {
                let performer = unquote(rest);
                match (in_track, cue.files.last_mut().and_then(|file| file.tracks.last_mut())) {
                    (true, Some(track)) => track.performer = performer,
                    _ => cue.performer = performer,
                }
            },
            "INDEX" => {
                let mut parts = rest.split_whitespace();
                let number = parts.next().and_then(|n| n.parse::<u32>().ok());
                let time = parts.next().and_then(parse_time);
                if number == Some(1)
                    && let Some(track) = cue.files.last_mut().and_then(|file| file.tracks.last_mut())
                {
                    track.start_ms = time;
                }
            },
            "REM" => {
                if let Some((key, value)) = split_command(rest)
                    && key == "DATE"
                {
                    cue.year = unquote(value).get(..4).and_then(|year| year.parse().ok());
                }
            },
            _ => {},
        }
    }
    cue
}

impl ParsedCue {
    /// The track list of `file` as Som is told it: tracks that have a
    /// start, in the order of their offsets, each titled (`Track NN` when
    /// the sheet gives none). A track whose performer is the album's own
    /// carries none of its own.
    pub fn sheet_for(&self, file: &CueFile, cover: Option<String>) -> CueSheet {
        let mut tracks: Vec<CueTrack> = file
            .tracks
            .iter()
            .filter_map(|track| {
                let start_ms = track.start_ms?;
                let title = if track.title.is_empty() { format!("Track {:02}", track.number) } else { track.title.clone() };
                let artist = if track.performer == self.performer { String::new() } else { track.performer.clone() };
                Some(CueTrack { start_ms, title, artist })
            })
            .collect();
        tracks.sort_by_key(|track| track.start_ms);
        CueSheet { album: self.album.clone(), artist: self.performer.clone(), year: self.year, cover, tracks }
    }
}

/// Audio containers Som's player decodes.
fn is_playable(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .is_some_and(|e| matches!(e.as_str(), "flac" | "mp3" | "ogg" | "oga"))
}

/// The audio file a sheet's `FILE` names, looked for next to the sheet.
/// A name that is not there (or names a format that is not played, as
/// ripped `.wav`/`.ape` often are once re-encoded) falls back to the same
/// name with a playable extension, then to the sheet's own name.
pub fn find_audio_file(sheet_path: &Path, file_name: &str) -> Option<PathBuf> {
    let dir = sheet_path.parent().unwrap_or(Path::new("."));
    // Sheets written on Windows name sub-folders with backslashes.
    let named = dir.join(file_name.replace('\\', "/"));
    if named.is_file() && is_playable(&named) {
        return Some(named);
    }
    let stems = [named.with_extension(""), sheet_path.with_extension("")];
    for stem in stems {
        for extension in ["flac", "mp3", "ogg", "oga"] {
            let candidate = stem.with_extension(extension);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

const COVER_STEMS: [&str; 6] = ["front", "cover", "folder", "albumart", "album", "artwork"];
const COVER_EXTENSIONS: [&str; 3] = ["jpg", "jpeg", "png"];
const COVER_DIRS: [&str; 5] = ["covers", "cover", "artwork", "scans", "images"];

/// The album's picture next to the sheet: a `front`/`cover`/`folder` ...
/// image in the sheet's folder, else the best image of a `Covers`-like
/// sub-folder (a name with `front` first, then by name).
pub fn find_cover(sheet_path: &Path) -> Option<PathBuf> {
    let dir = sheet_path.parent().unwrap_or(Path::new("."));
    let images_in = |folder: &Path| -> Vec<PathBuf> {
        let Ok(entries) = std::fs::read_dir(folder) else {
            return Vec::new();
        };
        let mut images: Vec<PathBuf> = entries
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| {
                path.is_file()
                    && path
                        .extension()
                        .and_then(|e| e.to_str())
                        .is_some_and(|e| COVER_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
            })
            .collect();
        images.sort();
        images
    };

    let here = images_in(dir);
    for stem in COVER_STEMS {
        let found = here.iter().find(|path| {
            path.file_stem().and_then(|s| s.to_str()).is_some_and(|s| s.eq_ignore_ascii_case(stem))
        });
        if let Some(found) = found {
            return Some(found.clone());
        }
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return None;
    };
    let mut folders: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.is_dir()
                && path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| COVER_DIRS.contains(&n.to_ascii_lowercase().as_str()))
        })
        .collect();
    folders.sort();
    for folder in folders {
        let images = images_in(&folder);
        let front = images.iter().find(|path| {
            path.file_stem().and_then(|s| s.to_str()).is_some_and(|s| s.to_ascii_lowercase().contains("front"))
        });
        if let Some(image) = front.or_else(|| images.first()) {
            return Some(image.clone());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const FINAL_CUT: &str = "REM GENRE Rock\r\nREM DATE 1983\r\nREM DISCID A00A200C\r\nREM COMMENT \"ExactAudioCopy v1.0b3\"\r\nPERFORMER \"Pink Floyd\"\r\nTITLE \"The Final Cut\"\r\nFILE \"Pink Floyd - The Final Cut.flac\" WAVE\r\n  TRACK 01 AUDIO\r\n    TITLE \"The Post War Dream\"\r\n    PERFORMER \"Pink Floyd\"\r\n    INDEX 01 00:00:00\r\n  TRACK 02 AUDIO\r\n    TITLE \"Your Possible Pasts\"\r\n    PERFORMER \"Pink Floyd\"\r\n    INDEX 01 02:59:25\r\n";

    #[test]
    fn a_ripped_album_sheet_is_parsed() {
        let cue = parse(FINAL_CUT);
        assert_eq!(cue.album, "The Final Cut");
        assert_eq!(cue.performer, "Pink Floyd");
        assert_eq!(cue.year, Some(1983));
        assert_eq!(cue.files.len(), 1);
        assert_eq!(cue.files[0].name, "Pink Floyd - The Final Cut.flac");
        assert_eq!(cue.files[0].tracks.len(), 2);
        assert_eq!(cue.files[0].tracks[1].title, "Your Possible Pasts");
        // 2:59 and 25 frames = 25 / 75 s.
        assert_eq!(cue.files[0].tracks[1].start_ms, Some(179_333));
    }

    #[test]
    fn the_track_list_leaves_out_the_albums_own_performer() {
        let cue = parse(FINAL_CUT);
        let sheet = cue.sheet_for(&cue.files[0], None);
        assert_eq!(sheet.artist, "Pink Floyd");
        assert_eq!(sheet.tracks.len(), 2);
        assert_eq!(sheet.tracks[0].artist, "");
        assert_eq!(sheet.tracks[0].title, "The Post War Dream");
    }

    #[test]
    fn a_track_with_its_own_performer_keeps_it() {
        let cue = parse(
            "PERFORMER \"Various\"\nTITLE \"Mix\"\nFILE \"a.flac\" WAVE\n TRACK 01 AUDIO\n  TITLE \"One\"\n  PERFORMER \"Someone\"\n  INDEX 01 00:00:00\n",
        );
        let sheet = cue.sheet_for(&cue.files[0], None);
        assert_eq!(sheet.tracks[0].artist, "Someone");
    }

    #[test]
    fn index_00_is_not_the_start_of_a_track() {
        let cue = parse(
            "FILE \"a.flac\" WAVE\n TRACK 01 AUDIO\n  INDEX 01 00:00:00\n TRACK 02 AUDIO\n  INDEX 00 03:10:00\n  INDEX 01 03:12:00\n",
        );
        assert_eq!(cue.files[0].tracks[1].start_ms, Some(192_000));
    }

    #[test]
    fn a_track_without_a_title_is_numbered() {
        let cue = parse("FILE \"a.flac\" WAVE\n TRACK 07 AUDIO\n  INDEX 01 00:00:00\n");
        assert_eq!(cue.sheet_for(&cue.files[0], None).tracks[0].title, "Track 07");
    }

    #[test]
    fn a_track_without_a_start_is_dropped() {
        let cue = parse("FILE \"a.flac\" WAVE\n TRACK 01 AUDIO\n  TITLE \"x\"\n");
        assert!(cue.sheet_for(&cue.files[0], None).tracks.is_empty());
    }

    #[test]
    fn several_files_keep_their_own_tracks() {
        let cue = parse(
            "FILE \"1.flac\" WAVE\n TRACK 01 AUDIO\n  TITLE \"A\"\n  INDEX 01 00:00:00\nFILE \"2.flac\" WAVE\n TRACK 02 AUDIO\n  TITLE \"B\"\n  INDEX 01 00:00:00\n",
        );
        assert_eq!(cue.files.len(), 2);
        assert_eq!(cue.files[1].tracks[0].title, "B");
    }

    #[test]
    fn a_byte_order_mark_and_a_windows_code_page_are_decoded() {
        let with_bom = [&[0xEF, 0xBB, 0xBF][..], b"TITLE \"x\""].concat();
        assert_eq!(decode_text(&with_bom), "TITLE \"x\"");
        // "Пинк Флойд" in windows-1251.
        let cp1251: Vec<u8> = vec![0xCF, 0xE8, 0xED, 0xEA, 0x20, 0xD4, 0xEB, 0xEE, 0xE9, 0xE4];
        let text = decode_text(&[b"PERFORMER \"", &cp1251[..], b"\""].concat());
        assert!(text.contains("Пинк Флойд"), "got {text:?}");
    }

    #[test]
    fn times_are_minutes_seconds_and_frames() {
        assert_eq!(parse_time("00:00:00"), Some(0));
        assert_eq!(parse_time("01:02:37"), Some(62_493));
        assert_eq!(parse_time("1:2"), None);
    }

    #[test]
    fn the_real_album_folder_is_understood() {
        let sheet = Path::new("F:/Pink Floyd © 1983 - The Final Cut (Remaster 1994 EMI)/Pink Floyd - The Final Cut.cue");
        let Ok(bytes) = std::fs::read(sheet) else {
            eprintln!("skipping: {} not present", sheet.display());
            return;
        };
        let cue = parse(&decode_text(&bytes));
        assert_eq!(cue.files.len(), 1);
        assert_eq!(cue.files[0].tracks.len(), 12);
        let audio = find_audio_file(sheet, &cue.files[0].name).unwrap();
        assert!(audio.is_file());
        let cover = find_cover(sheet).unwrap();
        assert!(cover.ends_with("front.jpg"));
    }
}
