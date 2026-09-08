//! Phone-number parsing that copes with how people actually type numbers:
//! `212-736-5000`, `(212) 736 5000`, `1 212 736 5000`, `+972 50-123-4567`,
//! `0501234567` (with a default region), or a shared Telegram contact.

use phonenumber::country::Id;
use phonenumber::Mode;
use std::str::FromStr;

/// Parse `input` into E.164 using `default_region` (ISO 3166-1 alpha-2, e.g. `US`)
/// when no country code is present. Returns a human-readable reason on failure.
pub fn parse_phone(input: &str, default_region: &str) -> Result<String, String> {
    let raw = input.trim();
    if raw.is_empty() {
        return Err("no number given".into());
    }
    // Letters are almost always a typo or a word; digits/punctuation only.
    if raw.chars().any(|c| c.is_alphabetic()) {
        return Err("that has letters in it".into());
    }
    let region = Id::from_str(&default_region.to_ascii_uppercase()).unwrap_or(Id::US);
    let mut candidate = raw.to_owned();
    // "00 44 ..." international prefix → "+44 ..."
    let digits: String = raw.chars().filter(|c| c.is_ascii_digit()).collect();
    if raw.starts_with("00") {
        candidate = format!("+{}", &digits[2..]);
    }
    let attempt = |s: &str| {
        phonenumber::parse(Some(region), s)
            .ok()
            .filter(|n| n.is_valid())
    };
    let parsed = attempt(&candidate)
        // Sometimes people paste "+1 (440)…" with stray characters; retry on digits only.
        .or_else(|| {
            if raw.starts_with('+') {
                attempt(&format!("+{digits}"))
            } else {
                attempt(&digits)
            }
        })
        // A bare 11-digit number starting with 1 typed by a US user.
        .or_else(|| {
            if digits.len() == 11 && digits.starts_with('1') {
                attempt(&format!("+{digits}"))
            } else {
                None
            }
        });
    match parsed {
        Some(n) => Ok(n.format().mode(Mode::E164).to_string()),
        None => {
            if digits.len() < 7 {
                Err("too short".into())
            } else {
                Err(format!(
                    "not a valid number for {} — include the country code, e.g. +1…",
                    region.as_ref()
                ))
            }
        }
    }
}

/// Parse a free-text list of numbers (comma / space / newline separated),
/// returning (valid E.164, rejected inputs).
pub fn parse_phone_list(input: &str, default_region: &str) -> (Vec<String>, Vec<String>) {
    let mut ok = Vec::new();
    let mut bad = Vec::new();
    // Split on commas/semicolons/newlines first; then treat each piece as one number
    // (spaces inside a number are common: "212 736 5000").
    for piece in input.split([',', ';', '\n']) {
        let piece = piece.trim();
        if piece.is_empty() {
            continue;
        }
        match parse_phone(piece, default_region) {
            Ok(p) => {
                if !ok.contains(&p) {
                    ok.push(p)
                }
            }
            Err(_) => {
                // Maybe several numbers separated by spaces: "+1440… +1216…"
                let parts: Vec<&str> = piece.split_whitespace().collect();
                if parts.len() > 1
                    && parts.iter().all(|p| {
                        p.starts_with('+') || p.chars().filter(|c| c.is_ascii_digit()).count() >= 10
                    })
                {
                    for p in parts {
                        match parse_phone(p, default_region) {
                            Ok(v) => {
                                if !ok.contains(&v) {
                                    ok.push(v)
                                }
                            }
                            Err(_) => bad.push(p.to_owned()),
                        }
                    }
                } else {
                    bad.push(piece.to_owned());
                }
            }
        }
    }
    (ok, bad)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn us_formats() {
        for s in [
            "2127365000",
            "212-736-5000",
            "(212) 736 5000",
            "1 212 736 5000",
            "+1 555 010 0123",
            "12127365000",
            "+12127365000",
        ] {
            assert_eq!(parse_phone(s, "US").unwrap(), "+12127365000", "{s}");
        }
    }

    #[test]
    fn international_and_defaults() {
        assert_eq!(
            parse_phone("+972 50-123-4567", "US").unwrap(),
            "+972501234567"
        );
        assert_eq!(parse_phone("0501234567", "IL").unwrap(), "+972501234567");
        assert_eq!(
            parse_phone("00972501234567", "US").unwrap(),
            "+972501234567"
        );
        assert_eq!(
            parse_phone("+44 20 7946 0958", "US").unwrap(),
            "+442079460958"
        );
        assert!(parse_phone("hello", "US").is_err());
        assert!(parse_phone("12345", "US").is_err());
        assert!(
            parse_phone("2127365000", "IL").is_err(),
            "10 digits is not an Israeli number"
        );
    }

    #[test]
    fn lists() {
        let (ok, bad) = parse_phone_list("212-736-5000, +972501234567; 216 555 0100\nabc", "US");
        assert_eq!(ok, vec!["+12127365000", "+972501234567", "+12165550100"]);
        assert_eq!(bad, vec!["abc"]);
        let (ok, _) = parse_phone_list("+12127365000 +12165550100", "US");
        assert_eq!(ok.len(), 2);
    }
}
