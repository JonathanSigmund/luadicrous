use crate::instruction::layout::LayoutDiscriminants;
use nom::{
    Err, IResult,
    error::{Error, ErrorKind, ParseError},
    number::complete::le_u8,
};
#[derive(Debug, Clone, Copy)]
#[repr(u8)]
pub enum OperationCode {
    Move = 0,
    LoadConstant,
    LoadBoolean,
    LoadNil,
    GetUpvalue,
    GetGlobal,
    GetIndex,
    SetGlobal,
    SetUpvalue,
    SetIndex,
    NewTable,
    PrepMethodCall,
    Add,
    Subtract,
    Multiply,
    Divide,
    Modulo,
    Power,
    Minus,
    Not,
    Length,
    Concatenate,
    Jump,
    Equal,
    LessThan,
    LessThanOrEqual,
    Test,
    TestSet,
    Call,
    TailCall,
    Return,
    IterateNumericForLoop,
    InitNumericForLoop,
    IterateGenericForLoop,
    SetList,
    Close,
    Closure,
    VarArg,
}

impl OperationCode {
    pub fn from_u8(value: u8) -> Option<Self> {
        Some(match value {
            0 => Self::Move,
            1 => Self::LoadConstant,
            2 => Self::LoadBoolean,
            3 => Self::LoadNil,
            4 => Self::GetUpvalue,
            5 => Self::GetGlobal,
            6 => Self::GetIndex,
            7 => Self::SetGlobal,
            8 => Self::SetUpvalue,
            9 => Self::SetIndex,
            10 => Self::NewTable,
            11 => Self::PrepMethodCall,
            12 => Self::Add,
            13 => Self::Subtract,
            14 => Self::Multiply,
            15 => Self::Divide,
            16 => Self::Modulo,
            17 => Self::Power,
            18 => Self::Minus,
            19 => Self::Not,
            20 => Self::Length,
            21 => Self::Concatenate,
            22 => Self::Jump,
            23 => Self::Equal,
            24 => Self::LessThan,
            25 => Self::LessThanOrEqual,
            26 => Self::Test,
            27 => Self::TestSet,
            28 => Self::Call,
            29 => Self::TailCall,
            30 => Self::Return,
            31 => Self::IterateNumericForLoop,
            32 => Self::InitNumericForLoop,
            33 => Self::IterateGenericForLoop,
            34 => Self::SetList,
            35 => Self::Close,
            36 => Self::Closure,
            37 => Self::VarArg,
            _ => return None,
        })
    }

    pub fn parse(input: &[u8]) -> IResult<&[u8], Self> {
        let (input, operation_code) = le_u8(input)?;
        let operation_code = operation_code & 0x3F;

        Ok((
            input,
            match Self::from_u8(operation_code) {
                None => {
                    return Err(Err::Failure(Error::from_error_kind(
                        input,
                        ErrorKind::Switch,
                    )));
                }
                Some(operation_code) => operation_code,
            },
        ))
    }

    pub fn instruction_layout(&self) -> LayoutDiscriminants {
        /*
           0 = BC
           1 = BX
           2 = BSx
        */

        match self {
            Self::Move => LayoutDiscriminants::BC,
            Self::LoadConstant => LayoutDiscriminants::BX,
            Self::LoadBoolean => LayoutDiscriminants::BC,
            Self::LoadNil => LayoutDiscriminants::BC,
            Self::GetUpvalue => LayoutDiscriminants::BC,
            Self::GetGlobal => LayoutDiscriminants::BX,
            Self::GetIndex => LayoutDiscriminants::BC,
            Self::SetGlobal => LayoutDiscriminants::BX,
            Self::SetUpvalue => LayoutDiscriminants::BC,
            Self::SetIndex => LayoutDiscriminants::BC,
            Self::NewTable => LayoutDiscriminants::BC,
            Self::PrepMethodCall => LayoutDiscriminants::BC,
            Self::Add => LayoutDiscriminants::BC,
            Self::Subtract => LayoutDiscriminants::BC,
            Self::Multiply => LayoutDiscriminants::BC,
            Self::Divide => LayoutDiscriminants::BC,
            Self::Modulo => LayoutDiscriminants::BC,
            Self::Power => LayoutDiscriminants::BC,
            Self::Minus => LayoutDiscriminants::BC,
            Self::Not => LayoutDiscriminants::BC,
            Self::Length => LayoutDiscriminants::BC,
            Self::Concatenate => LayoutDiscriminants::BC,
            Self::Jump => LayoutDiscriminants::BSx,
            Self::Equal => LayoutDiscriminants::BC,
            Self::LessThan => LayoutDiscriminants::BC,
            Self::LessThanOrEqual => LayoutDiscriminants::BC,
            Self::Test => LayoutDiscriminants::BC,
            Self::TestSet => LayoutDiscriminants::BC,
            Self::Call => LayoutDiscriminants::BC,
            Self::TailCall => LayoutDiscriminants::BC,
            Self::Return => LayoutDiscriminants::BC,
            Self::IterateNumericForLoop => LayoutDiscriminants::BSx,
            Self::InitNumericForLoop => LayoutDiscriminants::BSx,
            Self::IterateGenericForLoop => LayoutDiscriminants::BC,
            Self::SetList => LayoutDiscriminants::BC,
            Self::Close => LayoutDiscriminants::BC,
            Self::Closure => LayoutDiscriminants::BX,
            Self::VarArg => LayoutDiscriminants::BC,
        }
    }
}
