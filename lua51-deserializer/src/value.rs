use enum_as_inner::EnumAsInner;
use nom::{
    Err, IResult,
    bytes::complete::take,
    error::{Error, ErrorKind, ParseError},
    multi::count,
    number::complete::{le_f64, le_u8, le_u32, le_u64},
};

#[derive(Debug, EnumAsInner)]
pub enum Value<'a> {
    Nil,
    Boolean(bool),
    Number(f64),
    String(&'a [u8]),
}

impl<'a> Value<'a> {
    pub fn parse(input: &'a [u8], size_t_width: u8) -> IResult<&'a [u8], Self> {
        let (input, kind) = le_u8(input)?;

        match kind {
            0 => Ok((input, Self::Nil)),
            1 => {
                let (input, value) = le_u8(input)?;

                Ok((input, Self::Boolean(value != 0)))
            }
            3 => {
                let (input, value) = le_f64(input)?;

                Ok((input, Self::Number(value)))
            }
            4 => {
                let (input, value) = parse_string(input, size_t_width)?;
                let value = value.strip_suffix(&[0]).unwrap_or(value);
                Ok((input, Self::String(value)))
            }
            _ => Err(Err::Failure(Error::from_error_kind(
                input,
                ErrorKind::Switch,
            ))),
        }
    }
}

pub fn parse_string(input: &[u8], size_t_width: u8) -> IResult<&[u8], &[u8]> {
    let (input, string_length) = match size_t_width {
        4 => {
            let (input, value) = le_u32(input)?;
            (input, u64::from(value))
        }
        8 => le_u64(input)?,
        _ => {
            return Err(Err::Failure(Error::from_error_kind(
                input,
                ErrorKind::Verify,
            )));
        }
    };
    let length = usize::try_from(string_length)
        .map_err(|_| Err::Failure(Error::from_error_kind(input, ErrorKind::TooLarge)))?;
    take(length)(input)
}

pub fn parse_strings(input: &[u8], size_t_width: u8) -> IResult<&[u8], Vec<&[u8]>> {
    let (input, string_count) = le_u32(input)?;
    let (input, strings) = count(
        |input| parse_string(input, size_t_width),
        string_count as usize,
    )(input)?;

    Ok((input, strings))
}
