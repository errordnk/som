//! somsrp's connection to `somsrv`'s binary side channel: the channel itself
//! is `srp_client::channel::SrvChannel`; somsrp, which may run before any
//! daemon, starts (or replaces a stale) one first, as the reference client
//! that ships next to `somsrv`.

pub use srp_client::channel::{Incoming, SrvChannel};

/// Connects to the local daemon: the one Som runs (the `somsrv` inside Som, taken out at its start). somsrp
/// starts none and replaces none: which `somsrv` runs is Som's to decide, and nothing is installed next to
/// somsrp for it to find.
pub fn connect() -> Result<SrvChannel, String> {
    SrvChannel::connect().map_err(|err| format!("{err} (somsrp does not start a somsrv: Som runs one, start Som)"))
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
