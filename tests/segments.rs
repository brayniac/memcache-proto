//! `ResponseBytes::parse` and `ResponseBytes::parse_segments` against the
//! copy parser `Response::parse`, over fixtures and random inputs, with the
//! segmented input split at every point.

#![cfg(feature = "ascii")]

use bytes::Bytes;
use memcache_proto::{ParseError, Response, ResponseBytes, Value};

const ITERATIONS: usize = 50_000;
const MAX_INPUT_LEN: usize = 48;

fn next_u64(state: &mut u64) -> u64 {
    *state = state
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    *state
}

const FIXTURES: &[&[u8]] = &[
    b"STORED\r\n",
    b"NOT_STORED\r\n",
    b"DELETED\r\n",
    b"NOT_FOUND\r\n",
    b"EXISTS\r\n",
    b"END\r\n",
    b"OK\r\n",
    b"ERROR\r\n",
    b"CLIENT_ERROR bad data\r\n",
    b"SERVER_ERROR out of memory\r\n",
    b"VERSION 1.6.21\r\n",
    b"12345\r\n",
    b"VALUE k 0 3\r\nabc\r\nEND\r\n",
    b"VALUE key 5 2 99\r\nhi\r\nEND\r\n",
    b"VALUE a 0 1\r\nx\r\nVALUE bb 1 2\r\nyz\r\nEND\r\n",
    b"VALUE k 0 3\r\nabcX\r\nEND\r\n",
    b"VALUE k 0\r\nEND\r\n",
    b"VALUE k x 3\r\nabc\r\nEND\r\n",
    b"VALUE k 0 3\r\nab",
    b"BOGUS\r\n",
    b"STORED\r",
    b"VALUE k 0 2\r\n\r\n\r\nEND\r\n",
];

/// A fixture, a fixture with its tail cut, or random bytes drawn mostly from
/// the protocol's alphabet.
fn input_for(iteration: usize, state: &mut u64) -> Vec<u8> {
    let fixture = FIXTURES[iteration % FIXTURES.len()];
    match iteration % 3 {
        0 => fixture.to_vec(),
        1 => fixture[..(next_u64(state) as usize) % (fixture.len() + 1)].to_vec(),
        _ => {
            const ALPHABET: &[u8] = b"VALUE END STORED 0123456789\r\n\r\nkx";
            let len = (next_u64(state) as usize) % (MAX_INPUT_LEN + 1);
            (0..len)
                .map(|_| {
                    let r = next_u64(state);
                    if r.is_multiple_of(8) {
                        r as u8
                    } else {
                        ALPHABET[(r >> 8) as usize % ALPHABET.len()]
                    }
                })
                .collect()
        }
    }
}

/// `ResponseBytes` as the copy parser's `Response`, to compare the two.
fn as_response(r: &ResponseBytes) -> Response {
    match r {
        ResponseBytes::Values(values) => Response::Values(
            values
                .iter()
                .map(|v| Value {
                    key: v.key.to_vec(),
                    flags: v.flags,
                    data: v.data.to_vec(),
                    cas: v.cas,
                })
                .collect(),
        ),
        ResponseBytes::Stored => Response::Stored,
        ResponseBytes::NotStored => Response::NotStored,
        ResponseBytes::Deleted => Response::Deleted,
        ResponseBytes::NotFound => Response::NotFound,
        ResponseBytes::Exists => Response::Exists,
        ResponseBytes::Ok => Response::Ok,
        ResponseBytes::Numeric(n) => Response::Numeric(*n),
        ResponseBytes::Version(v) => Response::Version(v.to_vec()),
        ResponseBytes::Error => Response::Error,
        ResponseBytes::ClientError(m) => Response::ClientError(m.to_vec()),
        ResponseBytes::ServerError(m) => Response::ServerError(m.to_vec()),
    }
}

fn split_at(input: &[u8], cuts: &[usize]) -> Vec<Bytes> {
    let mut pieces = Vec::with_capacity(cuts.len() + 1);
    let mut start = 0;
    for &cut in cuts {
        pieces.push(Bytes::copy_from_slice(&input[start..cut]));
        start = cut;
    }
    pieces.push(Bytes::copy_from_slice(&input[start..]));
    pieces
}

type Outcome = Result<(Response, usize), ParseError>;

fn outcome(r: Result<(ResponseBytes, usize), ParseError>) -> Outcome {
    r.map(|(v, n)| (as_response(&v), n))
}

#[test]
fn zero_copy_and_segmented_parsers_match_the_copy_parser() {
    // A short line limit reaches "line too long" within the inputs.
    for max_line_len in [memcache_proto::DEFAULT_MAX_RESPONSE_LINE_LEN, 8] {
        let mut state = 0x6d63_7365_676d_656e;
        let mut parsed = 0usize;
        for iteration in 0..ITERATIONS {
            let input = input_for(iteration, &mut state);
            let copied = Response::parse_with_max_line_len(&input, max_line_len);
            let whole = outcome(ResponseBytes::parse_with_max_line_len(
                Bytes::copy_from_slice(&input),
                max_line_len,
            ));
            assert_eq!(whole, copied, "input {input:?}");
            parsed += usize::from(copied.is_ok());

            let check = |cuts: &[usize]| {
                let pieces = split_at(&input, cuts);
                let segmented = outcome(ResponseBytes::parse_segments_with_max_line_len(
                    &pieces,
                    max_line_len,
                ));
                assert_eq!(segmented, copied, "input {input:?} split at {cuts:?}");
            };
            for cut in 0..=input.len() {
                check(&[cut]);
            }
            // One byte per piece, past the eight pieces kept on the stack, and
            // the same with an empty piece between every two bytes.
            let every: Vec<usize> = (1..input.len()).collect();
            check(&every);
            let doubled: Vec<usize> = every.iter().flat_map(|&c| [c, c]).collect();
            check(&doubled);
            for _ in 0..3 {
                let n = 2 + (next_u64(&mut state) as usize) % 3;
                let mut cuts: Vec<usize> = (0..n)
                    .map(|_| (next_u64(&mut state) as usize) % (input.len() + 1))
                    .collect();
                cuts.sort_unstable();
                check(&cuts);
            }
        }
        assert!(parsed > ITERATIONS / 10, "too few inputs parse: {parsed}");
    }
}

#[test]
fn no_pieces_is_incomplete() {
    assert_eq!(
        ResponseBytes::parse_segments(&[]),
        Err(ParseError::Incomplete)
    );
}

/// The line limit counts from the start of the unfinished line: a VALUE
/// response whose second header line has not ended is incomplete while that
/// line fits the limit.
#[test]
fn segmented_line_limit_counts_from_the_line_start() {
    let input = b"VALUE k 0 1\r\nx\r\nVALUE";
    let copied = Response::parse_with_max_line_len(input, 8);
    assert!(matches!(copied, Err(ParseError::Incomplete)), "{copied:?}");
    for cut in 0..=input.len() {
        let pieces = split_at(input, &[cut]);
        assert_eq!(
            outcome(ResponseBytes::parse_segments_with_max_line_len(&pieces, 8)),
            copied,
            "split at {cut}"
        );
    }
}

/// A value that lies within one piece is a slice of that piece; one that
/// spans pieces is a copy.
#[test]
fn segmented_values_are_zero_copy_within_a_piece() {
    let first = Bytes::from_static(b"VALUE a 0 5\r\nhello\r\nVALUE b 0 5\r\nwo");
    let second = Bytes::from_static(b"rld\r\nEND\r\n");
    let (response, consumed) =
        ResponseBytes::parse_segments(&[first.clone(), second.clone()]).unwrap();
    assert_eq!(consumed, first.len() + second.len());
    let ResponseBytes::Values(values) = response else {
        panic!("expected values")
    };
    let within = |b: &Bytes, piece: &Bytes| piece.as_ptr_range().contains(&b.as_ptr());
    assert_eq!(&values[0].data[..], b"hello");
    assert_eq!(&values[1].data[..], b"world");
    assert!(within(&values[0].data, &first));
    assert!(!within(&values[1].data, &first) && !within(&values[1].data, &second));
}
