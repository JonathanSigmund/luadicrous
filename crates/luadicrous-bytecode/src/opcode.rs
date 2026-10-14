use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OpcodeEncoding {
    Canonical,
    LegacyV6,
    ScrambledV7,
}

impl OpcodeEncoding {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Canonical => "canonical",
            Self::LegacyV6 => "legacy_v6",
            Self::ScrambledV7 => "scrambled_v7",
        }
    }
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Opcode {
    Nop,
    Break,
    LoadNil,
    LoadB,
    LoadN,
    LoadK,
    Move,
    GetGlobal,
    SetGlobal,
    GetUpvalue,
    SetUpvalue,
    CloseUpvalues,
    GetImport,
    GetTable,
    SetTable,
    GetTableKs,
    SetTableKs,
    GetTableN,
    SetTableN,
    NewClosure,
    Namecall,
    Call,
    Return,
    Jump,
    JumpBack,
    JumpIf,
    JumpIfNot,
    JumpIfEq,
    JumpIfLe,
    JumpIfLt,
    JumpIfNotEq,
    JumpIfNotLe,
    JumpIfNotLt,
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Pow,
    AddK,
    SubK,
    MulK,
    DivK,
    ModK,
    PowK,
    And,
    Or,
    AndK,
    OrK,
    Concat,
    Not,
    Minus,
    Length,
    NewTable,
    DupTable,
    SetList,
    ForNPrep,
    ForNLoop,
    ForGLoop,
    ForGPrepInext,
    FastCall3,
    ForGPrepNext,
    NativeCall,
    GetVarargs,
    DupClosure,
    PrepVarargs,
    LoadKx,
    JumpX,
    FastCall,
    Coverage,
    Capture,
    SubRk,
    DivRk,
    FastCall1,
    FastCall2,
    FastCall2K,
    ForGPrep,
    JumpXEqKNil,
    JumpXEqKB,
    JumpXEqKN,
    JumpXEqKS,
    IDiv,
    IDivK,
    GetUdataKs,
    SetUdataKs,
    NamecallUdata,
    NewClassMember,
    CallFb,
    CmpProto,
    FastPcall,
}

const CANONICAL_OPCODES: [Opcode; 90] = [
    Opcode::Nop,
    Opcode::Break,
    Opcode::LoadNil,
    Opcode::LoadB,
    Opcode::LoadN,
    Opcode::LoadK,
    Opcode::Move,
    Opcode::GetGlobal,
    Opcode::SetGlobal,
    Opcode::GetUpvalue,
    Opcode::SetUpvalue,
    Opcode::CloseUpvalues,
    Opcode::GetImport,
    Opcode::GetTable,
    Opcode::SetTable,
    Opcode::GetTableKs,
    Opcode::SetTableKs,
    Opcode::GetTableN,
    Opcode::SetTableN,
    Opcode::NewClosure,
    Opcode::Namecall,
    Opcode::Call,
    Opcode::Return,
    Opcode::Jump,
    Opcode::JumpBack,
    Opcode::JumpIf,
    Opcode::JumpIfNot,
    Opcode::JumpIfEq,
    Opcode::JumpIfLe,
    Opcode::JumpIfLt,
    Opcode::JumpIfNotEq,
    Opcode::JumpIfNotLe,
    Opcode::JumpIfNotLt,
    Opcode::Add,
    Opcode::Sub,
    Opcode::Mul,
    Opcode::Div,
    Opcode::Mod,
    Opcode::Pow,
    Opcode::AddK,
    Opcode::SubK,
    Opcode::MulK,
    Opcode::DivK,
    Opcode::ModK,
    Opcode::PowK,
    Opcode::And,
    Opcode::Or,
    Opcode::AndK,
    Opcode::OrK,
    Opcode::Concat,
    Opcode::Not,
    Opcode::Minus,
    Opcode::Length,
    Opcode::NewTable,
    Opcode::DupTable,
    Opcode::SetList,
    Opcode::ForNPrep,
    Opcode::ForNLoop,
    Opcode::ForGLoop,
    Opcode::ForGPrepInext,
    Opcode::FastCall3,
    Opcode::ForGPrepNext,
    Opcode::NativeCall,
    Opcode::GetVarargs,
    Opcode::DupClosure,
    Opcode::PrepVarargs,
    Opcode::LoadKx,
    Opcode::JumpX,
    Opcode::FastCall,
    Opcode::Coverage,
    Opcode::Capture,
    Opcode::SubRk,
    Opcode::DivRk,
    Opcode::FastCall1,
    Opcode::FastCall2,
    Opcode::FastCall2K,
    Opcode::ForGPrep,
    Opcode::JumpXEqKNil,
    Opcode::JumpXEqKB,
    Opcode::JumpXEqKN,
    Opcode::JumpXEqKS,
    Opcode::IDiv,
    Opcode::IDivK,
    Opcode::GetUdataKs,
    Opcode::SetUdataKs,
    Opcode::NamecallUdata,
    Opcode::NewClassMember,
    Opcode::CallFb,
    Opcode::CmpProto,
    Opcode::FastPcall,
];

impl Opcode {
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        CANONICAL_OPCODES
            .iter()
            .copied()
            .find(|opcode| opcode.name().eq_ignore_ascii_case(name.trim()))
    }

    #[must_use]
    pub fn from_number(number: u8) -> Option<Self> {
        canonical_opcode(number)
    }

    #[must_use]
    pub fn from_raw(raw: u8, encoding: OpcodeEncoding) -> Option<Self> {
        match encoding {
            OpcodeEncoding::Canonical => canonical_opcode(raw),
            OpcodeEncoding::LegacyV6 => legacy_v6_opcode(raw),
            OpcodeEncoding::ScrambledV7 => canonical_opcode(raw.wrapping_mul(203)),
        }
    }

    #[must_use]
    pub const fn number(self) -> u8 {
        self as u8
    }

    #[must_use]
    pub fn raw_for_encoding(self, encoding: OpcodeEncoding) -> Option<u8> {
        (u8::MIN..=u8::MAX).find(|raw| Self::from_raw(*raw, encoding) == Some(self))
    }

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Nop => "NOP",
            Self::Break => "BREAK",
            Self::LoadNil => "LOADNIL",
            Self::LoadB => "LOADB",
            Self::LoadN => "LOADN",
            Self::LoadK => "LOADK",
            Self::Move => "MOVE",
            Self::GetGlobal => "GETGLOBAL",
            Self::SetGlobal => "SETGLOBAL",
            Self::GetUpvalue => "GETUPVAL",
            Self::SetUpvalue => "SETUPVAL",
            Self::CloseUpvalues => "CLOSEUPVALS",
            Self::GetImport => "GETIMPORT",
            Self::GetTable => "GETTABLE",
            Self::SetTable => "SETTABLE",
            Self::GetTableKs => "GETTABLEKS",
            Self::SetTableKs => "SETTABLEKS",
            Self::GetTableN => "GETTABLEN",
            Self::SetTableN => "SETTABLEN",
            Self::NewClosure => "NEWCLOSURE",
            Self::Namecall => "NAMECALL",
            Self::Call => "CALL",
            Self::Return => "RETURN",
            Self::Jump => "JUMP",
            Self::JumpBack => "JUMPBACK",
            Self::JumpIf => "JUMPIF",
            Self::JumpIfNot => "JUMPIFNOT",
            Self::JumpIfEq => "JUMPIFEQ",
            Self::JumpIfLe => "JUMPIFLE",
            Self::JumpIfLt => "JUMPIFLT",
            Self::JumpIfNotEq => "JUMPIFNOTEQ",
            Self::JumpIfNotLe => "JUMPIFNOTLE",
            Self::JumpIfNotLt => "JUMPIFNOTLT",
            Self::Add => "ADD",
            Self::Sub => "SUB",
            Self::Mul => "MUL",
            Self::Div => "DIV",
            Self::Mod => "MOD",
            Self::Pow => "POW",
            Self::AddK => "ADDK",
            Self::SubK => "SUBK",
            Self::MulK => "MULK",
            Self::DivK => "DIVK",
            Self::ModK => "MODK",
            Self::PowK => "POWK",
            Self::And => "AND",
            Self::Or => "OR",
            Self::AndK => "ANDK",
            Self::OrK => "ORK",
            Self::Concat => "CONCAT",
            Self::Not => "NOT",
            Self::Minus => "MINUS",
            Self::Length => "LENGTH",
            Self::NewTable => "NEWTABLE",
            Self::DupTable => "DUPTABLE",
            Self::SetList => "SETLIST",
            Self::ForNPrep => "FORNPREP",
            Self::ForNLoop => "FORNLOOP",
            Self::ForGLoop => "FORGLOOP",
            Self::ForGPrepInext => "FORGPREP_INEXT",
            Self::FastCall3 => "FASTCALL3",
            Self::ForGPrepNext => "FORGPREP_NEXT",
            Self::NativeCall => "NATIVECALL",
            Self::GetVarargs => "GETVARARGS",
            Self::DupClosure => "DUPCLOSURE",
            Self::PrepVarargs => "PREPVARARGS",
            Self::LoadKx => "LOADKX",
            Self::JumpX => "JUMPX",
            Self::FastCall => "FASTCALL",
            Self::Coverage => "COVERAGE",
            Self::Capture => "CAPTURE",
            Self::SubRk => "SUBRK",
            Self::DivRk => "DIVRK",
            Self::FastCall1 => "FASTCALL1",
            Self::FastCall2 => "FASTCALL2",
            Self::FastCall2K => "FASTCALL2K",
            Self::ForGPrep => "FORGPREP",
            Self::JumpXEqKNil => "JUMPXEQKNIL",
            Self::JumpXEqKB => "JUMPXEQKB",
            Self::JumpXEqKN => "JUMPXEQKN",
            Self::JumpXEqKS => "JUMPXEQKS",
            Self::IDiv => "IDIV",
            Self::IDivK => "IDIVK",
            Self::GetUdataKs => "GETUDATAKS",
            Self::SetUdataKs => "SETUDATAKS",
            Self::NamecallUdata => "NAMECALLUDATA",
            Self::NewClassMember => "NEWCLASSMEMBER",
            Self::CallFb => "CALLFB",
            Self::CmpProto => "CMPPROTO",
            Self::FastPcall => "FASTPCALL",
        }
    }

    #[must_use]
    pub const fn minimum_version(self) -> u8 {
        match self {
            Self::NewClassMember => 10,
            Self::CallFb | Self::CmpProto => 11,
            Self::FastPcall => 14,
            _ => 1,
        }
    }

    #[must_use]
    pub const fn has_aux(self) -> bool {
        matches!(
            self,
            Self::GetGlobal
                | Self::SetGlobal
                | Self::GetImport
                | Self::GetTableKs
                | Self::SetTableKs
                | Self::Namecall
                | Self::NewTable
                | Self::SetList
                | Self::ForGLoop
                | Self::FastCall3
                | Self::LoadKx
                | Self::FastCall2
                | Self::FastCall2K
                | Self::JumpIfEq
                | Self::JumpIfLe
                | Self::JumpIfLt
                | Self::JumpIfNotEq
                | Self::JumpIfNotLe
                | Self::JumpIfNotLt
                | Self::JumpXEqKNil
                | Self::JumpXEqKB
                | Self::JumpXEqKN
                | Self::JumpXEqKS
                | Self::GetUdataKs
                | Self::SetUdataKs
                | Self::NamecallUdata
                | Self::NewClassMember
                | Self::CallFb
                | Self::CmpProto
        )
    }

    #[must_use]
    pub const fn operand_type(self) -> &'static str {
        match self {
            Self::Nop
            | Self::Break
            | Self::ForGPrepInext
            | Self::ForGPrepNext
            | Self::NativeCall => "none",
            Self::LoadNil | Self::CloseUpvalues | Self::PrepVarargs | Self::LoadKx => "iA",
            Self::Move
            | Self::GetUpvalue
            | Self::SetUpvalue
            | Self::Return
            | Self::Not
            | Self::Minus
            | Self::Length
            | Self::NewTable
            | Self::GetVarargs
            | Self::Capture
            | Self::ForGPrep => "iAB",
            Self::GetGlobal | Self::SetGlobal | Self::FastCall => "iAC",
            Self::LoadN
            | Self::LoadK
            | Self::GetImport
            | Self::NewClosure
            | Self::DupTable
            | Self::ForNPrep
            | Self::ForNLoop
            | Self::ForGLoop
            | Self::DupClosure => "iABx",
            Self::Jump | Self::JumpBack => "isBx",
            Self::JumpIf
            | Self::JumpIfNot
            | Self::JumpIfEq
            | Self::JumpIfLe
            | Self::JumpIfLt
            | Self::JumpIfNotEq
            | Self::JumpIfNotLe
            | Self::JumpIfNotLt
            | Self::JumpXEqKNil
            | Self::JumpXEqKB
            | Self::JumpXEqKN
            | Self::JumpXEqKS => "iAsBx",
            Self::CmpProto => "iAsBx",
            Self::JumpX | Self::Coverage => "isAx",
            _ => "iABC",
        }
    }

    #[must_use]
    pub const fn is_jump(self) -> bool {
        matches!(
            self,
            Self::Jump
                | Self::JumpBack
                | Self::JumpIf
                | Self::JumpIfNot
                | Self::JumpIfEq
                | Self::JumpIfLe
                | Self::JumpIfLt
                | Self::JumpIfNotEq
                | Self::JumpIfNotLe
                | Self::JumpIfNotLt
                | Self::ForNPrep
                | Self::ForNLoop
                | Self::ForGLoop
                | Self::ForGPrepInext
                | Self::ForGPrepNext
                | Self::ForGPrep
                | Self::JumpX
                | Self::JumpXEqKNil
                | Self::JumpXEqKB
                | Self::JumpXEqKN
                | Self::JumpXEqKS
                | Self::CmpProto
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DecodedInstruction {
    pub pc: usize,
    pub word: u32,
    pub opcode: Option<Opcode>,
    pub aux: Option<u32>,
}

impl DecodedInstruction {
    #[must_use]
    pub const fn a(&self) -> u8 {
        ((self.word >> 8) & 0xff) as u8
    }

    #[must_use]
    pub const fn b(&self) -> u8 {
        ((self.word >> 16) & 0xff) as u8
    }

    #[must_use]
    pub const fn c(&self) -> u8 {
        ((self.word >> 24) & 0xff) as u8
    }

    #[must_use]
    pub const fn d(&self) -> i16 {
        (self.word >> 16) as i16
    }

    #[must_use]
    pub const fn e(&self) -> i32 {
        i32::from_ne_bytes(self.word.to_ne_bytes()) >> 8
    }

    #[must_use]
    pub const fn bx(&self) -> u16 {
        (self.word >> 16) as u16
    }

    #[must_use]
    pub const fn raw_opcode(&self) -> u8 {
        (self.word & 0xff) as u8
    }

    #[must_use]
    pub fn jump_target(&self) -> Option<isize> {
        let opcode = self.opcode?;
        if !opcode.is_jump() {
            return None;
        }
        let offset = if opcode == Opcode::JumpX {
            self.e() as isize
        } else {
            self.d() as isize
        };
        Some(isize::try_from(self.pc).ok()? + 1 + offset)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OpcodeQuality {
    pub score: i64,
    pub decoded_instructions: usize,
    pub aux_words: usize,
    pub unknown_opcodes: usize,
    pub truncated_aux: usize,
    pub invalid_jump_targets: usize,
    pub return_instructions: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DecodedPrototype {
    pub encoding: OpcodeEncoding,
    pub instructions: Vec<DecodedInstruction>,
    pub quality: OpcodeQuality,
}

#[must_use]
pub fn decode_prototype(words: &[u32], version: u8) -> DecodedPrototype {
    let order = if version >= 7 {
        [
            OpcodeEncoding::Canonical,
            OpcodeEncoding::ScrambledV7,
            OpcodeEncoding::LegacyV6,
        ]
    } else {
        [
            OpcodeEncoding::LegacyV6,
            OpcodeEncoding::ScrambledV7,
            OpcodeEncoding::Canonical,
        ]
    };
    let [first, second, third] = order;
    let mut best = decode_with_encoding(words, first, version);
    for candidate in [second, third].map(|encoding| decode_with_encoding(words, encoding, version))
    {
        if candidate.quality.score > best.quality.score {
            best = candidate;
        }
    }
    best
}

fn decode_with_encoding(words: &[u32], encoding: OpcodeEncoding, version: u8) -> DecodedPrototype {
    let mut instructions = Vec::with_capacity(words.len());
    let mut starts = vec![false; words.len()];
    let mut pc = 0;
    let mut aux_words = 0;
    let mut unknown_opcodes = 0;
    let mut truncated_aux = 0;
    let mut return_instructions = 0;

    while pc < words.len() {
        starts[pc] = true;
        let word = words[pc];
        let opcode = Opcode::from_raw((word & 0xff) as u8, encoding)
            .filter(|opcode| version >= opcode.minimum_version());
        let aux = opcode.filter(|value| value.has_aux()).and_then(|_| {
            let value = words.get(pc + 1).copied();
            if value.is_some() {
                aux_words += 1;
            } else {
                truncated_aux += 1;
            }
            value
        });
        if opcode.is_none() {
            unknown_opcodes += 1;
        }
        if opcode == Some(Opcode::Return) {
            return_instructions += 1;
        }
        instructions.push(DecodedInstruction {
            pc,
            word,
            opcode,
            aux,
        });
        pc += usize::from(opcode.is_some_and(Opcode::has_aux)) + 1;
    }

    let invalid_jump_targets = instructions
        .iter()
        .filter_map(DecodedInstruction::jump_target)
        .filter(|target| {
            usize::try_from(*target)
                .ok()
                .is_none_or(|target| target >= starts.len() || !starts[target])
        })
        .count()
        + instructions.iter().filter(|instruction| {
            if instruction.opcode != Some(Opcode::FastPcall) {
                return false;
            }
            let target = instruction.pc + 1 + usize::from(instruction.c());
            !starts.get(target).copied().unwrap_or(false)
                || words.get(target).and_then(|word| {
                    Opcode::from_raw((word & 0xff) as u8, encoding)
                }) != Some(Opcode::Call)
        }).count();
    let ending_return = instructions
        .last()
        .is_some_and(|instruction| instruction.opcode == Some(Opcode::Return));
    let decoded_count = instructions.len().saturating_sub(unknown_opcodes);
    let score = i64::try_from(decoded_count)
        .unwrap_or(i64::MAX)
        .saturating_add(
            i64::try_from(return_instructions)
                .unwrap_or(i64::MAX)
                .saturating_mul(100),
        )
        .saturating_add(if ending_return { 1_000 } else { 0 })
        .saturating_sub(
            i64::try_from(unknown_opcodes)
                .unwrap_or(i64::MAX)
                .saturating_mul(10_000),
        )
        .saturating_sub(
            i64::try_from(truncated_aux)
                .unwrap_or(i64::MAX)
                .saturating_mul(20_000),
        )
        .saturating_sub(
            i64::try_from(invalid_jump_targets)
                .unwrap_or(i64::MAX)
                .saturating_mul(5_000),
        );

    DecodedPrototype {
        encoding,
        instructions,
        quality: OpcodeQuality {
            score,
            decoded_instructions: decoded_count,
            aux_words,
            unknown_opcodes,
            truncated_aux,
            invalid_jump_targets,
            return_instructions,
        },
    }
}

fn canonical_opcode(raw: u8) -> Option<Opcode> {
    CANONICAL_OPCODES.get(usize::from(raw)).copied()
}

fn legacy_v6_opcode(raw: u8) -> Option<Opcode> {
    Some(match raw {
        0x00 => Opcode::Nop,
        0xe3 => Opcode::Break,
        0xc6 => Opcode::LoadNil,
        0xa9 => Opcode::LoadB,
        0x8c => Opcode::LoadN,
        0x6f => Opcode::LoadK,
        0x52 => Opcode::Move,
        0x35 => Opcode::GetGlobal,
        0x18 => Opcode::SetGlobal,
        0xfb => Opcode::GetUpvalue,
        0xde => Opcode::SetUpvalue,
        0xc1 => Opcode::CloseUpvalues,
        0xa4 => Opcode::GetImport,
        0x87 => Opcode::GetTable,
        0x6a => Opcode::SetTable,
        0x4d => Opcode::GetTableKs,
        0x30 => Opcode::SetTableKs,
        0x13 => Opcode::GetTableN,
        0xf6 => Opcode::SetTableN,
        0xd9 => Opcode::NewClosure,
        0xbc => Opcode::Namecall,
        0x9f => Opcode::Call,
        0x82 => Opcode::Return,
        0x65 => Opcode::Jump,
        0x48 => Opcode::JumpBack,
        0x2b => Opcode::JumpIf,
        0x0e => Opcode::JumpIfNot,
        0xf1 => Opcode::JumpIfEq,
        0xd4 => Opcode::JumpIfLe,
        0xb7 => Opcode::JumpIfLt,
        0x9a => Opcode::JumpIfNotEq,
        0x7d => Opcode::JumpIfNotLe,
        0x60 => Opcode::JumpIfNotLt,
        0x43 => Opcode::Add,
        0x26 => Opcode::Sub,
        0x09 => Opcode::Mul,
        0xec => Opcode::Div,
        0xcf => Opcode::Mod,
        0xb2 => Opcode::Pow,
        0x95 => Opcode::AddK,
        0x78 => Opcode::SubK,
        0x5b => Opcode::MulK,
        0x3e => Opcode::DivK,
        0x21 => Opcode::ModK,
        0x04 => Opcode::PowK,
        0xe7 => Opcode::And,
        0xca => Opcode::Or,
        0xad => Opcode::AndK,
        0x90 => Opcode::OrK,
        0x73 => Opcode::Concat,
        0x56 => Opcode::Not,
        0x39 => Opcode::Minus,
        0x1c => Opcode::Length,
        0xff => Opcode::NewTable,
        0xe2 => Opcode::DupTable,
        0xc5 => Opcode::SetList,
        0xa8 => Opcode::ForNPrep,
        0x8b => Opcode::ForNLoop,
        0x6e => Opcode::ForGLoop,
        0x51 => Opcode::ForGPrepInext,
        0x34 => Opcode::FastCall3,
        0x17 => Opcode::ForGPrepNext,
        0xfa => Opcode::NativeCall,
        0xdd => Opcode::GetVarargs,
        0xc0 => Opcode::DupClosure,
        0xa3 => Opcode::PrepVarargs,
        0x86 => Opcode::LoadKx,
        0x69 => Opcode::JumpX,
        0x4c => Opcode::FastCall,
        0x2f => Opcode::Coverage,
        0x12 => Opcode::Capture,
        0xf5 => Opcode::SubRk,
        0xd8 => Opcode::DivRk,
        0xbb => Opcode::FastCall1,
        0x9e => Opcode::FastCall2,
        0x81 => Opcode::FastCall2K,
        0x64 => Opcode::ForGPrep,
        0x47 => Opcode::JumpXEqKNil,
        0x2a => Opcode::JumpXEqKB,
        0x0d => Opcode::JumpXEqKN,
        0xf0 => Opcode::JumpXEqKS,
        0xd3 => Opcode::IDiv,
        0xb6 => Opcode::IDivK,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn abc(opcode: Opcode, a: u8, b: u8, c: u8) -> u32 {
        u32::from(opcode.number())
            | (u32::from(a) << 8)
            | (u32::from(b) << 16)
            | (u32::from(c) << 24)
    }

    #[test]
    fn detects_canonical_static_chunk() {
        let words = [
            abc(Opcode::PrepVarargs, 0, 0, 0),
            abc(Opcode::NewTable, 0, 0, 0),
            1,
            abc(Opcode::Return, 0, 2, 0),
        ];
        let decoded = decode_prototype(&words, 6);
        assert_eq!(decoded.encoding, OpcodeEncoding::Canonical);
        assert_eq!(decoded.instructions.len(), 3);
        assert_eq!(decoded.quality.unknown_opcodes, 0);
        assert_eq!(decoded.quality.aux_words, 1);
    }

    #[test]
    fn detects_legacy_v6_chunk() {
        let words = [0xa3, 0x0000_0082];
        let decoded = decode_prototype(&words, 6);
        assert_eq!(decoded.encoding, OpcodeEncoding::LegacyV6);
        assert_eq!(decoded.quality.return_instructions, 1);
    }

    #[test]
    fn fast_pcall_is_one_word_and_requires_v14_in_both_encodings() {
        for encoding in [OpcodeEncoding::Canonical, OpcodeEncoding::ScrambledV7] {
            let words: Vec<_> = [abc(Opcode::FastPcall, 0, 1, 1), abc(Opcode::Move, 0, 1, 0),
                abc(Opcode::Call, 0, 2, 2), abc(Opcode::Return, 0, 2, 0)]
                .into_iter().map(|word| {
                    let opcode = Opcode::from_number((word & 0xff) as u8).unwrap();
                    (word & !0xff) | u32::from(opcode.raw_for_encoding(encoding).unwrap())
                }).collect();
            let decoded = decode_with_encoding(&words, encoding, 14);
            assert_eq!(decoded.instructions[0].opcode, Some(Opcode::FastPcall));
            assert_eq!(decoded.instructions.len(), 4);
            assert_eq!(decoded.quality.aux_words, 0);
            assert_eq!(decoded.quality.invalid_jump_targets, 0);
            assert_eq!(decode_with_encoding(&words, encoding, 13).instructions[0].opcode, None);
        }
    }
}
