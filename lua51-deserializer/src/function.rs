use nom::{
    IResult,
    bytes::complete::take,
    multi::count,
    number::complete::{le_u8, le_u32},
};

use crate::{
    instruction::{Instruction, position::Position},
    local::Local,
    value::{self, Value},
};

#[derive(Debug)]
pub struct Function<'a> {
    pub name: &'a [u8],
    pub line_defined: u32,
    pub last_line_defined: u32,
    pub number_of_upvalues: u8,
    pub vararg_flag: u8,
    pub maximum_stack_size: u8,
    pub code: Vec<Instruction>,
    pub raw_code: Vec<u32>,
    pub constants: Vec<Value<'a>>,
    pub closures: Vec<Function<'a>>,
    pub positions: Vec<Position>,
    pub locals: Vec<Local<'a>>,
    pub upvalues: Vec<&'a [u8]>,
    pub number_of_parameters: u8,
}

impl<'a> Function<'a> {
    pub fn parse(input: &'a [u8], size_t_width: u8) -> IResult<&'a [u8], Self> {
        let (input, name) = value::parse_string(input, size_t_width)?;
        let (input, line_defined) = le_u32(input)?;
        let (input, last_line_defined) = le_u32(input)?;
        let (input, number_of_upvalues) = le_u8(input)?;
        let (input, number_of_parameters) = le_u8(input)?;
        let (input, vararg_flag) = le_u8(input)?;
        let (input, maximum_stack_size) = le_u8(input)?;
        let (input, code_length) = le_u32(input)?;
        let code_bytes_length = usize::try_from(code_length)
            .unwrap_or(usize::MAX)
            .saturating_mul(4);
        let (input, code_bytes) = take(code_bytes_length)(input)?;
        let (_, code) = count(Instruction::parse, code_length as usize)(code_bytes)?;
        let raw_code = code_bytes
            .chunks_exact(4)
            .map(|bytes| u32::from_le_bytes(bytes.try_into().expect("four-byte instruction")))
            .collect();
        let (input, constants_length) = le_u32(input)?;
        let (input, constants) = count(
            |input| Value::parse(input, size_t_width),
            constants_length as usize,
        )(input)?;
        let (input, closures_length) = le_u32(input)?;
        let (input, closures) = count(
            |input| Self::parse(input, size_t_width),
            closures_length as usize,
        )(input)?;
        let (input, positions) = Position::parse(input)?;
        let (input, locals) = Local::parse_list(input, size_t_width)?;
        let (input, upvalues) = value::parse_strings(input, size_t_width)?;

        Ok((
            input,
            Self {
                name,
                line_defined,
                last_line_defined,
                number_of_upvalues,
                vararg_flag,
                maximum_stack_size,
                code,
                raw_code,
                constants,
                closures,
                positions,
                locals,
                upvalues,
                number_of_parameters,
            },
        ))
    }
}
