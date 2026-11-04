//! Shared parsing for HTTP `Content-Range` response headers.
//!
//! The parser crate handles the wire grammar. Grab keeps a small internal type
//! so callers can apply their own transfer-specific range and validator checks.

/// A parsed byte-range response header.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ParsedContentRange {
    /// A satisfiable byte range, with an optional complete representation length.
    Satisfied {
        first_byte: u64,
        last_byte: u64,
        complete_length: Option<u64>,
    },
    /// An unsatisfied byte range, returned with HTTP 416.
    Unsatisfied { complete_length: u64 },
}

/// Parse an HTTP `Content-Range` header into its byte-range representation.
///
/// The upstream parser validates the range grammar, numeric bounds, ordering,
/// and the relationship between the returned range and a known total length.
/// Grab's callers remain responsible for checking the requested offset and the
/// expected resource identity.
///
#[must_use]
pub(crate) fn parse_content_range(value: &str) -> Option<ParsedContentRange> {
    use http_content_range::ContentRange;

    match ContentRange::parse(value)? {
        ContentRange::Bytes(range) => Some(ParsedContentRange::Satisfied {
            first_byte: range.first_byte,
            last_byte: range.last_byte,
            complete_length: Some(range.complete_length),
        }),
        ContentRange::UnboundBytes(range) => Some(ParsedContentRange::Satisfied {
            first_byte: range.first_byte,
            last_byte: range.last_byte,
            complete_length: None,
        }),
        ContentRange::Unsatisfied(range) => Some(ParsedContentRange::Unsatisfied {
            complete_length: range.complete_length,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::{ParsedContentRange, parse_content_range};

    #[test]
    fn parses_satisfied_ranges() {
        assert_eq!(
            parse_content_range("bytes 42-69/420"),
            Some(ParsedContentRange::Satisfied {
                first_byte: 42,
                last_byte: 69,
                complete_length: Some(420),
            }),
        );
        assert_eq!(
            parse_content_range("bytes 0-0/1"),
            Some(ParsedContentRange::Satisfied {
                first_byte: 0,
                last_byte: 0,
                complete_length: Some(1),
            }),
        );
    }

    #[test]
    fn parses_ranges_with_unknown_complete_length() {
        assert_eq!(
            parse_content_range("bytes 42-69/*"),
            Some(ParsedContentRange::Satisfied {
                first_byte: 42,
                last_byte: 69,
                complete_length: None,
            }),
        );
    }

    #[test]
    fn parses_unsatisfied_ranges() {
        assert_eq!(
            parse_content_range("bytes */420"),
            Some(ParsedContentRange::Unsatisfied {
                complete_length: 420,
            }),
        );
    }

    #[test]
    fn accepts_optional_protocol_whitespace() {
        assert_eq!(
            parse_content_range("bytes\t 0 \t -\t \t  \t9 / 20   "),
            Some(ParsedContentRange::Satisfied {
                first_byte: 0,
                last_byte: 9,
                complete_length: Some(20),
            }),
        );
    }

    #[test]
    fn rejects_malformed_and_out_of_bounds_ranges() {
        for header in [
            "",
            "items 1-2/3",
            "bytes 99-0/12345",
            "bytes 1-20/20",
            "bytes 0-0/0",
            "bytes 1-2/not-a-number",
            "bytes 1-18446744073709551616/20",
            "bytes 0-9/20 extra",
        ] {
            assert_eq!(
                parse_content_range(header),
                None,
                "unexpectedly accepted {header:?}",
            );
        }
    }
}
