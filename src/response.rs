use crate::{connection::HttpStream, Error};
use std::collections::HashMap;
use std::io::{Bytes, Read};
use std::str;

#[derive(Clone, Copy, PartialEq, Debug)]
enum StringValidityState {
    Unchecked,
    CheckedValid,
    CheckedInvalid(str::Utf8Error),
}

/// An HTTP response.
#[derive(Clone, PartialEq, Debug)]
pub struct Response {
    /// The status code of the response, eg. 404.
    pub status_code: i32,
    /// The reason phrase of the response, eg. "Not Found".
    pub reason_phrase: String,
    /// The headers of the response.
    pub headers: HashMap<String, String>,

    body: Vec<u8>,
    // TODO: Is caching a Cell<Utf8Error> too much bloat for Response?
    body_str_state: std::cell::Cell<StringValidityState>,
}

impl Response {
    pub(crate) fn create(mut parent: ResponseLazy, is_head: bool) -> Result<Response, Error> {
        let mut body = Vec::new();
        if !is_head {
            for byte in &mut parent {
                let (byte, length) = byte?;
                body.reserve(length);
                body.push(byte);
            }
        }

        let ResponseLazy {
            status_code,
            reason_phrase,
            headers,
            ..
        } = parent;

        Ok(Response {
            status_code,
            reason_phrase,
            headers,
            body,
            body_str_state: std::cell::Cell::new(StringValidityState::Unchecked),
        })
    }

    /// Returns the body as an `&str`.
    ///
    /// ## Implementation note
    ///
    /// As the body of the `Response` is never mutated, it is safe to
    /// only check its UTF-8 validity once. Because of that, this
    /// function might take a few microseconds the first time you call
    /// it, but it will be practically free after that, as the result
    /// of the check is cached within the `Response`.
    ///
    /// # Errors
    ///
    /// Returns
    /// [`InvalidUtf8InBody`](enum.Error.html#variant.InvalidUtf8InBody)
    /// if the body is not UTF-8, with a description as to why the
    /// provided slice is not UTF-8.
    ///
    /// # Example
    /// ```no_run
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// # let url = "http://example.org/";
    /// let response = minreq::get(url).send()?;
    /// println!("{}", response.as_str()?);
    /// # Ok(())
    /// # }
    /// ```
    pub fn as_str(&self) -> Result<&str, Error> {
        use StringValidityState::*;
        match self.body_str_state.get() {
            Unchecked => match str::from_utf8(&self.body) {
                Ok(s) => {
                    self.body_str_state.set(CheckedValid);
                    Ok(s)
                }
                Err(err) => {
                    self.body_str_state.set(CheckedInvalid(err));
                    Err(Error::InvalidUtf8InBody(err))
                }
            },
            CheckedValid => unsafe { Ok(str::from_utf8_unchecked(&self.body)) },
            CheckedInvalid(err) => Err(Error::InvalidUtf8InBody(err)),
        }
    }

    /// Returns a reference to the contained bytes. If you want the
    /// `Vec<u8>` itself, use [`into_bytes()`](#method.into_bytes)
    /// instead.
    ///
    /// Usage:
    /// ```no_run
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// # let url = "http://example.org/";
    /// let response = minreq::get(url).send()?;
    /// println!("{:?}", response.as_bytes());
    /// # Ok(())
    /// # }
    /// ```
    pub fn as_bytes(&self) -> &[u8] {
        &self.body
    }

    /// Turns the `Response` into the inner `Vec<u8>`. If you just
    /// need a `&[u8]`, use [`as_bytes()`](#method.as_bytes) instead.
    ///
    /// Usage:
    /// ```no_run
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// # let url = "http://example.org/";
    /// let response = minreq::get(url).send()?;
    /// println!("{:?}", response.into_bytes());
    /// // This would error, as into_bytes consumes the Response:
    /// // let x = response.status_code;
    /// # Ok(())
    /// # }
    /// ```
    pub fn into_bytes(self) -> Vec<u8> {
        self.body
    }

    /// Converts JSON body to a `struct` using Serde.
    ///
    /// # Errors
    ///
    /// Returns
    /// [`SerdeJsonError`](enum.Error.html#variant.SerdeJsonError) if
    /// Serde runs into a problem, or
    /// [`InvalidUtf8InBody`](enum.Error.html#variant.InvalidUtf8InBody)
    /// if the body is not UTF-8.
    ///
    /// # Example
    /// In case compiler cannot figure out return type you might need to declare it explicitly:
    ///
    /// ```no_run
    /// use serde_derive::Deserialize;
    ///
    /// #[derive(Deserialize)]
    /// struct User {
    ///     name: String,
    ///     email: String,
    /// }
    ///
    /// # fn main() -> Result<(), minreq::Error> {
    /// # let url_to_json_resource = "http://example.org/resource.json";
    /// let user_name = minreq::get(url_to_json_resource).send()?
    ///     .json::<User>()? // explicitly declared type `User`
    ///     .name;
    /// println!("User name is '{}'", &user_name);
    /// # Ok(())
    /// # }
    /// ```
    #[cfg(feature = "json-using-serde")]
    pub fn json<'a, T>(&'a self) -> Result<T, Error>
    where
        T: serde::de::Deserialize<'a>,
    {
        let str = match self.as_str() {
            Ok(str) => str,
            Err(_) => return Err(Error::InvalidUtf8InResponse),
        };
        match serde_json::from_str(str) {
            Ok(json) => Ok(json),
            Err(err) => Err(Error::SerdeJsonError(err)),
        }
    }
}

/// An HTTP response, which is loaded lazily.
///
/// In comparison to [`Response`](struct.Response.html), this is
/// returned from
/// [`send_lazy()`](struct.Request.html#method.send_lazy), where as
/// [`Response`](struct.Response.html) is returned from
/// [`send()`](struct.Request.html#method.send).
///
/// In practice, "lazy loading" means that the bytes are only loaded
/// as you iterate through them. The bytes are provided in the form of
/// a `Result<(u8, usize), minreq::Error>`, as the reading operation
/// can fail in various ways. The `u8` is the actual byte that was
/// read, and `usize` is how many bytes we are expecting to read in
/// the future (including this byte). Note, however, that the `usize`
/// can change, particularly when the `Transfer-Encoding` is
/// `chunked`: then it will reflect how many bytes are left of the
/// current chunk.
///
/// # Example
/// ```no_run
/// // This is how the normal Response works behind the scenes, and
/// // how you might use ResponseLazy.
/// # fn main() -> Result<(), minreq::Error> {
/// let response = minreq::get("http://httpbin.org/ip").send_lazy()?;
/// let mut vec = Vec::new();
/// for result in response {
///     let (byte, length) = result?;
///     vec.reserve(length);
///     vec.push(byte);
/// }
/// # Ok(())
/// # }
///
/// ```
pub struct ResponseLazy {
    /// The status code of the response, eg. 404.
    pub status_code: i32,
    /// The reason phrase of the response, eg. "Not Found".
    pub reason_phrase: String,
    /// The headers of the response.
    pub headers: HashMap<String, String>,

    stream: Bytes<HttpStream>,
    content_length: Option<usize>,
    chunked: bool,
    chunks_done: bool,
}

impl ResponseLazy {
    pub(crate) fn from_stream(stream: HttpStream) -> Result<ResponseLazy, Error> {
        let mut stream = stream.bytes();
        let ResponseMetadata {
            status_code,
            reason_phrase,
            headers,
            chunked,
            content_length,
        } = read_metadata(&mut stream)?;

        Ok(ResponseLazy {
            status_code,
            reason_phrase,
            headers,
            content_length,
            stream,
            chunked,
            chunks_done: false,
        })
    }
}

impl Iterator for ResponseLazy {
    type Item = Result<(u8, usize), Error>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.chunked {
            if self.chunks_done {
                return None;
            }

            if let Some(content_length) = self.content_length {
                if content_length == 0 {
                    // Get the size of the next chunk
                    let count_line = match read_line(&mut self.stream) {
                        Ok(line) => line,
                        Err(err) => return Some(Err(err)),
                    };
                    match usize::from_str_radix(&count_line, 16) {
                        Ok(incoming_count) => {
                            if incoming_count == 0 {
                                // FIXME: Trailer header handling
                                self.chunks_done = true;
                                return None;
                            }
                            self.content_length = Some(incoming_count);
                        }
                        Err(_) => return Some(Err(Error::MalformedChunkLength)),
                    }
                }
            } else {
                return Some(Err(Error::Other(
                    "content length was None in a chunked transfer",
                )));
            }
        }

        if let Some(content_length) = self.content_length {
            if content_length > 0 {
                self.content_length = Some(content_length - 1);

                if let Some(byte) = self.stream.next() {
                    match byte {
                        Ok(byte) => {
                            if self.chunked && content_length - 1 == 0 {
                                // The last byte of the chunk was read, pop the trailing \r\n
                                if let Err(err) = read_line(&mut self.stream) {
                                    return Some(Err(err));
                                }
                            }
                            return Some(Ok((byte, content_length)));
                        }
                        Err(err) => return Some(Err(Error::IoError(err))),
                    }
                }
            }
        } else {
            // TODO: Check if this behaviour matches the HTTP spec

            // Content-Length wasn't specified, and this is not a
            // chunked transfer. So just keep getting the bytes until
            // the connection ends, I guess?
            if let Some(byte) = self.stream.next() {
                match byte {
                    Ok(byte) => return Some(Ok((byte, 1))),
                    Err(err) => return Some(Err(Error::IoError(err))),
                }
            }
        }
        None
    }
}

// This struct is just used in the Response and ResponseLazy
// constructors, but not in their structs, for api-cleanliness
// reasons. (Eg. response.status_code is much cleaner than
// response.meta.status_code or similar.)
struct ResponseMetadata {
    status_code: i32,
    reason_phrase: String,
    headers: HashMap<String, String>,
    chunked: bool,
    content_length: Option<usize>,
}

fn read_metadata(stream: &mut Bytes<HttpStream>) -> Result<ResponseMetadata, Error> {
    let (status_code, reason_phrase) = parse_status_line(read_line(stream)?);
    let mut headers = HashMap::new();
    let mut chunked = false;
    let mut content_length = None;

    // Read headers
    loop {
        let line = read_line(stream)?;
        if line.is_empty() {
            // Body starts here
            break;
        }
        if let Some(header) = parse_header(line) {
            if !chunked
                && header.0.to_lowercase().trim() == "transfer-encoding"
                && header.1.to_lowercase().trim() == "chunked"
            {
                chunked = true;
                content_length = Some(0);
            }
            if content_length.is_none() && header.0.to_lowercase().trim() == "content-length" {
                match str::parse::<usize>(&header.1.trim()) {
                    Ok(length) => content_length = Some(length),
                    Err(_) => return Err(Error::MalformedContentLength),
                }
            }
            headers.insert(header.0, header.1);
        }
    }

    Ok(ResponseMetadata {
        status_code,
        reason_phrase,
        headers,
        chunked,
        content_length,
    })
}

fn read_line(stream: &mut Bytes<HttpStream>) -> Result<String, Error> {
    let mut bytes = Vec::with_capacity(32);
    for byte in stream {
        match byte {
            Ok(byte) => {
                if byte == b'\n' {
                    // Pop the \r off, as HTTP lines end in \r\n.
                    bytes.pop();
                    break;
                } else {
                    bytes.push(byte);
                }
            }
            Err(err) => {
                return Err(Error::IoError(err));
            }
        }
    }
    match String::from_utf8(bytes) {
        Ok(line) => Ok(line.to_string()),
        Err(_) => Err(Error::InvalidUtf8InResponse),
    }
}

fn parse_status_line(line: String) -> (i32, String) {
    let mut split = line.split(' ');
    if let Some(code) = split.nth(1) {
        if let Ok(code) = code.parse::<i32>() {
            if let Some(reason) = split.next() {
                return (code, reason.to_string());
            }
        }
    }
    (503, "Server did not provide a status line".to_owned())
}

fn parse_header(mut line: String) -> Option<(String, String)> {
    if let Some(location) = line.find(':') {
        let value = line.split_off(location + 2);
        line.truncate(location);
        return Some((line, value));
    }
    None
}