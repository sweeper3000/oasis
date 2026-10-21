use crate::entity::error::Error;
use rocket::{
    http::Status,
    request::{FromRequest, Outcome},
    Request,
};

#[derive(Clone, Copy)]
pub enum ByteRange {
    FromTo(u64, Option<u64>),
    Suffix(u64),
}
pub struct Range {
    pub range: Option<ByteRange>,
}

impl ByteRange {
    pub fn parse(value: &str) -> Result<Self, ()> {
        let value = value.strip_prefix("bytes=").ok_or(())?;
        let (start, end) = value.split_once('-').ok_or(())?;
        if start.is_empty() {
            let suffix = end.parse::<u64>().map_err(|_| ())?;
            return if suffix > 0 {
                Ok(Self::Suffix(suffix))
            } else {
                Err(())
            };
        }
        let start = start.parse::<u64>().map_err(|_| ())?;
        let end = if end.is_empty() {
            None
        } else {
            Some(end.parse::<u64>().map_err(|_| ())?)
        };
        if end.map(|e| e < start).unwrap_or(false) {
            return Err(());
        }
        Ok(Self::FromTo(start, end))
    }

    pub fn resolve(self, size: u64) -> Option<(u64, u64)> {
        if size == 0 {
            return None;
        }
        match self {
            Self::Suffix(length) => Some((size.saturating_sub(length), size - 1)),
            Self::FromTo(start, end) if start < size => {
                Some((start, end.unwrap_or(size - 1).min(size - 1)))
            }
            _ => None,
        }
    }
}

#[rocket::async_trait]
impl<'r> FromRequest<'r> for Range {
    type Error = Error;
    async fn from_request(request: &'r Request<'_>) -> Outcome<Self, Self::Error> {
        match request.headers().get_one("Range") {
            None => Outcome::Success(Self { range: None }),
            Some(value) => match ByteRange::parse(value) {
                Ok(range) => Outcome::Success(Self { range: Some(range) }),
                Err(()) => Outcome::Failure((Status::RangeNotSatisfiable, Error::BadRequest)),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn resolves_bounded_open_and_suffix_ranges() {
        for (input, expected) in [
            ("bytes=0-0", Some((0, 0))),
            ("bytes=5-", Some((5, 9))),
            ("bytes=-3", Some((7, 9))),
            ("bytes=1-999", Some((1, 9))),
            ("bytes=10-", None),
        ] {
            assert_eq!(ByteRange::parse(input).unwrap().resolve(10), expected);
        }
        for value in ["bytes=3-1", "bytes=-0", "bytes=0-1,4-5", "items=0-2"] {
            assert!(ByteRange::parse(value).is_err());
        }
        assert_eq!(ByteRange::parse("bytes=0-").unwrap().resolve(0), None);
    }
}
