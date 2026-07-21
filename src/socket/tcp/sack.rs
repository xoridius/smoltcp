use crate::storage::Assembler;
use crate::wire::TcpSeqNumber;

/// Sender-side SACK ranges, stored as offsets from SND.UNA.
#[repr(transparent)]
#[derive(Debug)]
pub(super) struct SackScoreboard(Assembler);

impl SackScoreboard {
    #[inline]
    pub(super) const fn new() -> Self {
        Self(Assembler::new())
    }

    #[inline]
    pub(super) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    #[inline]
    pub(super) fn clear(&mut self) {
        self.0.clear();
    }

    /// Discard cumulatively acknowledged bytes and rebase surviving ranges.
    #[inline]
    pub(super) fn discard_acked(&mut self, len: usize) {
        self.0.shift_front(len);
    }

    /// Record valid SACK evidence bounded by transmitted, buffered data.
    pub(super) fn ingest(
        &mut self,
        ranges: &[Option<(u32, u32)>; 3],
        snd_una: TcpSeqNumber,
        snd_nxt: TcpSeqNumber,
        tx_len: usize,
    ) {
        for &(left, right) in ranges.iter().flatten() {
            let left = TcpSeqNumber(left as i32);
            let mut right = TcpSeqNumber(right as i32);

            if right <= left || right <= snd_una || left >= snd_nxt {
                continue;
            }
            right = right.min(snd_nxt);

            let left = left.max(snd_una);
            let offset = left - snd_una;
            if offset >= tx_len {
                continue;
            }
            let len = (right - left).min(tx_len - offset);
            let _ = self.0.add(offset, len);
        }
    }

    /// Advance a transmit cursor out of a SACKed range.
    #[inline]
    pub(super) fn advance_cursor(&self, cursor: usize) -> usize {
        for (start, end) in self.0.iter_data() {
            if cursor < start {
                break;
            }
            if cursor < end {
                return end;
            }
        }
        cursor
    }

    /// Limit a segment so it ends before the next SACKed range.
    #[inline]
    pub(super) fn clamp_segment(&self, cursor: usize, size: usize) -> usize {
        for (start, _) in self.0.iter_data() {
            if start > cursor {
                return size.min(start - cursor);
            }
        }
        size
    }

    /// Count SACKed bytes below a transmit cursor.
    #[inline]
    pub(super) fn sacked_bytes_before(&self, cursor: usize) -> usize {
        let mut sacked = 0usize;
        for (start, end) in self.0.iter_data() {
            if start >= cursor {
                break;
            }
            sacked = sacked.saturating_add(end.min(cursor).saturating_sub(start));
        }
        sacked
    }

    #[cfg(test)]
    pub(super) fn insert(&mut self, offset: usize, len: usize) {
        let _ = self.0.add(offset, len);
    }

    #[cfg(test)]
    pub(super) fn ranges(&self) -> impl Iterator<Item = (usize, usize)> + '_ {
        self.0.iter_data()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::TcpSeqNumber;

    fn block(left: i32, right: i32) -> Option<(u32, u32)> {
        Some((left as u32, right as u32))
    }

    #[test]
    fn ingest_bounds_ranges_to_transmitted_buffered_data() {
        let mut scoreboard = SackScoreboard::new();

        scoreboard.ingest(
            &[block(95, 106), block(118, 190), block(200, 210)],
            TcpSeqNumber(100),
            TcpSeqNumber(124),
            24,
        );

        assert_eq!(
            scoreboard.ranges().collect::<std::vec::Vec<_>>(),
            [(0, 6), (18, 24)]
        );
    }

    #[test]
    fn discard_acked_rebases_remaining_ranges() {
        let mut scoreboard = SackScoreboard::new();
        scoreboard.ingest(
            &[block(106, 112), block(118, 124), None],
            TcpSeqNumber(100),
            TcpSeqNumber(124),
            24,
        );

        scoreboard.discard_acked(6);

        assert_eq!(
            scoreboard.ranges().collect::<std::vec::Vec<_>>(),
            [(0, 6), (12, 18)]
        );
    }

    #[test]
    fn selective_walk_skips_and_clamps_sacked_ranges() {
        let mut scoreboard = SackScoreboard::new();
        scoreboard.ingest(
            &[block(106, 112), block(118, 124), None],
            TcpSeqNumber(100),
            TcpSeqNumber(124),
            24,
        );

        assert_eq!(scoreboard.advance_cursor(0), 0);
        assert_eq!(scoreboard.advance_cursor(6), 12);
        assert_eq!(scoreboard.advance_cursor(9), 12);
        assert_eq!(scoreboard.clamp_segment(0, 24), 6);
        assert_eq!(scoreboard.clamp_segment(12, 12), 6);
        assert_eq!(scoreboard.sacked_bytes_before(9), 3);
        assert_eq!(scoreboard.sacked_bytes_before(24), 12);
    }
}
