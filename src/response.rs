//! Memcache response types and parsing/encoding.
//!
//! Response types:
//! - `VALUE <key> <flags> <bytes>\r\n<data>\r\n... END\r\n` - GET response
//! - `STORED\r\n` - SET success
//! - `NOT_STORED\r\n` - SET failure
//! - `DELETED\r\n` - DELETE success
//! - `NOT_FOUND\r\n` - DELETE miss
//! - `ERROR\r\n` - Generic error
//! - `CLIENT_ERROR <msg>\r\n` - Client error
//! - `SERVER_ERROR <msg>\r\n` - Server error

use crate::error::ParseError;
use crate::input::{Input, Segments};
use bytes::Bytes;
use std::io::Write;

/// A single value from a GET response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Value {
    pub key: Vec<u8>,
    pub flags: u32,
    pub data: Vec<u8>,
    /// CAS unique token, present when the response is from a `gets` command.
    pub cas: Option<u64>,
}

/// A parsed Memcache response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Response {
    /// Value response from GET (may contain multiple values for multi-get)
    Values(Vec<Value>),
    /// STORED response from SET
    Stored,
    /// NOT_STORED response from SET
    NotStored,
    /// DELETED response from DELETE
    Deleted,
    /// NOT_FOUND response from DELETE
    NotFound,
    /// EXISTS response from CAS
    Exists,
    /// OK response (from flush_all, touch, etc.)
    Ok,
    /// Numeric response from INCR/DECR (the new value after the operation).
    Numeric(u64),
    /// VERSION response
    Version(Vec<u8>),
    /// Generic error
    Error,
    /// Client error with message
    ClientError(Vec<u8>),
    /// Server error with message
    ServerError(Vec<u8>),
}

impl Response {
    // ========================================================================
    // Constructors (for server-side encoding)
    // ========================================================================

    /// Create a STORED response.
    #[inline]
    pub fn stored() -> Self {
        Response::Stored
    }

    /// Create a NOT_STORED response.
    #[inline]
    pub fn not_stored() -> Self {
        Response::NotStored
    }

    /// Create a DELETED response.
    #[inline]
    pub fn deleted() -> Self {
        Response::Deleted
    }

    /// Create a NOT_FOUND response.
    #[inline]
    pub fn not_found() -> Self {
        Response::NotFound
    }

    /// Create an OK response.
    #[inline]
    pub fn ok() -> Self {
        Response::Ok
    }

    /// Create a numeric response (from INCR/DECR).
    #[inline]
    pub fn numeric(value: u64) -> Self {
        Response::Numeric(value)
    }

    /// Create an empty VALUES response (cache miss).
    #[inline]
    pub fn miss() -> Self {
        Response::Values(vec![])
    }

    /// Create a VALUES response with a single value (cache hit).
    #[inline]
    pub fn hit(key: &[u8], flags: u32, data: &[u8]) -> Self {
        Response::Values(vec![Value {
            key: key.to_vec(),
            flags,
            data: data.to_vec(),
            cas: None,
        }])
    }

    /// Create an ERROR response.
    #[inline]
    pub fn error() -> Self {
        Response::Error
    }

    /// Create a CLIENT_ERROR response.
    #[inline]
    pub fn client_error(msg: &[u8]) -> Self {
        Response::ClientError(msg.to_vec())
    }

    /// Create a SERVER_ERROR response.
    #[inline]
    pub fn server_error(msg: &[u8]) -> Self {
        Response::ServerError(msg.to_vec())
    }

    // ========================================================================
    // Type checks
    // ========================================================================

    /// Returns true if this is an error response.
    #[inline]
    pub fn is_error(&self) -> bool {
        matches!(
            self,
            Response::Error | Response::ClientError(_) | Response::ServerError(_)
        )
    }

    /// Returns true if this represents a cache miss.
    #[inline]
    pub fn is_miss(&self) -> bool {
        match self {
            Response::Values(values) => values.is_empty(),
            Response::NotFound => true,
            _ => false,
        }
    }

    /// Returns true if this is a successful storage response.
    #[inline]
    pub fn is_stored(&self) -> bool {
        matches!(self, Response::Stored)
    }

    // ========================================================================
    // Parsing (client-side)
    // ========================================================================

    /// Parse a response from a byte buffer.
    ///
    /// Returns the parsed response and the number of bytes consumed.
    ///
    /// Uses [`DEFAULT_MAX_RESPONSE_LINE_LEN`] as the line-length bound. A
    /// buffer that grows past it without producing a CRLF is rejected with
    /// `Protocol("line too long")` rather than reported as `Incomplete`
    /// forever. Use [`Self::parse_with_max_line_len`] to change the bound.
    #[inline]
    pub fn parse(data: &[u8]) -> Result<(Self, usize), ParseError> {
        Self::parse_with_max_line_len(data, DEFAULT_MAX_RESPONSE_LINE_LEN)
    }

    /// Parse a response with an explicit maximum line length.
    ///
    /// See [`Self::parse`] for the default.
    pub fn parse_with_max_line_len(
        data: &[u8],
        max_line_len: usize,
    ) -> Result<(Self, usize), ParseError> {
        // Find the first line
        let line_end = find_crlf(data, max_line_len)?.ok_or(ParseError::Incomplete)?;
        let line = &data[..line_end];

        // Check for simple responses first
        if line == b"STORED" {
            return Ok((Response::Stored, line_end + 2));
        }
        if line == b"NOT_STORED" {
            return Ok((Response::NotStored, line_end + 2));
        }
        if line == b"DELETED" {
            return Ok((Response::Deleted, line_end + 2));
        }
        if line == b"NOT_FOUND" {
            return Ok((Response::NotFound, line_end + 2));
        }
        if line == b"EXISTS" {
            return Ok((Response::Exists, line_end + 2));
        }
        if line == b"END" {
            // Empty GET response (miss)
            return Ok((Response::Values(vec![]), line_end + 2));
        }
        if line == b"OK" {
            return Ok((Response::Ok, line_end + 2));
        }
        if line == b"ERROR" {
            return Ok((Response::Error, line_end + 2));
        }
        if line.starts_with(b"CLIENT_ERROR ") {
            let msg = line[13..].to_vec();
            return Ok((Response::ClientError(msg), line_end + 2));
        }
        if line.starts_with(b"SERVER_ERROR ") {
            let msg = line[13..].to_vec();
            return Ok((Response::ServerError(msg), line_end + 2));
        }
        if line.starts_with(b"VERSION ") {
            let version = line[8..].to_vec();
            return Ok((Response::Version(version), line_end + 2));
        }

        // Check for VALUE response
        if line.starts_with(b"VALUE ") {
            return parse_value_response(data, max_line_len);
        }

        // Check for numeric response (INCR/DECR returns `<number>\r\n`)
        if !line.is_empty() && line.iter().all(|&b| b.is_ascii_digit()) {
            let value = parse_u64(line)?;
            return Ok((Response::Numeric(value), line_end + 2));
        }

        Err(ParseError::Protocol("unknown response"))
    }

    // ========================================================================
    // Encoding (server-side)
    // ========================================================================

    /// Encode this response into a buffer.
    ///
    /// Returns the number of bytes written.
    pub fn encode(&self, buf: &mut [u8]) -> usize {
        match self {
            Response::Stored => {
                buf[..8].copy_from_slice(b"STORED\r\n");
                8
            }
            Response::NotStored => {
                buf[..12].copy_from_slice(b"NOT_STORED\r\n");
                12
            }
            Response::Deleted => {
                buf[..9].copy_from_slice(b"DELETED\r\n");
                9
            }
            Response::NotFound => {
                buf[..11].copy_from_slice(b"NOT_FOUND\r\n");
                11
            }
            Response::Exists => {
                buf[..8].copy_from_slice(b"EXISTS\r\n");
                8
            }
            Response::Ok => {
                buf[..4].copy_from_slice(b"OK\r\n");
                4
            }
            Response::Numeric(value) => {
                let mut cursor = std::io::Cursor::new(&mut buf[..]);
                write!(cursor, "{}\r\n", value).unwrap();
                cursor.position() as usize
            }
            Response::Error => {
                buf[..7].copy_from_slice(b"ERROR\r\n");
                7
            }
            Response::ClientError(msg) => {
                let mut pos = 0;
                buf[pos..pos + 13].copy_from_slice(b"CLIENT_ERROR ");
                pos += 13;
                buf[pos..pos + msg.len()].copy_from_slice(msg);
                pos += msg.len();
                buf[pos..pos + 2].copy_from_slice(b"\r\n");
                pos + 2
            }
            Response::ServerError(msg) => {
                let mut pos = 0;
                buf[pos..pos + 13].copy_from_slice(b"SERVER_ERROR ");
                pos += 13;
                buf[pos..pos + msg.len()].copy_from_slice(msg);
                pos += msg.len();
                buf[pos..pos + 2].copy_from_slice(b"\r\n");
                pos + 2
            }
            Response::Version(v) => {
                let mut pos = 0;
                buf[pos..pos + 8].copy_from_slice(b"VERSION ");
                pos += 8;
                buf[pos..pos + v.len()].copy_from_slice(v);
                pos += v.len();
                buf[pos..pos + 2].copy_from_slice(b"\r\n");
                pos + 2
            }
            Response::Values(values) => encode_values(buf, values),
        }
    }

    // ========================================================================
    // Direct encoding helpers (for server hot path)
    // ========================================================================

    /// Encode STORED directly to buffer.
    #[inline]
    pub fn encode_stored(buf: &mut [u8]) -> usize {
        buf[..8].copy_from_slice(b"STORED\r\n");
        8
    }

    /// Encode NOT_STORED directly to buffer.
    #[inline]
    pub fn encode_not_stored(buf: &mut [u8]) -> usize {
        buf[..12].copy_from_slice(b"NOT_STORED\r\n");
        12
    }

    /// Encode DELETED directly to buffer.
    #[inline]
    pub fn encode_deleted(buf: &mut [u8]) -> usize {
        buf[..9].copy_from_slice(b"DELETED\r\n");
        9
    }

    /// Encode NOT_FOUND directly to buffer.
    #[inline]
    pub fn encode_not_found(buf: &mut [u8]) -> usize {
        buf[..11].copy_from_slice(b"NOT_FOUND\r\n");
        11
    }

    /// Encode END (empty get response) directly to buffer.
    #[inline]
    pub fn encode_end(buf: &mut [u8]) -> usize {
        buf[..5].copy_from_slice(b"END\r\n");
        5
    }

    /// Encode a single VALUE response directly to buffer.
    #[inline]
    pub fn encode_value(buf: &mut [u8], key: &[u8], flags: u32, data: &[u8]) -> usize {
        let mut pos = 0;

        // VALUE <key> <flags> <bytes>\r\n
        buf[pos..pos + 6].copy_from_slice(b"VALUE ");
        pos += 6;
        buf[pos..pos + key.len()].copy_from_slice(key);
        pos += key.len();
        buf[pos] = b' ';
        pos += 1;

        let mut cursor = std::io::Cursor::new(&mut buf[pos..]);
        write!(cursor, "{} {}\r\n", flags, data.len()).unwrap();
        pos += cursor.position() as usize;

        // <data>\r\n
        buf[pos..pos + data.len()].copy_from_slice(data);
        pos += data.len();
        buf[pos..pos + 2].copy_from_slice(b"\r\n");
        pos += 2;

        // END\r\n
        buf[pos..pos + 5].copy_from_slice(b"END\r\n");
        pos + 5
    }

    /// Encode a single VALUE response with CAS token directly to buffer (GETS response).
    ///
    /// Format: `VALUE <key> <flags> <bytes> <cas>\r\n<data>\r\nEND\r\n`
    #[inline]
    pub fn encode_value_with_cas(
        buf: &mut [u8],
        key: &[u8],
        flags: u32,
        data: &[u8],
        cas: u64,
    ) -> usize {
        let mut pos = 0;

        // VALUE <key> <flags> <bytes> <cas>\r\n
        buf[pos..pos + 6].copy_from_slice(b"VALUE ");
        pos += 6;
        buf[pos..pos + key.len()].copy_from_slice(key);
        pos += key.len();
        buf[pos] = b' ';
        pos += 1;

        let mut cursor = std::io::Cursor::new(&mut buf[pos..]);
        write!(cursor, "{} {} {}\r\n", flags, data.len(), cas).unwrap();
        pos += cursor.position() as usize;

        // <data>\r\n
        buf[pos..pos + data.len()].copy_from_slice(data);
        pos += data.len();
        buf[pos..pos + 2].copy_from_slice(b"\r\n");
        pos += 2;

        // END\r\n
        buf[pos..pos + 5].copy_from_slice(b"END\r\n");
        pos + 5
    }

    /// Encode EXISTS response directly to buffer.
    #[inline]
    pub fn encode_exists(buf: &mut [u8]) -> usize {
        buf[..8].copy_from_slice(b"EXISTS\r\n");
        8
    }

    /// Encode a numeric response directly to buffer (INCR/DECR result).
    #[inline]
    pub fn encode_numeric(buf: &mut [u8], value: u64) -> usize {
        let mut cursor = std::io::Cursor::new(&mut buf[..]);
        write!(cursor, "{}\r\n", value).unwrap();
        cursor.position() as usize
    }

    /// Encode a SERVER_ERROR response directly to buffer.
    #[inline]
    pub fn encode_server_error(buf: &mut [u8], msg: &[u8]) -> usize {
        let mut pos = 0;
        buf[pos..pos + 13].copy_from_slice(b"SERVER_ERROR ");
        pos += 13;
        buf[pos..pos + msg.len()].copy_from_slice(msg);
        pos += msg.len();
        buf[pos..pos + 2].copy_from_slice(b"\r\n");
        pos + 2
    }
}

/// Find the first `\r\n` in `data`, returning the position of the `\r`.
///
/// A bare `\r` not followed by `\n` is ordinary line content, so the scan
/// continues past it; only a real CRLF terminates a line. Returns `None` when
/// no CRLF is present yet, which callers treat as "need more data".
fn find_crlf(data: &[u8], max_line_len: usize) -> Result<Option<usize>, ParseError> {
    if let Some(pos) = memchr::memmem::find(data, b"\r\n") {
        return Ok(Some(pos));
    }

    // No CRLF found - check if we've exceeded the line length limit
    if data.len() > max_line_len {
        return Err(ParseError::Protocol("line too long"));
    }

    Ok(None)
}

/// Default maximum length of a single ASCII response line, in bytes.
///
/// The longest legitimate response line is a VALUE header --
/// `VALUE <key> <flags> <bytes> [<cas>]` -- which is bounded by the 250-byte
/// key limit at roughly 310 bytes. This default leaves generous headroom for
/// long `SERVER_ERROR` text while still bounding how much a peer can make a
/// caller buffer before the line is rejected.
pub const DEFAULT_MAX_RESPONSE_LINE_LEN: usize = 8192;

/// Parse a VALUE response (potentially with multiple values).
fn parse_value_response(data: &[u8], max_line_len: usize) -> Result<(Response, usize), ParseError> {
    let mut values = Vec::new();
    let mut pos = 0;

    loop {
        // Find the line end
        let remaining = &data[pos..];
        let line_end = find_crlf(remaining, max_line_len)?.ok_or(ParseError::Incomplete)?;
        let line = &remaining[..line_end];

        // Check for END
        if line == b"END" {
            pos += line_end + 2;
            break;
        }

        // Parse VALUE line: VALUE <key> <flags> <bytes> [<cas unique>]
        if !line.starts_with(b"VALUE ") {
            return Err(ParseError::Protocol("expected VALUE or END"));
        }

        let parts: Vec<&[u8]> = line[6..].split(|&b| b == b' ').collect();
        if parts.len() < 3 {
            return Err(ParseError::Protocol("invalid VALUE line"));
        }

        let key = parts[0].to_vec();
        let flags = parse_u32(parts[1])?;
        let bytes = parse_usize(parts[2])?;
        let cas = if parts.len() >= 4 {
            Some(parse_u64(parts[3])?)
        } else {
            None
        };

        // Move past the VALUE line
        pos += line_end + 2;

        // Read the data
        let data_end = pos + bytes;
        if data.len() < data_end + 2 {
            return Err(ParseError::Incomplete);
        }

        // Verify trailing \r\n
        if data[data_end] != b'\r' || data[data_end + 1] != b'\n' {
            return Err(ParseError::Protocol("missing data terminator"));
        }

        let value_data = data[pos..data_end].to_vec();
        pos = data_end + 2;

        values.push(Value {
            key,
            flags,
            data: value_data,
            cas,
        });
    }

    Ok((Response::Values(values), pos))
}

// ========================================================================
// Zero-copy Bytes-based types and parsing
// ========================================================================

/// A single value from a GET response, using zero-copy `Bytes` references.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValueBytes {
    pub key: Bytes,
    pub flags: u32,
    pub data: Bytes,
    /// CAS unique token, present when the response is from a `gets` command.
    pub cas: Option<u64>,
}

/// A parsed Memcache response using zero-copy `Bytes` for string fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResponseBytes {
    /// Value response from GET (may contain multiple values for multi-get)
    Values(Vec<ValueBytes>),
    /// STORED response from SET
    Stored,
    /// NOT_STORED response from SET
    NotStored,
    /// DELETED response from DELETE
    Deleted,
    /// NOT_FOUND response from DELETE
    NotFound,
    /// EXISTS response from CAS
    Exists,
    /// OK response (from flush_all, touch, etc.)
    Ok,
    /// Numeric response from INCR/DECR (the new value after the operation).
    Numeric(u64),
    /// VERSION response
    Version(Bytes),
    /// Generic error
    Error,
    /// Client error with message
    ClientError(Bytes),
    /// Server error with message
    ServerError(Bytes),
}

impl ResponseBytes {
    /// Returns true if this is an error response.
    #[inline]
    pub fn is_error(&self) -> bool {
        matches!(
            self,
            ResponseBytes::Error | ResponseBytes::ClientError(_) | ResponseBytes::ServerError(_)
        )
    }

    /// Returns true if this represents a cache miss.
    #[inline]
    pub fn is_miss(&self) -> bool {
        match self {
            ResponseBytes::Values(values) => values.is_empty(),
            ResponseBytes::NotFound => true,
            _ => false,
        }
    }

    /// Returns true if this is a successful storage response.
    #[inline]
    pub fn is_stored(&self) -> bool {
        matches!(self, ResponseBytes::Stored)
    }

    /// Parse a response from a `Bytes` buffer, returning zero-copy slices.
    ///
    /// Same parsing logic as [`Response::parse`] but returns `Bytes::slice()`
    /// references into the input buffer instead of copying into `Vec<u8>`.
    ///
    /// Uses [`DEFAULT_MAX_RESPONSE_LINE_LEN`] as the line-length bound; see
    /// [`Self::parse_with_max_line_len`] to change it.
    #[inline]
    pub fn parse(data: Bytes) -> Result<(Self, usize), ParseError> {
        Self::parse_with_max_line_len(data, DEFAULT_MAX_RESPONSE_LINE_LEN)
    }

    /// Parse a response from a `Bytes` buffer with an explicit maximum line length.
    pub fn parse_with_max_line_len(
        data: Bytes,
        max_line_len: usize,
    ) -> Result<(Self, usize), ParseError> {
        let line_end = find_crlf(&data, max_line_len)?.ok_or(ParseError::Incomplete)?;
        let line = &data[..line_end];

        if line == b"STORED" {
            return Ok((ResponseBytes::Stored, line_end + 2));
        }
        if line == b"NOT_STORED" {
            return Ok((ResponseBytes::NotStored, line_end + 2));
        }
        if line == b"DELETED" {
            return Ok((ResponseBytes::Deleted, line_end + 2));
        }
        if line == b"NOT_FOUND" {
            return Ok((ResponseBytes::NotFound, line_end + 2));
        }
        if line == b"EXISTS" {
            return Ok((ResponseBytes::Exists, line_end + 2));
        }
        if line == b"END" {
            return Ok((ResponseBytes::Values(vec![]), line_end + 2));
        }
        if line == b"OK" {
            return Ok((ResponseBytes::Ok, line_end + 2));
        }
        if line == b"ERROR" {
            return Ok((ResponseBytes::Error, line_end + 2));
        }
        if line.starts_with(b"CLIENT_ERROR ") {
            let msg = data.slice(13..line_end);
            return Ok((ResponseBytes::ClientError(msg), line_end + 2));
        }
        if line.starts_with(b"SERVER_ERROR ") {
            let msg = data.slice(13..line_end);
            return Ok((ResponseBytes::ServerError(msg), line_end + 2));
        }
        if line.starts_with(b"VERSION ") {
            let version = data.slice(8..line_end);
            return Ok((ResponseBytes::Version(version), line_end + 2));
        }

        if line.starts_with(b"VALUE ") {
            return parse_value_response_bytes(&data, max_line_len);
        }

        if !line.is_empty() && line.iter().all(|&b| b.is_ascii_digit()) {
            let value = parse_u64(line)?;
            return Ok((ResponseBytes::Numeric(value), line_end + 2));
        }

        Err(ParseError::Protocol("unknown response"))
    }

    /// Parse a response from bytes that arrived in several pieces.
    ///
    /// `segments` are consecutive pieces of one byte stream, in order, as a
    /// receive path that does not copy them into one buffer delivers them.
    /// The result is the same as [`parse`](Self::parse) on the pieces joined:
    /// the same response, the same number of bytes consumed (counted across
    /// pieces), and the same errors. A key, value or message within one piece
    /// is a `Bytes::slice()` reference into it; one that spans pieces is
    /// copied into a new `Bytes`.
    ///
    /// Each call is linear in the number of pieces. Piece offsets are kept on
    /// the stack for up to eight pieces and in a heap buffer above that, and a
    /// line or field that spans pieces is copied. A caller that retries after
    /// [`ParseError::Incomplete`] as pieces arrive should join small pieces
    /// first, or the retries cost the square of the piece count.
    #[inline]
    pub fn parse_segments(segments: &[Bytes]) -> Result<(Self, usize), ParseError> {
        Self::parse_segments_with_max_line_len(segments, DEFAULT_MAX_RESPONSE_LINE_LEN)
    }

    /// Parse a response from bytes in several pieces, with an explicit
    /// maximum line length. See [`parse_segments`](Self::parse_segments).
    pub fn parse_segments_with_max_line_len(
        segments: &[Bytes],
        max_line_len: usize,
    ) -> Result<(Self, usize), ParseError> {
        if let [one] = segments {
            return Self::parse_with_max_line_len(one.clone(), max_line_len);
        }
        parse_response_at(&Segments::new(segments), max_line_len)
    }

    /// Find the end of the response at the start of `segments` without
    /// building it.
    ///
    /// Returns [`Frame::Complete`] with the byte count
    /// [`parse_segments`](Self::parse_segments) would consume, or
    /// [`Frame::Incomplete`] with a lower bound on the bytes still needed,
    /// and the same errors that parse would return. It does not copy a
    /// value's data. It allocates the piece offset table above eight pieces
    /// and a copy of each line that spans pieces. A caller that receives
    /// bytes in pieces can call it on each arrival and parse once the
    /// response is complete: a parse of an incomplete response copies each
    /// key and value that spans pieces, and then discards the copies.
    #[inline]
    pub fn scan_segments(segments: &[Bytes]) -> Result<Frame, ParseError> {
        Self::scan_segments_with_max_line_len(segments, DEFAULT_MAX_RESPONSE_LINE_LEN)
    }

    /// Find the end of a response in several pieces, with an explicit
    /// maximum line length. See [`scan_segments`](Self::scan_segments).
    pub fn scan_segments_with_max_line_len(
        segments: &[Bytes],
        max_line_len: usize,
    ) -> Result<Frame, ParseError> {
        scan_response_at(&Segments::new(segments), max_line_len)
    }
}

/// The result of [`ResponseBytes::scan_segments`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Frame {
    /// The input starts with a complete response of this many bytes.
    Complete(usize),
    /// The input is the start of a response that needs at least this many
    /// more bytes.
    Incomplete(usize),
}

/// The bytes in the shortest response (`0\r\n`, a numeric reply).
const MIN_RESPONSE_LEN: usize = 3;

/// The bytes that end a `VALUE` response: `END\r\n`.
const END_LEN: usize = 5;

/// The position of the CR that ends the line at `pos`, or, when the input
/// holds no CRLF after `pos` yet, the bytes still needed: 1 after a final CR,
/// 2 otherwise.
#[inline]
fn scan_line<I: Input>(
    inp: &I,
    pos: usize,
    max_line_len: usize,
) -> Result<Result<usize, usize>, ParseError> {
    match inp.find_crlf(pos, max_line_len)? {
        Some(cr) => Ok(Ok(cr)),
        None if pos < inp.len() && inp.byte(inp.len() - 1) == b'\r' => Ok(Err(1)),
        None => Ok(Err(2)),
    }
}

/// Scan one response from the start of `inp`, with the checks of
/// [`parse_response_at`] in its order.
fn scan_response_at<I: Input>(inp: &I, max_line_len: usize) -> Result<Frame, ParseError> {
    if inp.len() == 0 {
        return Ok(Frame::Incomplete(MIN_RESPONSE_LEN));
    }
    let line_end = match scan_line(inp, 0, max_line_len)? {
        Ok(end) => end,
        Err(short) => return Ok(Frame::Incomplete(short)),
    };
    match inp.with_range(0, line_end, classify_first_line)? {
        FirstLine::Values => scan_value_response_at(inp, max_line_len),
        _ => Ok(Frame::Complete(line_end + 2)),
    }
}

/// [`parse_value_line`]'s checks, in its order, without collecting the
/// fields: the data length.
#[inline]
fn scan_value_line(line: &[u8]) -> Result<usize, ParseError> {
    if !line.starts_with(b"VALUE ") {
        return Err(ParseError::Protocol("expected VALUE or END"));
    }
    let mut parts = line[6..].split(|&b| b == b' ');
    let _key = parts.next();
    let (Some(flags), Some(bytes)) = (parts.next(), parts.next()) else {
        return Err(ParseError::Protocol("invalid VALUE line"));
    };
    parse_u32(flags)?;
    let bytes = parse_usize(bytes)?;
    if let Some(cas) = parts.next() {
        parse_u64(cas)?;
    }
    Ok(bytes)
}

/// Scan a VALUE response, with the checks of [`parse_value_response_at`] in
/// its order.
fn scan_value_response_at<I: Input>(inp: &I, max_line_len: usize) -> Result<Frame, ParseError> {
    let mut pos = 0;
    loop {
        if pos >= inp.len() {
            return Ok(Frame::Incomplete(END_LEN));
        }
        let line_end = match scan_line(inp, pos, max_line_len)? {
            Ok(end) => end,
            Err(short) => return Ok(Frame::Incomplete(short)),
        };
        let bytes = inp.with_range(pos, line_end, |line| {
            if line == b"END" {
                Ok(None)
            } else {
                scan_value_line(line).map(Some)
            }
        })?;
        let Some(bytes) = bytes else {
            return Ok(Frame::Complete(line_end + 2));
        };

        pos = line_end + 2;
        let data_end = pos + bytes;
        if inp.len() < data_end + 2 {
            // The data, its CRLF and at least `END\r\n`.
            return Ok(Frame::Incomplete(data_end + 2 + END_LEN - inp.len()));
        }
        if inp.byte(data_end) != b'\r' || inp.byte(data_end + 1) != b'\n' {
            return Err(ParseError::Protocol("missing data terminator"));
        }
        pos = data_end + 2;
    }
}

/// Parse a VALUE response producing zero-copy `ValueBytes` with `Bytes::slice()`.
fn parse_value_response_bytes(
    data: &Bytes,
    max_line_len: usize,
) -> Result<(ResponseBytes, usize), ParseError> {
    let mut values = Vec::new();
    let mut pos = 0;

    loop {
        let remaining = &data[pos..];
        let line_end = find_crlf(remaining, max_line_len)?.ok_or(ParseError::Incomplete)?;
        let line = &remaining[..line_end];

        if line == b"END" {
            pos += line_end + 2;
            break;
        }

        if !line.starts_with(b"VALUE ") {
            return Err(ParseError::Protocol("expected VALUE or END"));
        }

        // Parse: VALUE <key> <flags> <bytes> [<cas>]
        // We need absolute offsets into `data` for Bytes::slice().
        let header_start = pos + 6; // skip "VALUE "
        let header_end = pos + line_end;
        let header = &data[header_start..header_end];

        let parts: Vec<&[u8]> = header.split(|&b| b == b' ').collect();
        if parts.len() < 3 {
            return Err(ParseError::Protocol("invalid VALUE line"));
        }

        // Compute absolute offset of the key within `data`.
        let key_start = header_start;
        let key_end = key_start + parts[0].len();
        let key = data.slice(key_start..key_end);

        let flags = parse_u32(parts[1])?;
        let bytes = parse_usize(parts[2])?;
        let cas = if parts.len() >= 4 {
            Some(parse_u64(parts[3])?)
        } else {
            None
        };

        // Move past the VALUE line
        pos += line_end + 2;

        // Read the data
        let data_end = pos + bytes;
        if data.len() < data_end + 2 {
            return Err(ParseError::Incomplete);
        }

        if data[data_end] != b'\r' || data[data_end + 1] != b'\n' {
            return Err(ParseError::Protocol("missing data terminator"));
        }

        let value_data = data.slice(pos..data_end);
        pos = data_end + 2;

        values.push(ValueBytes {
            key,
            flags,
            data: value_data,
            cas,
        });
    }

    Ok((ResponseBytes::Values(values), pos))
}

// ========================================================================
// Segmented parsing
// ========================================================================
//
// `parse_segments` with more than one piece parses over an `Input`. One piece,
// and `parse`, use the contiguous functions above; tests/segments.rs compares
// both with the copy parser.

/// The kind of a response, decided from its first line.
enum FirstLine {
    Simple(ResponseBytes),
    /// `CLIENT_ERROR`, `SERVER_ERROR` or `VERSION`: the message starts this
    /// many bytes into the line.
    Message(fn(Bytes) -> ResponseBytes, usize),
    Values,
    Numeric(u64),
}

#[inline]
fn classify_first_line(line: &[u8]) -> Result<FirstLine, ParseError> {
    let simple = match line {
        b"STORED" => Some(ResponseBytes::Stored),
        b"NOT_STORED" => Some(ResponseBytes::NotStored),
        b"DELETED" => Some(ResponseBytes::Deleted),
        b"NOT_FOUND" => Some(ResponseBytes::NotFound),
        b"EXISTS" => Some(ResponseBytes::Exists),
        b"END" => Some(ResponseBytes::Values(vec![])),
        b"OK" => Some(ResponseBytes::Ok),
        b"ERROR" => Some(ResponseBytes::Error),
        _ => None,
    };
    if let Some(response) = simple {
        return Ok(FirstLine::Simple(response));
    }
    if line.starts_with(b"CLIENT_ERROR ") {
        return Ok(FirstLine::Message(ResponseBytes::ClientError, 13));
    }
    if line.starts_with(b"SERVER_ERROR ") {
        return Ok(FirstLine::Message(ResponseBytes::ServerError, 13));
    }
    if line.starts_with(b"VERSION ") {
        return Ok(FirstLine::Message(ResponseBytes::Version, 8));
    }
    if line.starts_with(b"VALUE ") {
        return Ok(FirstLine::Values);
    }
    if !line.is_empty() && line.iter().all(|&b| b.is_ascii_digit()) {
        return Ok(FirstLine::Numeric(parse_u64(line)?));
    }
    Err(ParseError::Protocol("unknown response"))
}

/// Parse one response from the start of `inp`.
fn parse_response_at<I: Input>(
    inp: &I,
    max_line_len: usize,
) -> Result<(ResponseBytes, usize), ParseError> {
    let line_end = inp
        .find_crlf(0, max_line_len)?
        .ok_or(ParseError::Incomplete)?;
    match inp.with_range(0, line_end, classify_first_line)? {
        FirstLine::Simple(response) => Ok((response, line_end + 2)),
        FirstLine::Message(make, start) => Ok((make(inp.slice(start, line_end)), line_end + 2)),
        FirstLine::Values => parse_value_response_at(inp, max_line_len),
        FirstLine::Numeric(value) => Ok((ResponseBytes::Numeric(value), line_end + 2)),
    }
}

/// The fields of a `VALUE <key> <flags> <bytes> [<cas>]` line: the key's
/// length, the flags, the data length and the CAS token.
#[inline]
fn parse_value_line(line: &[u8]) -> Result<(usize, u32, usize, Option<u64>), ParseError> {
    if !line.starts_with(b"VALUE ") {
        return Err(ParseError::Protocol("expected VALUE or END"));
    }
    let parts: Vec<&[u8]> = line[6..].split(|&b| b == b' ').collect();
    if parts.len() < 3 {
        return Err(ParseError::Protocol("invalid VALUE line"));
    }
    let flags = parse_u32(parts[1])?;
    let bytes = parse_usize(parts[2])?;
    let cas = if parts.len() >= 4 {
        Some(parse_u64(parts[3])?)
    } else {
        None
    };
    Ok((parts[0].len(), flags, bytes, cas))
}

/// Parse a VALUE response producing zero-copy `ValueBytes`.
fn parse_value_response_at<I: Input>(
    inp: &I,
    max_line_len: usize,
) -> Result<(ResponseBytes, usize), ParseError> {
    let mut values = Vec::new();
    let mut pos = 0;

    loop {
        let line_end = inp
            .find_crlf(pos, max_line_len)?
            .ok_or(ParseError::Incomplete)?;
        let fields = inp.with_range(pos, line_end, |line| {
            if line == b"END" {
                Ok(None)
            } else {
                parse_value_line(line).map(Some)
            }
        })?;
        let Some((key_len, flags, bytes, cas)) = fields else {
            pos = line_end + 2;
            break;
        };

        let key_start = pos + 6; // skip "VALUE "
        let key = inp.slice(key_start, key_start + key_len);

        // Move past the VALUE line
        pos = line_end + 2;

        // Read the data
        let data_end = pos + bytes;
        if inp.len() < data_end + 2 {
            return Err(ParseError::Incomplete);
        }

        if inp.byte(data_end) != b'\r' || inp.byte(data_end + 1) != b'\n' {
            return Err(ParseError::Protocol("missing data terminator"));
        }

        let value_data = inp.slice(pos, data_end);
        pos = data_end + 2;

        values.push(ValueBytes {
            key,
            flags,
            data: value_data,
            cas,
        });
    }

    Ok((ResponseBytes::Values(values), pos))
}

/// Encode multiple values as a response.
fn encode_values(buf: &mut [u8], values: &[Value]) -> usize {
    let mut pos = 0;

    for value in values {
        // VALUE <key> <flags> <bytes> [<cas>]\r\n
        buf[pos..pos + 6].copy_from_slice(b"VALUE ");
        pos += 6;
        buf[pos..pos + value.key.len()].copy_from_slice(&value.key);
        pos += value.key.len();
        buf[pos] = b' ';
        pos += 1;

        let mut cursor = std::io::Cursor::new(&mut buf[pos..]);
        if let Some(cas) = value.cas {
            write!(cursor, "{} {} {}\r\n", value.flags, value.data.len(), cas).unwrap();
        } else {
            write!(cursor, "{} {}\r\n", value.flags, value.data.len()).unwrap();
        }
        pos += cursor.position() as usize;

        // <data>\r\n
        buf[pos..pos + value.data.len()].copy_from_slice(&value.data);
        pos += value.data.len();
        buf[pos..pos + 2].copy_from_slice(b"\r\n");
        pos += 2;
    }

    // END\r\n
    buf[pos..pos + 5].copy_from_slice(b"END\r\n");
    pos + 5
}

/// Parse a u32 from ASCII decimal.
fn parse_u32(data: &[u8]) -> Result<u32, ParseError> {
    std::str::from_utf8(data)
        .map_err(|_| ParseError::InvalidNumber)?
        .parse()
        .map_err(|_| ParseError::InvalidNumber)
}

/// Parse a u64 from ASCII decimal.
fn parse_u64(data: &[u8]) -> Result<u64, ParseError> {
    std::str::from_utf8(data)
        .map_err(|_| ParseError::InvalidNumber)?
        .parse()
        .map_err(|_| ParseError::InvalidNumber)
}

/// Maximum value data size (1MB, matching DEFAULT_MAX_VALUE_LEN).
const MAX_VALUE_DATA_LEN: usize = 1024 * 1024;

/// Parse a usize from ASCII decimal, with a maximum limit.
fn parse_usize(data: &[u8]) -> Result<usize, ParseError> {
    let value: usize = std::str::from_utf8(data)
        .map_err(|_| ParseError::InvalidNumber)?
        .parse()
        .map_err(|_| ParseError::InvalidNumber)?;

    if value > MAX_VALUE_DATA_LEN {
        return Err(ParseError::Protocol("value data too large"));
    }

    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_stored() {
        let (resp, consumed) = Response::parse(b"STORED\r\n").unwrap();
        assert_eq!(resp, Response::Stored);
        assert_eq!(consumed, 8);
    }

    #[test]
    fn test_parse_not_stored() {
        let (resp, consumed) = Response::parse(b"NOT_STORED\r\n").unwrap();
        assert_eq!(resp, Response::NotStored);
        assert_eq!(consumed, 12);
    }

    #[test]
    fn test_parse_deleted() {
        let (resp, consumed) = Response::parse(b"DELETED\r\n").unwrap();
        assert_eq!(resp, Response::Deleted);
        assert_eq!(consumed, 9);
    }

    #[test]
    fn test_parse_not_found() {
        let (resp, consumed) = Response::parse(b"NOT_FOUND\r\n").unwrap();
        assert_eq!(resp, Response::NotFound);
        assert_eq!(consumed, 11);
    }

    #[test]
    fn test_parse_end() {
        let (resp, consumed) = Response::parse(b"END\r\n").unwrap();
        assert_eq!(resp, Response::Values(vec![]));
        assert_eq!(consumed, 5);
        assert!(resp.is_miss());
    }

    #[test]
    fn test_parse_value() {
        let data = b"VALUE mykey 0 7\r\nmyvalue\r\nEND\r\n";
        let (resp, consumed) = Response::parse(data).unwrap();
        assert_eq!(consumed, data.len());
        match resp {
            Response::Values(values) => {
                assert_eq!(values.len(), 1);
                assert_eq!(values[0].key, b"mykey");
                assert_eq!(values[0].flags, 0);
                assert_eq!(values[0].data, b"myvalue");
            }
            _ => panic!("expected Values"),
        }
    }

    #[test]
    fn test_parse_multi_value() {
        let data = b"VALUE key1 0 3\r\nfoo\r\nVALUE key2 0 3\r\nbar\r\nEND\r\n";
        let (resp, consumed) = Response::parse(data).unwrap();
        assert_eq!(consumed, data.len());
        match resp {
            Response::Values(values) => {
                assert_eq!(values.len(), 2);
                assert_eq!(values[0].key, b"key1");
                assert_eq!(values[0].data, b"foo");
                assert_eq!(values[1].key, b"key2");
                assert_eq!(values[1].data, b"bar");
            }
            _ => panic!("expected Values"),
        }
    }

    #[test]
    fn test_parse_error() {
        let (resp, _) = Response::parse(b"ERROR\r\n").unwrap();
        assert!(resp.is_error());
    }

    #[test]
    fn test_parse_server_error() {
        let (resp, _) = Response::parse(b"SERVER_ERROR out of memory\r\n").unwrap();
        assert!(resp.is_error());
        match resp {
            Response::ServerError(msg) => assert_eq!(msg, b"out of memory"),
            _ => panic!("expected ServerError"),
        }
    }

    #[test]
    fn test_parse_incomplete() {
        assert!(matches!(
            Response::parse(b"VALUE mykey 0 7\r\nmyval"),
            Err(ParseError::Incomplete)
        ));
    }

    #[test]
    fn test_encode_stored() {
        let mut buf = [0u8; 64];
        let len = Response::stored().encode(&mut buf);
        assert_eq!(&buf[..len], b"STORED\r\n");
    }

    #[test]
    fn test_encode_deleted() {
        let mut buf = [0u8; 64];
        let len = Response::deleted().encode(&mut buf);
        assert_eq!(&buf[..len], b"DELETED\r\n");
    }

    #[test]
    fn test_encode_value() {
        let mut buf = [0u8; 128];
        let len = Response::encode_value(&mut buf, b"mykey", 0, b"myvalue");
        assert_eq!(&buf[..len], b"VALUE mykey 0 7\r\nmyvalue\r\nEND\r\n");
    }

    #[test]
    fn test_encode_miss() {
        let mut buf = [0u8; 64];
        let len = Response::miss().encode(&mut buf);
        assert_eq!(&buf[..len], b"END\r\n");
    }

    #[test]
    fn test_roundtrip() {
        let mut buf = [0u8; 256];

        // Test simple responses
        let responses = vec![
            Response::stored(),
            Response::not_stored(),
            Response::deleted(),
            Response::not_found(),
            Response::ok(),
            Response::numeric(0),
            Response::numeric(42),
            Response::numeric(18446744073709551615),
            Response::error(),
            Response::miss(),
        ];

        for original in responses {
            let len = original.encode(&mut buf);
            let (parsed, consumed) = Response::parse(&buf[..len]).unwrap();
            assert_eq!(original, parsed);
            assert_eq!(len, consumed);
        }
    }

    // Additional tests for improved coverage

    #[test]
    fn test_parse_exists() {
        let (resp, consumed) = Response::parse(b"EXISTS\r\n").unwrap();
        assert_eq!(resp, Response::Exists);
        assert_eq!(consumed, 8);
    }

    #[test]
    fn test_parse_client_error() {
        let (resp, consumed) = Response::parse(b"CLIENT_ERROR bad request\r\n").unwrap();
        assert!(resp.is_error());
        assert_eq!(consumed, 26);
        match resp {
            Response::ClientError(msg) => assert_eq!(msg, b"bad request"),
            _ => panic!("expected ClientError"),
        }
    }

    #[test]
    fn test_parse_version() {
        let (resp, consumed) = Response::parse(b"VERSION 1.6.9\r\n").unwrap();
        assert_eq!(consumed, 15);
        match resp {
            Response::Version(v) => assert_eq!(v, b"1.6.9"),
            _ => panic!("expected Version"),
        }
    }

    #[test]
    fn test_parse_unknown_response() {
        let result = Response::parse(b"UNKNOWN\r\n");
        assert!(matches!(
            result,
            Err(ParseError::Protocol("unknown response"))
        ));
    }

    #[test]
    fn test_parse_incomplete_no_crlf() {
        assert!(matches!(
            Response::parse(b"STORED"),
            Err(ParseError::Incomplete)
        ));
    }

    #[test]
    fn test_parse_value_incomplete_data() {
        // VALUE line complete but data incomplete
        assert!(matches!(
            Response::parse(b"VALUE k 0 10\r\nshort\r\nEND\r\n"),
            Err(ParseError::Incomplete)
        ));
    }

    #[test]
    fn test_parse_value_missing_terminator() {
        // Data is correct length but missing \r\n terminator
        assert!(matches!(
            Response::parse(b"VALUE k 0 5\r\nhelloXXEND\r\n"),
            Err(ParseError::Protocol("missing data terminator"))
        ));
    }

    #[test]
    fn test_parse_value_invalid_format() {
        // VALUE line with too few parts
        assert!(matches!(
            Response::parse(b"VALUE k\r\nEND\r\n"),
            Err(ParseError::Protocol("invalid VALUE line"))
        ));
    }

    #[test]
    fn test_parse_value_invalid_flags() {
        // Non-numeric flags
        assert!(matches!(
            Response::parse(b"VALUE k abc 5\r\nhello\r\nEND\r\n"),
            Err(ParseError::InvalidNumber)
        ));
    }

    #[test]
    fn test_parse_value_invalid_bytes() {
        // Non-numeric bytes
        assert!(matches!(
            Response::parse(b"VALUE k 0 xyz\r\nhello\r\nEND\r\n"),
            Err(ParseError::InvalidNumber)
        ));
    }

    #[test]
    fn test_parse_value_expected_end() {
        // After VALUE data, expect END or another VALUE
        assert!(matches!(
            Response::parse(b"VALUE k 0 5\r\nhello\r\nSTORED\r\n"),
            Err(ParseError::Protocol("expected VALUE or END"))
        ));
    }

    #[test]
    fn test_parse_ok() {
        let (resp, consumed) = Response::parse(b"OK\r\n").unwrap();
        assert_eq!(resp, Response::Ok);
        assert_eq!(consumed, 4);
    }

    #[test]
    fn test_encode_ok() {
        let mut buf = [0u8; 64];
        let len = Response::ok().encode(&mut buf);
        assert_eq!(&buf[..len], b"OK\r\n");
    }

    #[test]
    fn test_parse_numeric() {
        let (resp, consumed) = Response::parse(b"42\r\n").unwrap();
        assert_eq!(resp, Response::Numeric(42));
        assert_eq!(consumed, 4);
    }

    #[test]
    fn test_parse_numeric_zero() {
        let (resp, consumed) = Response::parse(b"0\r\n").unwrap();
        assert_eq!(resp, Response::Numeric(0));
        assert_eq!(consumed, 3);
    }

    #[test]
    fn test_parse_numeric_large() {
        let (resp, consumed) = Response::parse(b"18446744073709551615\r\n").unwrap();
        assert_eq!(resp, Response::Numeric(u64::MAX));
        assert_eq!(consumed, 22);
    }

    #[test]
    fn test_encode_numeric() {
        let mut buf = [0u8; 64];
        let len = Response::numeric(42).encode(&mut buf);
        assert_eq!(&buf[..len], b"42\r\n");
    }

    #[test]
    fn test_encode_numeric_zero() {
        let mut buf = [0u8; 64];
        let len = Response::numeric(0).encode(&mut buf);
        assert_eq!(&buf[..len], b"0\r\n");
    }

    #[test]
    fn test_direct_encode_numeric() {
        let mut buf = [0u8; 64];
        let len = Response::encode_numeric(&mut buf, 12345);
        assert_eq!(&buf[..len], b"12345\r\n");
    }

    #[test]
    fn test_is_error_variants() {
        assert!(Response::Error.is_error());
        assert!(Response::ClientError(b"msg".to_vec()).is_error());
        assert!(Response::ServerError(b"msg".to_vec()).is_error());
        assert!(!Response::Stored.is_error());
        assert!(!Response::NotStored.is_error());
        assert!(!Response::Deleted.is_error());
        assert!(!Response::NotFound.is_error());
        assert!(!Response::Exists.is_error());
        assert!(!Response::Ok.is_error());
        assert!(!Response::Numeric(42).is_error());
        assert!(!Response::Values(vec![]).is_error());
        assert!(!Response::Version(b"1.0".to_vec()).is_error());
    }

    #[test]
    fn test_is_miss_variants() {
        // Empty values is a miss
        assert!(Response::Values(vec![]).is_miss());
        // NotFound is a miss
        assert!(Response::NotFound.is_miss());
        // Non-empty values is not a miss
        assert!(
            !Response::Values(vec![Value {
                key: b"k".to_vec(),
                flags: 0,
                data: b"v".to_vec(),
                cas: None,
            }])
            .is_miss()
        );
        // Other responses are not misses
        assert!(!Response::Stored.is_miss());
        assert!(!Response::Deleted.is_miss());
        assert!(!Response::Ok.is_miss());
        assert!(!Response::Numeric(0).is_miss());
        assert!(!Response::Error.is_miss());
    }

    #[test]
    fn test_is_stored_variants() {
        assert!(Response::Stored.is_stored());
        assert!(!Response::NotStored.is_stored());
        assert!(!Response::Deleted.is_stored());
        assert!(!Response::Error.is_stored());
    }

    #[test]
    fn test_encode_not_stored() {
        let mut buf = [0u8; 64];
        let len = Response::not_stored().encode(&mut buf);
        assert_eq!(&buf[..len], b"NOT_STORED\r\n");
    }

    #[test]
    fn test_encode_not_found() {
        let mut buf = [0u8; 64];
        let len = Response::not_found().encode(&mut buf);
        assert_eq!(&buf[..len], b"NOT_FOUND\r\n");
    }

    #[test]
    fn test_encode_exists() {
        let mut buf = [0u8; 64];
        let len = Response::Exists.encode(&mut buf);
        assert_eq!(&buf[..len], b"EXISTS\r\n");
    }

    #[test]
    fn test_encode_error() {
        let mut buf = [0u8; 64];
        let len = Response::error().encode(&mut buf);
        assert_eq!(&buf[..len], b"ERROR\r\n");
    }

    #[test]
    fn test_encode_client_error() {
        let mut buf = [0u8; 64];
        let len = Response::client_error(b"bad request").encode(&mut buf);
        assert_eq!(&buf[..len], b"CLIENT_ERROR bad request\r\n");
    }

    #[test]
    fn test_encode_server_error() {
        let mut buf = [0u8; 64];
        let len = Response::server_error(b"out of memory").encode(&mut buf);
        assert_eq!(&buf[..len], b"SERVER_ERROR out of memory\r\n");
    }

    #[test]
    fn test_encode_version() {
        let mut buf = [0u8; 64];
        let len = Response::Version(b"1.6.9".to_vec()).encode(&mut buf);
        assert_eq!(&buf[..len], b"VERSION 1.6.9\r\n");
    }

    #[test]
    fn test_encode_hit() {
        let mut buf = [0u8; 128];
        let resp = Response::hit(b"mykey", 42, b"myvalue");
        let len = resp.encode(&mut buf);
        assert_eq!(&buf[..len], b"VALUE mykey 42 7\r\nmyvalue\r\nEND\r\n");
    }

    #[test]
    fn test_encode_multi_values() {
        let mut buf = [0u8; 256];
        let resp = Response::Values(vec![
            Value {
                key: b"k1".to_vec(),
                flags: 0,
                data: b"v1".to_vec(),
                cas: None,
            },
            Value {
                key: b"k2".to_vec(),
                flags: 1,
                data: b"v2".to_vec(),
                cas: None,
            },
        ]);
        let len = resp.encode(&mut buf);
        assert_eq!(
            &buf[..len],
            b"VALUE k1 0 2\r\nv1\r\nVALUE k2 1 2\r\nv2\r\nEND\r\n"
        );
    }

    #[test]
    fn test_direct_encode_stored() {
        let mut buf = [0u8; 64];
        let len = Response::encode_stored(&mut buf);
        assert_eq!(&buf[..len], b"STORED\r\n");
    }

    #[test]
    fn test_direct_encode_not_stored() {
        let mut buf = [0u8; 64];
        let len = Response::encode_not_stored(&mut buf);
        assert_eq!(&buf[..len], b"NOT_STORED\r\n");
    }

    #[test]
    fn test_direct_encode_deleted() {
        let mut buf = [0u8; 64];
        let len = Response::encode_deleted(&mut buf);
        assert_eq!(&buf[..len], b"DELETED\r\n");
    }

    #[test]
    fn test_direct_encode_not_found() {
        let mut buf = [0u8; 64];
        let len = Response::encode_not_found(&mut buf);
        assert_eq!(&buf[..len], b"NOT_FOUND\r\n");
    }

    #[test]
    fn test_direct_encode_end() {
        let mut buf = [0u8; 64];
        let len = Response::encode_end(&mut buf);
        assert_eq!(&buf[..len], b"END\r\n");
    }

    #[test]
    fn test_direct_encode_server_error() {
        let mut buf = [0u8; 64];
        let len = Response::encode_server_error(&mut buf, b"error message");
        assert_eq!(&buf[..len], b"SERVER_ERROR error message\r\n");
    }

    #[test]
    fn test_roundtrip_client_error() {
        let mut buf = [0u8; 256];
        let original = Response::client_error(b"test error");
        let len = original.encode(&mut buf);
        let (parsed, consumed) = Response::parse(&buf[..len]).unwrap();
        assert_eq!(original, parsed);
        assert_eq!(len, consumed);
    }

    #[test]
    fn test_roundtrip_server_error() {
        let mut buf = [0u8; 256];
        let original = Response::server_error(b"test error");
        let len = original.encode(&mut buf);
        let (parsed, consumed) = Response::parse(&buf[..len]).unwrap();
        assert_eq!(original, parsed);
        assert_eq!(len, consumed);
    }

    #[test]
    fn test_roundtrip_values() {
        let mut buf = [0u8; 256];
        let original = Response::hit(b"testkey", 123, b"testvalue");
        let len = original.encode(&mut buf);
        let (parsed, consumed) = Response::parse(&buf[..len]).unwrap();
        assert_eq!(original, parsed);
        assert_eq!(len, consumed);
    }

    #[test]
    fn test_value_debug() {
        let v = Value {
            key: b"k".to_vec(),
            flags: 0,
            data: b"v".to_vec(),
            cas: None,
        };
        let debug_str = format!("{:?}", v);
        assert!(debug_str.contains("Value"));
    }

    #[test]
    fn test_value_clone() {
        let v1 = Value {
            key: b"k".to_vec(),
            flags: 42,
            data: b"v".to_vec(),
            cas: None,
        };
        let v2 = v1.clone();
        assert_eq!(v1, v2);
    }

    #[test]
    fn test_response_debug() {
        let resp = Response::Stored;
        let debug_str = format!("{:?}", resp);
        assert!(debug_str.contains("Stored"));
    }

    #[test]
    fn test_response_clone() {
        let r1 = Response::Stored;
        let r2 = r1.clone();
        assert_eq!(r1, r2);
    }

    #[test]
    fn test_parse_value_with_flags() {
        let data = b"VALUE mykey 12345 5\r\nhello\r\nEND\r\n";
        let (resp, _) = Response::parse(data).unwrap();
        match resp {
            Response::Values(values) => {
                assert_eq!(values[0].flags, 12345);
            }
            _ => panic!("expected Values"),
        }
    }

    #[test]
    fn test_find_crlf_edge_cases() {
        // \r at end without \n
        assert!(Response::parse(b"STORED\r").is_err());
    }

    #[test]
    fn find_crlf_skips_bare_cr() {
        const MAX: usize = DEFAULT_MAX_RESPONSE_LINE_LEN;
        // A bare \r is line content, not a terminator: keep scanning.
        assert_eq!(find_crlf(b"a\rb\r\n", MAX), Ok(Some(3)));
        assert_eq!(find_crlf(b"\r\n", MAX), Ok(Some(0)));
        assert_eq!(find_crlf(b"\r\r\n", MAX), Ok(Some(1)));
        // No CRLF yet -- caller needs more data.
        assert_eq!(find_crlf(b"a\rb", MAX), Ok(None));
        assert_eq!(find_crlf(b"abc", MAX), Ok(None));
        assert_eq!(find_crlf(b"", MAX), Ok(None));
    }

    #[test]
    fn unterminated_line_is_bounded() {
        // Without a bound this reported Incomplete forever, so a peer that
        // never sends a newline could make the caller buffer without limit.
        let flood = vec![b'x'; DEFAULT_MAX_RESPONSE_LINE_LEN + 1];
        assert!(matches!(
            Response::parse(&flood),
            Err(ParseError::Protocol("line too long"))
        ));
        assert!(matches!(
            ResponseBytes::parse(Bytes::from(flood.clone())),
            Err(ParseError::Protocol("line too long"))
        ));

        // At or below the bound it is still just "need more data".
        let ok = vec![b'x'; DEFAULT_MAX_RESPONSE_LINE_LEN];
        assert!(matches!(Response::parse(&ok), Err(ParseError::Incomplete)));

        // The bound is configurable.
        assert!(matches!(
            Response::parse_with_max_line_len(b"xxxxxxxx", 4),
            Err(ParseError::Protocol("line too long"))
        ));
    }

    #[test]
    fn large_values_are_not_mistaken_for_long_lines() {
        // The bound applies to a LINE, not to value data. A value far larger
        // than the line bound must still parse: the VALUE header's CRLF is
        // found immediately, and the payload is consumed by declared length.
        let big = vec![b'v'; DEFAULT_MAX_RESPONSE_LINE_LEN * 8];
        let mut data = format!("VALUE k 0 {}\r\n", big.len()).into_bytes();
        data.extend_from_slice(&big);
        data.extend_from_slice(b"\r\nEND\r\n");

        let (resp, consumed) = Response::parse(&data).expect("large value must parse");
        assert_eq!(consumed, data.len());
        match resp {
            Response::Values(v) => {
                assert_eq!(v.len(), 1);
                assert_eq!(v[0].data.len(), big.len());
            }
            other => panic!("expected Values, got {other:?}"),
        }

        // Same on the zero-copy path.
        let (rb, consumed) =
            ResponseBytes::parse(Bytes::from(data.clone())).expect("large value must parse");
        assert_eq!(consumed, data.len());
        assert!(matches!(rb, ResponseBytes::Values(ref v) if v[0].data.len() == big.len()));
    }

    #[test]
    fn value_loop_is_bounded_too() {
        // A well-formed first line followed by an unterminated flood must not
        // string the caller along inside the VALUE loop either.
        let mut data = b"VALUE k 0 5\r\nhello\r\n".to_vec();
        data.extend(std::iter::repeat_n(b'x', DEFAULT_MAX_RESPONSE_LINE_LEN + 1));
        assert!(matches!(
            Response::parse(&data),
            Err(ParseError::Protocol("line too long"))
        ));
    }

    #[test]
    fn bare_cr_in_line_does_not_stall_parser() {
        // Regression: the line is complete and CRLF-terminated, so the parser
        // must not report Incomplete and ask for bytes that cannot help.
        let data = b"SERVER_ERROR out\rof memory\r\n";
        let (resp, consumed) = Response::parse(data).expect("complete line must parse");
        assert_eq!(consumed, data.len());
        assert!(matches!(resp, Response::ServerError(ref m) if m == b"out\rof memory"));
    }

    #[test]
    fn test_parse_value_data_too_large() {
        // Value data size exceeding MAX_VALUE_DATA_LEN should be rejected
        let data = b"VALUE k 0 18446744073709551615\r\n";
        let result = Response::parse(data);
        assert!(matches!(
            result,
            Err(ParseError::Protocol("value data too large"))
        ));
    }

    #[test]
    fn test_parse_value_with_cas() {
        let data = b"VALUE mykey 0 5 12345\r\nhello\r\nEND\r\n";
        let (resp, consumed) = Response::parse(data).unwrap();
        assert_eq!(consumed, data.len());
        match resp {
            Response::Values(values) => {
                assert_eq!(values.len(), 1);
                assert_eq!(values[0].key, b"mykey");
                assert_eq!(values[0].data, b"hello");
                assert_eq!(values[0].cas, Some(12345));
            }
            _ => panic!("expected Values"),
        }
    }

    #[test]
    fn test_parse_value_without_cas() {
        let data = b"VALUE mykey 0 5\r\nhello\r\nEND\r\n";
        let (resp, _) = Response::parse(data).unwrap();
        match resp {
            Response::Values(values) => {
                assert_eq!(values[0].cas, None);
            }
            _ => panic!("expected Values"),
        }
    }

    #[test]
    fn test_roundtrip_value_with_cas() {
        let mut buf = [0u8; 256];
        let original = Response::Values(vec![Value {
            key: b"testkey".to_vec(),
            flags: 0,
            data: b"testvalue".to_vec(),
            cas: Some(98765),
        }]);
        let len = original.encode(&mut buf);
        let (parsed, consumed) = Response::parse(&buf[..len]).unwrap();
        assert_eq!(original, parsed);
        assert_eq!(len, consumed);
    }

    #[test]
    fn test_parse_multi_value_with_cas() {
        let data = b"VALUE k1 0 2 100\r\nv1\r\nVALUE k2 0 2 200\r\nv2\r\nEND\r\n";
        let (resp, consumed) = Response::parse(data).unwrap();
        assert_eq!(consumed, data.len());
        match resp {
            Response::Values(values) => {
                assert_eq!(values.len(), 2);
                assert_eq!(values[0].cas, Some(100));
                assert_eq!(values[1].cas, Some(200));
            }
            _ => panic!("expected Values"),
        }
    }

    // ── ResponseBytes / parse_bytes tests ──────────────────────────────

    #[test]
    fn test_parse_bytes_stored() {
        let data = Bytes::from_static(b"STORED\r\n");
        let (resp, consumed) = ResponseBytes::parse(data).unwrap();
        assert_eq!(resp, ResponseBytes::Stored);
        assert_eq!(consumed, 8);
    }

    #[test]
    fn test_parse_bytes_not_stored() {
        let data = Bytes::from_static(b"NOT_STORED\r\n");
        let (resp, consumed) = ResponseBytes::parse(data).unwrap();
        assert_eq!(resp, ResponseBytes::NotStored);
        assert_eq!(consumed, 12);
    }

    #[test]
    fn test_parse_bytes_deleted() {
        let data = Bytes::from_static(b"DELETED\r\n");
        let (resp, consumed) = ResponseBytes::parse(data).unwrap();
        assert_eq!(resp, ResponseBytes::Deleted);
        assert_eq!(consumed, 9);
    }

    #[test]
    fn test_parse_bytes_not_found() {
        let data = Bytes::from_static(b"NOT_FOUND\r\n");
        let (resp, consumed) = ResponseBytes::parse(data).unwrap();
        assert_eq!(resp, ResponseBytes::NotFound);
        assert_eq!(consumed, 11);
    }

    #[test]
    fn test_parse_bytes_exists() {
        let data = Bytes::from_static(b"EXISTS\r\n");
        let (resp, consumed) = ResponseBytes::parse(data).unwrap();
        assert_eq!(resp, ResponseBytes::Exists);
        assert_eq!(consumed, 8);
    }

    #[test]
    fn test_parse_bytes_end() {
        let data = Bytes::from_static(b"END\r\n");
        let (resp, consumed) = ResponseBytes::parse(data).unwrap();
        assert_eq!(resp, ResponseBytes::Values(vec![]));
        assert_eq!(consumed, 5);
        assert!(resp.is_miss());
    }

    #[test]
    fn test_parse_bytes_ok() {
        let data = Bytes::from_static(b"OK\r\n");
        let (resp, consumed) = ResponseBytes::parse(data).unwrap();
        assert_eq!(resp, ResponseBytes::Ok);
        assert_eq!(consumed, 4);
    }

    #[test]
    fn test_parse_bytes_error() {
        let data = Bytes::from_static(b"ERROR\r\n");
        let (resp, _) = ResponseBytes::parse(data).unwrap();
        assert!(resp.is_error());
    }

    #[test]
    fn test_parse_bytes_numeric() {
        let data = Bytes::from_static(b"42\r\n");
        let (resp, consumed) = ResponseBytes::parse(data).unwrap();
        assert_eq!(resp, ResponseBytes::Numeric(42));
        assert_eq!(consumed, 4);
    }

    #[test]
    fn test_parse_bytes_client_error() {
        let data = Bytes::from_static(b"CLIENT_ERROR bad request\r\n");
        let (resp, consumed) = ResponseBytes::parse(data).unwrap();
        assert!(resp.is_error());
        assert_eq!(consumed, 26);
        match resp {
            ResponseBytes::ClientError(msg) => assert_eq!(&msg[..], b"bad request"),
            _ => panic!("expected ClientError"),
        }
    }

    #[test]
    fn test_parse_bytes_server_error() {
        let data = Bytes::from_static(b"SERVER_ERROR out of memory\r\n");
        let (resp, _) = ResponseBytes::parse(data).unwrap();
        assert!(resp.is_error());
        match resp {
            ResponseBytes::ServerError(msg) => assert_eq!(&msg[..], b"out of memory"),
            _ => panic!("expected ServerError"),
        }
    }

    #[test]
    fn test_parse_bytes_version() {
        let data = Bytes::from_static(b"VERSION 1.6.9\r\n");
        let (resp, consumed) = ResponseBytes::parse(data).unwrap();
        assert_eq!(consumed, 15);
        match resp {
            ResponseBytes::Version(v) => assert_eq!(&v[..], b"1.6.9"),
            _ => panic!("expected Version"),
        }
    }

    #[test]
    fn test_parse_bytes_value() {
        let data = Bytes::from_static(b"VALUE mykey 0 7\r\nmyvalue\r\nEND\r\n");
        let (resp, consumed) = ResponseBytes::parse(data).unwrap();
        assert_eq!(consumed, 31);
        match resp {
            ResponseBytes::Values(values) => {
                assert_eq!(values.len(), 1);
                assert_eq!(&values[0].key[..], b"mykey");
                assert_eq!(values[0].flags, 0);
                assert_eq!(&values[0].data[..], b"myvalue");
                assert_eq!(values[0].cas, None);
            }
            _ => panic!("expected Values"),
        }
    }

    #[test]
    fn test_parse_bytes_multi_value() {
        let raw = b"VALUE key1 0 3\r\nfoo\r\nVALUE key2 0 3\r\nbar\r\nEND\r\n";
        let data = Bytes::from_static(raw);
        let (resp, consumed) = ResponseBytes::parse(data).unwrap();
        assert_eq!(consumed, raw.len());
        match resp {
            ResponseBytes::Values(values) => {
                assert_eq!(values.len(), 2);
                assert_eq!(&values[0].key[..], b"key1");
                assert_eq!(&values[0].data[..], b"foo");
                assert_eq!(&values[1].key[..], b"key2");
                assert_eq!(&values[1].data[..], b"bar");
            }
            _ => panic!("expected Values"),
        }
    }

    #[test]
    fn test_parse_bytes_value_with_cas() {
        let data = Bytes::from_static(b"VALUE mykey 0 5 12345\r\nhello\r\nEND\r\n");
        let (resp, consumed) = ResponseBytes::parse(data).unwrap();
        assert_eq!(consumed, 35);
        match resp {
            ResponseBytes::Values(values) => {
                assert_eq!(values.len(), 1);
                assert_eq!(&values[0].key[..], b"mykey");
                assert_eq!(&values[0].data[..], b"hello");
                assert_eq!(values[0].cas, Some(12345));
            }
            _ => panic!("expected Values"),
        }
    }

    #[test]
    fn test_parse_bytes_value_with_flags() {
        let data = Bytes::from_static(b"VALUE mykey 12345 5\r\nhello\r\nEND\r\n");
        let (resp, _) = ResponseBytes::parse(data).unwrap();
        match resp {
            ResponseBytes::Values(values) => {
                assert_eq!(values[0].flags, 12345);
            }
            _ => panic!("expected Values"),
        }
    }

    #[test]
    fn test_parse_bytes_incomplete() {
        let data = Bytes::from_static(b"VALUE mykey 0 7\r\nmyval");
        assert!(matches!(
            ResponseBytes::parse(data),
            Err(ParseError::Incomplete)
        ));
    }

    #[test]
    fn test_parse_bytes_unknown() {
        let data = Bytes::from_static(b"UNKNOWN\r\n");
        assert!(matches!(
            ResponseBytes::parse(data),
            Err(ParseError::Protocol("unknown response"))
        ));
    }

    #[test]
    fn test_parse_bytes_zero_copy_slices() {
        // Verify that parsed Bytes share the same backing allocation
        let raw = b"VALUE mykey 0 7\r\nmyvalue\r\nEND\r\n";
        let data = Bytes::copy_from_slice(raw);
        let (resp, _) = ResponseBytes::parse(data.clone()).unwrap();
        match resp {
            ResponseBytes::Values(values) => {
                // The key and data should be slices into the original Bytes
                assert_eq!(&values[0].key[..], b"mykey");
                assert_eq!(&values[0].data[..], b"myvalue");
            }
            _ => panic!("expected Values"),
        }
    }

    #[test]
    fn test_parse_bytes_is_stored() {
        assert!(ResponseBytes::Stored.is_stored());
        assert!(!ResponseBytes::NotStored.is_stored());
    }
}
