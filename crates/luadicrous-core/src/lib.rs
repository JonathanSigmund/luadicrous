mod static_data;
mod structured;

use luadicrous_bytecode::{
    Constant as WireConstant, DebugInfo as WireDebugInfo, DecodedInstruction,
    LineInfo as WireLineInfo, Opcode, ParseError, ParseOptions, ParsedBytecode, ParsedChunk,
    Prototype as WirePrototype, TableEntry, decode_prototype, normalize_custom_opcodes, parse,
};
use luadicrous_ir::{
    ClassShapeValue, ConstantValue, Diagnostic, DiagnosticCode, FormatMetadata, Function,
    FunctionDebug, FunctionId, IrString, LineInfo, LocalDebug, Module, RawInstruction, Severity,
    Signature, StringId, TableTemplateEntry,
};
use serde::Serialize;
use thiserror::Error;

pub use luadicrous_bytecode::{CustomOpcodeError, CustomOpcodeTable, looks_like_luau_source};
pub use static_data::DecompileStrategy;

#[derive(Debug, Clone, Default)]
pub struct Engine {
    options: ParseOptions,
    opcode_table: Option<CustomOpcodeTable>,
}

impl Engine {
    #[must_use]
    pub const fn new(options: ParseOptions) -> Self {
        Self {
            options,
            opcode_table: None,
        }
    }

    #[must_use]
    pub fn with_opcode_table(mut self, opcode_table: CustomOpcodeTable) -> Self {
        self.opcode_table = Some(opcode_table);
        self
    }

    pub fn analyze(&self, bytes: &[u8]) -> Result<Analysis, EngineError> {
        if is_puc_lua(bytes) {
            return analyze_lua51(bytes);
        }
        match self.parse(bytes)? {
            ParsedBytecode::CompilationError(message) => Err(EngineError::Compilation(message)),
            ParsedBytecode::Chunk(chunk) => {
                Ok(lower_and_validate(chunk, self.opcode_table.is_some()))
            }
        }
    }

    pub fn decompile(&self, bytes: &[u8]) -> Result<Decompilation, EngineError> {
        if is_puc_lua(bytes) {
            let recovered = lua51_lifter::decompile(bytes)
                .map_err(|error| EngineError::Lua51(error.to_string()))?;
            let analysis = analysis_from_lua51_inventory(recovered.inventory);
            return Ok(Decompilation {
                source: recovered.source,
                strategy: DecompileStrategy::Lua51,
                analysis,
            });
        }
        match self.parse(bytes)? {
            ParsedBytecode::CompilationError(message) => Err(EngineError::Compilation(message)),
            ParsedBytecode::Chunk(chunk) => {
                let analysis = lower_and_validate(chunk.clone(), self.opcode_table.is_some());
                if analysis.quality.errors != 0 && std::env::var("LUADICROUS_FORCE").is_err() {
                    return Err(EngineError::UnsafeDecompilation {
                        errors: analysis.quality.errors,
                    });
                }
                let (source, strategy) = match static_data::decompile(&chunk) {
                    Ok(recovered) => (recovered.source, recovered.strategy),
                    Err(static_reason) => match structured::decompile(&chunk) {
                        Ok(source) => (source, DecompileStrategy::Structured),
                        Err(structured_reason) => {
                            return Err(EngineError::UnsupportedDecompilation {
                                reason: format!(
                                    "static-data: {static_reason}; structured: {structured_reason}"
                                ),
                            });
                        }
                    },
                };
                Ok(Decompilation {
                    source,
                    strategy,
                    analysis,
                })
            }
        }
    }

    pub fn disassemble(&self, bytes: &[u8]) -> Result<String, EngineError> {
        if is_puc_lua(bytes) {
            return lua51_lifter::disassemble(bytes)
                .map_err(|error| EngineError::Lua51(error.to_string()));
        }
        match self.parse(bytes)? {
            ParsedBytecode::CompilationError(message) => Err(EngineError::Compilation(message)),
            ParsedBytecode::Chunk(chunk) => {
                Ok(disassemble_chunk(&chunk, self.opcode_table.is_some()))
            }
        }
    }

    fn parse(&self, bytes: &[u8]) -> Result<ParsedBytecode, EngineError> {
        let mut parsed = parse(bytes, &self.options)?;
        if let (ParsedBytecode::Chunk(chunk), Some(opcode_table)) =
            (&mut parsed, &self.opcode_table)
        {
            normalize_custom_opcodes(chunk, opcode_table)?;
        }
        Ok(parsed)
    }
}

#[derive(Debug, Error)]
pub enum EngineError {
    #[error(transparent)]
    Parse(#[from] ParseError),
    #[error(transparent)]
    CustomOpcode(#[from] CustomOpcodeError),
    #[error("bytecode contains a compiler error: {0}")]
    Compilation(String),
    #[error("no correctness-safe source strategy matched: {reason}")]
    UnsupportedDecompilation { reason: String },
    #[error("source recovery produced {errors} structural validation errors")]
    UnsafeDecompilation { errors: usize },
    #[error("Lua bytecode error: {0}")]
    Lua51(String),
}

#[derive(Debug, Clone, Serialize)]
pub struct Decompilation {
    pub source: String,
    pub strategy: DecompileStrategy,
    pub analysis: Analysis,
}

#[derive(Debug, Clone, Serialize)]
pub struct Analysis {
    pub module: Module,
    pub inventory: Inventory,
    pub quality: QualityReport,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Inventory {
    pub luau_version: u8,
    pub types_version: u8,
    pub wire_flavor: String,
    pub string_count: usize,
    pub proto_count: usize,
    pub main_proto: u32,
    pub consumed_bytes: usize,
    pub trailing_bytes: usize,
    pub instruction_counts: Vec<usize>,
    pub constant_counts: Vec<usize>,
    pub opcode_encodings: Vec<String>,
    pub decoded_instruction_counts: Vec<usize>,
    pub unknown_opcode_counts: Vec<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct QualityReport {
    pub score: i64,
    pub errors: usize,
    pub warnings: usize,
    pub clean: bool,
}

fn lower_and_validate(chunk: ParsedChunk, custom_opcodes: bool) -> Analysis {
    let inventory = build_inventory(&chunk, custom_opcodes);
    let mut diagnostics = trailing_diagnostics(inventory.trailing_bytes);
    diagnostics.extend(opcode_diagnostics(&chunk));
    let strings = chunk
        .strings
        .into_iter()
        .map(|value| IrString {
            bytes: value.bytes,
            text_lossy: value.text_lossy,
            valid_utf8: value.valid_utf8,
        })
        .collect();

    let function_count = chunk.prototypes.len();
    let mut context = LoweringContext {
        string_count: inventory.string_count,
        function_count,
        diagnostics: &mut diagnostics,
    };
    let mut functions = Vec::with_capacity(function_count);
    for (index, prototype) in chunk.prototypes.into_iter().enumerate() {
        functions.push(context.function(prototype, index));
    }

    let entry = FunctionId(chunk.main_proto);
    if index_out_of_bounds(entry.0, functions.len()) {
        diagnostics.push(Diagnostic::error(
            DiagnosticCode::InvalidMain,
            None,
            format!("main prototype {} does not exist", entry.0),
        ));
    }
    let quality = build_quality(&diagnostics, inventory.trailing_bytes);
    let module = Module {
        format: FormatMetadata {
            luau_version: inventory.luau_version,
            types_version: inventory.types_version,
            wire_flavor: inventory.wire_flavor.clone(),
            consumed_bytes: inventory.consumed_bytes,
            trailing_bytes: inventory.trailing_bytes,
        },
        strings,
        functions,
        entry,
        diagnostics,
    };
    Analysis {
        module,
        inventory,
        quality,
    }
}

fn is_puc_lua(bytes: &[u8]) -> bool {
    bytes.starts_with(b"\x1bLua")
}

fn analyze_lua51(bytes: &[u8]) -> Result<Analysis, EngineError> {
    let inventory =
        lua51_lifter::inspect(bytes).map_err(|error| EngineError::Lua51(error.to_string()))?;
    Ok(analysis_from_lua51_inventory(inventory))
}

fn analysis_from_lua51_inventory(inventory: lua51_lifter::Lua51Inventory) -> Analysis {
    let diagnostics = trailing_diagnostics(inventory.trailing_bytes);
    let quality = build_quality(&diagnostics, inventory.trailing_bytes);
    let module = Module {
        format: FormatMetadata {
            luau_version: 0,
            types_version: 0,
            wire_flavor: "lua51".to_owned(),
            consumed_bytes: inventory.consumed_bytes,
            trailing_bytes: inventory.trailing_bytes,
        },
        strings: Vec::new(),
        functions: Vec::new(),
        entry: FunctionId(0),
        diagnostics,
    };
    Analysis {
        module,
        inventory: Inventory {
            luau_version: 0,
            types_version: 0,
            wire_flavor: "lua51".to_owned(),
            string_count: inventory.string_constant_count,
            proto_count: inventory.proto_count,
            main_proto: 0,
            consumed_bytes: inventory.consumed_bytes,
            trailing_bytes: inventory.trailing_bytes,
            instruction_counts: inventory.instruction_counts.clone(),
            constant_counts: inventory.constant_counts,
            opcode_encodings: vec!["lua51".to_owned(); inventory.proto_count],
            decoded_instruction_counts: inventory.instruction_counts,
            unknown_opcode_counts: vec![0; inventory.proto_count],
        },
        quality,
    }
}

fn build_inventory(chunk: &ParsedChunk, custom_opcodes: bool) -> Inventory {
    let decoded = chunk
        .prototypes
        .iter()
        .map(|prototype| decode_prototype(&prototype.instructions, chunk.header.luau_version))
        .collect::<Vec<_>>();
    Inventory {
        luau_version: chunk.header.luau_version,
        types_version: chunk.header.types_version,
        wire_flavor: format!("{:?}", chunk.flavor).to_lowercase(),
        string_count: chunk.strings.len(),
        proto_count: chunk.prototypes.len(),
        main_proto: chunk.main_proto,
        consumed_bytes: chunk.consumed_bytes,
        trailing_bytes: chunk.trailing_bytes.len(),
        instruction_counts: chunk
            .prototypes
            .iter()
            .map(|prototype| prototype.instructions.len())
            .collect(),
        constant_counts: chunk
            .prototypes
            .iter()
            .map(|prototype| prototype.constants.len())
            .collect(),
        opcode_encodings: decoded
            .iter()
            .map(|prototype| {
                if custom_opcodes {
                    "custom".to_owned()
                } else {
                    prototype.encoding.name().to_owned()
                }
            })
            .collect(),
        decoded_instruction_counts: decoded
            .iter()
            .map(|prototype| prototype.quality.decoded_instructions)
            .collect(),
        unknown_opcode_counts: decoded
            .iter()
            .map(|prototype| prototype.quality.unknown_opcodes)
            .collect(),
    }
}

fn opcode_diagnostics(chunk: &ParsedChunk) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    for (index, prototype) in chunk.prototypes.iter().enumerate() {
        let function = FunctionId(u32::try_from(index).unwrap_or(u32::MAX));
        let decoded = decode_prototype(&prototype.instructions, chunk.header.luau_version);
        if decoded.quality.unknown_opcodes != 0 {
            diagnostics.push(Diagnostic::error(
                DiagnosticCode::UnknownOpcode,
                Some(function),
                format!(
                    "{} instruction words do not decode under {} encoding",
                    decoded.quality.unknown_opcodes,
                    decoded.encoding.name()
                ),
            ));
        }
        if decoded.quality.truncated_aux != 0 {
            diagnostics.push(Diagnostic::error(
                DiagnosticCode::TruncatedAuxWord,
                Some(function),
                format!(
                    "{} instructions require an unavailable AUX word",
                    decoded.quality.truncated_aux
                ),
            ));
        }
        if decoded.quality.invalid_jump_targets != 0 {
            diagnostics.push(Diagnostic::error(
                DiagnosticCode::InvalidJumpTarget,
                Some(function),
                format!(
                    "{} jumps or protected-call fallbacks target invalid instructions",
                    decoded.quality.invalid_jump_targets
                ),
            ));
        }
        for instruction in &decoded.instructions {
            validate_opcode_reference(
                prototype,
                instruction,
                chunk.prototypes.len(),
                function,
                &mut diagnostics,
            );
        }
        validate_constant_references(prototype, function, &mut diagnostics);
    }
    diagnostics
}

fn validate_opcode_reference(
    prototype: &WirePrototype,
    instruction: &DecodedInstruction,
    prototype_count: usize,
    function: FunctionId,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let Some(opcode) = instruction.opcode else {
        return;
    };
    if opcode == Opcode::FastPcall && instruction.a() > 1 {
        diagnostics.push(Diagnostic::error(
            DiagnosticCode::InvalidOpcodeReference,
            Some(function),
            format!("FASTPCALL at pc {} has invalid protected function id {}", instruction.pc, instruction.a()),
        ));
    }
    let constant = match opcode {
        Opcode::LoadK | Opcode::GetImport | Opcode::DupTable | Opcode::DupClosure => {
            Some(u32::from(instruction.bx()))
        }
        Opcode::LoadKx
        | Opcode::GetGlobal
        | Opcode::SetGlobal
        | Opcode::GetTableKs
        | Opcode::SetTableKs
        | Opcode::Namecall
        | Opcode::GetUdataKs
        | Opcode::SetUdataKs
        | Opcode::NamecallUdata
        | Opcode::NewClassMember => instruction.aux,
        Opcode::AddK
        | Opcode::SubK
        | Opcode::MulK
        | Opcode::DivK
        | Opcode::ModK
        | Opcode::PowK
        | Opcode::AndK
        | Opcode::OrK
        | Opcode::IDivK => Some(u32::from(instruction.c())),
        Opcode::SubRk | Opcode::DivRk => Some(u32::from(instruction.b())),
        Opcode::JumpXEqKN | Opcode::JumpXEqKS => instruction.aux.map(|value| value & 0x00ff_ffff),
        _ => None,
    };
    if constant.is_some_and(|constant| index_out_of_bounds(constant, prototype.constants.len())) {
        diagnostics.push(Diagnostic::error(
            DiagnosticCode::InvalidOpcodeReference,
            Some(function),
            format!(
                "{} at pc {} references constant {} outside 0..{}",
                opcode.name(),
                instruction.pc,
                constant.unwrap_or(u32::MAX),
                prototype.constants.len()
            ),
        ));
    }
    if opcode == Opcode::NewClosure {
        let child = u32::from(instruction.bx());
        if index_out_of_bounds(child, prototype.child_protos.len()) {
            diagnostics.push(Diagnostic::error(
                DiagnosticCode::InvalidOpcodeReference,
                Some(function),
                format!(
                    "NEWCLOSURE at pc {} references child slot {} outside 0..{}",
                    instruction.pc,
                    child,
                    prototype.child_protos.len()
                ),
            ));
        }
    }
    if opcode == Opcode::CmpProto {
        if let Some(proto_id) = instruction.aux {
            if index_out_of_bounds(proto_id, prototype_count) {
                diagnostics.push(Diagnostic::error(
                    DiagnosticCode::InvalidOpcodeReference,
                    Some(function),
                    format!(
                        "CMPPROTO at pc {} references proto {} outside 0..{}",
                        instruction.pc, proto_id, prototype_count
                    ),
                ));
            }
        }
    }
}

fn validate_constant_references(
    prototype: &WirePrototype,
    function: FunctionId,
    diagnostics: &mut Vec<Diagnostic>,
) {
    for (constant_index, constant) in prototype.constants.iter().enumerate() {
        let WireConstant::ClassShape(shape) = constant else {
            continue;
        };
        for (role, reference) in std::iter::once(("class name", shape.class_name)).chain(
            shape
                .members
                .iter()
                .copied()
                .map(|reference| ("class member", reference)),
        ) {
            let referenced = usize::try_from(reference)
                .ok()
                .and_then(|index| prototype.constants.get(index));
            if !matches!(referenced, Some(WireConstant::StringRef(_))) {
                diagnostics.push(Diagnostic::error(
                    DiagnosticCode::InvalidConstantReference,
                    Some(function),
                    format!(
                        "class shape constant {constant_index} {role} reference {reference} must point to a string constant"
                    ),
                ));
            }
        }
    }
}

fn disassemble_chunk(chunk: &ParsedChunk, custom_opcodes: bool) -> String {
    use std::fmt::Write as _;

    let mut output = String::new();
    let _ = writeln!(
        output,
        "; Luau v{} / types v{} / {:?}",
        chunk.header.luau_version, chunk.header.types_version, chunk.flavor
    );
    let _ = writeln!(
        output,
        "; {} prototypes / main {}",
        chunk.prototypes.len(),
        chunk.main_proto
    );
    for (index, prototype) in chunk.prototypes.iter().enumerate() {
        let decoded = decode_prototype(&prototype.instructions, chunk.header.luau_version);
        let _ = writeln!(output);
        let _ = writeln!(
            output,
            "; proto {index} stack={} params={} upvalues={} encoding={}",
            prototype.max_stack_size,
            prototype.num_params,
            prototype.num_upvalues,
            if custom_opcodes {
                "custom"
            } else {
                decoded.encoding.name()
            }
        );
        for instruction in decoded.instructions {
            if let Some(opcode) = instruction.opcode {
                let _ = write!(
                    output,
                    "{:06}  {:<16} A={:<3} B={:<3} C={:<3} D={:<7}",
                    instruction.pc,
                    opcode.name(),
                    instruction.a(),
                    instruction.b(),
                    instruction.c(),
                    instruction.d()
                );
                if let Some(aux) = instruction.aux {
                    let _ = write!(output, " AUX={aux}");
                }
                let _ = writeln!(output);
            } else {
                let _ = writeln!(
                    output,
                    "{:06}  UNKNOWN          RAW=0x{:08x}",
                    instruction.pc, instruction.word
                );
            }
        }
    }
    output
}

fn trailing_diagnostics(trailing_bytes: usize) -> Vec<Diagnostic> {
    if trailing_bytes == 0 {
        Vec::new()
    } else {
        vec![Diagnostic::warning(
            DiagnosticCode::TrailingBytes,
            None,
            format!("{trailing_bytes} bytes remain after the declared main prototype"),
        )]
    }
}

struct LoweringContext<'a> {
    string_count: usize,
    function_count: usize,
    diagnostics: &'a mut Vec<Diagnostic>,
}

impl LoweringContext<'_> {
    fn function(&mut self, prototype: WirePrototype, index: usize) -> Function {
        let WirePrototype {
            max_stack_size,
            num_params,
            num_upvalues,
            is_vararg,
            flags,
            type_info,
            instructions,
            constants,
            child_protos,
            line_defined,
            debug_name,
            line_info,
            debug_info,
            feedback_slots: _,
            inline_cost: _,
            extension_bytes: _,
        } = prototype;
        let id = FunctionId(u32::try_from(index).unwrap_or(u32::MAX));
        let instruction_count = instructions.len();
        let constant_count = constants.len();
        let instructions = instructions
            .into_iter()
            .enumerate()
            .map(|(pc, word)| RawInstruction {
                pc: u32::try_from(pc).unwrap_or(u32::MAX),
                word,
                encoded_opcode: u8::try_from(word & 0xff).expect("masked opcode fits in u8"),
            })
            .collect();
        let constants = constants
            .into_iter()
            .map(|constant| self.constant(constant, id, constant_count))
            .collect();
        let children = child_protos
            .into_iter()
            .map(|child| self.child(child, id))
            .collect();
        let debug = self.debug(
            id,
            line_defined,
            debug_name,
            line_info,
            debug_info,
            instruction_count,
        );

        Function {
            id,
            signature: Signature {
                max_stack_size,
                parameter_count: num_params,
                upvalue_count: num_upvalues,
                is_variadic: is_vararg,
            },
            flags,
            type_info,
            instructions,
            constants,
            children,
            debug,
        }
    }

    fn child(&mut self, child: u32, function: FunctionId) -> FunctionId {
        if index_out_of_bounds(child, self.function_count) {
            self.diagnostics.push(Diagnostic::error(
                DiagnosticCode::InvalidChildReference,
                Some(function),
                format!(
                    "child prototype {child} is outside 0..{}",
                    self.function_count
                ),
            ));
        }
        FunctionId(child)
    }

    fn debug(
        &mut self,
        function: FunctionId,
        line_defined: u32,
        debug_name: u32,
        line_info: Option<WireLineInfo>,
        debug_info: WireDebugInfo,
        instruction_count: usize,
    ) -> FunctionDebug {
        let name = self.string_reference(debug_name, function, "debug name");
        let locals = debug_info
            .locals
            .into_iter()
            .map(|local| {
                if local.start_pc > local.end_pc
                    || usize::try_from(local.end_pc)
                        .ok()
                        .is_none_or(|end| end > instruction_count)
                {
                    self.diagnostics.push(Diagnostic::warning(
                        DiagnosticCode::InvalidDebugRange,
                        Some(function),
                        format!(
                            "debug local range {}..{} exceeds instruction count {instruction_count}",
                            local.start_pc, local.end_pc
                        ),
                    ));
                }
                LocalDebug {
                    name: self.string_reference(local.name, function, "local name"),
                    start_pc: local.start_pc,
                    end_pc: local.end_pc,
                    register: local.register,
                }
            })
            .collect();
        let upvalue_names = debug_info
            .upvalue_names
            .into_iter()
            .map(|name| self.string_reference(name, function, "upvalue name"))
            .collect();
        FunctionDebug {
            line_defined,
            name,
            line_info: line_info.map(|line| LineInfo {
                gap_log2: line.gap_log2,
                relative_deltas: line.relative_deltas,
                absolute_deltas: line.absolute_deltas,
            }),
            locals,
            upvalue_names,
        }
    }

    fn constant(
        &mut self,
        constant: WireConstant,
        function: FunctionId,
        constant_count: usize,
    ) -> ConstantValue {
        match constant {
            WireConstant::Nil => ConstantValue::Nil,
            WireConstant::Boolean(value) => ConstantValue::Boolean(value),
            WireConstant::NumberBits(bits) => ConstantValue::NumberBits(bits),
            WireConstant::StringRef(index) => ConstantValue::String(
                self.string_reference(index, function, "constant string")
                    .unwrap_or(StringId(u32::MAX)),
            ),
            WireConstant::Import(value) => ConstantValue::Import(value),
            WireConstant::Table(keys) => self.table(keys, function, constant_count),
            WireConstant::TableWithValues(entries) => {
                self.table_with_values(entries, function, constant_count)
            }
            WireConstant::Closure(index) => {
                if index_out_of_bounds(index, self.function_count) {
                    self.diagnostics.push(Diagnostic::error(
                        DiagnosticCode::InvalidClosureReference,
                        Some(function),
                        format!(
                            "closure prototype {index} is outside 0..{}",
                            self.function_count
                        ),
                    ));
                }
                ConstantValue::Closure(FunctionId(index))
            }
            WireConstant::VectorBits(bits) => ConstantValue::VectorBits(bits),
            WireConstant::VectorDoubleBits(bits) => ConstantValue::VectorDoubleBits(bits),
            WireConstant::Integer(value) => ConstantValue::Integer(value),
            WireConstant::ClassShape(shape) => {
                self.constant_reference(shape.class_name, function, constant_count);
                for member in &shape.members {
                    self.constant_reference(*member, function, constant_count);
                }
                ConstantValue::ClassShape(ClassShapeValue {
                    class_name_constant: shape.class_name,
                    property_count: shape.property_count,
                    method_count: shape.method_count,
                    member_constants: shape.members,
                })
            }
        }
    }

    fn table(
        &mut self,
        keys: Vec<u32>,
        function: FunctionId,
        constant_count: usize,
    ) -> ConstantValue {
        for key in &keys {
            self.constant_reference(*key, function, constant_count);
        }
        ConstantValue::Table(keys)
    }

    fn table_with_values(
        &mut self,
        entries: Vec<TableEntry>,
        function: FunctionId,
        constant_count: usize,
    ) -> ConstantValue {
        ConstantValue::TableWithValues(
            entries
                .into_iter()
                .map(|TableEntry { key, value }| {
                    self.constant_reference(key, function, constant_count);
                    if let Some(value) = value {
                        self.constant_reference(value, function, constant_count);
                    }
                    TableTemplateEntry {
                        key_constant: key,
                        value_constant: value,
                    }
                })
                .collect(),
        )
    }

    fn constant_reference(&mut self, index: u32, function: FunctionId, count: usize) {
        if index_out_of_bounds(index, count) {
            self.diagnostics.push(Diagnostic::error(
                DiagnosticCode::InvalidConstantReference,
                Some(function),
                format!("constant index {index} is outside 0..{count}"),
            ));
        }
    }

    fn string_reference(
        &mut self,
        wire_index: u32,
        function: FunctionId,
        field: &str,
    ) -> Option<StringId> {
        if wire_index == 0 {
            return None;
        }
        let zero_based = wire_index - 1;
        if index_out_of_bounds(zero_based, self.string_count) {
            self.diagnostics.push(Diagnostic::error(
                DiagnosticCode::InvalidStringReference,
                Some(function),
                format!(
                    "{field} index {wire_index} is outside 1..={}",
                    self.string_count
                ),
            ));
        }
        Some(StringId(zero_based))
    }
}

fn index_out_of_bounds(index: u32, count: usize) -> bool {
    usize::try_from(index)
        .ok()
        .is_none_or(|value| value >= count)
}

fn build_quality(diagnostics: &[Diagnostic], trailing_bytes: usize) -> QualityReport {
    let errors = diagnostics
        .iter()
        .filter(|item| item.severity == Severity::Error)
        .count();
    let warnings = diagnostics
        .iter()
        .filter(|item| item.severity == Severity::Warning)
        .count();
    let score = 10_000_i64
        .saturating_sub(
            i64::try_from(errors)
                .unwrap_or(i64::MAX)
                .saturating_mul(1_000),
        )
        .saturating_sub(
            i64::try_from(warnings)
                .unwrap_or(i64::MAX)
                .saturating_mul(100),
        )
        .saturating_sub(i64::try_from(trailing_bytes).unwrap_or(i64::MAX));
    QualityReport {
        score,
        errors,
        warnings,
        clean: errors == 0 && warnings == 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lua51_return_42_chunk() -> Vec<u8> {
        let mut bytes = b"\x1bLua\x51\0\x01\x04\x08\x04\x08\0".to_vec();
        bytes.extend(0_u64.to_le_bytes());
        bytes.extend(0_u32.to_le_bytes());
        bytes.extend(0_u32.to_le_bytes());
        bytes.extend([0, 0, 2, 2]);
        bytes.extend(2_u32.to_le_bytes());
        bytes.extend(1_u32.to_le_bytes());
        bytes.extend((30_u32 | (2_u32 << 23)).to_le_bytes());
        bytes.extend(1_u32.to_le_bytes());
        bytes.push(3);
        bytes.extend(42_f64.to_le_bytes());
        bytes.extend(0_u32.to_le_bytes());
        bytes.extend(0_u32.to_le_bytes());
        bytes.extend(0_u32.to_le_bytes());
        bytes.extend(0_u32.to_le_bytes());
        bytes
    }

    fn v10_class_chunk(member_reference: u8) -> Vec<u8> {
        let mut bytes = vec![10, 3, 2];
        bytes.extend([6, b'W', b'i', b'd', b'g', b'e', b't']);
        bytes.extend([3, b'r', b'u', b'n']);
        bytes.push(0);
        bytes.extend([1, 2, 0, 0, 0, 0, 0, 3]);
        let new_class_member = u32::from(Opcode::NewClassMember.number()) | (1_u32 << 24);
        let return_value = u32::from(Opcode::Return.number()) | (2_u32 << 16);
        bytes.extend(new_class_member.to_le_bytes());
        bytes.extend(1_u32.to_le_bytes());
        bytes.extend(return_value.to_le_bytes());
        bytes.extend([
            3,
            3,
            1,
            3,
            2,
            10,
            0,
            0,
            1,
            member_reference,
            0,
            0,
            0,
            0,
            0,
            0,
        ]);
        bytes
    }

    fn v12_empty_chunk() -> Vec<u8> {
        let mut prototype = vec![0, 0, 0, 0, 0, 0, 2];
        prototype.extend(u32::from(Opcode::PrepVarargs.number()).to_le_bytes());
        prototype.extend((u32::from(Opcode::Return.number()) | (1 << 16)).to_le_bytes());
        prototype.extend([0, 0, 0, 0, 0, 0, 0]);
        let mut bytes = vec![12, 3, 0, 0, 1, prototype.len() as u8];
        bytes.extend(prototype);
        bytes.push(0);
        bytes
    }

    fn v12_cmp_proto_chunk(proto_id: u32) -> Vec<u8> {
        let mut prototype = vec![1, 0, 0, 0, 0, 0, 4];
        prototype.extend(u32::from(Opcode::PrepVarargs.number()).to_le_bytes());
        prototype.extend((u32::from(Opcode::CmpProto.number()) | (1 << 16)).to_le_bytes());
        prototype.extend(proto_id.to_le_bytes());
        prototype.extend((u32::from(Opcode::Return.number()) | (1 << 16)).to_le_bytes());
        prototype.extend([0, 0, 0, 0, 0, 0, 0]);
        let mut bytes = vec![12, 3, 0, 0, 1, prototype.len() as u8];
        bytes.extend(prototype);
        bytes.push(0);
        bytes
    }

    #[test]
    fn analyzes_minimal_chunk() {
        let bytes = [3, 0, 1, 1, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        let analysis = Engine::default().analyze(&bytes).unwrap();
        assert_eq!(analysis.inventory.proto_count, 1);
        assert_eq!(analysis.inventory.instruction_counts, vec![1]);
        assert!(analysis.quality.clean);
    }

    #[test]
    fn decompiles_empty_v12_main_without_inventing_statements() {
        let bytes = v12_empty_chunk();
        let analysis = Engine::default().analyze(&bytes).unwrap();
        assert_eq!(analysis.inventory.luau_version, 12);
        assert!(analysis.quality.clean);
        let decompiled = Engine::default().decompile(&bytes).unwrap();
        assert_eq!(decompiled.source.lines().last(), Some("return"));
    }

    #[test]
    fn validates_v12_cmp_proto_ids() {
        let valid = Engine::default().analyze(&v12_cmp_proto_chunk(0)).unwrap();
        assert!(valid.quality.clean);
        let invalid = Engine::default().analyze(&v12_cmp_proto_chunk(1)).unwrap();
        assert_eq!(invalid.quality.errors, 1);
        assert!(invalid.module.diagnostics.iter().any(|diagnostic| {
            diagnostic.code == DiagnosticCode::InvalidOpcodeReference
                && diagnostic.message.contains("CMPPROTO")
        }));
    }

    #[test]
    fn routes_lua51_through_the_lua_pipeline() {
        let bytes = lua51_return_42_chunk();
        let analysis = Engine::default().analyze(&bytes).unwrap();
        assert_eq!(analysis.inventory.wire_flavor, "lua51");
        assert_eq!(analysis.inventory.proto_count, 1);
        assert!(analysis.quality.clean);

        let decompiled = Engine::default().decompile(&bytes).unwrap();
        assert_eq!(decompiled.strategy, DecompileStrategy::Lua51);
        assert!(decompiled.source.contains("return 42"));
        assert!(
            Engine::default()
                .disassemble(&bytes)
                .unwrap()
                .contains("PUC Lua 5.1")
        );
    }

    #[test]
    fn validates_v10_class_shapes_without_fabricating_source() {
        let bytes = v10_class_chunk(1);
        let analysis = Engine::default().analyze(&bytes).unwrap();
        assert_eq!(analysis.inventory.luau_version, 10);
        assert!(analysis.quality.clean);
        let error = Engine::default().decompile(&bytes).unwrap_err();
        assert!(matches!(
            error,
            EngineError::UnsupportedDecompilation { .. }
        ));
        assert!(error.to_string().contains("class-shape"));
    }

    #[test]
    fn rejects_v10_class_shape_non_string_references() {
        let analysis = Engine::default().analyze(&v10_class_chunk(2)).unwrap();
        assert_eq!(analysis.quality.errors, 1);
        assert!(analysis.module.diagnostics.iter().any(|diagnostic| {
            diagnostic.code == DiagnosticCode::InvalidConstantReference
                && diagnostic
                    .message
                    .contains("must point to a string constant")
        }));
    }
}
