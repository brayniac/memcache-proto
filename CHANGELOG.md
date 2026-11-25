# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `ResponseBytes::parse_segments` and
  `ResponseBytes::parse_segments_with_max_line_len` parse a response from bytes
  that arrived in several pieces (`&[Bytes]`), as a receive path that does not
  copy them into one buffer delivers them. The result is what `parse` gives on
  the pieces joined: the same response, consumed count and error. A key, value
  or message within one piece is a slice of it; one that spans pieces is
  copied. One piece is parsed by `parse`; `parse` itself is unchanged.

### Changed

- `Command::parse` and `Command::parse_with_options`, `parse_streaming`,
  `Response::parse` and `Response::parse_with_max_line_len`,
  `ResponseBytes::parse` and `ResponseBytes::parse_with_max_line_len`, and
  `ResponseBytes::parse_segments` and `parse_segments_with_max_line_len` when
  given one piece, find the end of a line with `memchr` for the first `\n`
  instead of `memchr::memmem::find`, which builds a searcher for each buffer of
  64 bytes or more. Finding each line end while walking a pipelined buffer of
  GET hits and STORED replies took 4.05 ns per line instead of 16.4 on an AMD
  Ryzen 7 7840HS (Zen 4), 9.86 instead of 12.06 on AWS Graviton4, and 6.5
  instead of 6.9 on Apple M-series. A buffer of bare `\n` with no `\r\n` takes
  as long as before. `ResponseBytes::parse_segments` with more than one piece is
  unchanged. What counts as a line end is unchanged: a bare `\r` or `\n` is line
  content.

## [0.0.5] - 2026-08-19

### Fixed

- `ParseOptions::max_line_len` saturates instead of overflowing. `max_key_len`
  and `max_keys` are public fields, so their product can exceed `usize`. The
  unchecked form panicked in debug builds and wrapped in release ones, which
  inverted the limit: `max_key_len = usize::MAX` produced a bound of 4, causing
  the command parser to reject almost every line as `Protocol("line too long")`.

## [0.0.4] - 2026-08-19

### Added

- `DEFAULT_MAX_RESPONSE_LINE_LEN`, plus `Response::parse_with_max_line_len` and
  `ResponseBytes::parse_with_max_line_len` for callers that need a different bound.

### Fixed

- ASCII response parsing is now bounded by a maximum line length, matching the
  command and streaming parsers. `Response::parse` and `ResponseBytes::parse`
  previously reported `Incomplete` for an unterminated line no matter how large
  the buffer grew, so a peer that never sent a newline could make a caller
  buffer without limit. Both now return `Protocol("line too long")` past
  `DEFAULT_MAX_RESPONSE_LINE_LEN` (8 KiB).

## [0.0.3] - 2026-08-19

### Added

- Kani verification for binary header arithmetic and encode/parse round trips

### Changed

- `RequestHeader::value_length()` and `ResponseHeader::value_length()` now return
  `Option<usize>` instead of `usize`; `checked_value_length()` is removed.

### Fixed

- Reject inconsistent binary header lengths before waiting for body data
- ASCII line framing no longer stalls on a bare `\r`. `find_crlf` inspected only
  the first `\r` in the buffer and gave up if it was not followed by `\n`, so a
  complete CRLF-terminated line containing a bare `\r` reported `Incomplete`
  forever in the response parser, and reported `Protocol("line too long")` in the
  command and streaming parsers once the line-length guard tripped. All three now
  scan for a real CRLF.

## [0.0.1] - 2026-02-21

### Added

- Initial release extracted from ringline workspace
- ASCII memcache protocol parsing and encoding
- Binary memcache protocol parsing and encoding
- Streaming parser for incremental decoding
