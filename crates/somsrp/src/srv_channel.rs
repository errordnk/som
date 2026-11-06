//! somsrp's connection to `somsrv`'s binary side channel: the channel itself
//! is `srp_client::channel::SrvChannel`; somsrp, which may run before any
//! daemon, starts (or replaces a stale) one first, as the reference client
//! that ships next to `somsrv`.

pub use srp_client::channel::{Incoming, SrvChannel};
use somsrv::protocol::daemon_socket_path;

/// Connects to the local daemon, starting it (next to this executable) when
/// none runs. Fails hard rather than falling back to anything: a missing
/// daemon is a setup problem worth seeing.
pub fn connect() -> Result<SrvChannel, String> {
    let daemon_binary = somsrv::daemon::binary_path_next_to_current_exe().map_err(|err| {
        format!(
            "{err:#}
             (somsrv should be deployed next to somsrp's own executable —              check `~/.local/bin/somsrv` exists and is executable)"
        )
    })?;
    // A daemon older than the somsrv next to this somsrp is replaced first.
    if let Err(err) = somsrv::daemon::replace_if_stale(&daemon_binary) {
        eprintln!("somsrp: could not check whether the running somsrv is current: {err:#}");
    }
    let connection = somsrv::daemon::connect_or_spawn(&daemon_binary)
        .map_err(|err| format!("failed to connect to the somsrv daemon at {:?}: {err:#}", daemon_socket_path()))?;
    SrvChannel::over(connection, somsrv::protocol::current_handshake())
}

pub fn to_srv_content_type(content_type: terminal::rich_content_transport::ContentType) -> somsrv::protocol::ContentType {
    use terminal::rich_content_transport::ContentType as T;
    match content_type {
        T::Gif => somsrv::protocol::ContentType::Gif,
        T::Audio => somsrv::protocol::ContentType::Audio,
        T::Markdown => somsrv::protocol::ContentType::Markdown,
        T::Video => somsrv::protocol::ContentType::Video,
        T::Jpeg => somsrv::protocol::ContentType::Jpeg,
        T::Png => somsrv::protocol::ContentType::Png,
    }
}

/// Converts `crates/terminal`'s `ContentMetadata` to `somsrv::protocol`'s
/// own separate (but field-for-field identical) copy — see
/// `to_srv_content_type`'s doc comment for why these are two distinct
/// types rather than one shared one.
pub fn to_srv_metadata(metadata: terminal::rich_content_transport::ContentMetadata) -> somsrv::protocol::ContentMetadata {
    use terminal::rich_content_transport::ContentMetadata as M;
    match metadata {
        M::Image { width_px, height_px, color_bits, is_animated } => {
            somsrv::protocol::ContentMetadata::Image { width_px, height_px, color_bits, is_animated }
        },
        M::Audio { sample_rate, channels, bits_per_sample, duration_ms, extension } => {
            somsrv::protocol::ContentMetadata::Audio { sample_rate, channels, bits_per_sample, duration_ms, extension }
        },
        M::Video { width_px, height_px, fps_numerator, fps_denominator, codec, audio_stream_index, subtitle_stream_index, extension } => {
            somsrv::protocol::ContentMetadata::Video {
                width_px,
                height_px,
                fps_numerator,
                fps_denominator,
                codec: to_srv_video_codec(codec),
                audio_stream_index,
                subtitle_stream_index,
                extension,
            }
        },
        M::Markdown { base_dir } => somsrv::protocol::ContentMetadata::Markdown { base_dir },
    }
}

fn to_srv_video_codec(codec: terminal::rich_content_transport::VideoCodec) -> somsrv::protocol::VideoCodec {
    use terminal::rich_content_transport::VideoCodec as C;
    match codec {
        C::Unknown => somsrv::protocol::VideoCodec::Unknown,
        C::H264 => somsrv::protocol::VideoCodec::H264,
        C::H265 => somsrv::protocol::VideoCodec::H265,
        C::Vp9 => somsrv::protocol::VideoCodec::Vp9,
        C::Av1 => somsrv::protocol::VideoCodec::Av1,
        C::Mpeg4 => somsrv::protocol::VideoCodec::Mpeg4,
    }
}
