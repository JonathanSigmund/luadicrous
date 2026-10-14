use std::{fmt::Write as _, panic::AssertUnwindSafe};

use anyhow::{Context, Result, anyhow, bail};
use ast::{
    Traverse, local_declarations::LocalDeclarer, name_locals::name_locals,
    replace_locals::replace_locals,
};
use by_address::ByAddress;
use cfg::ssa::{
    self,
    structuring::{structure_conditionals, structure_jumps, structure_method_calls},
};
use indexmap::IndexMap;
use lua51_deserializer::{Function as BytecodeFunction, Value, chunk::Chunk};
use parking_lot::Mutex;
use petgraph::algo::dominators::simple_fast;
use rustc_hash::FxHashMap;
use triomphe::Arc;

use crate::lifter::Lifter;

mod lifter;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lua51Inventory {
    pub consumed_bytes: usize,
    pub trailing_bytes: usize,
    pub proto_count: usize,
    pub string_constant_count: usize,
    pub instruction_counts: Vec<usize>,
    pub constant_counts: Vec<usize>,
}

#[derive(Debug, Clone)]
pub struct Lua51Decompilation {
    pub source: String,
    pub inventory: Lua51Inventory,
}

pub fn inspect(bytes: &[u8]) -> Result<Lua51Inventory> {
    let (trailing, chunk) = parse(bytes)?;
    Ok(build_inventory(
        &chunk.function,
        bytes.len() - trailing.len(),
        trailing.len(),
    ))
}

pub fn disassemble(bytes: &[u8]) -> Result<String> {
    let (trailing, chunk) = parse(bytes)?;
    let mut output = String::new();
    writeln!(output, "; PUC Lua 5.1 bytecode")?;
    writeln!(
        output,
        "; consumed={} trailing={}",
        bytes.len() - trailing.len(),
        trailing.len()
    )?;
    write_function(&mut output, &chunk.function, 0, 0)?;
    Ok(output)
}

pub fn decompile(bytes: &[u8]) -> Result<Lua51Decompilation> {
    match std::panic::catch_unwind(AssertUnwindSafe(|| decompile_inner(bytes))) {
        Ok(result) => result,
        Err(_) => {
            bail!("Lua 5.1 CFG recovery rejected a malformed or unsupported instruction shape")
        }
    }
}

fn decompile_inner(bytes: &[u8]) -> Result<Lua51Decompilation> {
    let (trailing, chunk) = parse(bytes)?;
    let inventory = build_inventory(
        &chunk.function,
        bytes.len() - trailing.len(),
        trailing.len(),
    );
    let mut lifted = Vec::new();
    let (function, upvalues) = Lifter::lift(&chunk.function, &mut lifted);
    lifted.push((Arc::<Mutex<_>>::default(), function, upvalues));
    lifted.reverse();

    let (main, ..) = lifted
        .first()
        .cloned()
        .context("Lua 5.1 chunk has no main function")?;
    let mut upvalues = lifted
        .into_iter()
        .map(|(ast_function, mut function, upvalues_in)| {
            let (local_count, local_groups, upvalue_in_groups, upvalue_passed_groups) =
                cfg::ssa::construct(&mut function, &upvalues_in);
            let upvalue_to_group = upvalue_in_groups
                .into_iter()
                .chain(
                    upvalue_passed_groups
                        .into_iter()
                        .map(|groups| (ast::RcLocal::default(), groups)),
                )
                .flat_map(|(local, groups)| {
                    groups
                        .into_iter()
                        .map(move |upvalue| (upvalue, local.clone()))
                })
                .collect::<IndexMap<_, _>>();
            let local_to_group = local_groups
                .into_iter()
                .enumerate()
                .flat_map(|(index, group)| group.into_iter().map(move |local| (local, index)))
                .collect::<FxHashMap<_, _>>();

            let mut changed = true;
            while changed {
                changed = false;
                let dominators = simple_fast(function.graph(), function.entry().expect("entry"));
                changed |= structure_jumps(&mut function, &dominators);
                ssa::inline::inline(&mut function, &local_to_group, &upvalue_to_group);
                if structure_conditionals(&mut function) || structure_method_calls(&mut function) {
                    changed = true;
                }
                let mut local_map = FxHashMap::default();
                if ssa::construct::remove_unnecessary_params(&mut function, &mut local_map) {
                    changed = true;
                }
                ssa::construct::apply_local_map(&mut function, local_map);
            }
            ssa::Destructor::new(
                &mut function,
                upvalue_to_group,
                upvalues_in.iter().cloned().collect(),
                local_count,
            )
            .destruct();

            let params = std::mem::take(&mut function.parameters);
            let is_variadic = function.is_variadic;
            let block = Arc::new(restructure::lift(function).into());
            LocalDeclarer::default().declare_locals(
                Arc::clone(&block),
                &upvalues_in.iter().chain(params.iter()).cloned().collect(),
            );
            {
                let mut ast_function = ast_function.lock();
                ast_function.body = Arc::try_unwrap(block)
                    .expect("exclusive AST block")
                    .into_inner();
                ast_function.parameters = params;
                ast_function.is_variadic = is_variadic;
            }
            (ByAddress(ast_function), upvalues_in)
        })
        .collect::<FxHashMap<_, _>>();

    let main = ByAddress(main);
    upvalues.remove(&main);
    let mut body = Arc::try_unwrap(main.0)
        .map_err(|_| anyhow!("main AST still has shared owners"))?
        .into_inner()
        .body;
    link_upvalues(&mut body, &mut upvalues);
    name_locals(&mut body, true);
    Ok(Lua51Decompilation {
        source: format!(
            "-- Decompiled by Luadicrous V2\n-- Input format: PUC Lua 5.1 bytecode\n{}\n",
            body
        ),
        inventory,
    })
}

fn parse(bytes: &[u8]) -> Result<(&[u8], Chunk<'_>)> {
    if !bytes.starts_with(b"\x1bLua") {
        bail!("input does not have the PUC Lua bytecode signature");
    }
    if bytes.get(4).copied() != Some(0x51) {
        let version = bytes.get(4).copied().unwrap_or_default();
        bail!("unsupported PUC Lua bytecode version 0x{version:02x}; expected Lua 5.1");
    }
    Chunk::parse(bytes).map_err(|error| anyhow!("invalid or unsupported Lua 5.1 chunk: {error:?}"))
}

fn build_inventory(
    function: &BytecodeFunction<'_>,
    consumed_bytes: usize,
    trailing_bytes: usize,
) -> Lua51Inventory {
    let mut inventory = Lua51Inventory {
        consumed_bytes,
        trailing_bytes,
        proto_count: 0,
        string_constant_count: 0,
        instruction_counts: Vec::new(),
        constant_counts: Vec::new(),
    };
    collect_inventory(function, &mut inventory);
    inventory
}

fn collect_inventory(function: &BytecodeFunction<'_>, inventory: &mut Lua51Inventory) {
    inventory.proto_count += 1;
    inventory.instruction_counts.push(function.code.len());
    inventory.constant_counts.push(function.constants.len());
    inventory.string_constant_count += function
        .constants
        .iter()
        .filter(|constant| matches!(constant, Value::String(_)))
        .count();
    for child in &function.closures {
        collect_inventory(child, inventory);
    }
}

fn write_function(
    output: &mut String,
    function: &BytecodeFunction<'_>,
    index: usize,
    depth: usize,
) -> std::fmt::Result {
    let indent = "  ".repeat(depth);
    let name = String::from_utf8_lossy(function.name);
    writeln!(
        output,
        "\n{indent}; proto {index} name={name:?} lines={}..{} stack={} params={} upvalues={} constants={} children={}",
        function.line_defined,
        function.last_line_defined,
        function.maximum_stack_size,
        function.number_of_parameters,
        function.number_of_upvalues,
        function.constants.len(),
        function.closures.len()
    )?;
    for (pc, instruction) in function.code.iter().enumerate() {
        writeln!(output, "{indent}{pc:06}  {instruction:?}")?;
    }
    for (child_index, child) in function.closures.iter().enumerate() {
        write_function(output, child, child_index, depth + 1)?;
    }
    Ok(())
}

fn link_upvalues(
    body: &mut ast::Block,
    upvalues: &mut FxHashMap<ByAddress<Arc<Mutex<ast::Function>>>, Vec<ast::RcLocal>>,
) {
    for statement in &mut body.0 {
        statement.traverse_rvalues(&mut |rvalue| {
            if let ast::RValue::Closure(closure) = rvalue {
                let Some(old_upvalues) = upvalues.remove(&closure.function) else {
                    return;
                };
                let mut function = closure.function.lock();
                let mut local_map =
                    FxHashMap::with_capacity_and_hasher(old_upvalues.len(), Default::default());
                for (old, new) in old_upvalues
                    .iter()
                    .zip(closure.upvalues.iter().map(|upvalue| match upvalue {
                        ast::Upvalue::Copy(local) | ast::Upvalue::Ref(local) => local,
                    }))
                {
                    local_map.insert(old.clone(), new.clone());
                }
                link_upvalues(&mut function.body, upvalues);
                replace_locals(&mut function.body, &local_map);
            }
        });
        match statement {
            ast::Statement::If(value) => {
                link_upvalues(&mut value.then_block.lock(), upvalues);
                link_upvalues(&mut value.else_block.lock(), upvalues);
            }
            ast::Statement::While(value) => link_upvalues(&mut value.block.lock(), upvalues),
            ast::Statement::Repeat(value) => link_upvalues(&mut value.block.lock(), upvalues),
            ast::Statement::NumericFor(value) => link_upvalues(&mut value.block.lock(), upvalues),
            ast::Statement::GenericFor(value) => link_upvalues(&mut value.block.lock(), upvalues),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn return_42_chunk(size_t_width: u8) -> Vec<u8> {
        let mut bytes = b"\x1bLua\x51\0\x01\x04".to_vec();
        bytes.extend([size_t_width, 4, 8, 0]);
        match size_t_width {
            4 => bytes.extend(0_u32.to_le_bytes()),
            8 => bytes.extend(0_u64.to_le_bytes()),
            _ => unreachable!(),
        }
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

    #[test]
    fn parses_32_and_64_bit_size_t_chunks() {
        for size_t_width in [4, 8] {
            let bytes = return_42_chunk(size_t_width);
            let inventory = inspect(&bytes).unwrap();
            assert_eq!(inventory.trailing_bytes, 0);
            assert_eq!(inventory.proto_count, 1);
            assert_eq!(inventory.instruction_counts, vec![2]);
            assert_eq!(inventory.constant_counts, vec![1]);
        }
    }

    #[test]
    fn decompiles_a_minimal_chunk() {
        let recovered = decompile(&return_42_chunk(8)).unwrap();
        assert!(recovered.source.contains("return 42"));
        assert!(recovered.source.contains("PUC Lua 5.1"));
    }

    #[test]
    fn rejects_other_puc_lua_versions_cleanly() {
        let mut bytes = return_42_chunk(8);
        bytes[4] = 0x54;
        let error = inspect(&bytes).unwrap_err().to_string();
        assert!(error.contains("expected Lua 5.1"));
    }
}
