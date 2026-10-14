use serde_json::{Map, Value};
use thiserror::Error;

use crate::{Opcode, OpcodeEncoding, ParsedChunk};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomOpcodeTable {
    entries: [Option<Opcode>; 256],
    entry_count: usize,
}

impl CustomOpcodeTable {
    pub fn from_json_slice(bytes: &[u8]) -> Result<Self, CustomOpcodeError> {
        let value = serde_json::from_slice(bytes)?;
        Self::from_json_value(&value)
    }

    pub fn from_json_value(value: &Value) -> Result<Self, CustomOpcodeError> {
        let mut table = Self {
            entries: [None; 256],
            entry_count: 0,
        };
        match value {
            Value::Array(values) => {
                for (raw, opcode) in values.iter().take(256).enumerate() {
                    if let Some(opcode) = parse_opcode_reference(opcode)? {
                        table.insert(raw as u8, opcode)?;
                    }
                }
            }
            Value::Object(root) => table.read_object(root)?,
            _ => return Err(CustomOpcodeError::InvalidRoot),
        }
        if table.entry_count == 0 {
            return Err(CustomOpcodeError::Empty);
        }
        Ok(table)
    }

    #[must_use]
    pub const fn len(&self) -> usize {
        self.entry_count
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.entry_count == 0
    }

    #[must_use]
    pub fn get(&self, raw: u8) -> Option<Opcode> {
        self.entries[usize::from(raw)]
    }

    fn read_object(&mut self, root: &Map<String, Value>) -> Result<(), CustomOpcodeError> {
        let explicit_direction = root
            .get("direction")
            .and_then(Value::as_str)
            .map(str::to_ascii_lowercase);
        let (mapping, wrapper_direction) = if let Some(value) = root.get("raw_to_opcode") {
            (mapping_object(value)?, Some(Direction::RawToOpcode))
        } else if let Some(value) = root.get("raw_to_name") {
            (mapping_object(value)?, Some(Direction::RawToOpcode))
        } else if let Some(value) = root.get("raw_to_op") {
            (mapping_object(value)?, Some(Direction::RawToOpcode))
        } else if let Some(value) = root.get("opcode_to_raw") {
            (mapping_object(value)?, Some(Direction::OpcodeToRaw))
        } else if let Some(value) = root.get("name_to_raw") {
            (mapping_object(value)?, Some(Direction::OpcodeToRaw))
        } else if let Some(value) = root.get("mapping") {
            (mapping_object(value)?, None)
        } else {
            (root, None)
        };
        let direction = if let Some(direction) = wrapper_direction {
            direction
        } else if let Some(direction) = explicit_direction {
            Direction::parse(&direction)?
        } else if mapping
            .keys()
            .filter(|key| !is_metadata_key(key))
            .all(|key| Opcode::from_name(key).is_some())
        {
            Direction::OpcodeToRaw
        } else {
            Direction::RawToOpcode
        };

        for (left, right) in mapping {
            if is_metadata_key(left) {
                continue;
            }
            let (raw, opcode) = match direction {
                Direction::RawToOpcode => {
                    let Some(opcode) = parse_opcode_reference(right)? else {
                        continue;
                    };
                    (parse_opcode_byte(&Value::String(left.clone()))?, opcode)
                }
                Direction::OpcodeToRaw => {
                    let opcode = Opcode::from_name(left)
                        .ok_or_else(|| CustomOpcodeError::UnknownOpcode(left.clone()))?;
                    (parse_opcode_byte(right)?, opcode)
                }
            };
            self.insert(raw, opcode)?;
        }
        Ok(())
    }

    fn insert(&mut self, raw: u8, opcode: Opcode) -> Result<(), CustomOpcodeError> {
        let entry = &mut self.entries[usize::from(raw)];
        if let Some(previous) = entry {
            if *previous != opcode {
                return Err(CustomOpcodeError::ConflictingRaw {
                    raw,
                    first: previous.name(),
                    second: opcode.name(),
                });
            }
            return Ok(());
        }
        *entry = Some(opcode);
        self.entry_count += 1;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy)]
enum Direction {
    RawToOpcode,
    OpcodeToRaw,
}

impl Direction {
    fn parse(value: &str) -> Result<Self, CustomOpcodeError> {
        match value {
            "raw_to_opcode" | "raw_to_name" | "raw_to_op" => Ok(Self::RawToOpcode),
            "opcode_to_raw" | "name_to_raw" | "op_to_raw" => Ok(Self::OpcodeToRaw),
            _ => Err(CustomOpcodeError::UnknownDirection(value.to_owned())),
        }
    }
}

#[derive(Debug, Error)]
pub enum CustomOpcodeError {
    #[error("opcode table is not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("custom opcode table must be a JSON object or list")]
    InvalidRoot,
    #[error("custom opcode table mapping must be an object")]
    InvalidMapping,
    #[error("custom opcode table did not define any opcodes")]
    Empty,
    #[error("invalid opcode byte {0}")]
    InvalidByte(String),
    #[error("unknown canonical opcode {0:?}")]
    UnknownOpcode(String),
    #[error("invalid opcode reference {0}")]
    InvalidOpcodeReference(String),
    #[error("unknown custom opcode table direction {0:?}")]
    UnknownDirection(String),
    #[error("raw opcode 0x{raw:02x} maps to both {first} and {second}")]
    ConflictingRaw {
        raw: u8,
        first: &'static str,
        second: &'static str,
    },
    #[error("custom opcode {opcode} declares operand type {actual:?}, expected {expected:?}")]
    OperandTypeMismatch {
        opcode: &'static str,
        actual: String,
        expected: &'static str,
    },
    #[error("custom opcode {opcode} declares aux={actual}, expected aux={expected}")]
    AuxMismatch {
        opcode: &'static str,
        actual: bool,
        expected: bool,
    },
    #[error("prototype {prototype} pc {pc} uses unmapped raw opcode 0x{raw:02x}")]
    UnmappedOpcode {
        prototype: usize,
        pc: usize,
        raw: u8,
    },
    #[error("prototype {prototype} pc {pc} opcode {opcode} is missing its AUX word")]
    TruncatedAux {
        prototype: usize,
        pc: usize,
        opcode: &'static str,
    },
    #[error("opcode {opcode} cannot be represented in {encoding:?} encoding")]
    UnrepresentableOpcode {
        opcode: &'static str,
        encoding: OpcodeEncoding,
    },
    #[error("opcode {opcode} requires Luau v{minimum_version} or newer")]
    VersionMismatch { opcode: &'static str, minimum_version: u8 },
}

pub fn normalize_custom_opcodes(
    chunk: &mut ParsedChunk,
    table: &CustomOpcodeTable,
) -> Result<OpcodeEncoding, CustomOpcodeError> {
    let version = chunk.header.luau_version;
    let encoding = if version >= 7 {
        OpcodeEncoding::Canonical
    } else {
        OpcodeEncoding::LegacyV6
    };
    for (prototype_index, prototype) in chunk.prototypes.iter_mut().enumerate() {
        let mut pc = 0;
        while pc < prototype.instructions.len() {
            let word = prototype.instructions[pc];
            let raw = (word & 0xff) as u8;
            let opcode = table.get(raw).ok_or(CustomOpcodeError::UnmappedOpcode {
                prototype: prototype_index,
                pc,
                raw,
            })?;
            if version < opcode.minimum_version() {
                return Err(CustomOpcodeError::VersionMismatch {
                    opcode: opcode.name(),
                    minimum_version: opcode.minimum_version(),
                });
            }
            let normalized = opcode.raw_for_encoding(encoding).ok_or(
                CustomOpcodeError::UnrepresentableOpcode {
                    opcode: opcode.name(),
                    encoding,
                },
            )?;
            prototype.instructions[pc] = (word & !0xff) | u32::from(normalized);
            if opcode.has_aux() && pc + 1 >= prototype.instructions.len() {
                return Err(CustomOpcodeError::TruncatedAux {
                    prototype: prototype_index,
                    pc,
                    opcode: opcode.name(),
                });
            }
            pc += usize::from(opcode.has_aux()) + 1;
        }
    }
    Ok(encoding)
}

fn mapping_object(value: &Value) -> Result<&Map<String, Value>, CustomOpcodeError> {
    value.as_object().ok_or(CustomOpcodeError::InvalidMapping)
}

fn is_metadata_key(value: &str) -> bool {
    matches!(value, "comment" | "description" | "direction")
}

fn parse_opcode_byte(value: &Value) -> Result<u8, CustomOpcodeError> {
    if let Some(number) = value.as_u64() {
        return u8::try_from(number).map_err(|_| CustomOpcodeError::InvalidByte(value.to_string()));
    }
    let Some(text) = value.as_str() else {
        return Err(CustomOpcodeError::InvalidByte(value.to_string()));
    };
    let text = text.trim();
    let parsed = if let Some(hex) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        u16::from_str_radix(hex, 16)
    } else {
        text.parse::<u16>()
    };
    parsed
        .ok()
        .and_then(|number| u8::try_from(number).ok())
        .ok_or_else(|| CustomOpcodeError::InvalidByte(text.to_owned()))
}

fn parse_opcode_reference(value: &Value) -> Result<Option<Opcode>, CustomOpcodeError> {
    if value.is_null() || value.as_str().is_some_and(|value| value.trim().is_empty()) {
        return Ok(None);
    }
    if let Some(object) = value.as_object() {
        let reference = object
            .get("name")
            .or_else(|| object.get("opcode"))
            .or_else(|| object.get("op"))
            .or_else(|| object.get("number"))
            .ok_or_else(|| CustomOpcodeError::InvalidOpcodeReference(value.to_string()))?;
        let opcode = parse_opcode_reference(reference)?
            .ok_or_else(|| CustomOpcodeError::InvalidOpcodeReference(reference.to_string()))?;
        if let Some(actual) = object.get("type") {
            let actual = actual
                .as_str()
                .ok_or_else(|| CustomOpcodeError::InvalidOpcodeReference(value.to_string()))?;
            let expected = opcode.operand_type();
            if !actual.eq_ignore_ascii_case(expected) {
                return Err(CustomOpcodeError::OperandTypeMismatch {
                    opcode: opcode.name(),
                    actual: actual.to_owned(),
                    expected,
                });
            }
        }
        if let Some(actual) = object.get("aux") {
            let actual = actual
                .as_bool()
                .ok_or_else(|| CustomOpcodeError::InvalidOpcodeReference(value.to_string()))?;
            let expected = opcode.has_aux();
            if actual != expected {
                return Err(CustomOpcodeError::AuxMismatch {
                    opcode: opcode.name(),
                    actual,
                    expected,
                });
            }
        }
        return Ok(Some(opcode));
    }
    let opcode = if let Some(number) = value.as_u64() {
        u8::try_from(number).ok().and_then(Opcode::from_number)
    } else if let Some(text) = value.as_str() {
        Opcode::from_name(text)
            .or_else(|| parse_opcode_byte(value).ok().and_then(Opcode::from_number))
    } else {
        None
    };
    opcode.map(Some).ok_or_else(|| {
        CustomOpcodeError::UnknownOpcode(
            value
                .as_str()
                .map_or_else(|| value.to_string(), str::to_owned),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ChunkHeader, Prototype, WireFlavor};

    #[test]
    fn accepts_python_compatible_table_shapes() {
        for json in [
            br#"{"raw_to_opcode":{"0xc8":"LOADN","201":"RETURN"}}"#.as_slice(),
            br#"{"opcode_to_raw":{"LOADN":"0xc8","RETURN":201}}"#.as_slice(),
            br#"{"0xc8":"LOADN","201":{"name":"RETURN","type":"iAB"}}"#.as_slice(),
            br#"[null,null,null,null,"LOADN"]"#.as_slice(),
        ] {
            assert!(!CustomOpcodeTable::from_json_slice(json).unwrap().is_empty());
        }
    }

    #[test]
    fn normalizes_only_instruction_words() {
        let table = CustomOpcodeTable::from_json_slice(
            br#"{"raw_to_opcode":{"200":"GETGLOBAL","201":"RETURN"}}"#,
        )
        .unwrap();
        let mut chunk = ParsedChunk {
            header: ChunkHeader {
                luau_version: 7,
                types_version: 0,
                types_blob: Vec::new(),
            },
            flavor: WireFlavor::Standard,
            strings: Vec::new(),
            prototypes: vec![Prototype {
                max_stack_size: 1,
                num_params: 0,
                num_upvalues: 0,
                is_vararg: false,
                flags: 0,
                type_info: Vec::new(),
                instructions: vec![200, 0xdead_bec9, 201 | (2 << 16)],
                constants: Vec::new(),
                child_protos: Vec::new(),
                line_defined: 0,
                debug_name: 0,
                line_info: None,
                debug_info: Default::default(),
                feedback_slots: Vec::new(),
                inline_cost: None,
                extension_bytes: Vec::new(),
            }],
            main_proto: 0,
            consumed_bytes: 0,
            trailing_bytes: Vec::new(),
        };
        normalize_custom_opcodes(&mut chunk, &table).unwrap();
        assert_eq!(chunk.prototypes[0].instructions[0] & 0xff, 7);
        assert_eq!(chunk.prototypes[0].instructions[1], 0xdead_bec9);
        assert_eq!(chunk.prototypes[0].instructions[2] & 0xff, 22);
    }
}
