use std::{collections::BTreeMap, fmt::Write as _};

use luadicrous_bytecode::{
    Constant, DecodedInstruction, Opcode, ParsedChunk, Prototype, decode_prototype,
};
use serde::Serialize;

const MAX_STATIC_DEPTH: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DecompileStrategy {
    StaticData,
    Structured,
    Lua51,
}

#[derive(Debug)]
pub(super) struct RecoveredSource {
    pub source: String,
    pub strategy: DecompileStrategy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum StaticValue {
    Nil,
    Boolean(bool),
    Number(u64),
    Integer(i64),
    String(Vec<u8>),
    Vector([u32; 4]),
    VectorDouble([u64; 4]),
    Table(usize),
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum StaticKey {
    Boolean(bool),
    Number(u64),
    Integer(i64),
    String(Vec<u8>),
}

#[derive(Debug, Default)]
struct StaticTable {
    entries: BTreeMap<StaticKey, StaticValue>,
}

struct Evaluator<'a> {
    chunk: &'a ParsedChunk,
    prototype: &'a Prototype,
    registers: Vec<StaticValue>,
    tables: Vec<StaticTable>,
}

pub(super) fn decompile(chunk: &ParsedChunk) -> Result<RecoveredSource, String> {
    if chunk.prototypes.len() != 1 {
        return Err(format!(
            "static-data recovery requires one prototype; chunk has {}",
            chunk.prototypes.len()
        ));
    }
    let prototype = chunk
        .prototypes
        .get(usize::try_from(chunk.main_proto).map_err(|_| "main index overflow")?)
        .ok_or_else(|| format!("main prototype {} does not exist", chunk.main_proto))?;
    let decoded = decode_prototype(&prototype.instructions, chunk.header.luau_version);
    if decoded.quality.unknown_opcodes != 0
        || decoded.quality.truncated_aux != 0
        || decoded.quality.invalid_jump_targets != 0
    {
        return Err("opcode stream is not structurally clean".to_owned());
    }

    let register_count = usize::from(prototype.max_stack_size).max(1);
    let mut evaluator = Evaluator {
        chunk,
        prototype,
        registers: vec![StaticValue::Nil; register_count],
        tables: Vec::new(),
    };
    let root = evaluator.evaluate(&decoded.instructions)?;
    evaluator.validate_graph(&root)?;

    let mut source = String::with_capacity(prototype.instructions.len().saturating_mul(6));
    source.push_str("-- Decompiled by Luadicrous V2\n");
    source.push_str("-- Recovery strategy: constant-safe static data\n");
    source.push_str("return ");
    evaluator.write_value(&mut source, &root, 0)?;
    source.push('\n');
    Ok(RecoveredSource {
        source,
        strategy: DecompileStrategy::StaticData,
    })
}

impl Evaluator<'_> {
    fn evaluate(&mut self, instructions: &[DecodedInstruction]) -> Result<StaticValue, String> {
        for instruction in instructions {
            if let Some(value) = self.execute(instruction)? {
                return Ok(value);
            }
        }
        Err("prototype has no static RETURN".to_owned())
    }

    fn execute(&mut self, instruction: &DecodedInstruction) -> Result<Option<StaticValue>, String> {
        let opcode = instruction
            .opcode
            .ok_or_else(|| format!("unknown opcode at pc {}", instruction.pc))?;
        match opcode {
            Opcode::Nop | Opcode::PrepVarargs | Opcode::Coverage => {}
            Opcode::LoadNil => self.set_register(instruction.a(), StaticValue::Nil)?,
            Opcode::LoadB => self.load_boolean(instruction)?,
            Opcode::LoadN => self.set_register(
                instruction.a(),
                StaticValue::Number(f64::from(instruction.d()).to_bits()),
            )?,
            Opcode::LoadK | Opcode::LoadKx => self.load_constant(instruction)?,
            Opcode::Move => {
                let value = self.register(instruction.b())?.clone();
                self.set_register(instruction.a(), value)?;
            }
            Opcode::Minus => self.negate(instruction)?,
            Opcode::NewTable | Opcode::DupTable => self.load_table(instruction)?,
            Opcode::GetTable | Opcode::GetTableKs | Opcode::GetTableN => {
                self.get_table(instruction)?;
            }
            Opcode::SetTable | Opcode::SetTableKs | Opcode::SetTableN => {
                self.set_table_instruction(instruction)?;
            }
            Opcode::SetList => self.set_list(instruction)?,
            Opcode::Return => return self.return_value(instruction).map(Some),
            _ => {
                return Err(format!(
                    "{} at pc {} requires runtime execution",
                    opcode.name(),
                    instruction.pc
                ));
            }
        }
        Ok(None)
    }

    fn load_boolean(&mut self, instruction: &DecodedInstruction) -> Result<(), String> {
        if instruction.c() != 0 {
            return Err(format!(
                "LOADB control-flow skip at pc {} is not static",
                instruction.pc
            ));
        }
        self.set_register(instruction.a(), StaticValue::Boolean(instruction.b() != 0))
    }

    fn load_constant(&mut self, instruction: &DecodedInstruction) -> Result<(), String> {
        let constant = if instruction.opcode == Some(Opcode::LoadK) {
            u32::from(instruction.bx())
        } else {
            instruction
                .aux
                .ok_or_else(|| format!("LOADKX at pc {} has no AUX", instruction.pc))?
        };
        let value = self.constant_value(constant)?;
        self.set_register(instruction.a(), value)
    }

    fn negate(&mut self, instruction: &DecodedInstruction) -> Result<(), String> {
        let value = match self.register(instruction.b())? {
            StaticValue::Number(bits) => StaticValue::Number((-f64::from_bits(*bits)).to_bits()),
            StaticValue::Integer(value) => StaticValue::Integer(
                value
                    .checked_neg()
                    .ok_or_else(|| format!("integer negation overflow at pc {}", instruction.pc))?,
            ),
            _ => {
                return Err(format!(
                    "MINUS at pc {} has a nonnumeric operand",
                    instruction.pc
                ));
            }
        };
        self.set_register(instruction.a(), value)
    }

    fn load_table(&mut self, instruction: &DecodedInstruction) -> Result<(), String> {
        let value = if instruction.opcode == Some(Opcode::NewTable) {
            self.new_table()
        } else {
            self.duplicate_table(u32::from(instruction.bx()))?
        };
        self.set_register(instruction.a(), value)
    }

    fn get_table(&mut self, instruction: &DecodedInstruction) -> Result<(), String> {
        let table = self.table_register(instruction.b())?;
        let key = self.instruction_key(instruction)?;
        let value = self.tables[table]
            .entries
            .get(&key)
            .cloned()
            .unwrap_or(StaticValue::Nil);
        self.set_register(instruction.a(), value)
    }

    fn set_table_instruction(&mut self, instruction: &DecodedInstruction) -> Result<(), String> {
        let table = self.table_register(instruction.b())?;
        let key = self.instruction_key(instruction)?;
        let value = self.register(instruction.a())?.clone();
        self.set_table(table, key, value);
        Ok(())
    }

    fn instruction_key(&self, instruction: &DecodedInstruction) -> Result<StaticKey, String> {
        match instruction.opcode {
            Some(Opcode::GetTable | Opcode::SetTable) => {
                StaticKey::try_from(self.register(instruction.c())?)
            }
            Some(Opcode::GetTableKs | Opcode::SetTableKs) => {
                self.constant_key(instruction.aux.ok_or_else(|| {
                    format!("table string access at pc {} has no AUX", instruction.pc)
                })?)
            }
            Some(Opcode::GetTableN | Opcode::SetTableN) => {
                Ok(integer_key(u32::from(instruction.c()) + 1))
            }
            _ => Err(format!("invalid table operation at pc {}", instruction.pc)),
        }
    }

    fn set_list(&mut self, instruction: &DecodedInstruction) -> Result<(), String> {
        let table = self.table_register(instruction.a())?;
        let count = instruction.c();
        if count == 0 {
            return Err(format!(
                "SETLIST multret at pc {} is not static",
                instruction.pc
            ));
        }
        let start = instruction.aux.unwrap_or(1);
        for offset in 0..u32::from(count - 1) {
            let register = u32::from(instruction.b()) + offset;
            let register = u8::try_from(register)
                .map_err(|_| format!("SETLIST register overflow at pc {}", instruction.pc))?;
            let value = self.register(register)?.clone();
            self.set_table(table, integer_key(start + offset), value);
        }
        Ok(())
    }

    fn return_value(&self, instruction: &DecodedInstruction) -> Result<StaticValue, String> {
        if instruction.b() != 2 {
            return Err(format!(
                "RETURN at pc {} has {} values instead of one",
                instruction.pc,
                instruction.b().saturating_sub(1)
            ));
        }
        self.register(instruction.a()).cloned()
    }

    fn register(&self, register: u8) -> Result<&StaticValue, String> {
        self.registers.get(usize::from(register)).ok_or_else(|| {
            format!(
                "register R{register} exceeds max stack {}",
                self.registers.len()
            )
        })
    }

    fn set_register(&mut self, register: u8, value: StaticValue) -> Result<(), String> {
        let max_stack = self.registers.len();
        let target = self
            .registers
            .get_mut(usize::from(register))
            .ok_or_else(|| format!("register R{register} exceeds max stack {max_stack}"))?;
        *target = value;
        Ok(())
    }

    fn table_register(&self, register: u8) -> Result<usize, String> {
        match self.register(register)? {
            StaticValue::Table(table) => Ok(*table),
            _ => Err(format!("R{register} is not a statically known table")),
        }
    }

    fn new_table(&mut self) -> StaticValue {
        let id = self.tables.len();
        self.tables.push(StaticTable::default());
        StaticValue::Table(id)
    }

    fn set_table(&mut self, table: usize, key: StaticKey, value: StaticValue) {
        if value == StaticValue::Nil {
            self.tables[table].entries.remove(&key);
        } else {
            self.tables[table].entries.insert(key, value);
        }
    }

    fn constant_value(&self, index: u32) -> Result<StaticValue, String> {
        let constant = self
            .prototype
            .constants
            .get(usize::try_from(index).map_err(|_| "constant index overflow")?)
            .ok_or_else(|| {
                format!(
                    "constant {index} exceeds count {}",
                    self.prototype.constants.len()
                )
            })?;
        match constant {
            Constant::Nil => Ok(StaticValue::Nil),
            Constant::Boolean(value) => Ok(StaticValue::Boolean(*value)),
            Constant::NumberBits(bits) => Ok(StaticValue::Number(*bits)),
            Constant::StringRef(string) => self
                .chunk
                .strings
                .get(
                    usize::try_from(string.saturating_sub(1))
                        .map_err(|_| "string index overflow")?,
                )
                .map(|value| StaticValue::String(value.bytes.clone()))
                .ok_or_else(|| format!("string constant {string} is invalid")),
            Constant::VectorBits(bits) => Ok(StaticValue::Vector(*bits)),
            Constant::VectorDoubleBits(bits) => Ok(StaticValue::VectorDouble(*bits)),
            Constant::Integer(value) => Ok(StaticValue::Integer(*value)),
            Constant::Import(_)
            | Constant::Table(_)
            | Constant::TableWithValues(_)
            | Constant::Closure(_)
            | Constant::ClassShape(_) => {
                Err(format!("constant {index} is not a static scalar value"))
            }
        }
    }

    fn constant_key(&self, index: u32) -> Result<StaticKey, String> {
        StaticKey::try_from(&self.constant_value(index)?)
    }

    fn duplicate_table(&mut self, index: u32) -> Result<StaticValue, String> {
        let constant = self
            .prototype
            .constants
            .get(usize::try_from(index).map_err(|_| "constant index overflow")?)
            .cloned()
            .ok_or_else(|| format!("table constant {index} is invalid"))?;
        let table = self.new_table();
        let StaticValue::Table(table_id) = table else {
            unreachable!();
        };
        match constant {
            Constant::Table(_) => {}
            Constant::TableWithValues(entries) => {
                for entry in entries {
                    let Some(value) = entry.value else {
                        continue;
                    };
                    let key = self.constant_key(entry.key)?;
                    let value = self.constant_value(value)?;
                    self.set_table(table_id, key, value);
                }
            }
            Constant::ClassShape(_) => {
                return Err(format!(
                    "constant {index} is a class shape, not a table template"
                ));
            }
            _ => return Err(format!("constant {index} is not a table template")),
        }
        Ok(StaticValue::Table(table_id))
    }

    fn validate_graph(&self, root: &StaticValue) -> Result<(), String> {
        let mut references = vec![0_usize; self.tables.len()];
        for table in &self.tables {
            for value in table.entries.values() {
                if let StaticValue::Table(child) = value {
                    references[*child] += 1;
                }
            }
        }
        if references.iter().any(|references| *references > 1) {
            return Err("static table graph contains shared table identities".to_owned());
        }
        let StaticValue::Table(root) = root else {
            return if self.tables.is_empty() {
                Ok(())
            } else {
                Err("static evaluation created unreachable tables".to_owned())
            };
        };

        let mut state = vec![0_u8; self.tables.len()];
        let mut reachable = 0_usize;
        let mut stack = vec![(*root, 0_usize, false)];
        while let Some((table, depth, leaving)) = stack.pop() {
            if depth > MAX_STATIC_DEPTH {
                return Err(format!("static table depth exceeds {MAX_STATIC_DEPTH}"));
            }
            if leaving {
                state[table] = 2;
                continue;
            }
            match state[table] {
                1 => return Err("static table graph contains a cycle".to_owned()),
                2 => continue,
                _ => {}
            }
            state[table] = 1;
            reachable += 1;
            stack.push((table, depth, true));
            for value in self.tables[table].entries.values() {
                if let StaticValue::Table(child) = value {
                    stack.push((*child, depth + 1, false));
                }
            }
        }
        if reachable != self.tables.len() {
            return Err(format!(
                "{} static tables are unreachable from the return value",
                self.tables.len() - reachable
            ));
        }
        Ok(())
    }

    fn write_value(
        &self,
        output: &mut String,
        value: &StaticValue,
        depth: usize,
    ) -> Result<(), String> {
        match value {
            StaticValue::Nil => output.push_str("nil"),
            StaticValue::Boolean(value) => output.push_str(if *value { "true" } else { "false" }),
            StaticValue::Number(bits) => write_number(output, *bits),
            StaticValue::Integer(value) => {
                let _ = write!(output, "{value}");
            }
            StaticValue::String(bytes) => write_string(output, bytes),
            StaticValue::Vector(bits) => {
                if bits[3] != 0 {
                    return Err("four-component vectors are not source-safe".to_owned());
                }
                output.push_str("vector.create(");
                for (index, bits) in bits[..3].iter().enumerate() {
                    if index != 0 {
                        output.push_str(", ");
                    }
                    write_number(output, f64::from(f32::from_bits(*bits)).to_bits());
                }
                output.push(')');
            }
            StaticValue::Table(table) => self.write_table(output, *table, depth)?,
            StaticValue::VectorDouble(bits) => {
                output.push_str("vector.create(");
                let count = if bits[3] == 0 { 3 } else { 4 };
                for (index, component) in bits[..count].iter().enumerate() {
                    if index != 0 {
                        output.push_str(", ");
                    }
                    write_number(output, *component);
                }
                output.push(')');
            }
        }
        Ok(())
    }

    fn write_table(&self, output: &mut String, table: usize, depth: usize) -> Result<(), String> {
        let entries = &self.tables[table].entries;
        if entries.is_empty() {
            output.push_str("{}");
            return Ok(());
        }
        let inline = entries.len() <= 4
            && entries
                .values()
                .all(|value| !matches!(value, StaticValue::Table(_)));
        output.push('{');
        if inline {
            output.push(' ');
        } else {
            output.push('\n');
        }

        let mut array_index = 1_u32;
        while entries.contains_key(&integer_key(array_index)) {
            array_index += 1;
        }
        let array_len = array_index - 1;
        let mut emitted = 0_usize;
        for index in 1..=array_len {
            let value = &entries[&integer_key(index)];
            write_indent(output, depth + 1, inline);
            self.write_value(output, value, depth + 1)?;
            output.push(',');
            if inline {
                output.push(' ');
            } else {
                output.push('\n');
            }
            emitted += 1;
        }
        for (key, value) in entries {
            if key
                .positive_integer()
                .is_some_and(|index| index <= array_len)
            {
                continue;
            }
            write_indent(output, depth + 1, inline);
            write_key(output, key);
            output.push_str(" = ");
            self.write_value(output, value, depth + 1)?;
            output.push(',');
            if inline {
                output.push(' ');
            } else {
                output.push('\n');
            }
            emitted += 1;
        }
        debug_assert_eq!(emitted, entries.len());
        if inline {
            output.push('}');
        } else {
            write_indent(output, depth, false);
            output.push('}');
        }
        Ok(())
    }
}

impl TryFrom<&StaticValue> for StaticKey {
    type Error = String;

    fn try_from(value: &StaticValue) -> Result<Self, Self::Error> {
        match value {
            StaticValue::Boolean(value) => Ok(Self::Boolean(*value)),
            StaticValue::Number(bits) => {
                let value = f64::from_bits(*bits);
                if value.is_nan() {
                    Err("NaN cannot be used as a static table key".to_owned())
                } else {
                    Ok(Self::Number(normalize_number_key(*bits)))
                }
            }
            StaticValue::Integer(value) => Ok(Self::Integer(*value)),
            StaticValue::String(value) => Ok(Self::String(value.clone())),
            StaticValue::Nil => Err("nil cannot be used as a table key".to_owned()),
            StaticValue::Vector(_) | StaticValue::VectorDouble(_) => Err("vector table keys are not source-safe".to_owned()),
            StaticValue::Table(_) => Err("table-key identity is not source-safe".to_owned()),
        }
    }
}

impl StaticKey {
    fn positive_integer(&self) -> Option<u32> {
        match self {
            Self::Integer(value) => u32::try_from(*value).ok().filter(|value| *value > 0),
            Self::Number(bits) => positive_integer_from_bits(*bits),
            _ => None,
        }
    }
}

fn integer_key(value: u32) -> StaticKey {
    StaticKey::Number(f64::from(value).to_bits())
}

fn positive_integer_from_bits(bits: u64) -> Option<u32> {
    if bits >> 63 != 0 {
        return None;
    }
    let exponent_bits = u16::try_from((bits >> 52) & 0x7ff).ok()?;
    if exponent_bits == 0 || exponent_bits == 0x7ff {
        return None;
    }
    let exponent = i32::from(exponent_bits) - 1023;
    if !(0..=31).contains(&exponent) {
        return None;
    }
    let mantissa = (1_u64 << 52) | (bits & ((1_u64 << 52) - 1));
    let integer = if exponent >= 52 {
        mantissa.checked_shl(u32::try_from(exponent - 52).ok()?)?
    } else {
        let fractional_bits = u32::try_from(52 - exponent).ok()?;
        let fractional_mask = (1_u64 << fractional_bits) - 1;
        if mantissa & fractional_mask != 0 {
            return None;
        }
        mantissa >> fractional_bits
    };
    u32::try_from(integer).ok().filter(|value| *value > 0)
}

fn normalize_number_key(bits: u64) -> u64 {
    if f64::from_bits(bits) == 0.0 {
        0.0_f64.to_bits()
    } else {
        bits
    }
}

fn write_key(output: &mut String, key: &StaticKey) {
    if let StaticKey::String(bytes) = key {
        if is_identifier(bytes) {
            output.push_str(std::str::from_utf8(bytes).expect("ASCII identifier is UTF-8"));
            return;
        }
    }
    output.push('[');
    match key {
        StaticKey::Boolean(value) => output.push_str(if *value { "true" } else { "false" }),
        StaticKey::Number(bits) => write_number(output, *bits),
        StaticKey::Integer(value) => {
            let _ = write!(output, "{value}");
        }
        StaticKey::String(bytes) => write_string(output, bytes),
    }
    output.push(']');
}

fn write_indent(output: &mut String, depth: usize, inline: bool) {
    if !inline {
        for _ in 0..depth {
            output.push_str("    ");
        }
    }
}

fn write_number(output: &mut String, bits: u64) {
    let value = f64::from_bits(bits);
    if value.is_nan() {
        output.push_str("(0 / 0)");
    } else if value == f64::INFINITY {
        output.push_str("math.huge");
    } else if value == f64::NEG_INFINITY {
        output.push_str("-math.huge");
    } else if value == 0.0 && value.is_sign_negative() {
        output.push_str("-0.0");
    } else {
        let _ = write!(output, "{value}");
    }
}

fn write_string(output: &mut String, bytes: &[u8]) {
    output.push('"');
    for byte in bytes {
        match byte {
            b'"' => output.push_str("\\\""),
            b'\\' => output.push_str("\\\\"),
            b'\n' => output.push_str("\\n"),
            b'\r' => output.push_str("\\r"),
            b'\t' => output.push_str("\\t"),
            0x20..=0x7e => output.push(char::from(*byte)),
            _ => {
                let _ = write!(output, "\\{byte:03}");
            }
        }
    }
    output.push('"');
}

fn is_identifier(bytes: &[u8]) -> bool {
    let Some(first) = bytes.first() else {
        return false;
    };
    if !(*first == b'_' || first.is_ascii_alphabetic())
        || !bytes
            .iter()
            .all(|byte| *byte == b'_' || byte.is_ascii_alphanumeric())
    {
        return false;
    }
    !matches!(
        bytes,
        b"and"
            | b"break"
            | b"do"
            | b"else"
            | b"elseif"
            | b"end"
            | b"false"
            | b"for"
            | b"function"
            | b"if"
            | b"in"
            | b"local"
            | b"nil"
            | b"not"
            | b"or"
            | b"repeat"
            | b"return"
            | b"then"
            | b"true"
            | b"until"
            | b"while"
            | b"continue"
            | b"type"
            | b"export"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use luadicrous_bytecode::{
        ChunkHeader, DebugInfo, ParsedChunk, Prototype, StringEntry, WireFlavor,
    };

    fn abc(opcode: Opcode, a: u8, b: u8, c: u8) -> u32 {
        u32::from(opcode.number())
            | (u32::from(a) << 8)
            | (u32::from(b) << 16)
            | (u32::from(c) << 24)
    }

    fn ad(opcode: Opcode, a: u8, d: i16) -> u32 {
        let encoded = u16::from_ne_bytes(d.to_ne_bytes());
        u32::from(opcode.number()) | (u32::from(a) << 8) | (u32::from(encoded) << 16)
    }

    fn chunk(instructions: Vec<u32>, constants: Vec<Constant>) -> ParsedChunk {
        ParsedChunk {
            header: ChunkHeader {
                luau_version: 6,
                types_version: 3,
                types_blob: Vec::new(),
            },
            flavor: WireFlavor::Standard,
            strings: vec![StringEntry::new(b"x".to_vec())],
            prototypes: vec![Prototype {
                max_stack_size: 3,
                num_params: 0,
                num_upvalues: 0,
                is_vararg: true,
                flags: 0,
                type_info: Vec::new(),
                instructions,
                constants,
                child_protos: Vec::new(),
                line_defined: 1,
                debug_name: 0,
                line_info: None,
                debug_info: DebugInfo::default(),
                feedback_slots: Vec::new(),
                inline_cost: None,
                extension_bytes: Vec::new(),
            }],
            main_proto: 0,
            consumed_bytes: 0,
            trailing_bytes: Vec::new(),
        }
    }

    #[test]
    fn recovers_sparse_static_table() {
        let instructions = vec![
            abc(Opcode::PrepVarargs, 0, 0, 0),
            abc(Opcode::NewTable, 0, 0, 0),
            0,
            ad(Opcode::LoadN, 1, 42),
            abc(Opcode::SetTableN, 1, 0, 0),
            ad(Opcode::LoadK, 1, 0),
            ad(Opcode::LoadN, 2, 1_000),
            abc(Opcode::SetTable, 1, 0, 2),
            abc(Opcode::Return, 0, 2, 0),
        ];
        let recovered = decompile(&chunk(instructions, vec![Constant::StringRef(1)])).unwrap();
        assert_eq!(recovered.strategy, DecompileStrategy::StaticData);
        assert!(recovered.source.contains("42,"));
        assert!(recovered.source.contains("[1000] = \"x\""));
    }

    #[test]
    fn rejects_runtime_calls() {
        let instructions = vec![
            abc(Opcode::PrepVarargs, 0, 0, 0),
            abc(Opcode::Call, 0, 1, 1),
            abc(Opcode::Return, 0, 2, 0),
        ];
        let error = decompile(&chunk(instructions, Vec::new())).unwrap_err();
        assert!(error.contains("requires runtime execution"));
    }
}
