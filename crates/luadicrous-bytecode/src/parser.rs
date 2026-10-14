use crate::{
    ChunkHeader, ClassShape, Constant, Cursor, DebugInfo, FeedbackSlot, LineInfo, LocalDebug, ParseError,
    ParseLimits, ParseOptions, ParsedBytecode, ParsedChunk, Prototype, StringEntry, TableEntry,
    WireFlavor,
};

const DYNAMIC_TABLE_VALUE: u32 = u32::MAX;

pub fn parse(bytes: &[u8], options: &ParseOptions) -> Result<ParsedBytecode, ParseError> {
    if bytes.len() > options.limits.max_input_bytes {
        return Err(ParseError::LimitExceeded {
            field: "input bytes",
            value: bytes.len(),
            limit: options.limits.max_input_bytes,
            offset: 0,
        });
    }

    if looks_like_luau_source(bytes) {
        return Err(ParseError::SourceText);
    }

    if bytes.first().copied() == Some(0) {
        return parse_compilation_error(bytes, &options.limits);
    }

    match options.flavor {
        WireFlavor::Standard => parse_chunk(bytes, options, WireFlavor::Standard),
        WireFlavor::Roblox => parse_chunk(bytes, options, WireFlavor::Roblox),
        WireFlavor::Auto => {
            let standard = parse_chunk(bytes, options, WireFlavor::Standard);
            let roblox = parse_chunk(bytes, options, WireFlavor::Roblox);
            match (standard, roblox) {
                (Ok(left), Ok(right)) => {
                    let left_trailing = trailing_len(&left);
                    let right_trailing = trailing_len(&right);
                    if left_trailing <= right_trailing {
                        Ok(left)
                    } else {
                        Ok(right)
                    }
                }
                (Ok(value), Err(_)) | (Err(_), Ok(value)) => Ok(value),
                (Err(standard_error), Err(roblox_error)) => Err(ParseError::NoFlavorMatched {
                    standard: standard_error.to_string(),
                    roblox: roblox_error.to_string(),
                }),
            }
        }
    }
}

#[must_use]
pub fn looks_like_luau_source(data: &[u8]) -> bool {
    let sample = &data[..data.len().min(16_384)];
    if sample.is_empty() || sample.contains(&0) {
        return false;
    }
    let Ok(text) = std::str::from_utf8(sample) else {
        return false;
    };
    let character_count = text.chars().count();
    let printable_count = text
        .chars()
        .filter(|character| !character.is_control() || matches!(character, '\r' | '\n' | '\t'))
        .count();
    if printable_count * 100 < character_count * 96 {
        return false;
    }
    [
        "local ",
        "function ",
        "return ",
        "game:GetService",
        "getfenv(",
        "loadstring(",
        "--[[",
    ]
    .into_iter()
    .filter(|marker| text.contains(marker))
    .count()
        >= 2
}

fn trailing_len(bytecode: &ParsedBytecode) -> usize {
    match bytecode {
        ParsedBytecode::Chunk(chunk) => chunk.trailing_bytes.len(),
        ParsedBytecode::CompilationError(_) => 0,
    }
}

fn parse_compilation_error(
    bytes: &[u8],
    limits: &ParseLimits,
) -> Result<ParsedBytecode, ParseError> {
    let mut cursor = Cursor::new(bytes);
    let version = cursor.read_u8()?;
    debug_assert_eq!(version, 0);
    let length = usize_from_u32(cursor.read_var_u32()?, cursor.position())?;
    check_limit(
        "compilation error bytes",
        length,
        limits.max_string_bytes,
        cursor.position(),
    )?;
    let message = String::from_utf8_lossy(cursor.read_exact(length)?).into_owned();
    Ok(ParsedBytecode::CompilationError(message))
}

fn parse_chunk(
    bytes: &[u8],
    options: &ParseOptions,
    flavor: WireFlavor,
) -> Result<ParsedBytecode, ParseError> {
    let mut cursor = Cursor::new(bytes);
    let luau_version = cursor.read_u8()?;
    if !(options.min_version..=options.max_version).contains(&luau_version) {
        return Err(ParseError::UnsupportedVersion {
            version: luau_version,
            min: options.min_version,
            max: options.max_version,
        });
    }
    let types_version = if luau_version >= 4 {
        cursor.read_u8()?
    } else {
        0
    };

    let string_count = read_count(&mut cursor, "string count", options.limits.max_strings)?;
    let mut strings = Vec::with_capacity(string_count);
    let mut total_string_bytes = 0_usize;
    for _ in 0..string_count {
        let length = read_count(
            &mut cursor,
            "string length",
            options.limits.max_string_bytes,
        )?;
        total_string_bytes =
            total_string_bytes
                .checked_add(length)
                .ok_or(ParseError::LengthOverflow {
                    offset: cursor.position(),
                    length,
                })?;
        check_limit(
            "total string bytes",
            total_string_bytes,
            options.limits.max_total_string_bytes,
            cursor.position(),
        )?;
        strings.push(StringEntry::new(cursor.read_exact(length)?.to_vec()));
    }

    let mut types_blob = Vec::new();
    if luau_version >= 4 && types_version >= 3 {
        loop {
            let byte = cursor.read_u8()?;
            if byte == 0 {
                break;
            }
            check_limit(
                "types blob bytes",
                types_blob.len() + 1,
                options.limits.max_type_bytes,
                cursor.position(),
            )?;
            types_blob.push(byte);
        }
    }

    let prototype_count = read_count(
        &mut cursor,
        "prototype count",
        options.limits.max_prototypes,
    )?;
    let mut prototypes = Vec::with_capacity(prototype_count);
    for _ in 0..prototype_count {
        prototypes.push(parse_prototype(
            &mut cursor,
            luau_version,
            types_version,
            flavor,
            &options.limits,
        )?);
    }

    let main_proto = cursor.read_var_u32()?;
    if usize_from_u32(main_proto, cursor.position())? >= prototype_count {
        return Err(ParseError::InvalidReference {
            field: "main prototype",
            index: main_proto,
            count: prototype_count,
            offset: cursor.position(),
        });
    }

    let consumed_bytes = cursor.position();
    let trailing_bytes = cursor.remaining().to_vec();
    Ok(ParsedBytecode::Chunk(ParsedChunk {
        header: ChunkHeader {
            luau_version,
            types_version,
            types_blob,
        },
        flavor,
        strings,
        prototypes,
        main_proto,
        consumed_bytes,
        trailing_bytes,
    }))
}

fn parse_prototype(
    cursor: &mut Cursor<'_>,
    luau_version: u8,
    types_version: u8,
    flavor: WireFlavor,
    limits: &ParseLimits,
) -> Result<Prototype, ParseError> {
    let prototype_end = if luau_version >= 12 {
        let size = read_count(cursor, "prototype bytes", limits.max_input_bytes)?;
        let start = cursor.position();
        if size > cursor.remaining().len() {
            return Err(ParseError::UnexpectedEof {
                offset: start,
                needed: size,
                remaining: cursor.remaining().len(),
            });
        }
        Some(start.checked_add(size).ok_or(ParseError::LengthOverflow {
            offset: start,
            length: size,
        })?)
    } else {
        None
    };
    let start = cursor.position();
    let max_stack_size = cursor.read_u8()?;
    let num_params = cursor.read_u8()?;
    let num_upvalues = cursor.read_u8()?;
    let vararg = cursor.read_u8()?;
    if vararg > 1 {
        return Err(ParseError::InvalidFlag {
            field: "is_vararg",
            value: vararg,
            offset: start + 3,
        });
    }
    if num_params > max_stack_size {
        return Err(ParseError::InvalidPrototypeHeader {
            offset: start,
            reason: format!("{num_params} parameters exceed stack size {max_stack_size}"),
        });
    }
    let flags = if luau_version >= 4 {
        cursor.read_u8()?
    } else {
        0
    };
    let type_info = if luau_version >= 4 && types_version != 0 {
        let count = read_count(cursor, "prototype type bytes", limits.max_type_bytes)?;
        cursor.read_exact(count)?.to_vec()
    } else {
        Vec::new()
    };

    let instruction_count = read_count(
        cursor,
        "instruction count",
        limits.max_instructions_per_proto,
    )?;
    let mut instructions = Vec::with_capacity(instruction_count);
    for _ in 0..instruction_count {
        instructions.push(cursor.read_u32()?);
    }

    let constant_count = read_count(cursor, "constant count", limits.max_constants_per_proto)?;
    let mut constants = Vec::with_capacity(constant_count);
    for _ in 0..constant_count {
        constants.push(parse_constant(cursor, luau_version, flavor, limits)?);
    }

    let child_count = read_count(cursor, "child prototype count", limits.max_prototypes)?;
    let mut child_protos = Vec::with_capacity(child_count);
    for _ in 0..child_count {
        child_protos.push(cursor.read_var_u32()?);
    }

    let line_defined = cursor.read_var_u32()?;
    let debug_name = cursor.read_var_u32()?;
    let (line_info, debug_info) = parse_debug_footer(cursor, instruction_count, limits)?;
    let mut feedback_slots = Vec::new();
    if luau_version >= 11 {
        let count = read_count(cursor, "feedback slot count", limits.max_instructions_per_proto)?;
        feedback_slots.reserve(count);
        for _ in 0..count {
            let kind = cursor.read_u8()?;
            if kind != 0 {
                return Err(ParseError::InvalidFlag {
                    field: "feedback slot kind",
                    value: kind,
                    offset: cursor.position() - 1,
                });
            }
            let pc = cursor.read_var_u32()?;
            if usize_from_u32(pc, cursor.position())? >= instruction_count {
                return Err(ParseError::InvalidReference {
                    field: "feedback slot pc",
                    index: pc,
                    count: instruction_count,
                    offset: cursor.position(),
                });
            }
            feedback_slots.push(FeedbackSlot { kind, pc });
        }
    }
    let inline_cost = if luau_version >= 12 && flags & 0x08 != 0 {
        Some(cursor.read_var_u64()?)
    } else {
        None
    };
    let extension_bytes = if let Some(end) = prototype_end {
        if cursor.position() > end {
            return Err(ParseError::InvalidPrototypeHeader {
                offset: start,
                reason: format!("fields exceed declared prototype end {end}"),
            });
        }
        cursor.read_exact(end - cursor.position())?.to_vec()
    } else {
        Vec::new()
    };

    Ok(Prototype {
        max_stack_size,
        num_params,
        num_upvalues,
        is_vararg: vararg != 0,
        flags,
        type_info,
        instructions,
        constants,
        child_protos,
        line_defined,
        debug_name,
        line_info,
        debug_info,
        feedback_slots,
        inline_cost,
        extension_bytes,
    })
}

fn parse_debug_footer(
    cursor: &mut Cursor<'_>,
    instruction_count: usize,
    limits: &ParseLimits,
) -> Result<(Option<LineInfo>, DebugInfo), ParseError> {
    let has_line_info = cursor.read_u8()?;
    if has_line_info > 1 {
        return Err(ParseError::InvalidFlag {
            field: "has_line_info",
            value: has_line_info,
            offset: cursor.position() - 1,
        });
    }
    let line_info = if has_line_info == 1 {
        let gap_log2 = cursor.read_u8()?;
        if gap_log2 > 31 {
            return Err(ParseError::InvalidFlag {
                field: "line_gap_log2",
                value: gap_log2,
                offset: cursor.position() - 1,
            });
        }
        let relative_deltas = cursor.read_exact(instruction_count)?.to_vec();
        let interval_count = if instruction_count == 0 {
            0
        } else {
            ((instruction_count - 1) >> gap_log2) + 1
        };
        let mut absolute_deltas = Vec::with_capacity(interval_count);
        for _ in 0..interval_count {
            absolute_deltas.push(cursor.read_i32()?);
        }
        Some(LineInfo {
            gap_log2,
            relative_deltas,
            absolute_deltas,
        })
    } else {
        None
    };

    let debug_mode = cursor.read_u8()?;
    if debug_mode > 2 {
        return Err(ParseError::InvalidFlag {
            field: "debug mode",
            value: debug_mode,
            offset: cursor.position() - 1,
        });
    }
    let mut debug_info = DebugInfo {
        mode: debug_mode,
        ..DebugInfo::default()
    };
    if debug_mode == 1 {
        let local_count = read_count(cursor, "debug local count", limits.max_debug_entries)?;
        debug_info.locals.reserve(local_count);
        for _ in 0..local_count {
            debug_info.locals.push(LocalDebug {
                name: cursor.read_var_u32()?,
                start_pc: cursor.read_var_u32()?,
                end_pc: cursor.read_var_u32()?,
                register: cursor.read_u8()?,
            });
        }
        let upvalue_count = read_count(cursor, "debug upvalue count", limits.max_debug_entries)?;
        debug_info.upvalue_names.reserve(upvalue_count);
        for _ in 0..upvalue_count {
            debug_info.upvalue_names.push(cursor.read_var_u32()?);
        }
    }

    Ok((line_info, debug_info))
}

fn parse_constant(
    cursor: &mut Cursor<'_>,
    luau_version: u8,
    flavor: WireFlavor,
    limits: &ParseLimits,
) -> Result<Constant, ParseError> {
    let offset = cursor.position();
    let tag = cursor.read_u8()?;
    match flavor {
        WireFlavor::Standard => parse_standard_constant(cursor, luau_version, tag, offset, limits),
        WireFlavor::Roblox => parse_roblox_constant(cursor, luau_version, tag, offset, limits),
        WireFlavor::Auto => unreachable!("auto flavor is resolved before constant parsing"),
    }
}

fn parse_standard_constant(
    cursor: &mut Cursor<'_>,
    luau_version: u8,
    tag: u8,
    offset: usize,
    limits: &ParseLimits,
) -> Result<Constant, ParseError> {
    match tag {
        0 => Ok(Constant::Nil),
        1 => Ok(Constant::Boolean(cursor.read_u8()? != 0)),
        2 => Ok(Constant::NumberBits(cursor.read_u64()?)),
        3 => Ok(Constant::StringRef(cursor.read_var_u32()?)),
        4 => Ok(Constant::Import(cursor.read_u32()?)),
        5 => Ok(Constant::Table(read_varint_vector(
            cursor,
            "table key count",
            limits.max_constants_per_proto,
        )?)),
        6 => Ok(Constant::Closure(cursor.read_var_u32()?)),
        7 => Ok(Constant::VectorBits(read_vector_bits(cursor)?)),
        8 => {
            let count = read_count(cursor, "table entry count", limits.max_constants_per_proto)?;
            let mut entries = Vec::with_capacity(count);
            for _ in 0..count {
                let key = cursor.read_var_u32()?;
                let value = cursor.read_u32()?;
                entries.push(TableEntry {
                    key,
                    value: (value != DYNAMIC_TABLE_VALUE).then_some(value),
                });
            }
            Ok(Constant::TableWithValues(entries))
        }
        9 => {
            let sign = cursor.read_u8()?;
            if sign > 1 {
                return Err(ParseError::InvalidFlag {
                    field: "integer sign",
                    value: sign,
                    offset: cursor.position() - 1,
                });
            }
            let magnitude_offset = cursor.position();
            let magnitude = cursor.read_var_u64()?;
            let value = if sign == 1 && magnitude == 1_u64 << 63 {
                i64::MIN
            } else {
                let magnitude =
                    i64::try_from(magnitude).map_err(|_| ParseError::IntegerMagnitudeOverflow {
                        magnitude,
                        offset: magnitude_offset,
                    })?;
                if sign == 1 { -magnitude } else { magnitude }
            };
            Ok(Constant::Integer(value))
        }
        10 if luau_version >= 10 => Ok(Constant::ClassShape(read_class_shape(cursor, limits)?)),
        11 if luau_version >= 13 => Ok(Constant::VectorDoubleBits([
            cursor.read_u64()?,
            cursor.read_u64()?,
            cursor.read_u64()?,
            cursor.read_u64()?,
        ])),
        0x80 if luau_version >= 9 => Ok(Constant::Nil),
        _ => Err(ParseError::UnsupportedConstantTag {
            tag,
            flavor: WireFlavor::Standard,
            offset,
        }),
    }
}

fn read_class_shape(
    cursor: &mut Cursor<'_>,
    limits: &ParseLimits,
) -> Result<ClassShape, ParseError> {
    let class_name = cursor.read_var_u32()?;
    let property_count = cursor.read_var_u32()?;
    let method_count = cursor.read_var_u32()?;
    let property_count_usize = usize_from_u32(property_count, cursor.position())?;
    let method_count_usize = usize_from_u32(method_count, cursor.position())?;
    let member_count =
        property_count_usize
            .checked_add(method_count_usize)
            .ok_or(ParseError::LengthOverflow {
                offset: cursor.position(),
                length: usize::MAX,
            })?;
    check_limit(
        "class member count",
        member_count,
        limits.max_constants_per_proto,
        cursor.position(),
    )?;
    let mut members = Vec::with_capacity(member_count);
    for _ in 0..member_count {
        members.push(cursor.read_var_u32()?);
    }
    Ok(ClassShape {
        class_name,
        property_count,
        method_count,
        members,
    })
}

fn parse_roblox_constant(
    cursor: &mut Cursor<'_>,
    luau_version: u8,
    tag: u8,
    offset: usize,
    limits: &ParseLimits,
) -> Result<Constant, ParseError> {
    match tag {
        0 => Ok(Constant::Nil),
        1 => Ok(Constant::Boolean(false)),
        2 => Ok(Constant::Boolean(true)),
        3 => Ok(Constant::NumberBits(cursor.read_u64()?)),
        4 => Ok(Constant::StringRef(cursor.read_var_u32()?)),
        5 => Ok(Constant::Import(cursor.read_u32()?)),
        6 => Ok(Constant::Table(read_varint_vector(
            cursor,
            "table key count",
            limits.max_constants_per_proto,
        )?)),
        7 => Ok(Constant::Closure(cursor.read_var_u32()?)),
        8 => Ok(Constant::VectorBits(read_vector_bits(cursor)?)),
        0x80 if luau_version >= 9 => Ok(Constant::Nil),
        _ => Err(ParseError::UnsupportedConstantTag {
            tag,
            flavor: WireFlavor::Roblox,
            offset,
        }),
    }
}

fn read_vector_bits(cursor: &mut Cursor<'_>) -> Result<[u32; 4], ParseError> {
    Ok([
        cursor.read_u32()?,
        cursor.read_u32()?,
        cursor.read_u32()?,
        cursor.read_u32()?,
    ])
}

fn read_varint_vector(
    cursor: &mut Cursor<'_>,
    field: &'static str,
    limit: usize,
) -> Result<Vec<u32>, ParseError> {
    let count = read_count(cursor, field, limit)?;
    let mut values = Vec::with_capacity(count);
    for _ in 0..count {
        values.push(cursor.read_var_u32()?);
    }
    Ok(values)
}

fn read_count(
    cursor: &mut Cursor<'_>,
    field: &'static str,
    limit: usize,
) -> Result<usize, ParseError> {
    let offset = cursor.position();
    let count = usize_from_u32(cursor.read_var_u32()?, offset)?;
    check_limit(field, count, limit, offset)?;
    Ok(count)
}

fn usize_from_u32(value: u32, offset: usize) -> Result<usize, ParseError> {
    usize::try_from(value).map_err(|_| ParseError::IntegerConversion { value, offset })
}

fn check_limit(
    field: &'static str,
    value: usize,
    limit: usize,
    offset: usize,
) -> Result<(), ParseError> {
    if value > limit {
        return Err(ParseError::LimitExceeded {
            field,
            value,
            limit,
            offset,
        });
    }
    Ok(())
}
