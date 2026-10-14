//! Positional access to a response split across several `Bytes`.
//!
//! [`Segments`] is one logical byte sequence split across several `Bytes`,
//! as a receive path that does not copy into one buffer delivers it.
//! Positions are offsets into the logical sequence. `parse_segments` reads
//! multi-piece input through it; contiguous input goes to
//! `ResponseBytes::parse`.
//!
//! resp-proto's `src/input.rs` has the same `Input` trait and `Segments`,
//! with a `Bytes` implementation as well; changes to one belong in both.

use crate::error::ParseError;
use bytes::{Bytes, BytesMut};

pub(crate) trait Input {
    /// Length of the logical sequence.
    fn len(&self) -> usize;

    /// The byte at `pos`. `pos` must be less than `len()`.
    fn byte(&self, pos: usize) -> u8;

    /// Position of the first CR that is followed by LF, at or after `from`.
    ///
    /// Without one, fails with "line too long" when more than
    /// `max_line_len` bytes follow `from`, and returns `None` otherwise.
    fn find_crlf(&self, from: usize, max_line_len: usize) -> Result<Option<usize>, ParseError>;

    /// The bytes in `start..end` as `Bytes`: a slice of the input when they
    /// lie in one piece, a copy otherwise.
    fn slice(&self, start: usize, end: usize) -> Bytes;

    /// Run `f` on the bytes in `start..end` as one slice, copying them
    /// first only when they span pieces.
    fn with_range<R>(&self, start: usize, end: usize, f: impl FnOnce(&[u8]) -> R) -> R;
}

/// Piece counts up to this keep their start offsets on the stack.
const INLINE_PIECES: usize = 8;

/// A logical byte sequence split across several `Bytes`, in order.
pub(crate) struct Segments<'a> {
    segs: &'a [Bytes],
    /// `starts[i]` is the logical position of `segs[i]`'s first byte; on the
    /// stack for up to `INLINE_PIECES` pieces, allocated above that.
    inline: [usize; INLINE_PIECES],
    heap: Vec<usize>,
    len: usize,
}

impl<'a> Segments<'a> {
    pub(crate) fn new(segs: &'a [Bytes]) -> Self {
        let mut inline = [0usize; INLINE_PIECES];
        let mut heap = Vec::new();
        if segs.len() > INLINE_PIECES {
            heap.reserve_exact(segs.len());
        }
        let mut len = 0usize;
        for (i, s) in segs.iter().enumerate() {
            if segs.len() > INLINE_PIECES {
                heap.push(len);
            } else {
                inline[i] = len;
            }
            len += s.len();
        }
        Segments {
            segs,
            inline,
            heap,
            len,
        }
    }

    #[inline]
    fn starts(&self) -> &[usize] {
        if self.segs.len() > INLINE_PIECES {
            &self.heap
        } else {
            &self.inline[..self.segs.len()]
        }
    }

    /// The piece holding logical position `pos` (`pos < len`), and the
    /// offset of `pos` within it. Empty pieces are never returned.
    #[inline]
    fn locate(&self, pos: usize) -> (usize, usize) {
        // The last piece starting at or before `pos`; empty pieces share a
        // start with the next one, and `partition_point` skips past them.
        let starts = self.starts();
        let i = starts.partition_point(|&s| s <= pos) - 1;
        (i, pos - starts[i])
    }
}

impl Input for Segments<'_> {
    #[inline]
    fn len(&self) -> usize {
        self.len
    }

    #[inline]
    fn byte(&self, pos: usize) -> u8 {
        let (i, off) = self.locate(pos);
        self.segs[i][off]
    }

    fn find_crlf(&self, from: usize, max_line_len: usize) -> Result<Option<usize>, ParseError> {
        if from < self.len {
            let (mut i, mut off) = self.locate(from);
            while i < self.segs.len() {
                let seg = &self.segs[i][..];
                while let Some(rel) = memchr::memchr(b'\r', &seg[off..]) {
                    let cr = self.starts()[i] + off + rel;
                    // A CR ending this piece may meet its LF in the next one.
                    let next = cr + 1;
                    let lf = next < self.len && self.byte(next) == b'\n';
                    if lf {
                        return Ok(Some(cr));
                    }
                    off += rel + 1;
                }
                i += 1;
                off = 0;
            }
        }
        if self.len - from.min(self.len) > max_line_len {
            return Err(ParseError::Protocol("line too long"));
        }
        Ok(None)
    }

    fn slice(&self, start: usize, end: usize) -> Bytes {
        if start == end {
            return Bytes::new();
        }
        let (i, off) = self.locate(start);
        let seg = &self.segs[i];
        if off + (end - start) <= seg.len() {
            return seg.slice(off..off + (end - start));
        }
        let mut out = BytesMut::with_capacity(end - start);
        self.copy_into(start, end, &mut out);
        out.freeze()
    }

    fn with_range<R>(&self, start: usize, end: usize, f: impl FnOnce(&[u8]) -> R) -> R {
        if start == end {
            return f(&[]);
        }
        let (i, off) = self.locate(start);
        let seg = &self.segs[i];
        if off + (end - start) <= seg.len() {
            return f(&seg[off..off + (end - start)]);
        }
        let mut out = BytesMut::with_capacity(end - start);
        self.copy_into(start, end, &mut out);
        f(&out)
    }
}

impl Segments<'_> {
    fn copy_into(&self, start: usize, end: usize, out: &mut BytesMut) {
        let (mut i, mut off) = self.locate(start);
        let mut left = end - start;
        while left > 0 {
            let seg = &self.segs[i][off..];
            let take = left.min(seg.len());
            out.extend_from_slice(&seg[..take]);
            left -= take;
            i += 1;
            off = 0;
        }
    }
}
