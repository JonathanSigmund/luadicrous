use luadicrous_bytecode::{Constant, Opcode, ParseOptions, ParsedBytecode, WireFlavor, parse};
use luadicrous_core::{CustomOpcodeTable, Engine};
use luadicrous_ir::{ConstantValue, DiagnosticCode};

fn varint(mut value: usize) -> Vec<u8> {
    let mut bytes = Vec::new();
    while value >= 128 {
        bytes.push((value as u8 & 127) | 128);
        value >>= 7;
    }
    bytes.push(value as u8);
    bytes
}

fn chunk(version: u8, words: &[u32], constants: &[u8], constant_count: u8, params: u8) -> Vec<u8> {
    let mut prototype = vec![8, params, 0, 0, 0, 0];
    prototype.extend(varint(words.len()));
    for word in words {
        prototype.extend(word.to_le_bytes());
    }
    prototype.push(constant_count);
    prototype.extend(constants);
    prototype.extend([0, 0, 0, 0, 0, 0]);
    let mut bytes = vec![version, 3, 1, 5];
    bytes.extend(b"pcall");
    bytes.extend([0, 1]);
    bytes.extend(varint(prototype.len()));
    bytes.extend(prototype);
    bytes.push(0);
    bytes
}

fn vector_chunk(version: u8, components: [f64; 4], params: u8) -> Vec<u8> {
    let mut constants = vec![11];
    for component in components {
        constants.extend(component.to_bits().to_le_bytes());
    }
    let words = if params == 0 {
        vec![5, 22 | (2 << 16)]
    } else {
        vec![5 | (1 << 8), 21 | (2 << 16) | (2 << 24), 22 | (2 << 16)]
    };
    chunk(version, &words, &constants, 1, params)
}

fn protected_chunk(protected_id: u8, skip: u8) -> Vec<u8> {
    let words = [
        6 | (1 << 8),
        89 | (u32::from(protected_id) << 8) | (1 << 16) | (u32::from(skip) << 24),
        7,
        0,
        21 | (2 << 16) | (3 << 24),
        22 | (3 << 16),
    ];
    chunk(14, &words, &[3, 1], 1, 1)
}

#[test]
fn parses_double_vectors_without_rounding_any_component() {
    let components = [1.0000000000000002, -0.0, 1.2345678901234567, 4.000000000000001];
    for version in [13, 14] {
        let bytes = vector_chunk(version, components, 0);
        let options = ParseOptions { flavor: WireFlavor::Standard, ..ParseOptions::default() };
        let ParsedBytecode::Chunk(parsed) = parse(&bytes, &options).unwrap() else {
            panic!("expected a chunk");
        };
        assert_eq!(parsed.prototypes[0].constants[0], Constant::VectorDoubleBits(components.map(f64::to_bits)));
        assert!(parsed.trailing_bytes.is_empty());
        let analysis = Engine::default().analyze(&bytes).unwrap();
        assert_eq!(analysis.module.functions[0].constants[0], ConstantValue::VectorDoubleBits(components.map(f64::to_bits)));
    }
}

#[test]
fn rejects_double_vector_tag_in_v12_and_truncated_payloads() {
    let options = ParseOptions { flavor: WireFlavor::Standard, ..ParseOptions::default() };
    assert!(parse(&vector_chunk(12, [1.0; 4], 0), &options).is_err());
    let bytes = vector_chunk(13, [1.0; 4], 0);
    for length in 0..bytes.len() {
        assert!(parse(&bytes[..length], &options).is_err());
    }
}

#[test]
fn emits_double_vectors_in_static_and_structured_paths() {
    for params in [0, 1] {
        let bytes = vector_chunk(13, [1.0000000000000002, -0.0, 1.2345678901234567, 4.000000000000001], params);
        let decompiled = Engine::default().decompile(&bytes).unwrap();
        assert!(decompiled.source.contains("vector.create(1.0000000000000002, -0"));
        assert!(decompiled.source.contains("1.2345678901234567, 4.000000000000001)"));
    }
}

#[test]
fn emits_non_finite_double_vectors_as_luau_expressions() {
    for params in [0, 1] {
        let bytes = vector_chunk(13, [f64::INFINITY, f64::NEG_INFINITY, f64::NAN, 0.0], params);
        let decompiled = Engine::default().decompile(&bytes).unwrap();
        assert!(decompiled.source.contains("math.huge"));
        assert!(decompiled.source.contains("0/0") || decompiled.source.contains("0 / 0"));
        assert!(!decompiled.source.contains("NaN"));
        assert!(!decompiled.source.contains("inf"));
    }
}

#[test]
fn lifts_fast_pcall_once_through_its_original_fallback() {
    let bytes = protected_chunk(0, 2);
    let analysis = Engine::default().analyze(&bytes).unwrap();
    assert!(analysis.quality.clean);
    assert!(Engine::default().disassemble(&bytes).unwrap().contains("FASTPCALL"));
    let decompiled = Engine::default().decompile(&bytes).unwrap();
    assert_eq!(decompiled.source.matches("pcall(").count(), 1);
    assert!(!decompiled.source.contains("FASTPCALL"));
}

#[test]
fn recovery_methods_are_not_exposed_in_source_watermarks() {
    for bytes in [
        vector_chunk(13, [1.0, 2.0, 3.0, 0.0], 0),
        vector_chunk(13, [1.0, 2.0, 3.0, 0.0], 1),
        protected_chunk(0, 2),
    ] {
        let decompiled = Engine::default().decompile(&bytes).unwrap();
        assert!(decompiled.source.starts_with("-- Decompiled by Luadicrous V2\n"));
        let source = decompiled.source.to_lowercase();
        assert!(!source.contains("recovery strategy"));
        assert!(!source.contains("recovery method"));
    }
}

#[test]
fn rejects_invalid_protected_ids_and_non_call_or_aux_targets() {
    let invalid_id = Engine::default().analyze(&protected_chunk(2, 2)).unwrap();
    assert!(invalid_id.module.diagnostics.iter().any(|diagnostic| {
        diagnostic.code == DiagnosticCode::InvalidOpcodeReference
    }));
    for skip in [0, 1, 3, 255] {
        let analysis = Engine::default().analyze(&protected_chunk(0, skip)).unwrap();
        assert!(analysis.module.diagnostics.iter().any(|diagnostic| {
            diagnostic.code == DiagnosticCode::InvalidJumpTarget
        }));
        assert!(Engine::default().decompile(&protected_chunk(0, skip)).is_err());
    }
}

#[test]
fn custom_tables_support_fast_pcall_and_enforce_its_version() {
    let mapping = CustomOpcodeTable::from_json_slice(br#"{"6":"MOVE","7":"GETGLOBAL","21":"CALL","22":"RETURN","201":"FASTPCALL"}"#).unwrap();
    let mut bytes = protected_chunk(0, 2);
    let location = bytes.windows(4).position(|word| word == [89, 0, 1, 2]).unwrap();
    bytes[location] = 201;
    let engine = Engine::default().with_opcode_table(mapping);
    assert!(engine.analyze(&bytes).unwrap().quality.clean);
    assert_eq!(engine.decompile(&bytes).unwrap().source.matches("pcall(").count(), 1);
    bytes[0] = 13;
    assert!(engine.analyze(&bytes).unwrap_err().to_string().contains("v14"));
    assert_eq!(Opcode::FastPcall.number(), 89);
    assert!(!Opcode::FastPcall.has_aux());
}
