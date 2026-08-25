//! Decode + playback state for Som's own rich-content protocol's audio
//! files (`.mp3`/`.flac`) — the audio counterpart to
//! [`crate::rich_content_gif_player`] and [`crate::rich_content_player`].
//!
//! Unlike GIF/image players (rebuilt fresh from a cache re-decode on
//! every paint, see `rich_content_player::refresh_or_create`), an audio
//! player owns a real `cpal` output stream whose lifetime must span many
//! paints — a device stream can't be torn down and rebuilt every frame
//! the way an `Arc<RenderImage>` can be replaced. This is why
//! `RichContentAudioPlayer` lives in its own map on `Terminal`
//! (`rich_content_audio_players`), not folded into
//! `rich_content_player::RichContentPlayer`'s image-shaped state.
//!
//! Decoding is NOT progressive the way GIF's is (mp3/flac have no clean
//! "decode however many complete frames this byte prefix supports"
//! story worth building for a first cut) — playback starts only once the
//! whole file has arrived, mirroring the JPEG/PNG "wait for total_size"
//! branch in `rich_content_player::refresh_or_create`.
//!
//! Playback is entirely local to wherever Som itself is running, even
//! when the SRP-sending client (`somcat`) is on a remote SSH host — see
//! this crate's `SRP_INTEGRATION_GUIDE.md` audio section for why: Som is
//! the only process guaranteed to be physically local to the user's
//! speakers.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

/// Fully decoded PCM audio, ready to hand to a `cpal` output stream —
/// interleaved `f32` samples (symphonia's own `SampleBuffer<f32>` output
/// shape), at the source file's native sample rate/channel count. Not
/// resampled to the output device's own rate: `cpal`'s `StreamConfig` is
/// built to match this data's own `sample_rate`/`channels` directly
/// (`Device::default_output_config` merely picks the device; the actual
/// stream config sent to `build_output_stream` uses these fields) — most
/// consumer audio devices accept a wide range of sample rates directly,
/// so a resampler is not needed for a first cut.
struct DecodedAudio {
    samples: Vec<f32>,
    sample_rate: u32,
    channels: u16,
}

/// Decodes the entire file at `path` via `symphonia`'s probe + format
/// reader + decoder, producing flat interleaved `f32` PCM. Returns `Err`
/// for anything symphonia can't parse as one of its registered codecs
/// (mirrors `rich_content_gif_player`/`rich_content_static_image_player`'s
/// tolerance: a genuine decode failure is reported once, not retried).
fn decode_whole_file(path: &Path) -> Result<DecodedAudio, String> {
    use symphonia::core::audio::SampleBuffer;
    use symphonia::core::codecs::{CODEC_TYPE_NULL, DecoderOptions};
    use symphonia::core::errors::Error as SymphoniaError;
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;
    use symphonia::core::probe::Hint;

    let file = std::fs::File::open(path).map_err(|e| format!("opening {}: {e}", path.display()))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    let mut hint = Hint::new();
    if let Some(extension) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(extension);
    }

    let mut probed = symphonia::default::get_probe()
        .format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default())
        .map_err(|e| format!("probing {}: {e}", path.display()))?;

    let track = probed
        .format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .ok_or_else(|| format!("{}: no decodable audio track found", path.display()))?;
    let track_id = track.id;

    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| format!("building decoder for {}: {e}", path.display()))?;

    let mut samples: Vec<f32> = Vec::new();
    let mut sample_rate = 0u32;
    let mut channels = 0u16;
    let mut sample_buf: Option<SampleBuffer<f32>> = None;

    loop {
        let packet = match probed.format.next_packet() {
            Ok(packet) => packet,
            // Symphonia signals end-of-stream as a plain IoError wrapping
            // `UnexpectedEof` — not a distinct variant — same "ran out of
            // bytes" shape `rich_content_gif_player::is_truncation_error`
            // already deals with for GIF, just via a different crate's
            // error type.
            Err(SymphoniaError::IoError(_)) => break,
            Err(e) => return Err(format!("reading packet from {}: {e}", path.display())),
        };
        if packet.track_id() != track_id {
            continue;
        }

        let decoded = match decoder.decode(&packet) {
            Ok(decoded) => decoded,
            // A single malformed packet is skippable — the file as a
            // whole is still worth playing if the rest decodes fine,
            // same tolerance principle used throughout this protocol's
            // decode paths.
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(SymphoniaError::IoError(_)) => break,
            Err(e) => return Err(format!("decoding {}: {e}", path.display())),
        };

        if sample_buf.is_none() {
            let spec = *decoded.spec();
            sample_rate = spec.rate;
            channels = spec.channels.count() as u16;
            sample_buf = Some(SampleBuffer::<f32>::new(decoded.capacity() as u64, spec));
        }
        let buf = sample_buf.as_mut().expect("initialized above");
        buf.copy_interleaved_ref(decoded);
        samples.extend_from_slice(buf.samples());
    }

    if samples.is_empty() || sample_rate == 0 || channels == 0 {
        return Err(format!("{}: decoded zero playable samples", path.display()));
    }

    Ok(DecodedAudio { samples, sample_rate, channels })
}

/// One rich-content audio file's playback state — decoded PCM plus a
/// live `cpal` output stream. `position_frames`/`playing` are
/// `Arc<Atomic*>` (not plain fields) because they're read from the
/// paint path (`&self`, see `rich_content_player::RichContentPlayer`'s
/// identical `Cell`-based reasoning) AND written from `cpal`'s own
/// audio-callback thread, which runs independently of any paint call —
/// a `Cell`/`RefCell` isn't `Send`+`Sync` across that boundary, so
/// atomics are the minimum needed, not a stylistic choice.
pub struct RichContentAudioPlayer {
    audio: DecodedAudio,
    /// Kept alive only so `Drop`ping the player tears down the device
    /// stream — never read otherwise. `cpal::Stream` is not `Send` on
    /// every backend, so this field, and this whole struct, must stay
    /// on whichever thread creates it (the paint/main thread, same as
    /// every other `Terminal` field).
    _stream: cpal::Stream,
    position_frames: Arc<AtomicU64>,
    playing: Arc<AtomicBool>,
}

impl RichContentAudioPlayer {
    /// Decodes `path` fully and opens a `cpal` output stream against the
    /// default output device, starting paused (matching how a freshly
    /// opened media file/browser audio element starts paused, not
    /// autoplaying) — a placement only starts making sound once the user
    /// clicks its play control, wired up in a later step of this
    /// feature.
    pub fn open(path: &Path) -> Result<Self, String> {
        let audio = decode_whole_file(path)?;

        let host = cpal::default_host();
        let device = host.default_output_device().ok_or_else(|| "no default output audio device".to_string())?;

        let config = cpal::StreamConfig {
            channels: audio.channels,
            sample_rate: audio.sample_rate,
            buffer_size: cpal::BufferSize::Default,
        };

        let position_frames = Arc::new(AtomicU64::new(0));
        let playing = Arc::new(AtomicBool::new(false));

        let samples = audio.samples.clone();
        let channels_usize = audio.channels as usize;
        let total_frames = (samples.len() / channels_usize.max(1)) as u64;
        let cb_position = position_frames.clone();
        let cb_playing = playing.clone();

        let stream = device
            .build_output_stream(
                &config,
                move |output: &mut [f32], _info: &cpal::OutputCallbackInfo| {
                    if !cb_playing.load(Ordering::Acquire) {
                        output.fill(0.0);
                        return;
                    }
                    let mut frame = cb_position.load(Ordering::Acquire);
                    for out_frame in output.chunks_mut(channels_usize) {
                        if frame >= total_frames {
                            out_frame.fill(0.0);
                            cb_playing.store(false, Ordering::Release);
                            continue;
                        }
                        let start = frame as usize * channels_usize;
                        for (dst, src) in out_frame.iter_mut().zip(&samples[start..start + channels_usize]) {
                            *dst = *src;
                        }
                        frame += 1;
                    }
                    cb_position.store(frame.min(total_frames), Ordering::Release);
                },
                |err| log::error!("cpal output stream error: {err}"),
                None,
            )
            .map_err(|e| format!("building output stream: {e}"))?;
        stream.play().map_err(|e| format!("starting output stream: {e}"))?;

        Ok(Self { audio, _stream: stream, position_frames, playing })
    }

    pub fn is_playing(&self) -> bool {
        self.playing.load(Ordering::Acquire)
    }

    pub fn toggle_play_pause(&self) {
        self.playing.fetch_xor(true, Ordering::AcqRel);
    }

    pub fn set_playing(&self, playing: bool) {
        self.playing.store(playing, Ordering::Release);
    }

    /// Current playback position in the file, from 0.0 (start) to 1.0
    /// (end) — used both to render the seek bar's fill and to compute an
    /// absolute frame offset when seeking (`seek_to_fraction`'s inverse).
    pub fn position_fraction(&self) -> f32 {
        let total_frames = self.total_frames();
        if total_frames == 0 {
            return 0.0;
        }
        (self.position_frames.load(Ordering::Acquire) as f32 / total_frames as f32).clamp(0.0, 1.0)
    }

    pub fn elapsed(&self) -> std::time::Duration {
        std::time::Duration::from_secs_f64(
            self.position_frames.load(Ordering::Acquire) as f64 / self.audio.sample_rate as f64,
        )
    }

    pub fn duration(&self) -> std::time::Duration {
        std::time::Duration::from_secs_f64(self.total_frames() as f64 / self.audio.sample_rate as f64)
    }

    fn total_frames(&self) -> u64 {
        (self.audio.samples.len() / (self.audio.channels as usize).max(1)) as u64
    }

    /// Jumps playback to `fraction` (0.0..=1.0) of the file's total
    /// length — the seek-bar click/drag handler's job (added in a later
    /// step of this feature) is only to compute this fraction from a
    /// pixel offset within the bar's bounds and call this.
    pub fn seek_to_fraction(&self, fraction: f32) {
        let total_frames = self.total_frames();
        let target = (total_frames as f64 * fraction.clamp(0.0, 1.0) as f64) as u64;
        self.position_frames.store(target.min(total_frames), Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A short (1s, 440Hz tone) FLAC fixture — content doesn't matter for
    /// these tests (they check decode/playback bookkeeping, not audible
    /// correctness), just that it's real, valid FLAC. No FLAC/MP3
    /// *encoder* is pulled into this crate's dependency set (deliberately
    /// decode-only), so this is a checked-in fixture generated via
    /// `ffmpeg`, the same way `rich_content_player`'s GIF tests reuse
    /// `giphy.gif`.
    fn test_flac_path() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../test_fixtures/tone.flac")
    }

    #[test]
    fn decode_whole_file_reports_nonzero_sample_rate_and_channels() {
        let path = test_flac_path();
        if !path.exists() {
            eprintln!("skipping: {} not present", path.display());
            return;
        }
        let audio = decode_whole_file(&path).expect("fixture must decode");
        assert!(audio.sample_rate > 0);
        assert!(audio.channels > 0);
        assert!(!audio.samples.is_empty());
    }

    #[test]
    fn player_starts_paused_at_zero_position() {
        let path = test_flac_path();
        if !path.exists() {
            eprintln!("skipping: {} not present", path.display());
            return;
        }
        let Ok(player) = RichContentAudioPlayer::open(&path) else {
            // No output device available in this environment (e.g. a
            // headless CI runner) — not this test's concern.
            return;
        };
        assert!(!player.is_playing());
        assert_eq!(player.position_fraction(), 0.0);
    }

    #[test]
    fn seek_to_fraction_updates_position_fraction() {
        let path = test_flac_path();
        if !path.exists() {
            eprintln!("skipping: {} not present", path.display());
            return;
        }
        let Ok(player) = RichContentAudioPlayer::open(&path) else {
            return;
        };
        player.seek_to_fraction(0.5);
        assert!((player.position_fraction() - 0.5).abs() < 0.01);
    }

    #[test]
    fn toggle_play_pause_flips_state() {
        let path = test_flac_path();
        if !path.exists() {
            eprintln!("skipping: {} not present", path.display());
            return;
        }
        let Ok(player) = RichContentAudioPlayer::open(&path) else {
            return;
        };
        assert!(!player.is_playing());
        player.toggle_play_pause();
        assert!(player.is_playing());
        player.toggle_play_pause();
        assert!(!player.is_playing());
    }
}
