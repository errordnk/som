//! In-stream metadata of Shoutcast/Icecast radio ("ICY"): a client that
//! sends `Icy-MetaData: 1` gets, every `icy-metaint` bytes of audio, one
//! length byte (in units of 16 bytes) followed by a text block such as
//! `StreamTitle='Artist - Title';`. Any station speaking this - not only
//! one with a public API - tells us what is playing.
//!
//! [`IcyReader`] sits between the HTTP body and the cache: it hands the
//! audio on with every metadata block cut out (a decoder must never see
//! them) and reports each new title.

use std::io::{self, Read};

/// The `StreamTitle` of one metadata block (the text after its length
/// byte), or `None` if it has none or an empty one.
pub fn parse_stream_title(block: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(block);
    let text = text.trim_end_matches('\0');
    const KEY: &str = "StreamTitle='";
    let start = text.find(KEY)? + KEY.len();
    let rest = &text[start..];
    // The value ends at the first `';` - a title may itself hold quotes.
    let end = rest.find("';").or_else(|| rest.rfind('\''))?;
    let title = rest[..end].trim();
    (!title.is_empty()).then(|| title.to_string())
}

/// Strips ICY metadata out of an audio stream and reports each change of
/// title through `on_title`.
pub struct IcyReader<R, F> {
    inner: R,
    metaint: usize,
    /// Audio bytes left before the next metadata block.
    left: usize,
    on_title: F,
    last_title: Option<String>,
}

impl<R: Read, F: FnMut(String)> IcyReader<R, F> {
    pub fn new(inner: R, metaint: usize, on_title: F) -> Self {
        Self { inner, metaint, left: metaint, on_title, last_title: None }
    }
}

impl<R: Read, F: FnMut(String)> Read for IcyReader<R, F> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.left == 0 {
            let mut length = [0u8; 1];
            match self.inner.read_exact(&mut length) {
                Ok(()) => {},
                Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => return Ok(0),
                Err(err) => return Err(err),
            }
            let block_len = length[0] as usize * 16;
            if block_len > 0 {
                let mut block = vec![0u8; block_len];
                match self.inner.read_exact(&mut block) {
                    Ok(()) => {},
                    Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => return Ok(0),
                    Err(err) => return Err(err),
                }
                if let Some(title) = parse_stream_title(&block)
                    && self.last_title.as_deref() != Some(title.as_str())
                {
                    self.last_title = Some(title.clone());
                    (self.on_title)(title);
                }
            }
            self.left = self.metaint;
        }
        let want = buf.len().min(self.left);
        let read = self.inner.read(&mut buf[..want])?;
        self.left -= read;
        Ok(read)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(text: &str) -> Vec<u8> {
        let mut bytes = text.as_bytes().to_vec();
        let padded = bytes.len().div_ceil(16) * 16;
        bytes.resize(padded, 0);
        let mut out = vec![(padded / 16) as u8];
        out.extend(bytes);
        out
    }

    #[test]
    fn title_is_taken_from_the_block() {
        assert_eq!(
            parse_stream_title(b"StreamTitle='Daft Punk - Get Lucky';StreamUrl='';\0\0\0"),
            Some("Daft Punk - Get Lucky".to_string())
        );
    }

    #[test]
    fn a_quote_inside_the_title_is_kept() {
        assert_eq!(
            parse_stream_title(b"StreamTitle='Guns N' Roses - Don't Cry';StreamUrl='';"),
            Some("Guns N' Roses - Don't Cry".to_string())
        );
    }

    #[test]
    fn empty_or_missing_title_is_none() {
        assert_eq!(parse_stream_title(b"StreamTitle='';"), None);
        assert_eq!(parse_stream_title(b"StreamUrl='http://x';"), None);
        assert_eq!(parse_stream_title(b""), None);
    }

    #[test]
    fn metadata_is_cut_out_of_the_audio_and_reported_once_per_change() {
        let mut stream = Vec::new();
        stream.extend_from_slice(b"abcd"); // metaint = 4
        stream.extend(block("StreamTitle='A - B';"));
        stream.extend_from_slice(b"efgh");
        stream.extend(block("StreamTitle='A - B';")); // same title again
        stream.extend_from_slice(b"ijkl");
        stream.push(0); // no metadata this time
        stream.extend_from_slice(b"mn");

        let mut titles = Vec::new();
        let mut audio = Vec::new();
        IcyReader::new(io::Cursor::new(stream), 4, |t| titles.push(t)).read_to_end(&mut audio).unwrap();

        assert_eq!(audio, b"abcdefghijklmn");
        assert_eq!(titles, vec!["A - B".to_string()]);
    }

    #[test]
    fn a_new_title_in_a_later_block_is_reported() {
        let mut stream = Vec::new();
        stream.extend_from_slice(b"abcd");
        stream.extend(block("StreamTitle='One';"));
        stream.extend_from_slice(b"efgh");
        stream.extend(block("StreamTitle='Two';"));
        stream.extend_from_slice(b"ij");

        let mut titles = Vec::new();
        let mut audio = Vec::new();
        IcyReader::new(io::Cursor::new(stream), 4, |t| titles.push(t)).read_to_end(&mut audio).unwrap();

        assert_eq!(audio, b"abcdefghij");
        assert_eq!(titles, vec!["One".to_string(), "Two".to_string()]);
    }

    #[test]
    fn a_stream_cut_inside_a_metadata_block_ends_cleanly() {
        let mut stream = Vec::new();
        stream.extend_from_slice(b"abcd");
        stream.push(2); // promises 32 bytes, delivers 5
        stream.extend_from_slice(b"Strea");

        let mut audio = Vec::new();
        IcyReader::new(io::Cursor::new(stream), 4, |_| {}).read_to_end(&mut audio).unwrap();

        assert_eq!(audio, b"abcd");
    }
}
