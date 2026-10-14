use anyhow::{Context, Result, bail};
use luadicrous_bytecode::{
    Constant as ParsedConstant, Opcode, ParsedChunk, Prototype, decode_prototype,
};
use num_enum::TryFromPrimitive;

use crate::{
    deserializer::{constant::Constant, function::Function},
    instruction::Instruction,
    op_code::OpCode,
};

pub(crate) struct AdaptedChunk {
    pub strings: Vec<Vec<u8>>,
    pub functions: Vec<Function>,
    pub main: usize,
}

pub(crate) fn adapt_chunk(chunk: &ParsedChunk) -> Result<AdaptedChunk> {
    let main = usize::try_from(chunk.main_proto).context("main prototype index overflow")?;
    if main >= chunk.prototypes.len() {
        bail!("main prototype {} does not exist", chunk.main_proto);
    }

    let functions = chunk
        .prototypes
        .iter()
        .enumerate()
        .map(|(index, prototype)| adapt_function(chunk, prototype, index))
        .collect::<Result<Vec<_>>>()?;
    Ok(AdaptedChunk {
        strings: chunk
            .strings
            .iter()
            .map(|entry| entry.bytes.clone())
            .collect(),
        functions,
        main,
    })
}

fn adapt_function(chunk: &ParsedChunk, prototype: &Prototype, index: usize) -> Result<Function> {
    let decoded = decode_prototype(&prototype.instructions, chunk.header.luau_version);
    if decoded.quality.unknown_opcodes != 0
        || decoded.quality.truncated_aux != 0
        || decoded.quality.invalid_jump_targets != 0
    {
        bail!("prototype {index} has an unsafe opcode stream");
    }

    let nop = Instruction::BC {
        op_code: OpCode::LOP_NOP,
        a: 0,
        b: 0,
        c: 0,
        aux: 0,
    };
    let mut instructions = vec![nop; prototype.instructions.len()];
    for decoded_instruction in decoded.instructions {
        instructions[decoded_instruction.pc] = adapt_instruction(&decoded_instruction)
            .with_context(|| format!("prototype {index}, pc {}", decoded_instruction.pc))?;
    }

    Ok(Function {
        max_stack_size: prototype.max_stack_size,
        num_parameters: prototype.num_params,
        num_upvalues: prototype.num_upvalues,
        is_vararg: prototype.is_vararg,
        instructions,
        constants: prototype
            .constants
            .iter()
            .map(adapt_constant)
            .collect::<Result<Vec<_>>>()?,
        functions: prototype
            .child_protos
            .iter()
            .map(|child| usize::try_from(*child).context("child prototype index overflow"))
            .collect::<Result<Vec<_>>>()?,
        line_defined: usize::try_from(prototype.line_defined)
            .context("line-defined value overflow")?,
        function_name: usize::try_from(prototype.debug_name)
            .context("debug-name index overflow")?,
        line_gap_log2: None,
        line_info_delta: None,
        abs_line_info_delta: None,
    })
}

fn adapt_instruction(instruction: &luadicrous_bytecode::DecodedInstruction) -> Result<Instruction> {
    let mut opcode = instruction.opcode.context("unknown opcode")?;
    if opcode == Opcode::FastPcall && instruction.a() > 1 {
        bail!("FASTPCALL has an invalid protected function id");
    }
    if opcode == Opcode::Break {
        opcode = Opcode::Nop;
    }
    opcode = match opcode {
        Opcode::GetUdataKs => Opcode::GetTableKs,
        Opcode::SetUdataKs => Opcode::SetTableKs,
        Opcode::NamecallUdata => Opcode::Namecall,
        Opcode::CallFb => Opcode::Call,
        Opcode::FastPcall => Opcode::Nop,
        Opcode::NativeCall => bail!("NATIVECALL cannot be lifted safely"),
        Opcode::CmpProto => bail!("CMPPROTO requires a guarded fallback path"),
        other => other,
    };

    if opcode == Opcode::LoadKx {
        let constant = instruction.aux.context("LOADKX is missing AUX")?;
        let constant = i16::try_from(constant).context("LOADKX constant exceeds AD range")?;
        return Ok(Instruction::AD {
            op_code: OpCode::LOP_LOADK,
            a: instruction.a(),
            d: constant,
            aux: 0,
        });
    }

    if opcode == Opcode::NewClassMember {
        let member_name = instruction
            .aux
            .context("NEWCLASSMEMBER is missing its member-name AUX")?;
        return Ok(Instruction::BC {
            op_code: OpCode::LOP_SETTABLEKS,
            a: instruction.c(),
            b: instruction.a(),
            c: 0,
            aux: member_name,
        });
    }

    let number = opcode.number();
    let op_code = OpCode::try_from_primitive(number)
        .with_context(|| format!("opcode {} is newer than Medal's lifter", opcode.name()))?;
    let aux = instruction.aux.unwrap_or(0);
    Ok(match number {
        0
        | 1
        | 2
        | 3
        | 6..=11
        | 13..=18
        | 20..=22
        | 33..=53
        | 55
        | 60
        | 63
        | 65
        | 66
        | 68
        | 70..=75
        | 81
        | 82 => Instruction::BC {
            op_code,
            a: instruction.a(),
            b: instruction.b(),
            c: instruction.c(),
            aux,
        },
        4 | 5 | 12 | 19 | 23..=32 | 54 | 56..=59 | 61 | 62 | 64 | 76..=80 => Instruction::AD {
            op_code,
            a: instruction.a(),
            d: instruction.d(),
            aux,
        },
        67 | 69 => Instruction::E {
            op_code,
            e: instruction.e(),
        },
        _ => bail!("unsupported normalized opcode {}", opcode.name()),
    })
}

fn adapt_constant(constant: &ParsedConstant) -> Result<Constant> {
    Ok(match constant {
        ParsedConstant::Nil => Constant::Nil,
        ParsedConstant::Boolean(value) => Constant::Boolean(*value),
        ParsedConstant::NumberBits(bits) => Constant::Number(f64::from_bits(*bits)),
        ParsedConstant::StringRef(index) => {
            Constant::String(usize::try_from(*index).context("string constant index overflow")?)
        }
        ParsedConstant::Import(value) => {
            Constant::Import(usize::try_from(*value).context("import value overflow")?)
        }
        ParsedConstant::Table(keys) => Constant::Table(
            keys.iter()
                .map(|key| usize::try_from(*key).context("table key index overflow"))
                .collect::<Result<Vec<_>>>()?,
        ),
        ParsedConstant::TableWithValues(entries) => Constant::TableWithValues(
            entries
                .iter()
                .map(|entry| {
                    Ok((
                        usize::try_from(entry.key).context("table key index overflow")?,
                        entry
                            .value
                            .map(usize::try_from)
                            .transpose()
                            .context("table value index overflow")?,
                    ))
                })
                .collect::<Result<Vec<_>>>()?,
        ),
        ParsedConstant::Closure(function) => Constant::Closure(
            usize::try_from(*function).context("closure prototype index overflow")?,
        ),
        ParsedConstant::VectorBits(bits) => Constant::Vector(
            f32::from_bits(bits[0]),
            f32::from_bits(bits[1]),
            f32::from_bits(bits[2]),
            f32::from_bits(bits[3]),
        ),
        ParsedConstant::VectorDoubleBits(bits) => Constant::VectorDouble(
            f64::from_bits(bits[0]),
            f64::from_bits(bits[1]),
            f64::from_bits(bits[2]),
            f64::from_bits(bits[3]),
        ),
        ParsedConstant::Integer(value) => Constant::Number(*value as f64),
        ParsedConstant::ClassShape(_) => {
            bail!("Luau v10 class-shape constants are not source-liftable yet")
        }
    })
}
