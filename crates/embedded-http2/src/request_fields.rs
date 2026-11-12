use crate::HeaderField;

const PSEUDO_PREFIX: char = ':';
const METHOD: &str = ":method";
const SCHEME: &str = ":scheme";
const PATH: &str = ":path";
const AUTHORITY: &str = ":authority";
const CONNECT: &str = "CONNECT";
const CONTENT_LENGTH: &str = "content-length";
const TE: &str = "te";
const TRAILERS: &str = "trailers";
const CONNECTION_FIELDS: &[&str] = &[
    "connection",
    "proxy-connection",
    "keep-alive",
    "transfer-encoding",
    "upgrade",
];

#[derive(Debug)]
pub(super) struct MalformedRequest;

/// Remaining content bytes declared by a request, when it supplies a length.
pub(super) struct RequestLength(Option<u64>);

impl RequestLength {
    pub(super) fn parse(fields: &[HeaderField]) -> Result<Self, MalformedRequest> {
        let pseudo_count = fields
            .iter()
            .take_while(|field| field.name.starts_with(PSEUDO_PREFIX))
            .count();
        let (pseudo, regular) = fields.split_at(pseudo_count);
        parse_pseudo_headers(pseudo)?;

        let mut length = None;
        for field in regular {
            check_regular_field(field)?;
            if field.name == CONTENT_LENGTH {
                let parsed = parse_content_length(&field.value)?;
                if length
                    .replace(parsed)
                    .is_some_and(|previous| previous != parsed)
                {
                    return Err(MalformedRequest);
                }
            }
        }
        Ok(Self(length))
    }

    pub(super) fn consume(&mut self, bytes: usize) -> Result<(), MalformedRequest> {
        if let Some(remaining) = &mut self.0 {
            *remaining = remaining
                .checked_sub(bytes as u64)
                .ok_or(MalformedRequest)?;
        }
        Ok(())
    }

    pub(super) fn finish(&self) -> Result<(), MalformedRequest> {
        if self.0.is_some_and(|remaining| remaining != 0) {
            return Err(MalformedRequest);
        }
        Ok(())
    }
}

#[derive(Default)]
struct PseudoHeaders<'a> {
    method: Option<&'a str>,
    scheme: Option<&'a str>,
    path: Option<&'a str>,
    authority: Option<&'a str>,
}

fn parse_pseudo_headers(fields: &[HeaderField]) -> Result<(), MalformedRequest> {
    let mut headers = PseudoHeaders::default();
    for field in fields {
        let value = field.value.as_str();
        if value.is_empty() || !field_value(value) {
            return Err(MalformedRequest);
        }
        let slot = match field.name.as_str() {
            METHOD if token(value) => &mut headers.method,
            SCHEME => &mut headers.scheme,
            PATH => &mut headers.path,
            AUTHORITY => &mut headers.authority,
            _ => return Err(MalformedRequest),
        };
        if slot.replace(value).is_some() {
            return Err(MalformedRequest);
        }
    }

    // RFC 9113 §§8.3.1 and 8.5 define the request pseudo-header sets.
    match headers.method {
        Some(CONNECT)
            if headers.authority.is_some()
                && headers.scheme.is_none()
                && headers.path.is_none() =>
        {
            Ok(())
        }
        Some(method) if method != CONNECT && headers.scheme.is_some() && headers.path.is_some() => {
            Ok(())
        }
        _ => Err(MalformedRequest),
    }
}

fn parse_content_length(value: &str) -> Result<u64, MalformedRequest> {
    // RFC 9110 §8.6 defines the length as one or more decimal digits.
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(MalformedRequest);
    }
    value.parse().map_err(|_| MalformedRequest)
}

pub(super) fn check_trailers(fields: &[HeaderField]) -> Result<(), MalformedRequest> {
    for field in fields {
        if field.name == CONTENT_LENGTH {
            return Err(MalformedRequest);
        }
        check_regular_field(field)?;
    }
    Ok(())
}

// RFC 9113 §§8.2.1–8.2.2 constrain names, values, and connection-specific fields.
fn check_regular_field(field: &HeaderField) -> Result<(), MalformedRequest> {
    let name = field.name.as_str();
    let value = field.value.as_str();
    if !token(name)
        || name.bytes().any(|byte| byte.is_ascii_uppercase())
        || !field_value(value)
        || CONNECTION_FIELDS.contains(&name)
        || (name == TE && !value.eq_ignore_ascii_case(TRAILERS))
    {
        return Err(MalformedRequest);
    }
    Ok(())
}

fn token(value: &str) -> bool {
    // RFC 9110 §5.6.2 defines the non-alphanumeric tchar set.
    const TOKEN_PUNCTUATION: &[u8] = b"!#$%&'*+-.^_`|~";
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || TOKEN_PUNCTUATION.contains(&byte))
}

fn field_value(value: &str) -> bool {
    const FIELD_WHITESPACE: [char; 2] = [' ', '\t'];
    const FORBIDDEN_FIELD_CHARS: [char; 3] = ['\0', '\r', '\n'];
    !value.starts_with(FIELD_WHITESPACE)
        && !value.ends_with(FIELD_WHITESPACE)
        && !value.contains(FORBIDDEN_FIELD_CHARS)
}
