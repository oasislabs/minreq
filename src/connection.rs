use crate::{http, Request, Response};
#[cfg(feature = "https")]
use rustls::{self, ClientConfig, ClientSession};
use std::env;
use std::io::{BufReader, BufWriter, Error, ErrorKind, Read, Write};
use std::net::TcpStream;
#[cfg(feature = "https")]
use std::sync::Arc;
use std::time::Duration;
#[cfg(feature = "https")]
use webpki::DNSNameRef;
#[cfg(feature = "https")]
use webpki_roots::TLS_SERVER_ROOTS;

/// A connection to the server for sending
/// [`Request`](struct.Request.html)s.
pub struct Connection {
    request: Request,
    timeout: Option<u64>,
}

impl Connection {
    /// Creates a new `Connection`. See
    /// [`Request`](struct.Request.html) for specifics about *what* is
    /// being sent.
    pub(crate) fn new(request: Request) -> Connection {
        let timeout = request
            .timeout
            .or_else(|| match env::var("MINREQ_TIMEOUT") {
                Ok(t) => t.parse::<u64>().ok(),
                Err(_) => None,
            });
        Connection { request, timeout }
    }

    /// Sends the [`Request`](struct.Request.html), consumes this
    /// connection, and returns a [`Response`](struct.Response.html).
    #[cfg(feature = "https")]
    pub(crate) fn send_https(self) -> Result<Response, Error> {
        let host = self.request.host.clone();
        let is_head = self.request.method == http::Method::Head;
        let bytes = self.request.into_string().into_bytes();

        // Rustls setup
        let dns_name = host.clone();
        let dns_name = dns_name.split(":").next().unwrap();
        let dns_name = DNSNameRef::try_from_ascii_str(dns_name).unwrap();
        let mut config = ClientConfig::new();
        config
            .root_store
            .add_server_trust_anchors(&TLS_SERVER_ROOTS);
        let mut sess = ClientSession::new(&Arc::new(config), dns_name);

        // IO
        let mut stream = create_tcp_stream(host, self.timeout)?;
        let mut tls = rustls::Stream::new(&mut sess, &mut stream);
        tls.write(&bytes)?;
        match read_from_stream(tls, is_head) {
            Ok(result) => Ok(Response::from_string(result)),
            Err(err) => Err(err),
        }
    }

    /// Sends the [`Request`](struct.Request.html), consumes this
    /// connection, and returns a [`Response`](struct.Response.html).
    pub(crate) fn send(self) -> Result<Response, Error> {
        let host = self.request.host.clone();
        let is_head = self.request.method == http::Method::Head;
        let bytes = self.request.into_string().into_bytes();

        let tcp = create_tcp_stream(host, self.timeout)?;

        // Send request
        let mut stream = BufWriter::new(tcp);
        stream.write_all(&bytes)?;

        // Receive response
        let tcp = stream.into_inner()?;
        let mut stream = BufReader::new(tcp);
        match read_from_stream(&mut stream, is_head) {
            Ok(response) => Ok(Response::from_string(response)),
            Err(err) => match err.kind() {
                ErrorKind::WouldBlock | ErrorKind::TimedOut => Err(Error::new(
                    ErrorKind::TimedOut,
                    format!(
                        "Request timed out! Timeout: {:?}",
                        stream.get_ref().read_timeout()
                    ),
                )),
                _ => Err(err),
            },
        }
    }
}

fn create_tcp_stream(host: String, timeout: Option<u64>) -> Result<TcpStream, Error> {
    let stream = TcpStream::connect(host)?;
    if let Some(secs) = timeout {
        let dur = Some(Duration::from_secs(secs));
        stream.set_read_timeout(dur)?;
        stream.set_write_timeout(dur)?;
    }
    Ok(stream)
}

/// Reads the stream until it can't or it reaches the end of the HTTP
/// response.
fn read_from_stream<T: Read>(stream: T, head: bool) -> Result<String, Error> {
    let mut response = String::new();
    let mut response_length = None;
    let mut byte_count = 0;
    let mut blank_line = false;
    let mut status_code = None;

    for byte in stream.bytes() {
        let byte = byte?;
        let c = byte as char;
        response.push(c);
        byte_count += 1;
        if c == '\n' {
            // Read the status line if this was the first line
            if status_code.is_none() {
                status_code = Some(http::parse_status_line(&response).0);
            }
            if blank_line {
                if let Some(code) = status_code {
                    if head || code / 100 == 1 || code == 204 || code == 304 {
                        response_length = Some(response.len());
                    }
                }
                if response_length.is_none() {
                    // There should be a body, try to get the response length
                    let len = get_response_length(&response);
                    response_length = Some(len);
                    if len > response.len() {
                        // This should never not be true, but a malicious
                        // server could cause a panic if the check wasn't
                        // here, so there's the reasoning for this branch.
                        response.reserve(len - response.len());
                    }
                }
            }
            blank_line = true;
        } else if c != '\r' {
            // Normal character, reset blank_line
            blank_line = false;
        }

        if let Some(len) = response_length {
            if byte_count >= len {
                // We have reached the end of the HTTP
                // response, break the reading loop.
                break;
            }
        }
    }

    Ok(response)
}

/// Tries to find out how long the whole response will eventually be,
/// in bytes.
fn get_response_length(response: &str) -> usize {
    // The length of the headers
    let mut byte_count = 0;
    for line in response.lines() {
        byte_count += line.len() + 2;
        if line.starts_with("Content-Length: ") {
            byte_count += line.clone()[16..].parse::<usize>().unwrap();
        }
    }
    byte_count
}