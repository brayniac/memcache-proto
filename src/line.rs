//! Line scanning shared by the ASCII request and response parsers.

use crate::ParseError;

/// Find the first `\r\n` in `data`, returning the position of the `\r`.
///
/// A bare `\r` not followed by `\n` is ordinary line content, so the scan
/// continues past it; only a real CRLF terminates a line. Returns `Ok(None)`
/// when no CRLF is present yet, which callers treat as "need more data", and
/// `Err(ParseError::Protocol)` when `data` is longer than `max_line_len`
/// without one.
pub(crate) fn find_crlf(data: &[u8], max_line_len: usize) -> Result<Option<usize>, ParseError> {
    // A line usually ends at its first `\n`, which `memchr` finds without the
    // searcher `memmem::find` constructs for each haystack of 64 bytes or
    // more. After a bare `\n` the rest goes to `memmem`, so a buffer dense in
    // `\n` costs no more than a single `memmem` search.
    let found = match memchr::memchr(b'\n', data) {
        Some(nl) if nl > 0 && data[nl - 1] == b'\r' => Some(nl - 1),
        Some(nl) => memchr::memmem::find(&data[nl..], b"\r\n").map(|p| nl + p),
        None => None,
    };
    if found.is_some() {
        return Ok(found);
    }

    if data.len() > max_line_len {
        return Err(ParseError::Protocol("line too long"));
    }

    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAX: usize = 1024;

    #[test]
    fn finds_the_first_crlf() {
        assert_eq!(find_crlf(b"\r\n", MAX), Ok(Some(0)));
        assert_eq!(find_crlf(b"END\r\nSTORED\r\n", MAX), Ok(Some(3)));
    }

    #[test]
    fn a_bare_cr_or_lf_is_line_content() {
        assert_eq!(find_crlf(b"a\rb\r\n", MAX), Ok(Some(3)));
        assert_eq!(find_crlf(b"\r\r\n", MAX), Ok(Some(1)));
        assert_eq!(find_crlf(b"\n\r\n", MAX), Ok(Some(1)));
        assert_eq!(find_crlf(b"a\nb\r\n", MAX), Ok(Some(3)));
        assert_eq!(find_crlf(b"\n\n", MAX), Ok(None));
        assert_eq!(find_crlf(b"\n\n\na\r\n", MAX), Ok(Some(4)));
    }

    #[test]
    fn no_crlf_is_incomplete_until_the_line_limit() {
        assert_eq!(find_crlf(b"", MAX), Ok(None));
        assert_eq!(find_crlf(b"a\rb", MAX), Ok(None));
        assert_eq!(find_crlf(b"STORED\r", MAX), Ok(None));
        assert_eq!(find_crlf(b"abcd", 4), Ok(None));
        assert_eq!(
            find_crlf(b"abcde", 4),
            Err(ParseError::Protocol("line too long"))
        );
        // A CRLF past the limit is still found: the limit applies only when
        // there is no line end to return.
        assert_eq!(find_crlf(b"abcde\r\n", 4), Ok(Some(5)));
    }
}
