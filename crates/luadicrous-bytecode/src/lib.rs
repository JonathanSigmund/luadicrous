mod cursor;
mod custom_opcode;
mod model;
mod opcode;
mod parser;

use serde::Serialize;
use thiserror::Error;

use cursor::Cursor;
pub use custom_opcode::*;
pub use model::*;
pub use opcode::*;
pub use parser::{looks_like_luau_source, parse};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseLimits {
    pub max_input_bytes: usize,
    pub max_strings: usize,
    pub max_string_bytes: usize,
    pub max_total_string_bytes: usize,
    pub max_type_bytes: usize,
    pub max_prototypes: usize,
    pub max_instructions_per_proto: usize,
    pub max_constants_per_proto: usize,
    pub max_debug_entries: usize,
}

impl Default for ParseLimits {
    fn default() -> Self {
        Self {
            max_input_bytes: 512 * 1024 * 1024,
            max_strings: 1_000_000,
            max_string_bytes: 64 * 1024 * 1024,
            max_total_string_bytes: 256 * 1024 * 1024,
            max_type_bytes: 16 * 1024 * 1024,
            max_prototypes: 100_000,
            max_instructions_per_proto: 2_000_000,
            max_constants_per_proto: 500_000,
            max_debug_entries: 500_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseOptions {
    pub flavor: WireFlavor,
    pub min_version: u8,
    pub max_version: u8,
    pub limits: ParseLimits,
}

impl Default for ParseOptions {
    fn default() -> Self {
        Self {
            flavor: WireFlavor::Auto,
            min_version: 1,
            max_version: 14,
            limits: ParseLimits::default(),
        }
    }
}

#[derive(Debug, Error, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ParseError {
    #[error("input appears to be plain Luau source, not serialized Luau bytecode")]
    SourceText,
    #[error("unexpected end of bytecode at {offset}: needed {needed} bytes, {remaining} remain")]
    UnexpectedEof {
        offset: usize,
        needed: usize,
        remaining: usize,
    },
    #[error("varint overflows u32 at {offset}")]
    VarintOverflow { offset: usize },
    #[error("varint overflows u64 at {offset}")]
    Varint64Overflow { offset: usize },
    #[error("length {length} overflows address space at {offset}")]
    LengthOverflow { offset: usize, length: usize },
    #[error("{field} value {value} exceeds limit {limit} at {offset}")]
    LimitExceeded {
        field: &'static str,
        value: usize,
        limit: usize,
        offset: usize,
    },
    #[error("unsupported Luau bytecode version {version}; expected {min}..={max}")]
    UnsupportedVersion { version: u8, min: u8, max: u8 },
    #[error("invalid {field} flag {value} at {offset}")]
    InvalidFlag {
        field: &'static str,
        value: u8,
        offset: usize,
    },
    #[error("invalid prototype header at {offset}: {reason}")]
    InvalidPrototypeHeader { offset: usize, reason: String },
    #[error("invalid {field} index {index}; count is {count} at {offset}")]
    InvalidReference {
        field: &'static str,
        index: u32,
        count: usize,
        offset: usize,
    },
    #[error("unsupported {flavor:?} constant tag {tag:#04x} at {offset}")]
    UnsupportedConstantTag {
        tag: u8,
        flavor: WireFlavor,
        offset: usize,
    },
    #[error("cannot represent u32 value {value} as usize at {offset}")]
    IntegerConversion { value: u32, offset: usize },
    #[error("integer constant magnitude {magnitude} exceeds signed 64-bit range at {offset}")]
    IntegerMagnitudeOverflow { magnitude: u64, offset: usize },
    #[error("neither strict constant flavor matched (standard: {standard}; roblox: {roblox})")]
    NoFlavorMatched { standard: String, roblox: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_chunk(version: u8) -> Vec<u8> {
        let mut bytes = vec![version];
        if version >= 4 {
            bytes.push(0);
        }
        bytes.extend([
            0, // strings
            1, // prototypes
            1, 0, 0, 0, // stack, params, upvalues, vararg
        ]);
        if version >= 4 {
            bytes.push(0); // flags
        }
        bytes.extend([
            1, // instruction count
            0, 0, 0, 0, // raw instruction
            0, // constant count
            0, // child count
            0, // line defined
            0, // debug name
            0, // line info
            0, // debug info
            0, // main
        ]);
        bytes
    }

    fn v10_class_chunk() -> Vec<u8> {
        let mut bytes = vec![10, 3, 2];
        bytes.extend([6, b'W', b'i', b'd', b'g', b'e', b't']);
        bytes.extend([3, b'r', b'u', b'n']);
        bytes.push(0); // userdata type remapping terminator
        bytes.extend([
            1, // prototypes
            2, 0, 0, 0, // stack, params, upvalues, vararg
            0, // flags
            0, // type info
            3, // instruction words
        ]);
        let new_class_member = u32::from(Opcode::NewClassMember.number()) | (1_u32 << 24);
        let return_value = u32::from(Opcode::Return.number()) | (2_u32 << 16);
        bytes.extend(new_class_member.to_le_bytes());
        bytes.extend(1_u32.to_le_bytes()); // member-name constant
        bytes.extend(return_value.to_le_bytes());
        bytes.extend([
            3, // constants
            3, 1, // K0 = "Widget"
            3, 2, // K1 = "run"
            10, 0, 0, 1, 1, // K2 = class shape(name K0, 0 properties, method K1)
            0, // children
            0, // line defined
            0, // debug name
            0, // line info
            0, // debug info
            0, // main
        ]);
        bytes
    }

    fn feedback_chunk(version: u8, extension: &[u8]) -> Vec<u8> {
        let mut prototype = vec![2, 0, 0, 0, 8, 0, 3];
        prototype.extend((u32::from(Opcode::CallFb.number()) | (1 << 16) | (1 << 24)).to_le_bytes());
        prototype.extend(0_u32.to_le_bytes());
        prototype.extend((u32::from(Opcode::Return.number()) | (1 << 16)).to_le_bytes());
        prototype.extend([0, 0, 0, 0, 0, 0, 1, 0, 0]);
        if version >= 12 {
            prototype.push(42);
            prototype.extend(extension);
        }
        let mut bytes = vec![version, 3, 0, 0, 1];
        if version >= 12 {
            bytes.push(u8::try_from(prototype.len()).unwrap());
        }
        bytes.extend(prototype);
        bytes.push(0);
        bytes
    }

    #[test]
    fn parses_minimal_v3_chunk() {
        let parsed = parse(&minimal_chunk(3), &ParseOptions::default()).unwrap();
        let ParsedBytecode::Chunk(chunk) = parsed else {
            panic!("expected a chunk");
        };
        assert_eq!(chunk.header.luau_version, 3);
        assert_eq!(chunk.prototypes.len(), 1);
        assert_eq!(chunk.prototypes[0].instructions.len(), 1);
        assert!(chunk.trailing_bytes.is_empty());
    }

    #[test]
    fn parses_compilation_error() {
        let bytes = [0, 4, b'o', b'o', b'p', b's'];
        let parsed = parse(&bytes, &ParseOptions::default()).unwrap();
        assert_eq!(parsed, ParsedBytecode::CompilationError("oops".to_owned()));
    }

    #[test]
    fn rejects_source_starting_with_newline_as_source_not_v10() {
        let bytes = b"\n    --[]\nlocal value = 42\nfunction read() return value end\nreturn read";
        let error = parse(bytes, &ParseOptions::default()).unwrap_err();
        assert!(matches!(error, ParseError::SourceText));
        assert!(!error.to_string().contains("version 10"));
    }

    #[test]
    fn source_detector_rejects_binary_chunks() {
        assert!(looks_like_luau_source(
            b"local value = 42\nfunction read() return value end\nreturn read"
        ));
        assert!(!looks_like_luau_source(&minimal_chunk(3)));
    }

    #[test]
    fn parses_v10_class_shape_and_new_class_member() {
        let parsed = parse(&v10_class_chunk(), &ParseOptions::default()).unwrap();
        let ParsedBytecode::Chunk(chunk) = parsed else {
            panic!("expected a chunk");
        };
        assert_eq!(chunk.header.luau_version, 10);
        assert!(chunk.trailing_bytes.is_empty());
        assert_eq!(
            chunk.prototypes[0].constants[2],
            Constant::ClassShape(ClassShape {
                class_name: 0,
                property_count: 0,
                method_count: 1,
                members: vec![1],
            })
        );
        let decoded = decode_prototype(&chunk.prototypes[0].instructions, 10);
        assert_eq!(decoded.instructions[0].opcode, Some(Opcode::NewClassMember));
        assert_eq!(decoded.instructions[0].aux, Some(1));
        assert_eq!(decoded.quality.unknown_opcodes, 0);
    }

    #[test]
    fn parses_v11_feedback_slots() {
        let parsed = parse(&feedback_chunk(11, &[]), &ParseOptions::default()).unwrap();
        let ParsedBytecode::Chunk(chunk) = parsed else {
            panic!("expected a chunk");
        };
        assert_eq!(chunk.prototypes[0].feedback_slots, vec![FeedbackSlot { kind: 0, pc: 0 }]);
        assert_eq!(chunk.prototypes[0].inline_cost, None);
        assert!(chunk.trailing_bytes.is_empty());
    }

    #[test]
    fn parses_v12_proto_size_cost_and_extension() {
        let bytes = feedback_chunk(12, &[0xaa, 0xbb]);
        let parsed = parse(&bytes, &ParseOptions::default()).unwrap();
        let ParsedBytecode::Chunk(chunk) = parsed else {
            panic!("expected a chunk");
        };
        assert_eq!(chunk.prototypes[0].feedback_slots, vec![FeedbackSlot { kind: 0, pc: 0 }]);
        assert_eq!(chunk.prototypes[0].inline_cost, Some(42));
        assert_eq!(chunk.prototypes[0].extension_bytes, vec![0xaa, 0xbb]);
        assert_eq!(chunk.consumed_bytes, bytes.len());
        let decoded = decode_prototype(&chunk.prototypes[0].instructions, 12);
        assert_eq!(decoded.instructions[0].opcode, Some(Opcode::CallFb));
        assert_eq!(decoded.instructions[0].aux, Some(0));
        assert_eq!(decoded.quality.unknown_opcodes, 0);
    }

    #[test]
    fn rejects_v12_proto_size_shorter_than_fields() {
        let mut bytes = feedback_chunk(12, &[]);
        bytes[5] -= 1;
        let error = parse(&bytes, &ParseOptions::default()).unwrap_err();
        assert!(matches!(error, ParseError::NoFlavorMatched { .. }));
    }

    #[test]
    fn rejects_unbounded_counts() {
        let bytes = [3, 0xff, 0xff, 0xff, 0xff, 0x0f];
        let options = ParseOptions {
            flavor: WireFlavor::Standard,
            ..ParseOptions::default()
        };
        let error = parse(&bytes, &options).unwrap_err();
        assert!(matches!(error, ParseError::LimitExceeded { .. }));
    }

    #[test]
    fn reads_main_before_opaque_trailer() {
        let mut bytes = vec![3, 0, 3];
        let prototype = [
            1, 0, 0, 0, // stack, params, upvalues, vararg
            1, // instruction count
            0, 0, 0, 0, // raw instruction
            0, // constants
            0, // children
            0, // line defined
            0, // debug name
            0, // line info
            0, // debug info
        ];
        for _ in 0..3 {
            bytes.extend(prototype);
        }
        bytes.push(2);
        bytes.extend([0x17; 24]);

        let parsed = parse(&bytes, &ParseOptions::default()).unwrap();
        let ParsedBytecode::Chunk(chunk) = parsed else {
            panic!("expected a chunk");
        };
        assert_eq!(chunk.main_proto, 2);
        assert_eq!(chunk.trailing_bytes, vec![0x17; 24]);
        assert_eq!(chunk.consumed_bytes, bytes.len() - 24);
    }
}
