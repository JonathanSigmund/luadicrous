use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub struct FunctionId(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub struct StringId(pub u32);

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Module {
    pub format: FormatMetadata,
    pub strings: Vec<IrString>,
    pub functions: Vec<Function>,
    pub entry: FunctionId,
    pub diagnostics: Vec<Diagnostic>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FormatMetadata {
    pub luau_version: u8,
    pub types_version: u8,
    pub wire_flavor: String,
    pub consumed_bytes: usize,
    pub trailing_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IrString {
    pub bytes: Vec<u8>,
    pub text_lossy: String,
    pub valid_utf8: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Function {
    pub id: FunctionId,
    pub signature: Signature,
    pub flags: u8,
    pub type_info: Vec<u8>,
    pub instructions: Vec<RawInstruction>,
    pub constants: Vec<ConstantValue>,
    pub children: Vec<FunctionId>,
    pub debug: FunctionDebug,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Signature {
    pub max_stack_size: u8,
    pub parameter_count: u8,
    pub upvalue_count: u8,
    pub is_variadic: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct RawInstruction {
    pub pc: u32,
    pub word: u32,
    pub encoded_opcode: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum ConstantValue {
    Nil,
    Boolean(bool),
    NumberBits(u64),
    String(StringId),
    Import(u32),
    Table(Vec<u32>),
    TableWithValues(Vec<TableTemplateEntry>),
    Closure(FunctionId),
    VectorBits([u32; 4]),
    VectorDoubleBits([u64; 4]),
    Integer(i64),
    ClassShape(ClassShapeValue),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TableTemplateEntry {
    pub key_constant: u32,
    pub value_constant: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ClassShapeValue {
    pub class_name_constant: u32,
    pub property_count: u32,
    pub method_count: u32,
    pub member_constants: Vec<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct FunctionDebug {
    pub line_defined: u32,
    pub name: Option<StringId>,
    pub line_info: Option<LineInfo>,
    pub locals: Vec<LocalDebug>,
    pub upvalue_names: Vec<Option<StringId>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LineInfo {
    pub gap_log2: u8,
    pub relative_deltas: Vec<u8>,
    pub absolute_deltas: Vec<i32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LocalDebug {
    pub name: Option<StringId>,
    pub start_pc: u32,
    pub end_pc: u32,
    pub register: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticCode {
    TrailingBytes,
    InvalidMain,
    UnknownOpcode,
    TruncatedAuxWord,
    InvalidJumpTarget,
    InvalidOpcodeReference,
    InvalidStringReference,
    InvalidChildReference,
    InvalidClosureReference,
    InvalidConstantReference,
    InvalidDebugRange,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Diagnostic {
    pub severity: Severity,
    pub code: DiagnosticCode,
    pub function: Option<FunctionId>,
    pub message: String,
}

impl Diagnostic {
    pub fn warning(
        code: DiagnosticCode,
        function: Option<FunctionId>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            severity: Severity::Warning,
            code,
            function,
            message: message.into(),
        }
    }

    pub fn error(
        code: DiagnosticCode,
        function: Option<FunctionId>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            severity: Severity::Error,
            code,
            function,
            message: message.into(),
        }
    }
}
