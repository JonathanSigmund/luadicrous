use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WireFlavor {
    Auto,
    Standard,
    Roblox,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StringEntry {
    pub bytes: Vec<u8>,
    pub text_lossy: String,
    pub valid_utf8: bool,
}

impl StringEntry {
    #[must_use]
    pub fn new(bytes: Vec<u8>) -> Self {
        let valid_utf8 = std::str::from_utf8(&bytes).is_ok();
        let text_lossy = String::from_utf8_lossy(&bytes).into_owned();
        Self {
            bytes,
            text_lossy,
            valid_utf8,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ChunkHeader {
    pub luau_version: u8,
    pub types_version: u8,
    pub types_blob: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ParsedChunk {
    pub header: ChunkHeader,
    pub flavor: WireFlavor,
    pub strings: Vec<StringEntry>,
    pub prototypes: Vec<Prototype>,
    pub main_proto: u32,
    pub consumed_bytes: usize,
    pub trailing_bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum ParsedBytecode {
    Chunk(ParsedChunk),
    CompilationError(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Prototype {
    pub max_stack_size: u8,
    pub num_params: u8,
    pub num_upvalues: u8,
    pub is_vararg: bool,
    pub flags: u8,
    pub type_info: Vec<u8>,
    pub instructions: Vec<u32>,
    pub constants: Vec<Constant>,
    pub child_protos: Vec<u32>,
    pub line_defined: u32,
    pub debug_name: u32,
    pub line_info: Option<LineInfo>,
    pub debug_info: DebugInfo,
    pub feedback_slots: Vec<FeedbackSlot>,
    pub inline_cost: Option<u64>,
    pub extension_bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FeedbackSlot {
    pub kind: u8,
    pub pc: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LineInfo {
    pub gap_log2: u8,
    pub relative_deltas: Vec<u8>,
    pub absolute_deltas: Vec<i32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct DebugInfo {
    pub mode: u8,
    pub locals: Vec<LocalDebug>,
    pub upvalue_names: Vec<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LocalDebug {
    pub name: u32,
    pub start_pc: u32,
    pub end_pc: u32,
    pub register: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TableEntry {
    pub key: u32,
    pub value: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ClassShape {
    pub class_name: u32,
    pub property_count: u32,
    pub method_count: u32,
    pub members: Vec<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum Constant {
    Nil,
    Boolean(bool),
    NumberBits(u64),
    StringRef(u32),
    Import(u32),
    Table(Vec<u32>),
    TableWithValues(Vec<TableEntry>),
    Closure(u32),
    VectorBits([u32; 4]),
    VectorDoubleBits([u64; 4]),
    Integer(i64),
    ClassShape(ClassShape),
}
