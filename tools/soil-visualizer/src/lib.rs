/// One calibrated record emitted by `soil-demo`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sample {
    pub time_ms: u64,
    pub raw_adc: u16,
    pub filtered_adc: u16,
    pub moisture_percent: u8,
}

/// Parses only complete soil telemetry records and ignores diagnostic text.
pub fn parse_sample(line: &str) -> Option<Sample> {
    let mut fields = line.trim().split(',');
    if fields.next()? != "SOIL" {
        return None;
    }
    let sample = Sample {
        time_ms: fields.next()?.parse().ok()?,
        raw_adc: fields.next()?.parse().ok()?,
        filtered_adc: fields.next()?.parse().ok()?,
        moisture_percent: fields.next()?.parse().ok()?,
    };
    if fields.next().is_some() || sample.moisture_percent > 100 {
        return None;
    }
    Some(sample)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_soil_record() {
        assert_eq!(
            parse_sample("SOIL,12003,3510,3447,97\n"),
            Some(Sample {
                time_ms: 12_003,
                raw_adc: 3_510,
                filtered_adc: 3_447,
                moisture_percent: 97,
            })
        );
    }

    #[test]
    fn rejects_diagnostics_and_invalid_percentages() {
        assert_eq!(parse_sample("Wi-Fi ready"), None);
        assert_eq!(parse_sample("SOIL,1,3000,3000,101"), None);
        assert_eq!(parse_sample("SOIL,1,3000"), None);
    }
}
