use crate::HeaderField;

#[derive(Debug)]
pub(super) struct MalformedRequest;

/// Remaining content bytes declared by a request, when it supplies a length.
pub(super) struct RequestLength(Option<u64>);

impl RequestLength {
    pub(super) fn parse(fields: &[HeaderField]) -> Result<Self, MalformedRequest> {
        let mut method = None;
        let mut scheme = None;
        let mut path = None;
        let mut authority = None;
        let mut length = None;
        let mut regular_seen = false;

        for field in fields {
            let name = field.name.as_str();
            let value = field.value.as_str();
            if !field_value(value) {
                return Err(MalformedRequest);
            }
            if name.starts_with(':') {
                if regular_seen || value.is_empty() {
                    return Err(MalformedRequest);
                }
                let slot = match name {
                    ":method" if token(value) => &mut method,
                    ":scheme" => &mut scheme,
                    ":path" => &mut path,
                    ":authority" => &mut authority,
                    _ => return Err(MalformedRequest),
                };
                if slot.replace(value).is_some() {
                    return Err(MalformedRequest);
                }
                continue;
            }

            regular_seen = true;
            check_regular_field(name, value)?;
            if name == "content-length" {
                // RFC 9110 §8.6 defines the length as one or more decimal digits.
                if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                    return Err(MalformedRequest);
                }
                let parsed = value.parse::<u64>().map_err(|_| MalformedRequest)?;
                if length
                    .replace(parsed)
                    .is_some_and(|previous| previous != parsed)
                {
                    return Err(MalformedRequest);
                }
            }
        }

        // RFC 9113 §§8.3.1 and 8.5 define the request pseudo-header sets.
        match method {
            Some("CONNECT") if authority.is_some() && scheme.is_none() && path.is_none() => {}
            Some(method) if method != "CONNECT" && scheme.is_some() && path.is_some() => {}
            _ => return Err(MalformedRequest),
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

pub(super) fn check_trailers(fields: &[HeaderField]) -> Result<(), MalformedRequest> {
    for field in fields {
        if field.name == "content-length" || !field_value(&field.value) {
            return Err(MalformedRequest);
        }
        check_regular_field(&field.name, &field.value)?;
    }
    Ok(())
}

// RFC 9113 §§8.2.1–8.2.2 constrain names, connection-specific fields, and TE.
fn check_regular_field(name: &str, value: &str) -> Result<(), MalformedRequest> {
    if !token(name) || name.bytes().any(|byte| byte.is_ascii_uppercase()) {
        return Err(MalformedRequest);
    }
    match name {
        "connection" | "proxy-connection" | "keep-alive" | "transfer-encoding" | "upgrade" => {
            Err(MalformedRequest)
        }
        "te" if !value.eq_ignore_ascii_case("trailers") => Err(MalformedRequest),
        _ => Ok(()),
    }
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
    !value.starts_with(FIELD_WHITESPACE)
        && !value.ends_with(FIELD_WHITESPACE)
        && !value.contains(['\0', '\r', '\n'])
}
