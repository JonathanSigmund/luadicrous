use std::mem;

use nom::{
    Err, IResult,
    error::{Error, ErrorKind, ParseError},
};

pub use header::Header;

use crate::{
    chunk::header::{Endianness, Format},
    function::Function,
};

pub mod header;

#[derive(Debug)]
pub struct Chunk<'a> {
    pub header: Header,
    pub function: Function<'a>,
}

impl<'a> Chunk<'a> {
    pub fn parse(input: &'a [u8]) -> IResult<&'a [u8], Self> {
        let (input, header) = Header::parse(input)?;
        let supported = header.version_number == 0x51
            && header.format == Format::Official
            && header.endianness == Endianness::Little
            && header.int_width as usize == mem::size_of::<i32>()
            && matches!(header.size_t_width, 4 | 8)
            && header.instr_width as usize == mem::size_of::<u32>()
            && header.number_width as usize == mem::size_of::<f64>()
            && !header.number_is_integral;
        if !supported {
            return Err(Err::Failure(Error::from_error_kind(
                input,
                ErrorKind::Verify,
            )));
        }
        let (input, function) = Function::parse(input, header.size_t_width)?;

        Ok((input, Self { header, function }))
    }
}
