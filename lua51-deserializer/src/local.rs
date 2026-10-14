use std::ops::Range;

use nom::{IResult, multi::count, number::complete::le_u32};

use crate::value::parse_string;

#[derive(Debug)]
pub struct Local<'a> {
    pub name: &'a [u8],
    pub range: Range<u32>,
}

impl<'a> Local<'a> {
    pub fn parse_list(input: &'a [u8], size_t_width: u8) -> IResult<&'a [u8], Vec<Self>> {
        let (input, length) = le_u32(input)?;

        count(|input| Self::parse(input, size_t_width), length as usize)(input)
    }

    fn parse(input: &'a [u8], size_t_width: u8) -> IResult<&'a [u8], Self> {
        let (input, name) = parse_string(input, size_t_width)?;
        let (input, start) = le_u32(input)?;
        let (input, end) = le_u32(input)?;

        Ok((
            input,
            Self {
                name: name.strip_suffix(&[0]).unwrap_or(name),
                range: (start..end),
            },
        ))
    }
}
