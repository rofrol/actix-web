use std::mem;
use httparse;
use http::{Version, HttpTryFrom, HeaderMap, StatusCode};
use http::header::{self, HeaderName, HeaderValue};
use bytes::{Bytes, BytesMut};
use futures::{Poll, Async};

use error::{ParseError, PayloadError};

use server::{utils, IoStream};
use server::h1::{Decoder, chunked};

use super::ClientResponse;
use super::response::ClientMessage;

const MAX_BUFFER_SIZE: usize = 131_072;
const MAX_HEADERS: usize = 96;

#[derive(Default)]
pub struct HttpResponseParser {
    decoder: Option<Decoder>,
}

#[derive(Debug, Fail)]
pub enum HttpResponseParserError {
    /// Server disconnected
    #[fail(display="Server disconnected")]
    Disconnect,
    #[fail(display="{}", _0)]
    Error(#[cause] ParseError),
}

impl HttpResponseParser {

    pub fn parse<T>(&mut self, io: &mut T, buf: &mut BytesMut)
                    -> Poll<ClientResponse, HttpResponseParserError>
        where T: IoStream
    {
        // if buf is empty parse_message will always return NotReady, let's avoid that
        let read = if buf.is_empty() {
            match utils::read_from_io(io, buf) {
                Ok(Async::Ready(0)) => {
                    // debug!("Ignored premature client disconnection");
                    return Err(HttpResponseParserError::Disconnect);
                },
                Ok(Async::Ready(_)) => (),
                Ok(Async::NotReady) =>
                    return Ok(Async::NotReady),
                Err(err) =>
                    return Err(HttpResponseParserError::Error(err.into()))
            }
            false
        } else {
            true
        };

        loop {
            match HttpResponseParser::parse_message(buf).map_err(HttpResponseParserError::Error)? {
                Async::Ready((msg, decoder)) => {
                    self.decoder = decoder;
                    return Ok(Async::Ready(msg));
                },
                Async::NotReady => {
                    if buf.capacity() >= MAX_BUFFER_SIZE {
                        return Err(HttpResponseParserError::Error(ParseError::TooLarge));
                    }
                    if read {
                        match utils::read_from_io(io, buf) {
                            Ok(Async::Ready(0)) => return Err(HttpResponseParserError::Disconnect),
                            Ok(Async::Ready(_)) => (),
                            Ok(Async::NotReady) => return Ok(Async::NotReady),
                            Err(err) => return Err(HttpResponseParserError::Error(err.into())),
                        }
                    } else {
                        return Ok(Async::NotReady)
                    }
                },
            }
        }
    }

    pub fn parse_payload<T>(&mut self, io: &mut T, buf: &mut BytesMut)
                            -> Poll<Option<Bytes>, PayloadError>
        where T: IoStream
    {
        if let Some(ref mut decoder) = self.decoder {
            // read payload
            match utils::read_from_io(io, buf) {
                Ok(Async::Ready(0)) => return Err(PayloadError::Incomplete),
                Err(err) => return Err(err.into()),
                _ => (),
            }
            decoder.decode(buf).map_err(|e| e.into())
        } else {
            Ok(Async::Ready(None))
        }
    }

    fn parse_message(buf: &mut BytesMut) -> Poll<(ClientResponse, Option<Decoder>), ParseError> {
        // Parse http message
        let bytes_ptr = buf.as_ref().as_ptr() as usize;
        let mut headers: [httparse::Header; MAX_HEADERS] =
            unsafe{mem::uninitialized()};

        let (len, version, status, headers_len) = {
            let b = unsafe{ let b: &[u8] = buf; mem::transmute(b) };
            let mut resp = httparse::Response::new(&mut headers);
            match resp.parse(b)? {
                httparse::Status::Complete(len) => {
                    let version = if resp.version.unwrap() == 1 {
                        Version::HTTP_11
                    } else {
                        Version::HTTP_10
                    };
                    let status = StatusCode::from_u16(resp.code.unwrap())
                        .map_err(|_| ParseError::Status)?;

                    (len, version, status, resp.headers.len())
                }
                httparse::Status::Partial => return Ok(Async::NotReady),
            }
        };

        let slice = buf.split_to(len).freeze();

        // convert headers
        let mut hdrs = HeaderMap::new();
        for header in headers[..headers_len].iter() {
            if let Ok(name) = HeaderName::try_from(header.name) {
                let v_start = header.value.as_ptr() as usize - bytes_ptr;
                let v_end = v_start + header.value.len();
                let value = unsafe {
                    HeaderValue::from_shared_unchecked(slice.slice(v_start, v_end)) };
                hdrs.append(name, value);
            } else {
                return Err(ParseError::Header)
            }
        }

        let decoder = if let Some(len) = hdrs.get(header::CONTENT_LENGTH) {
            // Content-Length
            if let Ok(s) = len.to_str() {
                if let Ok(len) = s.parse::<u64>() {
                    Some(Decoder::length(len))
                } else {
                    debug!("illegal Content-Length: {:?}", len);
                    return Err(ParseError::Header)
                }
            } else {
                debug!("illegal Content-Length: {:?}", len);
                return Err(ParseError::Header)
            }
        } else if chunked(&hdrs)? {
            // Chunked encoding
            Some(Decoder::chunked())
        } else if hdrs.contains_key(header::UPGRADE) {
            Some(Decoder::eof())
        } else {
            None
        };

        if let Some(decoder) = decoder {
            //let info = PayloadInfo {
            //tx: PayloadType::new(&hdrs, psender),
            //    decoder: decoder,
            //};
            Ok(Async::Ready(
                (ClientResponse::new(
                    ClientMessage{status: status, version: version,
                                  headers: hdrs, cookies: None}), Some(decoder))))
        } else {
            Ok(Async::Ready(
                (ClientResponse::new(
                    ClientMessage{status: status, version: version,
                                  headers: hdrs, cookies: None}), None)))
        }
    }
}
