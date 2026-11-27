//! The few DER fields startup validation reads from a certificate: the outer
//! `Certificate` shape and the leaf's validity period. rustls (webpki) parses
//! the leaf for everything cryptographic; this walk only finds two dates, so
//! a full X.509 parser (and its dependency tree) stays out of the server.
//!
//! Strict: definite, minimal lengths only; single-byte tags only; nothing may
//! trail a structure that is read to its end. Anything unexpected is `None`.
use std::fmt;

const SEQUENCE: u8 = 0x30;
const INTEGER: u8 = 0x02;
const BIT_STRING: u8 = 0x03;
const UTC_TIME: u8 = 0x17;
const GENERALIZED_TIME: u8 = 0x18;
/// `[0] EXPLICIT`, the optional `version` of a TBSCertificate.
const VERSION: u8 = 0xa0;

/// A cursor over the contents of one DER structure.
struct Der<'a>(&'a [u8]);

impl<'a> Der<'a> {
    /// The next tag and its contents.
    fn next(&mut self) -> Option<(u8, &'a [u8])> {
        let (&tag, rest) = self.0.split_first()?;
        // High tag numbers (more tag bytes follow) never occur where we read.
        if tag & 0x1f == 0x1f {
            return None;
        }
        let (&first, rest) = rest.split_first()?;
        let (len, rest) = if first < 0x80 {
            (usize::from(first), rest)
        } else {
            // 0x80 is BER's indefinite length; more than four length bytes
            // can't describe anything inside a 1 MiB file.
            let count = usize::from(first & 0x7f);
            if count == 0 || count > 4 || rest.len() < count {
                return None;
            }
            let (digits, rest) = rest.split_at(count);
            let len = digits
                .iter()
                .fold(0_usize, |n, &b| (n << 8) | usize::from(b));
            // DER: the shortest encoding only.
            if digits[0] == 0 || len < 0x80 {
                return None;
            }
            (len, rest)
        };
        if rest.len() < len {
            return None;
        }
        let (contents, rest) = rest.split_at(len);
        self.0 = rest;
        Some((tag, contents))
    }

    fn expect(&mut self, tag: u8) -> Option<&'a [u8]> {
        self.next().filter(|(found, _)| *found == tag).map(|t| t.1)
    }

    fn end(&self) -> Option<()> {
        self.0.is_empty().then_some(())
    }
}

/// `Certificate ::= SEQUENCE { tbsCertificate, signatureAlgorithm,
/// signatureValue }` with nothing after it; returns the TBSCertificate's
/// contents.
fn tbs_certificate(certificate: &[u8]) -> Option<&[u8]> {
    let mut outer = Der(certificate);
    let mut fields = Der(outer.expect(SEQUENCE)?);
    outer.end()?;
    let tbs = fields.expect(SEQUENCE)?;
    fields.expect(SEQUENCE)?;
    fields.expect(BIT_STRING)?;
    fields.end()?;
    Some(tbs)
}

/// Whether the bytes have the outer shape of an X.509 certificate. Used for
/// intermediates, which the server sends but never interprets.
pub(super) fn is_certificate(certificate: &[u8]) -> bool {
    tbs_certificate(certificate).is_some()
}

/// The `notBefore` and `notAfter` of a certificate.
pub(super) fn validity(certificate: &[u8]) -> Option<(Time, Time)> {
    let mut tbs = Der(tbs_certificate(certificate)?);
    let (mut tag, mut field) = tbs.next()?;
    if tag == VERSION {
        let mut version = Der(field);
        version.expect(INTEGER)?;
        version.end()?;
        (tag, field) = tbs.next()?;
    }
    // serialNumber; the contents are not read.
    let _ = field;
    if tag != INTEGER {
        return None;
    }
    tbs.expect(SEQUENCE)?; // signature AlgorithmIdentifier
    tbs.expect(SEQUENCE)?; // issuer
    let mut validity = Der(tbs.expect(SEQUENCE)?);
    let (tag, text) = validity.next()?;
    let not_before = Time::parse(tag, text)?;
    let (tag, text) = validity.next()?;
    let not_after = Time::parse(tag, text)?;
    validity.end()?;
    Some((not_before, not_after))
}

/// A UTC instant as a certificate states it, to the second.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Time {
    year: i64,
    month: i64,
    day: i64,
    hour: i64,
    minute: i64,
    second: i64,
}

impl Time {
    /// RFC 5280 §4.1.2.5: UTCTime `YYMMDDHHMMSSZ` (50–99 are 19xx, 00–49 are
    /// 20xx) or GeneralizedTime `YYYYMMDDHHMMSSZ`; seconds present, no
    /// fraction, no offset.
    fn parse(tag: u8, text: &[u8]) -> Option<Self> {
        let (digits, zulu) = text.split_at_checked(text.len().checked_sub(1)?)?;
        if zulu != b"Z" || !digits.iter().all(u8::is_ascii_digit) {
            return None;
        }
        let number = |range: std::ops::Range<usize>| {
            digits[range]
                .iter()
                .fold(0_i64, |n, &d| n * 10 + i64::from(d - b'0'))
        };
        let (year, rest) = match (tag, digits.len()) {
            (UTC_TIME, 12) => {
                let year = number(0..2);
                (if year >= 50 { 1900 + year } else { 2000 + year }, 2)
            }
            (GENERALIZED_TIME, 14) => (number(0..4), 4),
            _ => return None,
        };
        let part = |i: usize| number(rest + 2 * i..rest + 2 * i + 2);
        let time = Self {
            year,
            month: part(0),
            day: part(1),
            hour: part(2),
            minute: part(3),
            second: part(4),
        };
        let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
        let days = match time.month {
            1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
            4 | 6 | 9 | 11 => 30,
            2 if leap => 29,
            2 => 28,
            _ => return None,
        };
        ((1..=days).contains(&time.day) && time.hour < 24 && time.minute < 60 && time.second < 60)
            .then_some(time)
    }

    /// Seconds since 1970-01-01T00:00:00Z (negative before).
    pub(super) fn unix(&self) -> i64 {
        // Days from the civil date (proleptic Gregorian), counting years
        // from March so the leap day is the last day of a year.
        let year = if self.month <= 2 {
            self.year - 1
        } else {
            self.year
        };
        let era = year.div_euclid(400);
        let year_of_era = year.rem_euclid(400);
        let month = if self.month > 2 {
            self.month - 3
        } else {
            self.month + 9
        };
        let day_of_year = (153 * month + 2) / 5 + self.day - 1;
        let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
        let days = era * 146_097 + day_of_era - 719_468;
        days * 86_400 + self.hour * 3_600 + self.minute * 60 + self.second
    }
}

/// `YYYY-MM-DD HH:MM:SSZ`.
impl fmt::Display for Time {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}Z",
            self.year, self.month, self.day, self.hour, self.minute, self.second
        )
    }
}

#[cfg(test)]
mod tests;
