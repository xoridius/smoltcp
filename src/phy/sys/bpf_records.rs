#[derive(Clone, Copy, Debug)]
pub(super) struct HeaderLayout {
    caplen_offset: usize,
    datalen_offset: usize,
    hdrlen_offset: usize,
    alignment: usize,
}

impl HeaderLayout {
    pub(super) const fn new(
        caplen_offset: usize,
        datalen_offset: usize,
        hdrlen_offset: usize,
        alignment: usize,
    ) -> Self {
        Self {
            caplen_offset,
            datalen_offset,
            hdrlen_offset,
            alignment,
        }
    }
}

#[derive(Debug)]
pub(super) struct ParseError;

#[derive(Debug)]
pub(super) struct Record<'a> {
    pub(super) packet: &'a [u8],
    pub(super) next_offset: usize,
}

fn native_u16(buffer: &[u8], offset: usize) -> Result<u16, ParseError> {
    let end = offset.checked_add(2).ok_or(ParseError)?;
    let bytes = buffer
        .get(offset..end)
        .ok_or(ParseError)?
        .try_into()
        .map_err(|_| ParseError)?;
    Ok(u16::from_ne_bytes(bytes))
}

fn native_u32(buffer: &[u8], offset: usize) -> Result<u32, ParseError> {
    let end = offset.checked_add(4).ok_or(ParseError)?;
    let bytes = buffer
        .get(offset..end)
        .ok_or(ParseError)?
        .try_into()
        .map_err(|_| ParseError)?;
    Ok(u32::from_ne_bytes(bytes))
}

fn word_align(len: usize, alignment: usize) -> Option<usize> {
    let mask = alignment.checked_sub(1)?;
    len.checked_add(mask).map(|len| len & !mask)
}

pub(super) fn parse_record(
    buffer: &[u8],
    offset: usize,
    layout: HeaderLayout,
) -> Result<Record<'_>, ParseError> {
    let record = buffer.get(offset..).ok_or(ParseError)?;
    let captured_len = native_u32(record, layout.caplen_offset)? as usize;
    let data_len = native_u32(record, layout.datalen_offset)? as usize;
    let header_len = native_u16(record, layout.hdrlen_offset)? as usize;
    let minimum_header_len = layout.hdrlen_offset.checked_add(2).ok_or(ParseError)?;

    if header_len < minimum_header_len || captured_len > data_len {
        return Err(ParseError);
    }

    let payload_start = offset.checked_add(header_len).ok_or(ParseError)?;
    let payload_end = payload_start.checked_add(captured_len).ok_or(ParseError)?;
    let packet = buffer.get(payload_start..payload_end).ok_or(ParseError)?;
    let record_len = header_len.checked_add(captured_len).ok_or(ParseError)?;
    let next_offset = offset
        .checked_add(word_align(record_len, layout.alignment).ok_or(ParseError)?)
        .ok_or(ParseError)?
        .min(buffer.len());

    Ok(Record {
        packet,
        next_offset,
    })
}

#[cfg(test)]
mod test {
    use super::*;

    const DARWIN_OPENBSD: HeaderLayout = HeaderLayout::new(8, 12, 16, 4);
    const NETBSD_32_FREEBSD_I686: HeaderLayout = HeaderLayout::new(8, 12, 16, 4);
    const FREEBSD_32_TIME64: HeaderLayout = HeaderLayout::new(16, 20, 24, 4);
    const NETBSD_64_FREEBSD_64: HeaderLayout = HeaderLayout::new(16, 20, 24, 8);

    fn record(
        layout: HeaderLayout,
        header_len: usize,
        captured_len: usize,
        data_len: usize,
        payload: &[u8],
        pad: bool,
    ) -> Vec<u8> {
        let mut bytes = vec![0; header_len];
        bytes[layout.caplen_offset..layout.caplen_offset + 4]
            .copy_from_slice(&(captured_len as u32).to_ne_bytes());
        bytes[layout.datalen_offset..layout.datalen_offset + 4]
            .copy_from_slice(&(data_len as u32).to_ne_bytes());
        bytes[layout.hdrlen_offset..layout.hdrlen_offset + 2]
            .copy_from_slice(&(header_len as u16).to_ne_bytes());
        bytes.extend_from_slice(payload);
        if pad {
            bytes.resize(word_align(bytes.len(), layout.alignment).unwrap(), 0);
        }
        bytes
    }

    #[test]
    fn parses_each_supported_header_layout() {
        for (layout, header_len) in [
            (DARWIN_OPENBSD, 18),
            (DARWIN_OPENBSD, 30),
            (NETBSD_32_FREEBSD_I686, 18),
            (NETBSD_64_FREEBSD_64, 34),
            (NETBSD_64_FREEBSD_64, 26),
        ] {
            let bytes = record(layout, header_len, 3, 3, b"one", false);
            let parsed = parse_record(&bytes, 0, layout).unwrap();
            assert_eq!(parsed.packet, b"one");
            assert_eq!(parsed.next_offset, bytes.len());
        }
    }

    #[test]
    fn parses_freebsd_32_time64_layout() {
        let mut bytes = record(FREEBSD_32_TIME64, 26, 7, 7, b"first!!", true);
        let second_offset = bytes.len();
        assert_eq!(second_offset, 36);
        bytes.extend(record(FREEBSD_32_TIME64, 26, 3, 3, b"two", false));

        assert!(parse_record(&bytes, 0, NETBSD_32_FREEBSD_I686).is_err());

        let first = parse_record(&bytes, 0, FREEBSD_32_TIME64).unwrap();
        assert_eq!(first.packet, b"first!!");
        assert_eq!(first.next_offset, second_offset);

        let align8 = HeaderLayout::new(16, 20, 24, 8);
        assert_eq!(parse_record(&bytes, 0, align8).unwrap().next_offset, 40);

        let second = parse_record(&bytes, first.next_offset, FREEBSD_32_TIME64).unwrap();
        assert_eq!(second.packet, b"two");
    }

    #[test]
    fn parses_multiple_aligned_records_in_order() {
        let mut bytes = record(DARWIN_OPENBSD, 18, 3, 3, b"one", true);
        let second_offset = bytes.len();
        bytes.extend(record(DARWIN_OPENBSD, 20, 5, 5, b"secon", false));

        let first = parse_record(&bytes, 0, DARWIN_OPENBSD).unwrap();
        assert_eq!(first.packet, b"one");
        assert_eq!(first.next_offset, second_offset);

        let second = parse_record(&bytes, first.next_offset, DARWIN_OPENBSD).unwrap();
        assert_eq!(second.packet, b"secon");
        assert_eq!(second.next_offset, bytes.len());
    }

    #[test]
    fn accepts_final_record_without_alignment_padding() {
        let bytes = record(NETBSD_64_FREEBSD_64, 26, 3, 3, b"end", false);
        assert_ne!(bytes.len() % NETBSD_64_FREEBSD_64.alignment, 0);

        let parsed = parse_record(&bytes, 0, NETBSD_64_FREEBSD_64).unwrap();
        assert_eq!(parsed.packet, b"end");
        assert_eq!(parsed.next_offset, bytes.len());
    }

    #[test]
    fn accepts_snaplen_truncated_capture() {
        let bytes = record(DARWIN_OPENBSD, 18, 3, 12, b"cap", false);
        assert_eq!(
            parse_record(&bytes, 0, DARWIN_OPENBSD).unwrap().packet,
            b"cap"
        );
    }

    #[test]
    fn rejects_capture_larger_than_original_data() {
        let bytes = record(DARWIN_OPENBSD, 18, 3, 2, b"bad", false);
        assert!(parse_record(&bytes, 0, DARWIN_OPENBSD).is_err());
    }

    #[test]
    fn rejects_truncated_header_fields() {
        let bytes = [0; 17];
        assert!(parse_record(&bytes, 0, DARWIN_OPENBSD).is_err());
    }

    #[test]
    fn rejects_header_length_beyond_record() {
        let mut bytes = record(DARWIN_OPENBSD, 18, 0, 0, b"", false);
        bytes[16..18].copy_from_slice(&32u16.to_ne_bytes());
        assert!(parse_record(&bytes, 0, DARWIN_OPENBSD).is_err());
    }

    #[test]
    fn rejects_truncated_payload() {
        let bytes = record(DARWIN_OPENBSD, 18, 4, 4, b"bad", false);
        assert!(parse_record(&bytes, 0, DARWIN_OPENBSD).is_err());
    }

    #[test]
    fn rejects_alignment_overflow() {
        assert_eq!(word_align(usize::MAX, 4), None);
        assert_eq!(word_align(usize::MAX - 3, 8), None);
    }

    #[test]
    fn yields_valid_prefix_before_malformed_tail() {
        let mut bytes = record(DARWIN_OPENBSD, 18, 3, 3, b"one", true);
        bytes.extend_from_slice(&[0; 17]);

        let first = parse_record(&bytes, 0, DARWIN_OPENBSD).unwrap();
        assert_eq!(first.packet, b"one");
        assert!(parse_record(&bytes, first.next_offset, DARWIN_OPENBSD).is_err());
    }
}
