use http::HeaderValue;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContentCoding {
    Identity,
    Gzip,
    Brotli,
    Zstd,
    NotAcceptable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EncodingNegotiation {
    pub preferred: ContentCoding,
    pub identity_acceptable: bool,
}

impl ContentCoding {
    pub fn as_str(self) -> Option<&'static str> {
        match self {
            Self::Identity | Self::NotAcceptable => None,
            Self::Gzip => Some("gzip"),
            Self::Brotli => Some("br"),
            Self::Zstd => Some("zstd"),
        }
    }
}

#[derive(Default)]
struct Preferences {
    gzip: Option<u16>,
    brotli: Option<u16>,
    zstd: Option<u16>,
    identity: Option<u16>,
    wildcard: Option<u16>,
}

impl Preferences {
    fn record(slot: &mut Option<u16>, quality: u16) {
        *slot = Some(slot.map_or(quality, |current| current.max(quality)));
    }

    fn parse_value(&mut self, value: &HeaderValue) {
        let Ok(value) = value.to_str() else {
            return;
        };
        for item in value.split(',') {
            let mut parts = item.trim().split(';');
            let name = parts.next().unwrap_or_default().trim();
            if name.is_empty() {
                continue;
            }
            let mut quality = 1_000;
            for parameter in parts {
                let Some((key, value)) = parameter.trim().split_once('=') else {
                    continue;
                };
                if key.trim().eq_ignore_ascii_case("q") {
                    quality = parse_quality(value.trim()).unwrap_or(0);
                }
            }

            if name.eq_ignore_ascii_case("gzip") {
                Self::record(&mut self.gzip, quality);
            } else if name.eq_ignore_ascii_case("br") {
                Self::record(&mut self.brotli, quality);
            } else if name.eq_ignore_ascii_case("zstd") {
                Self::record(&mut self.zstd, quality);
            } else if name.eq_ignore_ascii_case("identity") {
                Self::record(&mut self.identity, quality);
            } else if name == "*" {
                Self::record(&mut self.wildcard, quality);
            }
        }
    }
}

pub fn negotiate<'a>(values: impl IntoIterator<Item = &'a HeaderValue>) -> EncodingNegotiation {
    let mut values = values.into_iter();
    let Some(first) = values.next() else {
        return EncodingNegotiation {
            preferred: ContentCoding::Identity,
            identity_acceptable: true,
        };
    };
    let second = values.next();
    if second.is_none() {
        let value = first.as_bytes();
        let preferred = if value.eq_ignore_ascii_case(b"identity") {
            Some(ContentCoding::Identity)
        } else if value.eq_ignore_ascii_case(b"gzip") {
            Some(ContentCoding::Gzip)
        } else if value.eq_ignore_ascii_case(b"br") {
            Some(ContentCoding::Brotli)
        } else if value.eq_ignore_ascii_case(b"zstd") || value == b"*" {
            Some(ContentCoding::Zstd)
        } else {
            None
        };
        if let Some(preferred) = preferred {
            return EncodingNegotiation {
                preferred,
                identity_acceptable: true,
            };
        }
    }

    let mut preferences = Preferences::default();
    preferences.parse_value(first);
    if let Some(second) = second {
        preferences.parse_value(second);
    }
    for value in values {
        preferences.parse_value(value);
    }

    let wildcard = preferences.wildcard.unwrap_or(0);
    let identity_acceptable = preferences.identity.unwrap_or_else(|| {
        if preferences.wildcard == Some(0) {
            0
        } else {
            1_000
        }
    }) > 0;
    // An implicit identity representation is a safe fallback, not an instruction
    // to defeat every explicitly accepted compression coding. Only an explicit
    // identity quality participates in preference ordering.
    let identity_preference = preferences.identity.unwrap_or(0);
    let candidates = [
        (ContentCoding::Zstd, preferences.zstd.unwrap_or(wildcard)),
        (
            ContentCoding::Brotli,
            preferences.brotli.unwrap_or(wildcard),
        ),
        (ContentCoding::Gzip, preferences.gzip.unwrap_or(wildcard)),
    ];
    let mut selected = (ContentCoding::NotAcceptable, 0);
    for candidate in candidates {
        if candidate.1 > selected.1 {
            selected = candidate;
        }
    }

    let preferred = if selected.1 > 0 && selected.1 >= identity_preference {
        selected.0
    } else if identity_acceptable {
        ContentCoding::Identity
    } else {
        ContentCoding::NotAcceptable
    };
    EncodingNegotiation {
        preferred,
        identity_acceptable,
    }
}

fn parse_quality(value: &str) -> Option<u16> {
    if value == "0" {
        return Some(0);
    }
    if value == "1" {
        return Some(1_000);
    }

    let (whole, fraction) = value.split_once('.')?;
    if fraction.len() > 3 || !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    if whole == "1" {
        return fraction.bytes().all(|byte| byte == b'0').then_some(1_000);
    }
    if whole != "0" && !whole.is_empty() {
        return None;
    }
    let digits = fraction.parse::<u16>().ok()?;
    Some(match fraction.len() {
        0 => 0,
        1 => digits * 100,
        2 => digits * 10,
        3 => digits,
        _ => unreachable!(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn select(values: &[&str]) -> ContentCoding {
        let values = values
            .iter()
            .map(|value| HeaderValue::from_str(value).unwrap())
            .collect::<Vec<_>>();
        negotiate(values.iter()).preferred
    }

    #[test]
    fn quality_and_identity_preferences_are_respected() {
        assert_eq!(select(&[]), ContentCoding::Identity);
        assert_eq!(select(&["gzip;q=0"]), ContentCoding::Identity);
        assert_eq!(
            select(&["identity;q=1, gzip;q=.5"]),
            ContentCoding::Identity
        );
        assert_eq!(select(&["identity;q=0, gzip;q=.5"]), ContentCoding::Gzip);
        assert_eq!(
            select(&["identity;q=0, *;q=0"]),
            ContentCoding::NotAcceptable
        );
    }

    #[test]
    fn duplicate_fields_and_specific_wildcard_overrides_are_combined() {
        assert_eq!(select(&["gzip;q=0", "br"]), ContentCoding::Brotli);
        assert_eq!(select(&["*;q=0.5", "br;q=0"]), ContentCoding::Zstd);
        assert_eq!(select(&["gzip;q=0.4", "gzip;q=0.8"]), ContentCoding::Gzip);
    }

    #[test]
    fn exact_common_tokens_use_the_same_negotiation_semantics() {
        assert_eq!(select(&["identity"]), ContentCoding::Identity);
        assert_eq!(select(&["gzip"]), ContentCoding::Gzip);
        assert_eq!(select(&["BR"]), ContentCoding::Brotli);
        assert_eq!(select(&["zstd"]), ContentCoding::Zstd);
        assert_eq!(select(&["*"]), ContentCoding::Zstd);
    }

    #[test]
    fn malformed_or_out_of_range_quality_never_enables_a_coding() {
        assert_eq!(select(&["gzip;q=NaN"]), ContentCoding::Identity);
        assert_eq!(select(&["gzip;q=1.1"]), ContentCoding::Identity);
        assert_eq!(
            select(&["identity;q=0, gzip;q=0.0000"]),
            ContentCoding::NotAcceptable
        );
    }
}
