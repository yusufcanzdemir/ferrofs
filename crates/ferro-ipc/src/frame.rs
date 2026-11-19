//! Framing: `u32` little-endian length prefix followed by a postcard payload (SPEC §3.2).

use std::io::{Read, Write};

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::Error;

/// Largest accepted payload; protects both sides from bogus length prefixes.
pub const MAX_FRAME: usize = 64 << 20;

/// Encodes `message` and writes it as one frame with a single write call.
pub fn write_message<T: Serialize>(writer: &mut impl Write, message: &T) -> Result<(), Error> {
    let body = postcard::to_allocvec(message).map_err(|e| Error::Encode(e.to_string()))?;
    if body.len() > MAX_FRAME {
        return Err(Error::FrameTooLarge(body.len()));
    }
    let len = u32::try_from(body.len()).map_err(|_| Error::FrameTooLarge(body.len()))?;
    let mut frame = Vec::with_capacity(4 + body.len());
    frame.extend_from_slice(&len.to_le_bytes());
    frame.extend_from_slice(&body);
    writer.write_all(&frame)?;
    writer.flush()?;
    Ok(())
}

/// Reads one frame and decodes it.
pub fn read_message<T: DeserializeOwned>(reader: &mut impl Read) -> Result<T, Error> {
    let mut len = [0u8; 4];
    reader.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(Error::FrameTooLarge(len));
    }
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body)?;
    postcard::from_bytes(&body).map_err(|e| Error::Decode(e.to_string()))
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::protocol::{Request, Response, Row, SearchRequest, SearchResponse};

    #[test]
    fn roundtrip_several_messages() {
        let mut buf = Vec::new();
        let request = Request::Search(SearchRequest::new("şehir ext:pdf", 50));
        let response = Response::Search(SearchResponse {
            rows: vec![Row {
                path: r"C:\Users\me\Şehir.pdf".into(),
                size: 42,
                mtime: 1,
                flags: 0,
            }],
            total: 1,
            took_us: 7,
            complete: true,
        });
        write_message(&mut buf, &request).unwrap();
        write_message(&mut buf, &response).unwrap();

        let mut reader = Cursor::new(buf);
        assert_eq!(read_message::<Request>(&mut reader).unwrap(), request);
        assert_eq!(read_message::<Response>(&mut reader).unwrap(), response);
        assert!(matches!(
            read_message::<Request>(&mut reader),
            Err(Error::Io(_))
        ));
    }

    #[test]
    fn rejects_oversized_and_garbage_frames() {
        let mut huge = Vec::new();
        huge.extend_from_slice(&u32::MAX.to_le_bytes());
        assert!(matches!(
            read_message::<Request>(&mut Cursor::new(huge)),
            Err(Error::FrameTooLarge(_))
        ));

        let mut garbage = 3u32.to_le_bytes().to_vec();
        garbage.extend_from_slice(&[0xFF, 0xFF, 0xFF]);
        assert!(matches!(
            read_message::<Request>(&mut Cursor::new(garbage)),
            Err(Error::Decode(_))
        ));
    }
}
