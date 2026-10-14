mod adapter;
mod deserializer;
mod instruction;
mod lifter;
mod op_code;

use ast::{
    LocalRw, Reduce, Traverse, local_declarations::LocalDeclarer, name_locals::name_locals,
    replace_locals::replace_locals,
};

use by_address::ByAddress;
use cfg::{
    function::Function,
    ssa::{
        self,
        structuring::{structure_conditionals, structure_jumps},
    },
};
use indexmap::{IndexMap, IndexSet};
use itertools::Either;

use lifter::Lifter;

//use cfg_ir::{dot, function::Function, ssa};
use parking_lot::Mutex;
use petgraph::algo::dominators::simple_fast;
use petgraph::visit::EdgeRef;

use rustc_hash::FxHashMap;
use triomphe::Arc;

use deserializer::bytecode::Bytecode;
use deserializer::function::Function as BytecodeFunction;
use luadicrous_bytecode::ParsedChunk;

static PANIC_HOOK_LOCK: Mutex<()> = Mutex::new(());

#[cfg(feature = "dhat-heap")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

pub fn decompile_bytecode(bytecode: &[u8], encode_key: u8) -> String {
    let chunk = deserializer::deserialize(bytecode, encode_key).unwrap();
    match chunk {
        Bytecode::Error(msg) => msg,
        Bytecode::Chunk(chunk) => {
            decompile_functions(&chunk.functions, &chunk.string_table, chunk.main)
        }
    }
}

pub fn decompile_parsed_chunk(chunk: &ParsedChunk) -> anyhow::Result<String> {
    let adapted = adapter::adapt_chunk(chunk)?;
    Ok(decompile_functions(
        &adapted.functions,
        &adapted.strings,
        adapted.main,
    ))
}

fn decompile_functions(
    functions: &Vec<BytecodeFunction>,
    string_table: &Vec<Vec<u8>>,
    main: usize,
) -> String {
    ast::reset_local_ids();
    let debug_lifter = std::env::var_os("LUADICROUS_RS_DEBUG_LIFTER").is_some();
    let debug_timings = std::env::var_os("LUADICROUS_RS_DEBUG_TIMINGS").is_some();
    let total_started = std::time::Instant::now();
    let mut stage_started = total_started;
    let mut lifted = Vec::new();
    let mut stack = vec![(Arc::<Mutex<ast::Function>>::default(), main)];
    while let Some((ast_func, func_id)) = stack.pop() {
        if debug_lifter {
            eprintln!("[luadicrous-rs] lifting prototype {func_id}");
        }
        let (function, upvalues, child_functions) = Lifter::lift(functions, string_table, func_id);
        if debug_lifter {
            eprintln!(
                "[luadicrous-rs] lifted prototype {func_id}: {} blocks, {} children",
                function.blocks().count(),
                child_functions.len()
            );
        }
        lifted.push((ast_func, function, upvalues));
        stack.extend(child_functions.into_iter().map(|(a, f)| (a.0, f)));
    }
    if debug_timings {
        eprintln!(
            "[luadicrous-rs timing] lift: {:.3}s",
            stage_started.elapsed().as_secs_f64()
        );
    }
    stage_started = std::time::Instant::now();

    let (main, ..) = lifted.first().unwrap().clone();
    let mut upvalues = lifted
        .into_iter()
        .map(|(ast_function, function, upvalues_in)| {
            use std::panic;

            let function_id = function.id;
            let prototype_started = std::time::Instant::now();
            if debug_lifter {
                eprintln!("[luadicrous-rs] structuring prototype {function_id}");
            }
            let fallback = (ast_function.clone(), function.clone(), upvalues_in.clone());
            let mut args =
                std::panic::AssertUnwindSafe(Some((ast_function.clone(), function, upvalues_in)));

            let result = {
                let _panic_hook_guard = PANIC_HOOK_LOCK.lock();
                let prev_hook = panic::take_hook();
                let debug_panics = std::env::var_os("LUADICROUS_RS_DEBUG_PANICS").is_some();
                if debug_panics {
                    panic::set_hook(prev_hook);
                    panic::catch_unwind(move || {
                        let (ast_function, function, upvalues_in) = args.take().unwrap();
                        decompile_function(ast_function, function, upvalues_in)
                    })
                } else {
                    panic::set_hook(Box::new(|_| {}));
                    let result = panic::catch_unwind(move || {
                        let (ast_function, function, upvalues_in) = args.take().unwrap();
                        decompile_function(ast_function, function, upvalues_in)
                    });
                    panic::set_hook(prev_hook);
                    result
                }
            };

            let recovered = match result {
                Ok(recovered) => recovered,
                Err(error) => {
                    if debug_lifter {
                        let message = error
                            .downcast_ref::<String>()
                            .map(String::as_str)
                            .or_else(|| error.downcast_ref::<&str>().copied())
                            .unwrap_or("unknown panic");
                        eprintln!(
                            "[luadicrous-rs] prototype {function_id} fell back after: {message}"
                        );
                    }
                    decompile_raw_function(fallback.0, fallback.1, fallback.2)
                }
            };
            if debug_timings && prototype_started.elapsed().as_millis() >= 10 {
                eprintln!(
                    "[luadicrous-rs timing] prototype {function_id}: {:.3}s",
                    prototype_started.elapsed().as_secs_f64()
                );
            }
            recovered
        })
        .collect::<FxHashMap<_, _>>();
    if debug_timings {
        eprintln!(
            "[luadicrous-rs timing] structure: {:.3}s",
            stage_started.elapsed().as_secs_f64()
        );
    }
    stage_started = std::time::Instant::now();

    let main = ByAddress(main);
    upvalues.remove(&main);
    let mut body = Arc::try_unwrap(main.0).unwrap().into_inner().body;
    link_upvalues(&mut body, &mut upvalues);
    normalize_empty_guard_branches(&mut body);
    if debug_timings {
        eprintln!(
            "[luadicrous-rs timing] link upvalues: {:.3}s",
            stage_started.elapsed().as_secs_f64()
        );
    }
    stage_started = std::time::Instant::now();
    seed_semantic_local_names(&mut body);
    if debug_timings {
        eprintln!(
            "[luadicrous-rs timing] semantic names: {:.3}s",
            stage_started.elapsed().as_secs_f64()
        );
    }
    stage_started = std::time::Instant::now();
    name_locals(&mut body, false);
    make_local_names_scope_safe(&mut body);
    if debug_timings {
        eprintln!(
            "[luadicrous-rs timing] fallback names: {:.3}s",
            stage_started.elapsed().as_secs_f64()
        );
    }
    stage_started = std::time::Instant::now();
    let output = body.to_string();
    if debug_timings {
        eprintln!(
            "[luadicrous-rs timing] format: {:.3}s",
            stage_started.elapsed().as_secs_f64()
        );
        eprintln!(
            "[luadicrous-rs timing] total lifter: {:.3}s",
            total_started.elapsed().as_secs_f64()
        );
    }
    output
}

fn decompile_function(
    ast_function: Arc<Mutex<ast::Function>>,
    mut function: Function,
    upvalues_in: Vec<ast::RcLocal>,
) -> (ByAddress<Arc<Mutex<ast::Function>>>, Vec<ast::RcLocal>) {
    let function_id = function.id;
    let function_name = ast_function.lock().name.clone();
    let block_count = function.blocks().count();
    let statement_count = function
        .blocks()
        .map(|(_, block)| block.len())
        .sum::<usize>();
    if std::env::var_os("LUADICROUS_RS_DEBUG_LIFTER").is_some() {
        eprintln!(
            "[luadicrous-rs] prototype {} ({}) shape: {block_count} blocks, {statement_count} statements",
            function.id,
            function_name.as_deref().unwrap_or("anonymous")
        );
    }
    if statement_count >= 16_384 {
        return decompile_raw_function(ast_function, function, upvalues_in);
    }

    let profile_phases = std::env::var_os("LUADICROUS_RS_DEBUG_TIMINGS").is_some()
        && (block_count >= 128 || statement_count >= 4096);
    let mut phase_started = std::time::Instant::now();
    let (local_count, local_groups, upvalue_in_groups, upvalue_passed_groups) =
        cfg::ssa::construct(&mut function, &upvalues_in);
    if profile_phases {
        eprintln!(
            "[luadicrous-rs timing] prototype {} SSA construct: {:.3}s",
            function.id,
            phase_started.elapsed().as_secs_f64()
        );
    }
    let upvalue_to_group = upvalue_in_groups
        .into_iter()
        .chain(
            upvalue_passed_groups
                .into_iter()
                .map(|m| (ast::RcLocal::default(), m)),
        )
        .flat_map(|(i, g)| g.into_iter().map(move |u| (u, i.clone())))
        .collect::<IndexMap<_, _>>();
    // TODO: do we even need this?
    let local_to_group = local_groups
        .into_iter()
        .enumerate()
        .flat_map(|(i, g)| g.into_iter().map(move |l| (l, i)))
        .collect::<FxHashMap<_, _>>();
    // TODO: REFACTOR: some way to write a macro that states
    // if cfg::ssa::inline results in change then structure_jumps, structure_compound_conditionals,
    // structure_for_loops and remove_unnecessary_params must run again.
    // if structure_compound_conditionals results in change then dominators and post dominators
    // must be recalculated.
    // etc.
    // the macro could also maybe generate an optimal ordering?
    let mut changed = true;
    let mut optimization_rounds = 0usize;
    while changed {
        optimization_rounds += 1;
        changed = false;

        phase_started = std::time::Instant::now();
        let dominators = simple_fast(function.graph(), function.entry().unwrap());
        let jumps_changed = structure_jumps(&mut function, &dominators);
        changed |= jumps_changed;
        if profile_phases {
            eprintln!(
                "[luadicrous-rs timing] prototype {} round {optimization_rounds} jumps: {:.3}s changed={jumps_changed}",
                function.id,
                phase_started.elapsed().as_secs_f64()
            );
        }

        phase_started = std::time::Instant::now();
        ssa::inline::inline(&mut function, &local_to_group, &upvalue_to_group);
        if profile_phases {
            eprintln!(
                "[luadicrous-rs timing] prototype {} round {optimization_rounds} inline: {:.3}s",
                function.id,
                phase_started.elapsed().as_secs_f64()
            );
        }

        phase_started = std::time::Instant::now();
        let conditionals_changed = structure_conditionals(&mut function);
        if conditionals_changed
        // || {
        //     let post_dominators = post_dominators(function.graph_mut());
        //     structure_for_loops(&mut function, &dominators, &post_dominators)
        // }
        // we can't structure method calls like this because of __namecall
        // || structure_method_calls(&mut function)
        {
            changed = true;
        }
        if profile_phases {
            eprintln!(
                "[luadicrous-rs timing] prototype {} round {optimization_rounds} conditionals: {:.3}s changed={conditionals_changed}",
                function.id,
                phase_started.elapsed().as_secs_f64()
            );
        }
        let mut local_map = FxHashMap::default();
        // TODO: loop until returns false?
        phase_started = std::time::Instant::now();
        let params_changed =
            ssa::construct::remove_unnecessary_params(&mut function, &mut local_map);
        if params_changed {
            changed = true;
        }
        ssa::construct::apply_local_map(&mut function, local_map);
        if profile_phases {
            eprintln!(
                "[luadicrous-rs timing] prototype {} round {optimization_rounds} params: {:.3}s changed={params_changed}",
                function.id,
                phase_started.elapsed().as_secs_f64()
            );
        }
    }
    if std::env::var_os("LUADICROUS_RS_DEBUG_TIMINGS").is_some() && optimization_rounds >= 8 {
        eprintln!(
            "[luadicrous-rs timing] prototype {} optimization rounds: {optimization_rounds}",
            function.id
        );
    }
    // cfg::dot::render_to(&function, &mut std::io::stdout()).unwrap();
    phase_started = std::time::Instant::now();
    ssa::Destructor::new(
        &mut function,
        upvalue_to_group,
        upvalues_in.iter().cloned().collect(),
        local_count,
    )
    .destruct();
    if profile_phases {
        eprintln!(
            "[luadicrous-rs timing] prototype {} SSA destruct: {:.3}s",
            function.id,
            phase_started.elapsed().as_secs_f64()
        );
    }

    let params = std::mem::take(&mut function.parameters);
    let is_variadic = function.is_variadic;
    phase_started = std::time::Instant::now();
    let state_machine_input = function.clone();
    let mut structured = restructure::lift(function);
    inline_residual_gotos(&mut structured);
    let unstructured_jump_count = count_unstructured_jumps(&structured);
    let invalid_loop_control_count = count_invalid_loop_controls(&structured, 0);
    if std::env::var("LUADICROUS_RS_DEBUG_STRUCTURED_PROTO")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        == Some(function_id)
    {
        eprintln!(
            "[luadicrous-rs] prototype {function_id} pre-fallback structured output:\n{structured}"
        );
    }
    let (block, state_machine) = if unstructured_jump_count != 0 || invalid_loop_control_count != 0
    {
        match lift_state_machine(&state_machine_input) {
            Some(block) => (block, true),
            None => (structured, false),
        }
    } else {
        (structured, false)
    };
    if state_machine && std::env::var_os("LUADICROUS_RS_DEBUG_LIFTER").is_some() {
        eprintln!(
            "[luadicrous-rs] prototype {function_id} ({}) retained a state-machine fallback after {unstructured_jump_count} residual jumps/labels and {invalid_loop_control_count} unbound loop controls",
            function_name.as_deref().unwrap_or("anonymous"),
        );
    }
    if profile_phases {
        eprintln!(
            "[luadicrous-rs timing] prototype {} restructure: {:.3}s",
            function_id,
            phase_started.elapsed().as_secs_f64()
        );
    }
    let block = Arc::new(block.into());
    phase_started = std::time::Instant::now();
    declare_function_locals(Arc::clone(&block), &upvalues_in, &params, state_machine);
    if profile_phases {
        eprintln!(
            "[luadicrous-rs timing] prototype {} declarations: {:.3}s",
            function_id,
            phase_started.elapsed().as_secs_f64()
        );
    }

    {
        let mut ast_function = ast_function.lock();
        ast_function.body = Arc::try_unwrap(block).unwrap().into_inner();
        ast_function.parameters = params;
        ast_function.is_variadic = is_variadic;
    }
    (ByAddress(ast_function), upvalues_in)
}

fn decompile_raw_function(
    ast_function: Arc<Mutex<ast::Function>>,
    mut function: Function,
    upvalues_in: Vec<ast::RcLocal>,
) -> (ByAddress<Arc<Mutex<ast::Function>>>, Vec<ast::RcLocal>) {
    if std::env::var_os("LUADICROUS_RS_DEBUG_LIFTER").is_some() {
        eprintln!(
            "[luadicrous-rs] prototype {} entered raw control-flow recovery",
            function.id
        );
    }
    let params = std::mem::take(&mut function.parameters);
    let is_variadic = function.is_variadic;
    let block_count = function.blocks().count();
    let (block, state_machine) = lift_large_function(&function, block_count)
        .expect("control-flow graph must support state-machine lowering");
    let block = Arc::new(block.into());
    declare_function_locals(Arc::clone(&block), &upvalues_in, &params, state_machine);
    {
        let mut target = ast_function.lock();
        target.body = Arc::try_unwrap(block).unwrap().into_inner();
        target.parameters = params;
        target.is_variadic = is_variadic;
    }
    (ByAddress(ast_function), upvalues_in)
}

fn declare_function_locals(
    block: Arc<Mutex<ast::Block>>,
    upvalues: &[ast::RcLocal],
    params: &[ast::RcLocal],
    state_machine: bool,
) {
    let mut ignored = upvalues
        .iter()
        .chain(params)
        .cloned()
        .collect::<rustc_hash::FxHashSet<_>>();
    if let Some(register_bank) = spill_excess_locals(&mut block.lock(), &ignored) {
        ignored.insert(register_bank);
    }
    if state_machine {
        hoist_state_machine_locals(&mut block.lock(), &ignored);
    }
    LocalDeclarer::default().declare_locals(block, &ignored);
}

fn hoist_state_machine_locals(
    block: &mut ast::Block,
    ignored: &rustc_hash::FxHashSet<ast::RcLocal>,
) {
    let Some(dispatch) = block.iter().find_map(|statement| {
        statement
            .as_while()
            .map(|while_statement| while_statement.block.clone())
    }) else {
        return;
    };

    let mut locals = IndexSet::new();
    let mut captures = rustc_hash::FxHashSet::default();
    collect_function_locals(&mut dispatch.lock(), &mut locals, &mut captures);
    locals.retain(|local| !ignored.contains(local));
    if locals.is_empty() {
        return;
    }

    let mut declaration = ast::Assign::new(
        locals.into_iter().map(|local| local.into()).collect(),
        Vec::new(),
    );
    declaration.prefix = true;
    block.insert(0, declaration.into());
}

fn spill_excess_locals(
    block: &mut ast::Block,
    ignored: &rustc_hash::FxHashSet<ast::RcLocal>,
) -> Option<ast::RcLocal> {
    const LOCAL_BUDGET: usize = 128;

    let mut locals = IndexSet::new();
    let mut captures = rustc_hash::FxHashSet::default();
    collect_function_locals(block, &mut locals, &mut captures);
    locals.retain(|local| !ignored.contains(local) && !captures.contains(local));
    if locals.len() <= LOCAL_BUDGET {
        return None;
    }

    let spills = locals
        .into_iter()
        .skip(LOCAL_BUDGET)
        .enumerate()
        .map(|(index, local)| (local, index + 1))
        .collect::<FxHashMap<_, _>>();
    let register_bank = ast::RcLocal::new(ast::Local::new(Some("registers".to_owned())));
    replace_spilled_locals(block, &spills, &register_bank);

    let mut initialize = ast::Assign::new(
        vec![register_bank.clone().into()],
        vec![ast::Table::default().into()],
    );
    initialize.prefix = true;
    block.insert(0, initialize.into());
    Some(register_bank)
}

fn collect_function_locals(
    block: &mut ast::Block,
    locals: &mut IndexSet<ast::RcLocal>,
    captures: &mut rustc_hash::FxHashSet<ast::RcLocal>,
) {
    for statement in &mut block.0 {
        if let ast::Statement::Assign(assign) = statement {
            if assign.prefix {
                captures.extend(
                    assign
                        .left
                        .iter()
                        .filter_map(ast::LValue::as_local)
                        .cloned(),
                );
            }
        }
        locals.extend(statement.values().into_iter().cloned());
        statement.traverse_rvalues(&mut |value| {
            if let ast::RValue::Closure(closure) = value {
                captures.extend(closure.upvalues.iter().map(|upvalue| match upvalue {
                    ast::Upvalue::Copy(local) | ast::Upvalue::Ref(local) => local.clone(),
                }));
            }
        });
        match statement {
            ast::Statement::If(value) => {
                collect_function_locals(&mut value.then_block.lock(), locals, captures);
                collect_function_locals(&mut value.else_block.lock(), locals, captures);
            }
            ast::Statement::While(value) => {
                collect_function_locals(&mut value.block.lock(), locals, captures);
            }
            ast::Statement::Repeat(value) => {
                collect_function_locals(&mut value.block.lock(), locals, captures);
            }
            ast::Statement::NumericFor(value) => {
                collect_function_locals(&mut value.block.lock(), locals, captures);
            }
            ast::Statement::GenericFor(value) => {
                collect_function_locals(&mut value.block.lock(), locals, captures);
            }
            _ => {}
        }
    }
}

fn replace_spilled_locals(
    block: &mut ast::Block,
    spills: &FxHashMap<ast::RcLocal, usize>,
    register_bank: &ast::RcLocal,
) {
    for statement in &mut block.0 {
        statement.traverse_values(&mut |phase, value| {
            if matches!(phase, ast::PreOrPost::Pre) {
                match value {
                    Either::Left(left) => {
                        let index = match left {
                            ast::LValue::Local(local) => spills.get(local).copied(),
                            _ => None,
                        };
                        if let Some(index) = index {
                            *left = ast::Index::new(
                                register_bank.clone().into(),
                                ast::Literal::Number(index as f64).into(),
                            )
                            .into();
                        }
                    }
                    Either::Right(right) => {
                        let index = match right {
                            ast::RValue::Local(local) => spills.get(local).copied(),
                            _ => None,
                        };
                        if let Some(index) = index {
                            *right = ast::Index::new(
                                register_bank.clone().into(),
                                ast::Literal::Number(index as f64).into(),
                            )
                            .into();
                        }
                    }
                }
            }
            None::<()>
        });
        match statement {
            ast::Statement::If(value) => {
                replace_spilled_locals(&mut value.then_block.lock(), spills, register_bank);
                replace_spilled_locals(&mut value.else_block.lock(), spills, register_bank);
            }
            ast::Statement::While(value) => {
                replace_spilled_locals(&mut value.block.lock(), spills, register_bank);
            }
            ast::Statement::Repeat(value) => {
                replace_spilled_locals(&mut value.block.lock(), spills, register_bank);
            }
            ast::Statement::NumericFor(value) => {
                replace_spilled_locals(&mut value.block.lock(), spills, register_bank);
            }
            ast::Statement::GenericFor(value) => {
                replace_spilled_locals(&mut value.block.lock(), spills, register_bank);
            }
            _ => {}
        }
    }
}

fn lift_large_function(function: &Function, block_count: usize) -> Option<(ast::Block, bool)> {
    if block_count == 1 {
        let (node, block) = function.blocks().next()?;
        if function.edges(node).next().is_none() {
            return Some((optimize_raw_block(block.clone()), false));
        }
    }
    lift_state_machine(function).map(|block| (block, true))
}

fn count_unstructured_jumps(block: &ast::Block) -> usize {
    block
        .iter()
        .map(|statement| match statement {
            ast::Statement::Goto(_) | ast::Statement::Label(_) => 1,
            ast::Statement::If(value) => {
                count_unstructured_jumps(&value.then_block.lock())
                    + count_unstructured_jumps(&value.else_block.lock())
            }
            ast::Statement::While(value) => count_unstructured_jumps(&value.block.lock()),
            ast::Statement::Repeat(value) => count_unstructured_jumps(&value.block.lock()),
            ast::Statement::NumericFor(value) => count_unstructured_jumps(&value.block.lock()),
            ast::Statement::GenericFor(value) => count_unstructured_jumps(&value.block.lock()),
            _ => 0,
        })
        .sum()
}

fn count_invalid_loop_controls(block: &ast::Block, loop_depth: usize) -> usize {
    block
        .iter()
        .map(|statement| match statement {
            ast::Statement::Break(_) | ast::Statement::Continue(_) if loop_depth == 0 => 1,
            ast::Statement::If(value) => {
                count_invalid_loop_controls(&value.then_block.lock(), loop_depth)
                    + count_invalid_loop_controls(&value.else_block.lock(), loop_depth)
            }
            ast::Statement::While(value) => {
                count_invalid_loop_controls(&value.block.lock(), loop_depth + 1)
            }
            ast::Statement::Repeat(value) => {
                count_invalid_loop_controls(&value.block.lock(), loop_depth + 1)
            }
            ast::Statement::NumericFor(value) => {
                count_invalid_loop_controls(&value.block.lock(), loop_depth + 1)
            }
            ast::Statement::GenericFor(value) => {
                count_invalid_loop_controls(&value.block.lock(), loop_depth + 1)
            }
            _ => 0,
        })
        .sum()
}

fn inline_terminal_gotos(block: &mut ast::Block) -> usize {
    fn collect(block: &ast::Block, terminals: &mut FxHashMap<ast::Label, ast::Return>) {
        for statements in block.windows(2) {
            if let [
                ast::Statement::Label(label),
                ast::Statement::Return(return_statement),
            ] = statements
            {
                terminals.insert(label.clone(), return_statement.clone());
            }
        }
        for statement in block.iter() {
            match statement {
                ast::Statement::If(value) => {
                    collect(&value.then_block.lock(), terminals);
                    collect(&value.else_block.lock(), terminals);
                }
                ast::Statement::While(value) => collect(&value.block.lock(), terminals),
                ast::Statement::Repeat(value) => collect(&value.block.lock(), terminals),
                ast::Statement::NumericFor(value) => collect(&value.block.lock(), terminals),
                ast::Statement::GenericFor(value) => collect(&value.block.lock(), terminals),
                _ => {}
            }
        }
    }

    fn replace(block: &mut ast::Block, terminals: &FxHashMap<ast::Label, ast::Return>) -> usize {
        let mut replaced = 0;
        for statement in &mut block.0 {
            match statement {
                ast::Statement::Goto(goto_statement) => {
                    if let Some(return_statement) = terminals.get(&goto_statement.0) {
                        *statement = return_statement.clone().into();
                        replaced += 1;
                    }
                }
                ast::Statement::If(value) => {
                    replaced += replace(&mut value.then_block.lock(), terminals);
                    replaced += replace(&mut value.else_block.lock(), terminals);
                }
                ast::Statement::While(value) => {
                    replaced += replace(&mut value.block.lock(), terminals);
                }
                ast::Statement::Repeat(value) => {
                    replaced += replace(&mut value.block.lock(), terminals);
                }
                ast::Statement::NumericFor(value) => {
                    replaced += replace(&mut value.block.lock(), terminals);
                }
                ast::Statement::GenericFor(value) => {
                    replaced += replace(&mut value.block.lock(), terminals);
                }
                _ => {}
            }
        }
        block.retain(
            |statement| !matches!(statement, ast::Statement::Label(label) if terminals.contains_key(label)),
        );
        replaced
    }

    let mut terminals = FxHashMap::default();
    collect(block, &mut terminals);
    replace(block, &terminals)
}

fn inline_short_goto_targets(block: &mut ast::Block) -> usize {
    const MAX_TARGET_STATEMENTS: usize = 8;

    fn collect(block: &ast::Block, targets: &mut FxHashMap<ast::Label, Vec<ast::Statement>>) {
        for (index, statement) in block.iter().enumerate() {
            let ast::Statement::Label(label) = statement else {
                continue;
            };
            let mut target = Vec::new();
            for statement in block.iter().skip(index + 1) {
                if matches!(statement, ast::Statement::Label(_)) {
                    break;
                }
                target.push(statement.clone());
                if matches!(
                    statement,
                    ast::Statement::Goto(_) | ast::Statement::Return(_)
                ) {
                    if target.len() <= MAX_TARGET_STATEMENTS
                        && target.iter().all(|statement| {
                            !matches!(
                                statement,
                                ast::Statement::If(_)
                                    | ast::Statement::While(_)
                                    | ast::Statement::Repeat(_)
                                    | ast::Statement::NumericFor(_)
                                    | ast::Statement::GenericFor(_)
                            )
                        })
                        && !target.iter().any(
                            |statement| matches!(statement, ast::Statement::Goto(value) if value.0 == *label),
                        )
                    {
                        targets.insert(label.clone(), target);
                    }
                    break;
                }
                if target.len() > MAX_TARGET_STATEMENTS {
                    break;
                }
            }
        }
        for statement in block.iter() {
            match statement {
                ast::Statement::If(value) => {
                    collect(&value.then_block.lock(), targets);
                    collect(&value.else_block.lock(), targets);
                }
                ast::Statement::While(value) => collect(&value.block.lock(), targets),
                ast::Statement::Repeat(value) => collect(&value.block.lock(), targets),
                ast::Statement::NumericFor(value) => collect(&value.block.lock(), targets),
                ast::Statement::GenericFor(value) => collect(&value.block.lock(), targets),
                _ => {}
            }
        }
    }

    fn replace(
        block: &mut ast::Block,
        targets: &FxHashMap<ast::Label, Vec<ast::Statement>>,
    ) -> usize {
        let mut replaced = 0;
        let mut statements = Vec::with_capacity(block.len());
        for mut statement in std::mem::take(&mut block.0) {
            match &mut statement {
                ast::Statement::If(value) => {
                    replaced += replace(&mut value.then_block.lock(), targets);
                    replaced += replace(&mut value.else_block.lock(), targets);
                }
                ast::Statement::While(value) => {
                    replaced += replace(&mut value.block.lock(), targets);
                }
                ast::Statement::Repeat(value) => {
                    replaced += replace(&mut value.block.lock(), targets);
                }
                ast::Statement::NumericFor(value) => {
                    replaced += replace(&mut value.block.lock(), targets);
                }
                ast::Statement::GenericFor(value) => {
                    replaced += replace(&mut value.block.lock(), targets);
                }
                _ => {}
            }
            let target = match &statement {
                ast::Statement::Goto(goto_statement) => targets.get(&goto_statement.0),
                _ => None,
            };
            if let Some(target) = target {
                statements.extend(target.iter().cloned());
                replaced += 1;
            } else {
                statements.push(statement);
            }
        }
        block.0 = statements;
        replaced
    }

    fn collect_references(block: &ast::Block, references: &mut rustc_hash::FxHashSet<ast::Label>) {
        for statement in block.iter() {
            match statement {
                ast::Statement::Goto(value) => {
                    references.insert(value.0.clone());
                }
                ast::Statement::If(value) => {
                    collect_references(&value.then_block.lock(), references);
                    collect_references(&value.else_block.lock(), references);
                }
                ast::Statement::While(value) => {
                    collect_references(&value.block.lock(), references);
                }
                ast::Statement::Repeat(value) => {
                    collect_references(&value.block.lock(), references);
                }
                ast::Statement::NumericFor(value) => {
                    collect_references(&value.block.lock(), references);
                }
                ast::Statement::GenericFor(value) => {
                    collect_references(&value.block.lock(), references);
                }
                _ => {}
            }
        }
    }

    fn remove_unreferenced_labels(
        block: &mut ast::Block,
        references: &rustc_hash::FxHashSet<ast::Label>,
    ) {
        for statement in &mut block.0 {
            match statement {
                ast::Statement::If(value) => {
                    remove_unreferenced_labels(&mut value.then_block.lock(), references);
                    remove_unreferenced_labels(&mut value.else_block.lock(), references);
                }
                ast::Statement::While(value) => {
                    remove_unreferenced_labels(&mut value.block.lock(), references);
                }
                ast::Statement::Repeat(value) => {
                    remove_unreferenced_labels(&mut value.block.lock(), references);
                }
                ast::Statement::NumericFor(value) => {
                    remove_unreferenced_labels(&mut value.block.lock(), references);
                }
                ast::Statement::GenericFor(value) => {
                    remove_unreferenced_labels(&mut value.block.lock(), references);
                }
                _ => {}
            }
        }
        block.retain(
            |statement| !matches!(statement, ast::Statement::Label(label) if !references.contains(label)),
        );
    }

    let mut targets = FxHashMap::default();
    collect(block, &mut targets);
    let replaced = replace(block, &targets);
    if replaced != 0 {
        let mut references = rustc_hash::FxHashSet::default();
        collect_references(block, &mut references);
        remove_unreferenced_labels(block, &references);
    }
    replaced
}

fn inline_residual_gotos(block: &mut ast::Block) -> usize {
    let mut total = 0;
    for _ in 0..16 {
        let replaced = inline_terminal_gotos(block)
            + inline_short_goto_targets(block)
            + hoist_cross_branch_label_tails(block)
            + hoist_guarded_leading_continuations(block)
            + structure_forward_label_regions(block)
            + structure_false_guarded_cycles(block);
        total += replaced;
        if replaced == 0 {
            break;
        }
    }
    total
}

fn structure_forward_label_regions(block: &mut ast::Block) -> usize {
    fn count_references(block: &ast::Block, label: &ast::Label) -> usize {
        block
            .iter()
            .map(|statement| match statement {
                ast::Statement::Goto(value) => usize::from(value.0 == *label),
                ast::Statement::If(value) => {
                    count_references(&value.then_block.lock(), label)
                        + count_references(&value.else_block.lock(), label)
                }
                ast::Statement::While(value) => count_references(&value.block.lock(), label),
                ast::Statement::Repeat(value) => count_references(&value.block.lock(), label),
                ast::Statement::NumericFor(value) => count_references(&value.block.lock(), label),
                ast::Statement::GenericFor(value) => count_references(&value.block.lock(), label),
                _ => 0,
            })
            .sum()
    }

    fn replace_gotos(
        block: &mut ast::Block,
        label: &ast::Label,
        jump_flag: &ast::RcLocal,
    ) -> usize {
        let mut replaced = 0;
        let mut statements = Vec::with_capacity(block.len());
        for mut statement in std::mem::take(&mut block.0) {
            match &mut statement {
                ast::Statement::Goto(value) if value.0 == *label => {
                    statements.push(
                        ast::Assign::new(
                            vec![jump_flag.clone().into()],
                            vec![ast::Literal::Boolean(true).into()],
                        )
                        .into(),
                    );
                    statements.push(ast::Break {}.into());
                    replaced += 1;
                    continue;
                }
                ast::Statement::If(value) => {
                    replaced += replace_gotos(&mut value.then_block.lock(), label, jump_flag);
                    replaced += replace_gotos(&mut value.else_block.lock(), label, jump_flag);
                }
                ast::Statement::While(value) => {
                    let nested = replace_gotos(&mut value.block.lock(), label, jump_flag);
                    replaced += nested;
                    statements.push(statement);
                    if nested != 0 {
                        statements.push(
                            ast::If::new(
                                jump_flag.clone().into(),
                                ast::Block(vec![ast::Break {}.into()]),
                                ast::Block::default(),
                            )
                            .into(),
                        );
                    }
                    continue;
                }
                ast::Statement::Repeat(value) => {
                    let nested = replace_gotos(&mut value.block.lock(), label, jump_flag);
                    replaced += nested;
                    statements.push(statement);
                    if nested != 0 {
                        statements.push(
                            ast::If::new(
                                jump_flag.clone().into(),
                                ast::Block(vec![ast::Break {}.into()]),
                                ast::Block::default(),
                            )
                            .into(),
                        );
                    }
                    continue;
                }
                ast::Statement::NumericFor(value) => {
                    let nested = replace_gotos(&mut value.block.lock(), label, jump_flag);
                    replaced += nested;
                    statements.push(statement);
                    if nested != 0 {
                        statements.push(
                            ast::If::new(
                                jump_flag.clone().into(),
                                ast::Block(vec![ast::Break {}.into()]),
                                ast::Block::default(),
                            )
                            .into(),
                        );
                    }
                    continue;
                }
                ast::Statement::GenericFor(value) => {
                    let nested = replace_gotos(&mut value.block.lock(), label, jump_flag);
                    replaced += nested;
                    statements.push(statement);
                    if nested != 0 {
                        statements.push(
                            ast::If::new(
                                jump_flag.clone().into(),
                                ast::Block(vec![ast::Break {}.into()]),
                                ast::Block::default(),
                            )
                            .into(),
                        );
                    }
                    continue;
                }
                _ => {}
            }
            statements.push(statement);
        }
        block.0 = statements;
        replaced
    }

    fn recurse(block: &mut ast::Block, ambient_loop_depth: usize) -> bool {
        for statement in &mut block.0 {
            match statement {
                ast::Statement::If(value) => {
                    if recurse(&mut value.then_block.lock(), ambient_loop_depth)
                        || recurse(&mut value.else_block.lock(), ambient_loop_depth)
                    {
                        return true;
                    }
                }
                ast::Statement::While(value) => {
                    if recurse(&mut value.block.lock(), ambient_loop_depth + 1) {
                        return true;
                    }
                }
                ast::Statement::Repeat(value) => {
                    if recurse(&mut value.block.lock(), ambient_loop_depth + 1) {
                        return true;
                    }
                }
                ast::Statement::NumericFor(value) => {
                    if recurse(&mut value.block.lock(), ambient_loop_depth + 1) {
                        return true;
                    }
                }
                ast::Statement::GenericFor(value) => {
                    if recurse(&mut value.block.lock(), ambient_loop_depth + 1) {
                        return true;
                    }
                }
                _ => {}
            }
        }

        for label_index in 1..block.len() {
            let Some(ast::Statement::Label(label)) = block.get(label_index).cloned() else {
                continue;
            };
            let mut region = ast::Block(block.iter().take(label_index).cloned().collect());
            let suffix_block = ast::Block(block.iter().skip(label_index + 1).cloned().collect());
            let region_references = count_references(&region, &label);
            if region_references == 0 || region_references != count_references(block, &label) {
                continue;
            }
            if count_invalid_loop_controls(&region, 0) != 0
                || count_invalid_loop_controls(&suffix_block, ambient_loop_depth) != 0
            {
                continue;
            }

            let jump_flag = ast::RcLocal::default();
            let replaced = replace_gotos(&mut region, &label, &jump_flag);
            if replaced != region_references {
                continue;
            }
            region.insert(
                0,
                ast::Assign::new(
                    vec![jump_flag.into()],
                    vec![ast::Literal::Boolean(false).into()],
                )
                .into(),
            );
            block.0 = vec![ast::Repeat::new(ast::Literal::Boolean(true).into(), region).into()];
            block.extend(suffix_block.0);
            return true;
        }
        false
    }

    let mut changed = 0;
    for _ in 0..16 {
        if !recurse(block, 0) {
            break;
        }
        changed += 1;
    }
    changed
}

fn structure_false_guarded_cycles(block: &mut ast::Block) -> usize {
    fn count_references(block: &ast::Block, label: &ast::Label) -> usize {
        block
            .iter()
            .map(|statement| match statement {
                ast::Statement::Goto(value) => usize::from(value.0 == *label),
                ast::Statement::If(value) => {
                    count_references(&value.then_block.lock(), label)
                        + count_references(&value.else_block.lock(), label)
                }
                ast::Statement::While(value) => count_references(&value.block.lock(), label),
                ast::Statement::Repeat(value) => count_references(&value.block.lock(), label),
                ast::Statement::NumericFor(value) => count_references(&value.block.lock(), label),
                ast::Statement::GenericFor(value) => count_references(&value.block.lock(), label),
                _ => 0,
            })
            .sum()
    }

    fn rewrite_loop_body(
        block: &mut ast::Block,
        label: &ast::Label,
        jump_flag: &ast::RcLocal,
    ) -> usize {
        let mut replaced = 0;
        let mut statements = Vec::with_capacity(block.len());
        for mut statement in std::mem::take(&mut block.0) {
            match &mut statement {
                ast::Statement::Goto(value) if value.0 == *label => {
                    statements.push(
                        ast::Assign::new(
                            vec![jump_flag.clone().into()],
                            vec![ast::Literal::Boolean(true).into()],
                        )
                        .into(),
                    );
                    statements.push(ast::Break {}.into());
                    replaced += 1;
                    continue;
                }
                ast::Statement::If(value) => {
                    replaced += rewrite_loop_body(&mut value.then_block.lock(), label, jump_flag);
                    replaced += rewrite_loop_body(&mut value.else_block.lock(), label, jump_flag);
                }
                ast::Statement::While(value) => {
                    let nested = rewrite_loop_body(&mut value.block.lock(), label, jump_flag);
                    replaced += nested;
                    statements.push(statement);
                    if nested != 0 {
                        statements.push(
                            ast::If::new(
                                jump_flag.clone().into(),
                                ast::Block(vec![ast::Break {}.into()]),
                                ast::Block::default(),
                            )
                            .into(),
                        );
                    }
                    continue;
                }
                ast::Statement::Repeat(value) => {
                    let nested = rewrite_loop_body(&mut value.block.lock(), label, jump_flag);
                    replaced += nested;
                    statements.push(statement);
                    if nested != 0 {
                        statements.push(
                            ast::If::new(
                                jump_flag.clone().into(),
                                ast::Block(vec![ast::Break {}.into()]),
                                ast::Block::default(),
                            )
                            .into(),
                        );
                    }
                    continue;
                }
                ast::Statement::NumericFor(value) => {
                    let nested = rewrite_loop_body(&mut value.block.lock(), label, jump_flag);
                    replaced += nested;
                    statements.push(statement);
                    if nested != 0 {
                        statements.push(
                            ast::If::new(
                                jump_flag.clone().into(),
                                ast::Block(vec![ast::Break {}.into()]),
                                ast::Block::default(),
                            )
                            .into(),
                        );
                    }
                    continue;
                }
                ast::Statement::GenericFor(value) => {
                    let nested = rewrite_loop_body(&mut value.block.lock(), label, jump_flag);
                    replaced += nested;
                    statements.push(statement);
                    if nested != 0 {
                        statements.push(
                            ast::If::new(
                                jump_flag.clone().into(),
                                ast::Block(vec![ast::Break {}.into()]),
                                ast::Block::default(),
                            )
                            .into(),
                        );
                    }
                    continue;
                }
                _ => {}
            }
            statements.push(statement);
        }
        block.0 = statements;
        replaced
    }

    fn recurse(block: &mut ast::Block) -> bool {
        for statement in &mut block.0 {
            match statement {
                ast::Statement::If(value) => {
                    if recurse(&mut value.then_block.lock())
                        || recurse(&mut value.else_block.lock())
                    {
                        return true;
                    }
                }
                ast::Statement::While(value) => {
                    if recurse(&mut value.block.lock()) {
                        return true;
                    }
                }
                ast::Statement::Repeat(value) => {
                    if recurse(&mut value.block.lock()) {
                        return true;
                    }
                }
                ast::Statement::NumericFor(value) => {
                    if recurse(&mut value.block.lock()) {
                        return true;
                    }
                }
                ast::Statement::GenericFor(value) => {
                    if recurse(&mut value.block.lock()) {
                        return true;
                    }
                }
                _ => {}
            }
        }

        for index in 0..block.len() {
            let Some(false_loop) = block[index].as_while() else {
                continue;
            };
            if !matches!(
                &false_loop.condition,
                ast::RValue::Literal(ast::Literal::Boolean(false))
            ) {
                continue;
            }
            let mut continuation = false_loop.block.lock().clone();
            let Some(ast::Statement::Label(label)) = continuation.first().cloned() else {
                continue;
            };
            let mut suffix = ast::Block(block.iter().skip(index + 1).cloned().collect());
            let total_references = count_references(block, &label);
            let suffix_references = count_references(&suffix, &label);
            if suffix_references == 0 || suffix_references != total_references {
                continue;
            }
            let Some(last_statement) = suffix.last_mut() else {
                continue;
            };
            let outer_loop_body = match last_statement {
                ast::Statement::While(value) => &mut value.block,
                ast::Statement::Repeat(value) => &mut value.block,
                ast::Statement::NumericFor(value) => &mut value.block,
                ast::Statement::GenericFor(value) => &mut value.block,
                _ => continue,
            };
            if count_references(&outer_loop_body.lock(), &label) != suffix_references {
                continue;
            }

            let jump_flag = ast::RcLocal::default();
            let replaced = rewrite_loop_body(&mut outer_loop_body.lock(), &label, &jump_flag);
            if replaced != suffix_references {
                continue;
            }
            continuation.remove(0);
            let mut cycle = ast::Block(vec![
                ast::Assign::new(
                    vec![jump_flag.into()],
                    vec![ast::Literal::Boolean(false).into()],
                )
                .into(),
            ]);
            cycle.extend(suffix.0);
            cycle.extend(continuation.0);
            block.truncate(index);
            block.push(ast::While::new(ast::Literal::Boolean(true).into(), cycle).into());
            return true;
        }
        false
    }

    let mut changed = 0;
    for _ in 0..16 {
        if !recurse(block) {
            break;
        }
        changed += 1;
    }
    changed
}

fn rewrite_tail_gotos_as_breaks(
    block: &mut ast::Block,
    label: &ast::Label,
    loop_depth: usize,
    outside_loop_tail: bool,
) -> Result<usize, ()> {
    let mut replaced = 0;
    let block_len = block.len();
    for (index, statement) in block.iter_mut().enumerate() {
        let statement_is_tail = index + 1 == block_len;
        match statement {
            ast::Statement::Goto(value) if value.0 == *label => {
                if loop_depth != 1 || !outside_loop_tail {
                    return Err(());
                }
                *statement = ast::Break {}.into();
                replaced += 1;
            }
            ast::Statement::If(value) => {
                let child_tail = if loop_depth == 0 {
                    outside_loop_tail && statement_is_tail
                } else {
                    outside_loop_tail
                };
                replaced += rewrite_tail_gotos_as_breaks(
                    &mut value.then_block.lock(),
                    label,
                    loop_depth,
                    child_tail,
                )?;
                replaced += rewrite_tail_gotos_as_breaks(
                    &mut value.else_block.lock(),
                    label,
                    loop_depth,
                    child_tail,
                )?;
            }
            ast::Statement::While(value) => {
                replaced += rewrite_tail_gotos_as_breaks(
                    &mut value.block.lock(),
                    label,
                    loop_depth + 1,
                    outside_loop_tail && statement_is_tail,
                )?;
            }
            ast::Statement::Repeat(value) => {
                replaced += rewrite_tail_gotos_as_breaks(
                    &mut value.block.lock(),
                    label,
                    loop_depth + 1,
                    outside_loop_tail && statement_is_tail,
                )?;
            }
            ast::Statement::NumericFor(value) => {
                replaced += rewrite_tail_gotos_as_breaks(
                    &mut value.block.lock(),
                    label,
                    loop_depth + 1,
                    outside_loop_tail && statement_is_tail,
                )?;
            }
            ast::Statement::GenericFor(value) => {
                replaced += rewrite_tail_gotos_as_breaks(
                    &mut value.block.lock(),
                    label,
                    loop_depth + 1,
                    outside_loop_tail && statement_is_tail,
                )?;
            }
            _ => {}
        }
    }
    Ok(replaced)
}

fn hoist_guarded_leading_continuations(block: &mut ast::Block) -> usize {
    fn recurse(block: &mut ast::Block) -> bool {
        for statement in &mut block.0 {
            match statement {
                ast::Statement::If(value) => {
                    if recurse(&mut value.then_block.lock())
                        || recurse(&mut value.else_block.lock())
                    {
                        return true;
                    }
                }
                ast::Statement::While(value) => {
                    if recurse(&mut value.block.lock()) {
                        return true;
                    }
                }
                ast::Statement::Repeat(value) => {
                    if recurse(&mut value.block.lock()) {
                        return true;
                    }
                }
                ast::Statement::NumericFor(value) => {
                    if recurse(&mut value.block.lock()) {
                        return true;
                    }
                }
                ast::Statement::GenericFor(value) => {
                    if recurse(&mut value.block.lock()) {
                        return true;
                    }
                }
                _ => {}
            }
        }

        for index in 0..block.len() {
            let Some(if_statement) = block[index].as_if() else {
                continue;
            };
            let then_block = if_statement.then_block.lock().clone();
            let else_block = if_statement.else_block.lock().clone();
            let candidate = if else_block.is_empty() {
                match then_block.first() {
                    Some(ast::Statement::Label(label)) => Some((label.clone(), then_block, true)),
                    _ => None,
                }
            } else if then_block.is_empty() {
                match else_block.first() {
                    Some(ast::Statement::Label(label)) => Some((label.clone(), else_block, false)),
                    _ => None,
                }
            } else {
                None
            };
            let Some((label, mut continuation, invert)) = candidate else {
                continue;
            };
            let mut references = FxHashMap::default();
            let root_snapshot = block.clone();
            let mut all_references = FxHashMap::default();
            let mut suffix = ast::Block(block.iter().skip(index + 1).cloned().collect());
            if suffix.is_empty() {
                continue;
            }
            fn collect(block: &ast::Block, references: &mut FxHashMap<ast::Label, usize>) {
                for statement in block.iter() {
                    match statement {
                        ast::Statement::Goto(value) => {
                            *references.entry(value.0.clone()).or_default() += 1;
                        }
                        ast::Statement::If(value) => {
                            collect(&value.then_block.lock(), references);
                            collect(&value.else_block.lock(), references);
                        }
                        ast::Statement::While(value) => collect(&value.block.lock(), references),
                        ast::Statement::Repeat(value) => collect(&value.block.lock(), references),
                        ast::Statement::NumericFor(value) => {
                            collect(&value.block.lock(), references)
                        }
                        ast::Statement::GenericFor(value) => {
                            collect(&value.block.lock(), references)
                        }
                        _ => {}
                    }
                }
            }
            collect(&root_snapshot, &mut all_references);
            collect(&suffix, &mut references);
            let suffix_references = references.get(&label).copied().unwrap_or(0);
            if suffix_references == 0
                || all_references.get(&label).copied().unwrap_or(0) != suffix_references
            {
                continue;
            }
            let Ok(replaced) = rewrite_tail_gotos_as_breaks(&mut suffix, &label, 0, true) else {
                continue;
            };
            if replaced != suffix_references {
                continue;
            }

            continuation.remove(0);
            let mut condition = if_statement.condition.clone();
            if invert {
                condition = ast::Unary::new(condition, ast::UnaryOperation::Not).reduce_condition();
            }
            block.truncate(index);
            block.push(ast::If::new(condition, suffix, ast::Block::default()).into());
            block.extend(continuation.0);
            return true;
        }
        false
    }

    let mut changed = 0;
    for _ in 0..16 {
        if !recurse(block) {
            break;
        }
        changed += 1;
    }
    changed
}

fn hoist_cross_branch_label_tails(block: &mut ast::Block) -> usize {
    fn collect_references(block: &ast::Block, references: &mut FxHashMap<ast::Label, usize>) {
        for statement in block.iter() {
            match statement {
                ast::Statement::Goto(value) => {
                    *references.entry(value.0.clone()).or_default() += 1;
                }
                ast::Statement::If(value) => {
                    collect_references(&value.then_block.lock(), references);
                    collect_references(&value.else_block.lock(), references);
                }
                ast::Statement::While(value) => {
                    collect_references(&value.block.lock(), references);
                }
                ast::Statement::Repeat(value) => {
                    collect_references(&value.block.lock(), references);
                }
                ast::Statement::NumericFor(value) => {
                    collect_references(&value.block.lock(), references);
                }
                ast::Statement::GenericFor(value) => {
                    collect_references(&value.block.lock(), references);
                }
                _ => {}
            }
        }
    }

    fn count_references(block: &ast::Block, label: &ast::Label) -> usize {
        let mut references = FxHashMap::default();
        collect_references(block, &mut references);
        references.get(label).copied().unwrap_or(0)
    }

    fn try_hoist(block: &mut ast::Block, references: &FxHashMap<ast::Label, usize>) -> bool {
        for statement in &mut block.0 {
            match statement {
                ast::Statement::If(value) => {
                    if try_hoist(&mut value.then_block.lock(), references)
                        || try_hoist(&mut value.else_block.lock(), references)
                    {
                        return true;
                    }
                }
                ast::Statement::While(value) => {
                    if try_hoist(&mut value.block.lock(), references) {
                        return true;
                    }
                }
                ast::Statement::Repeat(value) => {
                    if try_hoist(&mut value.block.lock(), references) {
                        return true;
                    }
                }
                ast::Statement::NumericFor(value) => {
                    if try_hoist(&mut value.block.lock(), references) {
                        return true;
                    }
                }
                ast::Statement::GenericFor(value) => {
                    if try_hoist(&mut value.block.lock(), references) {
                        return true;
                    }
                }
                _ => {}
            }
        }

        for index in 0..block.len() {
            let Some(if_statement) = block[index].as_if() else {
                continue;
            };
            let then_block = if_statement.then_block.lock().clone();
            let else_block = if_statement.else_block.lock().clone();
            let (label, mut continuation, mut body, invert) =
                if let Some(ast::Statement::Label(label)) = then_block.first() {
                    (label.clone(), then_block, else_block, true)
                } else if let Some(ast::Statement::Label(label)) = else_block.first() {
                    (label.clone(), else_block, then_block, false)
                } else {
                    continue;
                };
            let body_references = count_references(&body, &label);
            if body_references == 0
                || references.get(&label).copied().unwrap_or(0) != body_references
            {
                continue;
            }
            let Ok(replaced) = rewrite_tail_gotos_as_breaks(&mut body, &label, 0, true) else {
                continue;
            };
            if replaced != body_references {
                continue;
            }

            continuation.remove(0);
            let mut condition = if_statement.condition.clone();
            if invert {
                condition = ast::Unary::new(condition, ast::UnaryOperation::Not).reduce_condition();
            }
            let replacement = ast::If::new(condition, body, ast::Block::default()).into();
            block.splice(
                index..=index,
                std::iter::once(replacement).chain(continuation.0),
            );
            return true;
        }
        false
    }

    let mut changed = 0;
    for _ in 0..16 {
        let mut references = FxHashMap::default();
        collect_references(block, &mut references);
        if !try_hoist(block, &references) {
            break;
        }
        changed += 1;
    }
    changed
}

fn lift_state_machine(function: &Function) -> Option<ast::Block> {
    let entry = function.entry().as_ref()?.index();
    let state = ast::RcLocal::new(ast::Local::new(Some("pc".to_owned())));
    let mut cases = function.blocks().collect::<Vec<_>>();
    cases.sort_unstable_by_key(|(node, _)| node.index());

    let mut dispatch = ast::Block::default();
    for (node, source_block) in cases {
        let mut case = optimize_raw_block(source_block.clone());
        let edges = function.edges(node).collect::<Vec<_>>();
        match edges.as_slice() {
            [] => {
                if !case
                    .last()
                    .is_some_and(|statement| statement.as_return().is_some())
                {
                    case.push(ast::Break {}.into());
                }
            }
            [edge] => {
                case.extend(state_transition(&state, edge).0);
            }
            [_, _] => {
                let (then_edge, else_edge) = function.conditional_edges(node)?;
                match case.pop()? {
                    ast::Statement::If(branch) => {
                        *branch.then_block.lock() = state_transition(&state, &then_edge);
                        *branch.else_block.lock() = state_transition(&state, &else_edge);
                        case.push(branch.into());
                    }
                    ast::Statement::NumForNext(next) => {
                        let counter = next.counter.0.as_local()?.clone();
                        case.push(
                            ast::Assign::new(
                                vec![counter.clone().into()],
                                vec![
                                    ast::Binary::new(
                                        next.counter.1,
                                        next.step.clone(),
                                        ast::BinaryOperation::Add,
                                    )
                                    .into(),
                                ],
                            )
                            .into(),
                        );
                        let positive = ast::Binary::new(
                            next.step.clone(),
                            ast::Literal::Number(0.0).into(),
                            ast::BinaryOperation::GreaterThan,
                        );
                        let positive_bound = ast::Binary::new(
                            counter.clone().into(),
                            next.limit.clone(),
                            ast::BinaryOperation::LessThanOrEqual,
                        );
                        let negative = ast::Binary::new(
                            next.step,
                            ast::Literal::Number(0.0).into(),
                            ast::BinaryOperation::LessThanOrEqual,
                        );
                        let negative_bound = ast::Binary::new(
                            counter.into(),
                            next.limit,
                            ast::BinaryOperation::GreaterThanOrEqual,
                        );
                        let condition = ast::Binary::new(
                            ast::Binary::new(
                                positive.into(),
                                positive_bound.into(),
                                ast::BinaryOperation::And,
                            )
                            .into(),
                            ast::Binary::new(
                                negative.into(),
                                negative_bound.into(),
                                ast::BinaryOperation::And,
                            )
                            .into(),
                            ast::BinaryOperation::Or,
                        );
                        case.push(
                            ast::If::new(
                                condition.into(),
                                state_transition(&state, &then_edge),
                                state_transition(&state, &else_edge),
                            )
                            .into(),
                        );
                    }
                    ast::Statement::GenericForNext(next) => {
                        let call = ast::Call::new(
                            next.generator,
                            vec![next.state, next.control.clone().into()],
                        );
                        case.push(
                            ast::Assign::new(
                                next.res_locals.clone(),
                                vec![ast::RValue::Select(call.into())],
                            )
                            .into(),
                        );
                        let first = next.res_locals.first()?.as_local()?.clone();
                        case.push(
                            ast::Assign::new(vec![next.control.into()], vec![first.clone().into()])
                                .into(),
                        );
                        let condition = ast::Binary::new(
                            first.into(),
                            ast::Literal::Nil.into(),
                            ast::BinaryOperation::NotEqual,
                        );
                        case.push(
                            ast::If::new(
                                condition.into(),
                                state_transition(&state, &then_edge),
                                state_transition(&state, &else_edge),
                            )
                            .into(),
                        );
                    }
                    _ => return None,
                }
            }
            _ => return None,
        }

        let condition = ast::Binary::new(
            state.clone().into(),
            ast::Literal::Number(node.index() as f64).into(),
            ast::BinaryOperation::Equal,
        );
        dispatch.push(ast::If::new(condition.into(), case, ast::Block::default()).into());
    }
    dispatch.push(ast::Break {}.into());

    Some(ast::Block(vec![
        ast::Assign::new(
            vec![state.clone().into()],
            vec![ast::Literal::Number(entry as f64).into()],
        )
        .into(),
        ast::While::new(ast::Literal::Boolean(true).into(), dispatch).into(),
    ]))
}

fn optimize_raw_block(block: ast::Block) -> ast::Block {
    let mut block = lower_state_machine_initializers(block);
    fold_overwritten_temp_setups(&mut block);
    block
}

fn fold_overwritten_temp_setups(block: &mut ast::Block) -> usize {
    let mut folded = 0;
    let mut index = 0;

    while index + 1 < block.len() {
        let Some((target, replacement)) = foldable_temp_setup(&block[index]) else {
            index += 1;
            continue;
        };

        if inline_overwritten_temp(&mut block[index + 1], &target, &replacement) {
            block.remove(index);
            folded += 1;
        } else {
            index += 1;
        }
    }

    for statement in &mut block.0 {
        folded += match statement {
            ast::Statement::If(value) => {
                fold_overwritten_temp_setups(&mut value.then_block.lock())
                    + fold_overwritten_temp_setups(&mut value.else_block.lock())
            }
            ast::Statement::While(value) => fold_overwritten_temp_setups(&mut value.block.lock()),
            ast::Statement::Repeat(value) => fold_overwritten_temp_setups(&mut value.block.lock()),
            ast::Statement::NumericFor(value) => {
                fold_overwritten_temp_setups(&mut value.block.lock())
            }
            ast::Statement::GenericFor(value) => {
                fold_overwritten_temp_setups(&mut value.block.lock())
            }
            _ => 0,
        };
    }

    folded
}

fn foldable_temp_setup(statement: &ast::Statement) -> Option<(ast::RcLocal, ast::RValue)> {
    let assign = statement.as_assign()?;
    if assign.prefix || assign.parallel || assign.left.len() != 1 || assign.right.len() != 1 {
        return None;
    }

    let target = assign.left[0].as_local()?.clone();
    let replacement = assign.right[0].clone();
    match &replacement {
        ast::RValue::Local(source) if source != &target => Some((target, replacement)),
        ast::RValue::Literal(_) => Some((target, replacement)),
        _ => None,
    }
}

fn inline_overwritten_temp(
    statement: &mut ast::Statement,
    target: &ast::RcLocal,
    replacement: &ast::RValue,
) -> bool {
    let Some(assign) = statement.as_assign_mut() else {
        return false;
    };
    if assign.prefix || assign.parallel || assign.left.len() != 1 || assign.right.len() != 1 {
        return false;
    }
    if assign.left[0].as_local() != Some(target) {
        return false;
    }
    if assign
        .values_read()
        .into_iter()
        .filter(|local| *local == target)
        .count()
        != 1
    {
        return false;
    }

    let mut replaced = false;
    assign.traverse_rvalues(&mut |value| {
        if matches!(value, ast::RValue::Local(local) if local == target) {
            *value = replacement.clone();
            replaced = true;
        }
    });
    replaced
}

fn lower_state_machine_initializers(block: ast::Block) -> ast::Block {
    ast::Block(
        block
            .0
            .into_iter()
            .flat_map(|statement| match statement {
                ast::Statement::GenericForInit(init) if init.1 => {
                    lower_generalized_for_initializer(init.0)
                }
                ast::Statement::GenericForInit(mut init) => {
                    init.0.parallel = true;
                    vec![ast::Statement::Assign(init.0)]
                }
                ast::Statement::NumForInit(init) => {
                    let initial_counter = ast::Binary::new(
                        init.counter.1,
                        init.step.1.clone(),
                        ast::BinaryOperation::Sub,
                    );
                    let mut assignment = ast::Assign::new(
                        vec![init.counter.0, init.limit.0, init.step.0],
                        vec![initial_counter.into(), init.limit.1, init.step.1],
                    );
                    assignment.parallel = true;
                    vec![ast::Statement::Assign(assignment)]
                }
                statement => vec![statement],
            })
            .collect(),
    )
}

fn lower_generalized_for_initializer(init: ast::Assign) -> Vec<ast::Statement> {
    let generator = init.left[0]
        .as_local()
        .expect("generic-for generator must be a local")
        .clone();
    let state = init.left[1]
        .as_local()
        .expect("generic-for state must be a local")
        .clone();
    let control = init.left[2]
        .as_local()
        .expect("generic-for control must be a local")
        .clone();
    let metatable = ast::RcLocal::default();

    let metatable_lookup = ast::Call::new(
        ast::Global::from("getmetatable").into(),
        vec![generator.clone().into()],
    );
    let assign_metatable = ast::Assign::new(
        vec![metatable.clone().into()],
        vec![metatable_lookup.into()],
    );

    let iter_method = ast::Index::new(
        metatable.clone().into(),
        ast::Literal::from("__iter").into(),
    );
    let has_iter_method = ast::Binary::new(
        metatable.clone().into(),
        iter_method.clone().into(),
        ast::BinaryOperation::And,
    );
    let iterator_call = ast::Call::new(iter_method.into(), vec![generator.clone().into()]);
    let mut use_iter_method = ast::Assign::new(
        vec![
            generator.clone().into(),
            state.clone().into(),
            control.clone().into(),
        ],
        vec![ast::RValue::Select(ast::Select::Call(iterator_call))],
    );
    use_iter_method.parallel = true;

    let generator_type = ast::Call::new(
        ast::Global::from("type").into(),
        vec![generator.clone().into()],
    );
    let is_table = ast::Binary::new(
        generator_type.into(),
        ast::Literal::from("table").into(),
        ast::BinaryOperation::Equal,
    );
    let no_metatable = ast::Binary::new(
        metatable.clone().into(),
        ast::Literal::Nil.into(),
        ast::BinaryOperation::Equal,
    );
    let call_method = ast::Index::new(metatable.into(), ast::Literal::from("__call").into());
    let no_call_method = ast::Binary::new(
        call_method.into(),
        ast::Literal::Nil.into(),
        ast::BinaryOperation::Equal,
    );
    let is_plain_table = ast::Binary::new(
        is_table.into(),
        ast::Binary::new(
            no_metatable.into(),
            no_call_method.into(),
            ast::BinaryOperation::Or,
        )
        .into(),
        ast::BinaryOperation::And,
    );
    let mut use_next = ast::Assign::new(
        vec![generator.clone().into(), state.into(), control.into()],
        vec![
            ast::Global::from("next").into(),
            generator.into(),
            ast::Literal::Nil.into(),
        ],
    );
    use_next.parallel = true;
    let plain_table_branch = ast::If::new(
        is_plain_table.into(),
        ast::Block(vec![use_next.into()]),
        ast::Block::default(),
    );
    let prepare = ast::If::new(
        has_iter_method.into(),
        ast::Block(vec![use_iter_method.into()]),
        ast::Block(vec![plain_table_branch.into()]),
    );

    vec![assign_metatable.into(), prepare.into()]
}

fn state_transition(
    state: &ast::RcLocal,
    edge: &petgraph::stable_graph::EdgeReference<'_, cfg::block::BlockEdge>,
) -> ast::Block {
    let mut statements = Vec::new();
    if !edge.weight().arguments.is_empty() {
        let mut assignment = ast::Assign::new(
            edge.weight()
                .arguments
                .iter()
                .map(|(local, _)| local.clone().into())
                .collect(),
            edge.weight()
                .arguments
                .iter()
                .map(|(_, value)| value.clone())
                .collect(),
        );
        assignment.parallel = true;
        statements.push(assignment.into());
    }
    statements.push(
        ast::Assign::new(
            vec![state.clone().into()],
            vec![ast::Literal::Number(edge.target().index() as f64).into()],
        )
        .into(),
    );
    statements.push(ast::Continue {}.into());
    ast::Block(statements)
}

fn link_upvalues(
    body: &mut ast::Block,
    upvalues: &mut FxHashMap<ByAddress<Arc<Mutex<ast::Function>>>, Vec<ast::RcLocal>>,
) {
    for stat in &mut body.0 {
        stat.traverse_rvalues(&mut |rvalue| {
            if let ast::RValue::Closure(closure) = rvalue {
                let old_upvalues = &upvalues[&closure.function];
                let mut function = closure.function.lock();
                // TODO: inefficient, try constructing a map of all up -> new up first
                // and then call replace_locals on main body
                let mut local_map =
                    FxHashMap::with_capacity_and_hasher(old_upvalues.len(), Default::default());
                for (old, new) in
                    old_upvalues
                        .iter()
                        .zip(closure.upvalues.iter().map(|u| match u {
                            ast::Upvalue::Copy(l) | ast::Upvalue::Ref(l) => l,
                        }))
                {
                    // println!("{} -> {}", old, new);
                    local_map.insert(old.clone(), new.clone());
                }
                link_upvalues(&mut function.body, upvalues);
                replace_locals(&mut function.body, &local_map);
            }
        });
        match stat {
            ast::Statement::If(r#if) => {
                link_upvalues(&mut r#if.then_block.lock(), upvalues);
                link_upvalues(&mut r#if.else_block.lock(), upvalues);
            }
            ast::Statement::While(r#while) => {
                link_upvalues(&mut r#while.block.lock(), upvalues);
            }
            ast::Statement::Repeat(repeat) => {
                link_upvalues(&mut repeat.block.lock(), upvalues);
            }
            ast::Statement::NumericFor(numeric_for) => {
                link_upvalues(&mut numeric_for.block.lock(), upvalues);
            }
            ast::Statement::GenericFor(generic_for) => {
                link_upvalues(&mut generic_for.block.lock(), upvalues);
            }
            _ => {}
        }
    }
}

fn seed_semantic_local_names(block: &mut ast::Block) {
    let mut write_counts = FxHashMap::default();
    let mut child_functions = Vec::new();
    collect_name_inputs(block, &mut write_counts, &mut child_functions);
    let mut used_names =
        rustc_hash::FxHashSet::from_iter(["pc".to_owned(), "registers".to_owned()]);
    collect_existing_local_names(block, &mut used_names);
    assign_prologue_name_hints(block, &write_counts, &mut used_names);
    assign_relationship_name_hints(block, &write_counts, &mut used_names);
    assign_roblox_name_hints(block);
    assign_stable_name_hints(block, &write_counts, &mut used_names);
    assign_closure_parameter_name_hints(block);
    assign_usage_role_name_hints(block, &write_counts, &mut used_names);

    for function in child_functions {
        seed_semantic_local_names(&mut function.lock().body);
    }
}

type ScopeBindings = FxHashMap<String, ast::RcLocal>;

fn make_local_names_scope_safe(block: &mut ast::Block) {
    let mut visited = rustc_hash::FxHashSet::default();
    make_function_scope_names_safe(block, &[], ScopeBindings::default(), &mut visited);
}

fn normalize_empty_guard_branches(block: &mut ast::Block) {
    let mut visited = rustc_hash::FxHashSet::default();
    normalize_empty_guard_branches_inner(block, &mut visited);
}

fn normalize_empty_guard_branches_inner(
    block: &mut ast::Block,
    visited: &mut rustc_hash::FxHashSet<ByAddress<Arc<Mutex<ast::Function>>>>,
) {
    for statement in &mut block.0 {
        statement.traverse_rvalues(&mut |value| {
            if let ast::RValue::Closure(closure) = value {
                if visited.insert(closure.function.clone()) {
                    normalize_empty_guard_branches_inner(
                        &mut closure.function.lock().body,
                        visited,
                    );
                }
            }
        });

        match statement {
            ast::Statement::If(value) => {
                normalize_empty_guard_branches_inner(&mut value.then_block.lock(), visited);
                normalize_empty_guard_branches_inner(&mut value.else_block.lock(), visited);

                let then_is_empty = value
                    .then_block
                    .lock()
                    .iter()
                    .all(|statement| matches!(statement, ast::Statement::Empty(_)));
                let else_is_empty = value
                    .else_block
                    .lock()
                    .iter()
                    .all(|statement| matches!(statement, ast::Statement::Empty(_)));
                if then_is_empty && !else_is_empty {
                    value.condition = ast::Unary::new(
                        value.condition.clone(),
                        ast::UnaryOperation::Not,
                    )
                    .reduce_condition();
                    std::mem::swap(&mut value.then_block, &mut value.else_block);
                }
            }
            ast::Statement::While(value) => {
                normalize_empty_guard_branches_inner(&mut value.block.lock(), visited)
            }
            ast::Statement::Repeat(value) => {
                normalize_empty_guard_branches_inner(&mut value.block.lock(), visited)
            }
            ast::Statement::NumericFor(value) => {
                normalize_empty_guard_branches_inner(&mut value.block.lock(), visited)
            }
            ast::Statement::GenericFor(value) => {
                normalize_empty_guard_branches_inner(&mut value.block.lock(), visited)
            }
            _ => {}
        }
    }
}

fn make_function_scope_names_safe(
    block: &mut ast::Block,
    parameters: &[ast::RcLocal],
    inherited_bindings: ScopeBindings,
    visited: &mut rustc_hash::FxHashSet<ByAddress<Arc<Mutex<ast::Function>>>>,
) {
    let mut bindings = refresh_scope_bindings(inherited_bindings.values());
    let mut reserved_names = rustc_hash::FxHashSet::from_iter(bindings.keys().cloned());
    let mut globals = rustc_hash::FxHashSet::default();
    collect_function_globals(block, &mut globals);
    reserved_names.extend(globals.iter().cloned());

    for global in &globals {
        let Some(local) = bindings.remove(global) else {
            continue;
        };
        let renamed = unique_name(global.clone(), &mut reserved_names);
        local.0.0.lock().0 = Some(renamed.clone());
        bindings.insert(renamed, local);
    }

    for parameter in parameters {
        register_scope_local(parameter, &mut bindings, &mut reserved_names);
    }

    let mut child_functions = Vec::new();
    register_block_scope_locals(
        block,
        &mut bindings,
        &mut reserved_names,
        &mut child_functions,
    );

    for (function_key, captures) in child_functions {
        if !visited.insert(function_key.clone()) {
            continue;
        }
        let mut child_bindings = ScopeBindings::default();
        for capture in captures {
            let Some(name) = capture.0.0.lock().0.clone() else {
                continue;
            };
            child_bindings.insert(name, capture);
        }
        let mut function = function_key.lock();
        let parameters = function.parameters.clone();
        make_function_scope_names_safe(&mut function.body, &parameters, child_bindings, visited);
    }
}

fn refresh_scope_bindings<'a>(locals: impl Iterator<Item = &'a ast::RcLocal>) -> ScopeBindings {
    let mut bindings = ScopeBindings::default();
    let mut seen = rustc_hash::FxHashSet::default();
    for local in locals {
        if !seen.insert(local.clone()) {
            continue;
        }
        if let Some(name) = local.0.0.lock().0.clone() {
            bindings.insert(name, local.clone());
        }
    }
    bindings
}

fn collect_function_globals(block: &mut ast::Block, globals: &mut rustc_hash::FxHashSet<String>) {
    for statement in &mut block.0 {
        statement.traverse_rvalues(&mut |value| {
            if let ast::RValue::Global(global) = value {
                if let Some(name) = identifier_hint(&global.0) {
                    globals.insert(name);
                }
            }
        });
        match statement {
            ast::Statement::If(value) => {
                collect_function_globals(&mut value.then_block.lock(), globals);
                collect_function_globals(&mut value.else_block.lock(), globals);
            }
            ast::Statement::While(value) => {
                collect_function_globals(&mut value.block.lock(), globals)
            }
            ast::Statement::Repeat(value) => {
                collect_function_globals(&mut value.block.lock(), globals)
            }
            ast::Statement::NumericFor(value) => {
                collect_function_globals(&mut value.block.lock(), globals)
            }
            ast::Statement::GenericFor(value) => {
                collect_function_globals(&mut value.block.lock(), globals)
            }
            _ => {}
        }
    }
}

fn register_block_scope_locals(
    block: &mut ast::Block,
    bindings: &mut ScopeBindings,
    reserved_names: &mut rustc_hash::FxHashSet<String>,
    child_functions: &mut Vec<(ByAddress<Arc<Mutex<ast::Function>>>, Vec<ast::RcLocal>)>,
) {
    for statement in &mut block.0 {
        statement.traverse_rvalues(&mut |value| {
            if let ast::RValue::Closure(closure) = value {
                child_functions.push((
                    closure.function.clone(),
                    closure
                        .upvalues
                        .iter()
                        .map(|upvalue| match upvalue {
                            ast::Upvalue::Copy(local) | ast::Upvalue::Ref(local) => local.clone(),
                        })
                        .collect(),
                ));
            }
        });

        match statement {
            ast::Statement::Assign(assign) if assign.prefix => {
                for local in assign.left.iter().filter_map(ast::LValue::as_local) {
                    register_scope_local(local, bindings, reserved_names);
                }
            }
            ast::Statement::If(value) => {
                let mut then_bindings = refresh_scope_bindings(bindings.values());
                let mut then_reserved_names = reserved_names.clone();
                register_block_scope_locals(
                    &mut value.then_block.lock(),
                    &mut then_bindings,
                    &mut then_reserved_names,
                    child_functions,
                );
                let mut else_bindings = refresh_scope_bindings(bindings.values());
                let mut else_reserved_names = reserved_names.clone();
                register_block_scope_locals(
                    &mut value.else_block.lock(),
                    &mut else_bindings,
                    &mut else_reserved_names,
                    child_functions,
                );
            }
            ast::Statement::While(value) => {
                let mut loop_bindings = refresh_scope_bindings(bindings.values());
                let mut loop_reserved_names = reserved_names.clone();
                register_block_scope_locals(
                    &mut value.block.lock(),
                    &mut loop_bindings,
                    &mut loop_reserved_names,
                    child_functions,
                );
            }
            ast::Statement::Repeat(value) => {
                let mut loop_bindings = refresh_scope_bindings(bindings.values());
                let mut loop_reserved_names = reserved_names.clone();
                register_block_scope_locals(
                    &mut value.block.lock(),
                    &mut loop_bindings,
                    &mut loop_reserved_names,
                    child_functions,
                );
            }
            ast::Statement::NumericFor(value) => {
                let mut loop_bindings = refresh_scope_bindings(bindings.values());
                let mut loop_reserved_names = reserved_names.clone();
                register_scope_local(
                    &value.counter,
                    &mut loop_bindings,
                    &mut loop_reserved_names,
                );
                register_block_scope_locals(
                    &mut value.block.lock(),
                    &mut loop_bindings,
                    &mut loop_reserved_names,
                    child_functions,
                );
            }
            ast::Statement::GenericFor(value) => {
                let mut loop_bindings = refresh_scope_bindings(bindings.values());
                let mut loop_reserved_names = reserved_names.clone();
                for local in &value.res_locals {
                    register_scope_local(
                        local,
                        &mut loop_bindings,
                        &mut loop_reserved_names,
                    );
                }
                register_block_scope_locals(
                    &mut value.block.lock(),
                    &mut loop_bindings,
                    &mut loop_reserved_names,
                    child_functions,
                );
            }
            _ => {}
        }
    }
}

fn register_scope_local(
    local: &ast::RcLocal,
    bindings: &mut ScopeBindings,
    reserved_names: &mut rustc_hash::FxHashSet<String>,
) {
    if bindings.values().any(|bound| bound == local) {
        return;
    }
    let Some(name) = local.0.0.lock().0.clone() else {
        return;
    };
    let name = if reserved_names.contains(&name) {
        unique_name(name, reserved_names)
    } else {
        reserved_names.insert(name.clone());
        name
    };
    local.0.0.lock().0 = Some(name.clone());
    bindings.insert(name, local.clone());
}

#[derive(Default)]
struct RelationshipNameInputs {
    table_definitions: rustc_hash::FxHashSet<ast::RcLocal>,
    closure_definitions: rustc_hash::FxHashSet<ast::RcLocal>,
    returned_locals: rustc_hash::FxHashSet<ast::RcLocal>,
    property_keys: FxHashMap<ast::RcLocal, rustc_hash::FxHashSet<String>>,
    property_aliases: FxHashMap<ast::RcLocal, rustc_hash::FxHashSet<String>>,
}

fn assign_relationship_name_hints(
    block: &ast::Block,
    write_counts: &FxHashMap<ast::RcLocal, usize>,
    used_names: &mut rustc_hash::FxHashSet<String>,
) {
    let mut inputs = RelationshipNameInputs::default();
    collect_relationship_name_inputs(block, &mut inputs);

    for local in &inputs.returned_locals {
        if inputs.table_definitions.contains(local)
            && write_counts.get(local) == Some(&1)
            && inputs
                .property_keys
                .get(local)
                .is_some_and(|keys| keys.len() >= 2)
        {
            assign_local_name(local, "module", used_names);
        }
    }

    for (local, aliases) in inputs.property_aliases {
        if inputs.closure_definitions.contains(&local)
            && write_counts.get(&local) == Some(&1)
            && aliases.len() == 1
        {
            if let Some(alias) = aliases.into_iter().next() {
                assign_local_name(&local, &lower_camel_case(alias), used_names);
            }
        }
    }
}

fn collect_relationship_name_inputs(block: &ast::Block, inputs: &mut RelationshipNameInputs) {
    for statement in &block.0 {
        if let ast::Statement::Assign(assign) = statement {
            if assign.left.len() == 1 && assign.right.len() == 1 {
                if let Some(local) = assign.left[0].as_local() {
                    match &assign.right[0] {
                        ast::RValue::Table(_) => {
                            inputs.table_definitions.insert(local.clone());
                        }
                        ast::RValue::Closure(_) => {
                            inputs.closure_definitions.insert(local.clone());
                        }
                        _ => {}
                    }
                }
            }

            if assign.left.len() == 1 && assign.right.len() == 1 {
                if let ast::LValue::Index(index) = &assign.left[0] {
                    if let Some((base, key)) = direct_local_property(index) {
                        inputs
                            .property_keys
                            .entry(base)
                            .or_default()
                            .insert(key.clone());
                        if let ast::RValue::Local(source) = &assign.right[0] {
                            inputs
                                .property_aliases
                                .entry(source.clone())
                                .or_default()
                                .insert(key);
                        }
                    }
                }
            }
        } else if let ast::Statement::Return(value) = statement {
            if value.values.len() == 1 {
                if let ast::RValue::Local(local) = &value.values[0] {
                    inputs.returned_locals.insert(local.clone());
                }
            }
        }

        match statement {
            ast::Statement::If(value) => {
                collect_relationship_name_inputs(&value.then_block.lock(), inputs);
                collect_relationship_name_inputs(&value.else_block.lock(), inputs);
            }
            ast::Statement::While(value) => {
                collect_relationship_name_inputs(&value.block.lock(), inputs)
            }
            ast::Statement::Repeat(value) => {
                collect_relationship_name_inputs(&value.block.lock(), inputs)
            }
            ast::Statement::NumericFor(value) => {
                collect_relationship_name_inputs(&value.block.lock(), inputs)
            }
            ast::Statement::GenericFor(value) => {
                collect_relationship_name_inputs(&value.block.lock(), inputs)
            }
            _ => {}
        }
    }
}

fn direct_local_property(index: &ast::Index) -> Option<(ast::RcLocal, String)> {
    let ast::RValue::Local(base) = index.left.as_ref() else {
        return None;
    };
    let ast::RValue::Literal(ast::Literal::String(key)) = index.right.as_ref() else {
        return None;
    };
    Some((base.clone(), identifier_hint(key)?))
}

#[derive(Clone)]
enum ConnectionCallback {
    Local(ast::RcLocal),
    Function(Arc<Mutex<ast::Function>>),
}

#[derive(Default)]
struct RobloxNameInputs {
    closure_functions: FxHashMap<ast::RcLocal, Arc<Mutex<ast::Function>>>,
    connections: Vec<(ast::RcLocal, String, ConnectionCallback)>,
    animation_track_writes: FxHashMap<ast::RcLocal, usize>,
    stable_hints: FxHashMap<ast::RcLocal, rustc_hash::FxHashSet<String>>,
    role_hints: FxHashMap<ast::RcLocal, rustc_hash::FxHashSet<String>>,
}

fn assign_roblox_name_hints(block: &mut ast::Block) {
    let mut inputs = RobloxNameInputs::default();
    let mut visited = rustc_hash::FxHashSet::default();
    collect_roblox_name_inputs(block, &mut inputs, &mut visited);

    let mut signal_receivers: FxHashMap<ast::RcLocal, rustc_hash::FxHashSet<String>> =
        FxHashMap::default();
    let mut event_boolean_writes: FxHashMap<
        ast::RcLocal,
        FxHashMap<String, rustc_hash::FxHashSet<bool>>,
    > = FxHashMap::default();

    for (receiver, signal, callback) in &inputs.connections {
        signal_receivers
            .entry(receiver.clone())
            .or_default()
            .insert(signal.clone());
        let function = match callback {
            ConnectionCallback::Local(local) => inputs.closure_functions.get(local).cloned(),
            ConnectionCallback::Function(function) => Some(function.clone()),
        };
        let Some(function) = function else {
            continue;
        };
        let function = function.lock();
        assign_event_parameter_names(&function.parameters, signal);
        collect_event_boolean_writes(
            &function.body,
            signal,
            &mut event_boolean_writes,
        );
    }

    for (receiver, signals) in signal_receivers {
        if signals.contains("Equipped")
            && signals.contains("Unequipped")
            && (signals.contains("Activated") || signals.contains("Deactivated"))
        {
            assign_contextual_local_name(&receiver, "tool");
        }
    }

    for (local, events) in event_boolean_writes {
        if events
            .get("Equipped")
            .is_some_and(|values| values.contains(&true))
            && events
                .get("Unequipped")
                .is_some_and(|values| values.contains(&false))
        {
            assign_contextual_local_name(&local, "isEquipped");
        }
    }

    for (local, count) in inputs.animation_track_writes {
        if count >= 2 {
            assign_contextual_local_name(&local, "animationTracks");
        }
    }

    for (local, hints) in inputs.stable_hints {
        if hints.len() == 1 {
            if let Some(hint) = hints.into_iter().next() {
                assign_contextual_local_name(&local, &hint);
            }
        }
    }

    for (local, roles) in inputs.role_hints {
        if roles.len() == 1 {
            if let Some(role) = roles.into_iter().next() {
                assign_role_local_name(&local, &role);
            }
        }
    }
}

fn collect_roblox_name_inputs(
    block: &mut ast::Block,
    inputs: &mut RobloxNameInputs,
    visited: &mut rustc_hash::FxHashSet<ByAddress<Arc<Mutex<ast::Function>>>>,
) {
    for statement in &mut block.0 {
        if let ast::Statement::Assign(assign) = statement {
            if assign.left.len() == 1 && assign.right.len() == 1 {
                if let Some(local) = assign.left[0].as_local() {
                    if let ast::RValue::Closure(closure) = &assign.right[0] {
                        inputs
                            .closure_functions
                            .insert(local.clone(), closure.function.0.clone());
                    }
                    if let Some(hint) = semantic_name_hint(&assign.right[0]) {
                        inputs
                            .stable_hints
                            .entry(local.clone())
                            .or_default()
                            .insert(hint);
                    }
                }
                if let ast::LValue::Index(index) = &assign.left[0] {
                    if let Some((base, _)) = direct_local_property(index) {
                        if matches!(
                            &assign.right[0],
                            ast::RValue::MethodCall(call)
                                | ast::RValue::Select(ast::Select::MethodCall(call))
                                if call.method == "LoadAnimation"
                        ) {
                            *inputs.animation_track_writes.entry(base).or_default() += 1;
                        }
                    }
                }
            }
        }
        if let ast::Statement::MethodCall(call) = statement {
            collect_roblox_method_call(call, inputs);
        }

        statement.traverse_rvalues(&mut |value| {
            if let ast::RValue::MethodCall(call)
            | ast::RValue::Select(ast::Select::MethodCall(call)) = value
            {
                collect_roblox_method_call(call, inputs);
            }
            if let ast::RValue::Closure(closure) = value {
                if visited.insert(closure.function.clone()) {
                    collect_roblox_name_inputs(&mut closure.function.lock().body, inputs, visited);
                }
            }
        });

        match statement {
            ast::Statement::If(value) => {
                collect_roblox_name_inputs(&mut value.then_block.lock(), inputs, visited);
                collect_roblox_name_inputs(&mut value.else_block.lock(), inputs, visited);
            }
            ast::Statement::While(value) => {
                collect_roblox_name_inputs(&mut value.block.lock(), inputs, visited)
            }
            ast::Statement::Repeat(value) => {
                collect_roblox_name_inputs(&mut value.block.lock(), inputs, visited)
            }
            ast::Statement::NumericFor(value) => {
                collect_roblox_name_inputs(&mut value.block.lock(), inputs, visited)
            }
            ast::Statement::GenericFor(value) => {
                collect_roblox_name_inputs(&mut value.block.lock(), inputs, visited)
            }
            _ => {}
        }
    }
}

fn collect_roblox_method_call(call: &ast::MethodCall, inputs: &mut RobloxNameInputs) {
    collect_method_argument_roles(call, &mut inputs.role_hints);
    if call.method != "Connect" {
        return;
    }
    let ast::RValue::Index(index) = call.value.as_ref() else {
        return;
    };
    let Some((receiver, signal)) = direct_local_property(index) else {
        return;
    };
    let Some(callback) = call.arguments.first().and_then(connection_callback) else {
        return;
    };
    inputs.connections.push((receiver, signal, callback));
}

fn connection_callback(value: &ast::RValue) -> Option<ConnectionCallback> {
    match value {
        ast::RValue::Local(local) => Some(ConnectionCallback::Local(local.clone())),
        ast::RValue::Closure(closure) => {
            Some(ConnectionCallback::Function(closure.function.0.clone()))
        }
        _ => None,
    }
}

fn collect_method_argument_roles(
    call: &ast::MethodCall,
    roles: &mut FxHashMap<ast::RcLocal, rustc_hash::FxHashSet<String>>,
) {
    let mut add_role = |index: usize, role: &str| {
        if let Some(ast::RValue::Local(local)) = call.arguments.get(index) {
            roles
                .entry(local.clone())
                .or_default()
                .insert(role.to_owned());
        }
    };
    match call.method.as_str() {
        "Raycast" => {
            add_role(0, "rayOrigin");
            add_role(1, "rayDirection");
            add_role(2, "raycastParams");
        }
        "FireServer"
            if matches!(
                call.value.as_ref(),
                ast::RValue::Index(index)
                    if direct_local_property(index)
                        .is_some_and(|(_, event)| event.contains("Shoot"))
            ) =>
        {
            add_role(0, "shotOriginCFrame");
            add_role(1, "targetCFrame");
        }
        _ => {}
    }
}

fn assign_event_parameter_names(
    parameters: &[ast::RcLocal],
    signal: &str,
) {
    let names: &[&str] = match signal {
        "TouchTapInWorld" => &["touchPosition", "processedByUi"],
        "InputBegan" | "InputChanged" | "InputEnded" => &["inputObject", "gameProcessed"],
        "RenderStepped" | "Heartbeat" | "PreSimulation" | "PostSimulation" => {
            &["deltaTime"]
        }
        "Stepped" => &["time", "deltaTime"],
        "Changed" | "AttributeChanged" => &["value"],
        "ChildAdded" | "ChildRemoved" | "DescendantAdded" | "DescendantRemoving" => &["child"],
        "PlayerAdded" | "PlayerRemoving" => &["player"],
        "AncestryChanged" => &["child", "parent"],
        _ => &[],
    };
    for (parameter, name) in parameters.iter().zip(names) {
        assign_contextual_local_name(parameter, name);
    }
}

fn assign_contextual_local_name(local: &ast::RcLocal, hint: &str) {
    if local.0.0.lock().0.is_none() {
        local.0.0.lock().0 = Some(hint.to_owned());
    }
}

fn assign_role_local_name(local: &ast::RcLocal, hint: &str) {
    let mut name = local.0.0.lock();
    if name.0.is_none()
        || matches!(
            name.0.as_deref(),
            Some("position" | "worldCFrame" | "origin" | "direction" | "value" | "result")
        )
    {
        name.0 = Some(hint.to_owned());
    }
}

fn collect_event_boolean_writes(
    block: &ast::Block,
    signal: &str,
    writes: &mut FxHashMap<
        ast::RcLocal,
        FxHashMap<String, rustc_hash::FxHashSet<bool>>,
    >,
) {
    for statement in &block.0 {
        if let ast::Statement::Assign(assign) = statement {
            if assign.left.len() == 1 && assign.right.len() == 1 {
                if let Some(local) = assign.left[0].as_local() {
                    if let ast::RValue::Literal(ast::Literal::Boolean(value)) = assign.right[0] {
                        writes
                            .entry(local.clone())
                            .or_default()
                            .entry(signal.to_owned())
                            .or_default()
                            .insert(value);
                    }
                }
            }
        }
        match statement {
            ast::Statement::If(value) => {
                collect_event_boolean_writes(&value.then_block.lock(), signal, writes);
                collect_event_boolean_writes(&value.else_block.lock(), signal, writes);
            }
            ast::Statement::While(value) => {
                collect_event_boolean_writes(&value.block.lock(), signal, writes)
            }
            ast::Statement::Repeat(value) => {
                collect_event_boolean_writes(&value.block.lock(), signal, writes)
            }
            ast::Statement::NumericFor(value) => {
                collect_event_boolean_writes(&value.block.lock(), signal, writes)
            }
            ast::Statement::GenericFor(value) => {
                collect_event_boolean_writes(&value.block.lock(), signal, writes)
            }
            _ => {}
        }
    }
}

fn collect_existing_local_names(
    block: &ast::Block,
    used_names: &mut rustc_hash::FxHashSet<String>,
) {
    for statement in &block.0 {
        for local in statement
            .values_read()
            .into_iter()
            .chain(statement.values_written())
        {
            if let Some(name) = local.0.0.lock().0.clone() {
                used_names.insert(name);
            }
        }
        match statement {
            ast::Statement::If(value) => {
                collect_existing_local_names(&value.then_block.lock(), used_names);
                collect_existing_local_names(&value.else_block.lock(), used_names);
            }
            ast::Statement::While(value) => {
                collect_existing_local_names(&value.block.lock(), used_names);
            }
            ast::Statement::Repeat(value) => {
                collect_existing_local_names(&value.block.lock(), used_names);
            }
            ast::Statement::NumericFor(value) => {
                collect_existing_local_names(&value.block.lock(), used_names);
            }
            ast::Statement::GenericFor(value) => {
                collect_existing_local_names(&value.block.lock(), used_names);
            }
            _ => {}
        }
    }
}

fn collect_name_inputs(
    block: &mut ast::Block,
    write_counts: &mut FxHashMap<ast::RcLocal, usize>,
    child_functions: &mut Vec<Arc<Mutex<ast::Function>>>,
) {
    for statement in &mut block.0 {
        for local in statement.values_written() {
            *write_counts.entry(local.clone()).or_default() += 1;
        }
        statement.traverse_rvalues(&mut |value| {
            if let ast::RValue::Closure(closure) = value {
                child_functions.push(closure.function.0.clone());
            }
        });
        match statement {
            ast::Statement::If(value) => {
                collect_name_inputs(&mut value.then_block.lock(), write_counts, child_functions);
                collect_name_inputs(&mut value.else_block.lock(), write_counts, child_functions);
            }
            ast::Statement::While(value) => {
                collect_name_inputs(&mut value.block.lock(), write_counts, child_functions)
            }
            ast::Statement::Repeat(value) => {
                collect_name_inputs(&mut value.block.lock(), write_counts, child_functions)
            }
            ast::Statement::NumericFor(value) => {
                collect_name_inputs(&mut value.block.lock(), write_counts, child_functions)
            }
            ast::Statement::GenericFor(value) => {
                collect_name_inputs(&mut value.block.lock(), write_counts, child_functions)
            }
            _ => {}
        }
    }
}

fn assign_prologue_name_hints(
    block: &mut ast::Block,
    write_counts: &FxHashMap<ast::RcLocal, usize>,
    used_names: &mut rustc_hash::FxHashSet<String>,
) {
    for statement in &mut block.0 {
        let ast::Statement::Assign(assign) = statement else {
            if matches!(
                statement,
                ast::Statement::Comment(_) | ast::Statement::Empty(_)
            ) {
                continue;
            }
            break;
        };

        if let Some(call) = assign.right.first().and_then(call_from_rvalue) {
            if matches!(call.value.as_ref(), ast::RValue::Global(global) if global.0 == b"pcall")
                && assign.left.len() >= 2
                && assign
                    .left
                    .iter()
                    .take(2)
                    .filter_map(ast::LValue::as_local)
                    .all(|local| write_counts.get(local) == Some(&1))
            {
                if let Some(success) = assign.left[0].as_local() {
                    assign_local_name(success, "success", used_names);
                }
                if let Some(result) = assign.left[1].as_local() {
                    assign_local_name(result, "result", used_names);
                }
                continue;
            }
        }

        if assign.left.len() != 1 || assign.right.len() != 1 {
            continue;
        }
        let Some(local) = assign.left[0].as_local() else {
            continue;
        };
        let hint = match &assign.right[0] {
            ast::RValue::Global(global) if global.0 == b"type" => Some("luaType"),
            ast::RValue::Global(global) if global.0 == b"typeof" => Some("typeOf"),
            ast::RValue::Global(global) if global.0 == b"Vector3" => Some("nativeVector3"),
            ast::RValue::Local(source)
                if source.0.0.lock().0.as_deref() == Some("nativeVector3") =>
            {
                Some("vector3")
            }
            _ => None,
        };
        if let Some(hint) = hint {
            assign_local_name(local, hint, used_names);
        }
    }
}

fn call_from_rvalue(value: &ast::RValue) -> Option<&ast::Call> {
    match value {
        ast::RValue::Call(call) => Some(call),
        ast::RValue::Select(ast::Select::Call(call)) => Some(call),
        _ => None,
    }
}

fn assign_local_name(
    local: &ast::RcLocal,
    hint: &str,
    used_names: &mut rustc_hash::FxHashSet<String>,
) {
    if local.0.0.lock().0.is_none() {
        local.0.0.lock().0 = Some(unique_name(hint.to_owned(), used_names));
    }
}

fn assign_stable_name_hints(
    block: &mut ast::Block,
    write_counts: &FxHashMap<ast::RcLocal, usize>,
    used_names: &mut rustc_hash::FxHashSet<String>,
) {
    for statement in &mut block.0 {
        if let ast::Statement::Assign(assign) = statement {
            if assign.left.len() == 1 && assign.right.len() == 1 {
                if let Some(local) = assign.left[0].as_local() {
                    if write_counts.get(local) == Some(&1) && local.0.0.lock().0.is_none() {
                        if let Some(hint) = semantic_name_hint(&assign.right[0]) {
                            let name = unique_name(hint, used_names);
                            local.0.0.lock().0 = Some(name);
                        }
                    }
                }
            }
        }
        match statement {
            ast::Statement::If(value) => {
                assign_stable_name_hints(&mut value.then_block.lock(), write_counts, used_names);
                assign_stable_name_hints(&mut value.else_block.lock(), write_counts, used_names);
            }
            ast::Statement::While(value) => {
                assign_stable_name_hints(&mut value.block.lock(), write_counts, used_names)
            }
            ast::Statement::Repeat(value) => {
                assign_stable_name_hints(&mut value.block.lock(), write_counts, used_names)
            }
            ast::Statement::NumericFor(value) => {
                assign_stable_name_hints(&mut value.block.lock(), write_counts, used_names)
            }
            ast::Statement::GenericFor(value) => {
                assign_stable_name_hints(&mut value.block.lock(), write_counts, used_names)
            }
            _ => {}
        }
    }
}

fn assign_closure_parameter_name_hints(block: &mut ast::Block) {
    for statement in &mut block.0 {
        statement.traverse_rvalues(&mut |value| {
            if let ast::RValue::Closure(closure) = value {
                assign_return_table_parameter_names(closure);
            }
        });
        match statement {
            ast::Statement::If(value) => {
                assign_closure_parameter_name_hints(&mut value.then_block.lock());
                assign_closure_parameter_name_hints(&mut value.else_block.lock());
            }
            ast::Statement::While(value) => {
                assign_closure_parameter_name_hints(&mut value.block.lock())
            }
            ast::Statement::Repeat(value) => {
                assign_closure_parameter_name_hints(&mut value.block.lock())
            }
            ast::Statement::NumericFor(value) => {
                assign_closure_parameter_name_hints(&mut value.block.lock())
            }
            ast::Statement::GenericFor(value) => {
                assign_closure_parameter_name_hints(&mut value.block.lock())
            }
            _ => {}
        }
    }
}

#[derive(Default)]
struct UsageRoleInputs {
    table_definitions: rustc_hash::FxHashSet<ast::RcLocal>,
    derived_from_table: FxHashMap<ast::RcLocal, ast::RcLocal>,
    closure_parameters: FxHashMap<ast::RcLocal, Vec<ast::RcLocal>>,
    role_hints: FxHashMap<ast::RcLocal, rustc_hash::FxHashSet<String>>,
}

fn assign_usage_role_name_hints(
    block: &mut ast::Block,
    write_counts: &FxHashMap<ast::RcLocal, usize>,
    used_names: &mut rustc_hash::FxHashSet<String>,
) {
    let mut inputs = UsageRoleInputs::default();
    collect_usage_role_definitions(block, &mut inputs);
    collect_return_table_roles(block, &mut inputs);
    collect_call_argument_roles(block, &mut inputs);

    for (local, roles) in inputs.role_hints {
        if inputs.table_definitions.contains(&local)
            && write_counts.get(&local) == Some(&1)
            && roles.len() == 1
        {
            if let Some(role) = roles.into_iter().next() {
                assign_local_name(&local, &lower_camel_case(role), used_names);
            }
        }
    }
}

fn collect_usage_role_definitions(block: &ast::Block, inputs: &mut UsageRoleInputs) {
    for statement in &block.0 {
        if let ast::Statement::Assign(assign) = statement {
            if assign.left.len() == 1 && assign.right.len() == 1 {
                if let Some(local) = assign.left[0].as_local() {
                    match &assign.right[0] {
                        ast::RValue::Table(_) => {
                            inputs.table_definitions.insert(local.clone());
                        }
                        ast::RValue::Index(index) => {
                            if let ast::RValue::Local(base) = index.left.as_ref() {
                                inputs
                                    .derived_from_table
                                    .insert(local.clone(), base.clone());
                            }
                        }
                        ast::RValue::Closure(closure) => {
                            inputs
                                .closure_parameters
                                .insert(local.clone(), closure.function.lock().parameters.clone());
                        }
                        _ => {}
                    }
                }
            }
        }
        match statement {
            ast::Statement::If(value) => {
                collect_usage_role_definitions(&value.then_block.lock(), inputs);
                collect_usage_role_definitions(&value.else_block.lock(), inputs);
            }
            ast::Statement::While(value) => {
                collect_usage_role_definitions(&value.block.lock(), inputs)
            }
            ast::Statement::Repeat(value) => {
                collect_usage_role_definitions(&value.block.lock(), inputs)
            }
            ast::Statement::NumericFor(value) => {
                collect_usage_role_definitions(&value.block.lock(), inputs)
            }
            ast::Statement::GenericFor(value) => {
                collect_usage_role_definitions(&value.block.lock(), inputs)
            }
            _ => {}
        }
    }
}

fn collect_return_table_roles(block: &mut ast::Block, inputs: &mut UsageRoleInputs) {
    for statement in &mut block.0 {
        statement.traverse_rvalues(&mut |value| {
            let ast::RValue::Closure(closure) = value else {
                return;
            };
            let function = closure.function.lock();
            let Some(table) = single_direct_return_table(&function.body) else {
                return;
            };
            for (key, value) in &table.0 {
                let Some(role) = key
                    .as_ref()
                    .and_then(ast::RValue::as_literal)
                    .and_then(ast::Literal::as_string)
                    .and_then(|key| identifier_hint(key))
                else {
                    continue;
                };
                let ast::RValue::Local(value) = value else {
                    continue;
                };
                if let Some(base) = inputs.derived_from_table.get(value) {
                    inputs
                        .role_hints
                        .entry(base.clone())
                        .or_default()
                        .insert(role);
                }
            }
        });
        match statement {
            ast::Statement::If(value) => {
                collect_return_table_roles(&mut value.then_block.lock(), inputs);
                collect_return_table_roles(&mut value.else_block.lock(), inputs);
            }
            ast::Statement::While(value) => {
                collect_return_table_roles(&mut value.block.lock(), inputs)
            }
            ast::Statement::Repeat(value) => {
                collect_return_table_roles(&mut value.block.lock(), inputs)
            }
            ast::Statement::NumericFor(value) => {
                collect_return_table_roles(&mut value.block.lock(), inputs)
            }
            ast::Statement::GenericFor(value) => {
                collect_return_table_roles(&mut value.block.lock(), inputs)
            }
            _ => {}
        }
    }
}

fn collect_call_argument_roles(block: &mut ast::Block, inputs: &mut UsageRoleInputs) {
    for statement in &mut block.0 {
        statement.traverse_rvalues(&mut |value| {
            let call = match value {
                ast::RValue::Call(call) | ast::RValue::Select(ast::Select::Call(call)) => call,
                _ => return,
            };
            let ast::RValue::Local(callee) = call.value.as_ref() else {
                return;
            };
            let Some(parameters) = inputs.closure_parameters.get(callee) else {
                return;
            };
            for (argument, parameter) in call.arguments.iter().zip(parameters) {
                let Some(role) = parameter.0.0.lock().0.clone() else {
                    continue;
                };
                if is_fallback_local_name(&role) {
                    continue;
                }
                let base = match argument {
                    ast::RValue::Index(index) => match index.left.as_ref() {
                        ast::RValue::Local(base) => Some(base),
                        _ => None,
                    },
                    ast::RValue::Local(local) => inputs.derived_from_table.get(local),
                    _ => None,
                };
                if let Some(base) = base {
                    inputs
                        .role_hints
                        .entry(base.clone())
                        .or_default()
                        .insert(role);
                }
            }
        });
        match statement {
            ast::Statement::If(value) => {
                collect_call_argument_roles(&mut value.then_block.lock(), inputs);
                collect_call_argument_roles(&mut value.else_block.lock(), inputs);
            }
            ast::Statement::While(value) => {
                collect_call_argument_roles(&mut value.block.lock(), inputs)
            }
            ast::Statement::Repeat(value) => {
                collect_call_argument_roles(&mut value.block.lock(), inputs)
            }
            ast::Statement::NumericFor(value) => {
                collect_call_argument_roles(&mut value.block.lock(), inputs)
            }
            ast::Statement::GenericFor(value) => {
                collect_call_argument_roles(&mut value.block.lock(), inputs)
            }
            _ => {}
        }
    }
}

fn is_fallback_local_name(name: &str) -> bool {
    let Some(suffix) = name.strip_prefix('v').or_else(|| name.strip_prefix('p')) else {
        return name == "_";
    };
    !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
}

fn assign_return_table_parameter_names(closure: &ast::Closure) {
    let function = closure.function.lock();
    let Some(table) = single_direct_return_table(&function.body) else {
        return;
    };
    let parameters = rustc_hash::FxHashSet::from_iter(function.parameters.iter().cloned());
    let mut hints: FxHashMap<ast::RcLocal, rustc_hash::FxHashSet<String>> = FxHashMap::default();
    for (key, value) in &table.0 {
        let Some(key) = key
            .as_ref()
            .and_then(ast::RValue::as_literal)
            .and_then(ast::Literal::as_string)
            .and_then(|key| identifier_hint(key))
        else {
            continue;
        };
        let ast::RValue::Local(local) = value else {
            continue;
        };
        if parameters.contains(local) {
            hints.entry(local.clone()).or_default().insert(key);
        }
    }

    let mut used_names = rustc_hash::FxHashSet::default();
    for parameter in &function.parameters {
        if let Some(name) = parameter.0.0.lock().0.clone() {
            used_names.insert(name);
        }
    }
    collect_existing_local_names(&function.body, &mut used_names);
    for (parameter, names) in hints {
        if names.len() == 1 {
            if let Some(name) = names.into_iter().next() {
                assign_local_name(&parameter, &lower_camel_case(name), &mut used_names);
            }
        }
    }
}

fn single_direct_return_table(block: &ast::Block) -> Option<&ast::Table> {
    let mut tables = block.0.iter().filter_map(|statement| {
        let value = statement.as_return()?;
        if value.values.len() != 1 {
            return None;
        }
        value.values[0].as_table()
    });
    let table = tables.next()?;
    if tables.next().is_some() {
        return None;
    }
    Some(table)
}

fn semantic_name_hint(value: &ast::RValue) -> Option<String> {
    match value {
        ast::RValue::Global(global) => identifier_hint(&global.0).map(lower_camel_case),
        ast::RValue::Local(local) => local.0.0.lock().0.clone(),
        ast::RValue::Index(_) => meaningful_path_terminal(value),
        ast::RValue::Call(call) => call_name_hint(call),
        ast::RValue::Select(ast::Select::Call(call)) => call_name_hint(call),
        ast::RValue::Select(ast::Select::MethodCall(call)) => method_call_name_hint(call),
        ast::RValue::MethodCall(call) if call.method == "GetService" => call
            .arguments
            .first()
            .and_then(ast::RValue::as_literal)
            .and_then(ast::Literal::as_string)
            .and_then(|name| identifier_hint(name))
            .map(lower_camel_case),
        ast::RValue::MethodCall(call) => method_call_name_hint(call),
        ast::RValue::Closure(closure) => closure_name_hint(closure),
        _ => None,
    }
}

fn closure_name_hint(closure: &ast::Closure) -> Option<String> {
    let function = closure.function.lock();
    if let Some(name) = function
        .name
        .as_deref()
        .and_then(|name| identifier_hint(name.as_bytes()))
    {
        return Some(lower_camel_case(name));
    }

    let table = single_direct_return_table(&function.body)?;
    for discriminator in ["Type", "Kind", "Variant", "Mode", "Side"] {
        let Some(value) = table.0.iter().find_map(|(key, value)| {
            let key = key
                .as_ref()?
                .as_literal()?
                .as_string()
                .and_then(|key| std::str::from_utf8(key).ok())?;
            key.eq_ignore_ascii_case(discriminator).then_some(value)
        }) else {
            continue;
        };
        let ast::RValue::Local(local) = value else {
            continue;
        };
        let name = local.0.0.lock().0.clone()?;
        return Some(prefixed_camel_name("make", &name));
    }
    None
}

fn prefixed_camel_name(prefix: &str, name: &str) -> String {
    let mut bytes = name.as_bytes().to_vec();
    if let Some(first) = bytes.first_mut() {
        first.make_ascii_uppercase();
    }
    format!(
        "{prefix}{}",
        String::from_utf8(bytes).expect("semantic local names are ASCII")
    )
}

fn call_name_hint(call: &ast::Call) -> Option<String> {
    if matches!(call.value.as_ref(), ast::RValue::Global(global) if global.0 == b"require") {
        return call.arguments.first().and_then(meaningful_path_terminal);
    }
    if let ast::RValue::Index(index) = call.value.as_ref() {
        if matches!(index.right.as_ref(), ast::RValue::Literal(ast::Literal::String(name)) if name == b"new")
        {
            return meaningful_path_terminal(index.left.as_ref());
        }
    }
    meaningful_path_terminal(call.value.as_ref())
}

fn method_call_name_hint(call: &ast::MethodCall) -> Option<String> {
    if call.method == "GetService" {
        return call
            .arguments
            .first()
            .and_then(ast::RValue::as_literal)
            .and_then(ast::Literal::as_string)
            .and_then(|name| identifier_hint(name))
            .map(lower_camel_case);
    }
    if matches!(
        call.method.as_str(),
        "WaitForChild" | "FindFirstChild" | "FindFirstChildOfClass" | "FindFirstChildWhichIsA"
    ) {
        return call
            .arguments
            .first()
            .and_then(ast::RValue::as_literal)
            .and_then(ast::Literal::as_string)
            .and_then(|name| identifier_hint(name))
            .map(lower_camel_case);
    }
    if call.method == "Clone" {
        return meaningful_path_terminal(call.value.as_ref());
    }
    match call.method.as_str() {
        "GetMouse" => Some("mouse".to_owned()),
        "GetMouseTargetCFrame" => Some("targetCFrame".to_owned()),
        "GetTargetPosition" => Some("targetPosition".to_owned()),
        "GetPlayerFromCharacter" => Some("player".to_owned()),
        "LoadAnimation" => Some("animationTrack".to_owned()),
        "Raycast" => Some("raycastResult".to_owned()),
        "GetChildren" => Some("children".to_owned()),
        "GetDescendants" => Some("descendants".to_owned()),
        _ => None,
    }
}

fn meaningful_path_terminal(value: &ast::RValue) -> Option<String> {
    match value {
        ast::RValue::Index(index) => {
            let terminal = index
                .right
                .as_literal()
                .and_then(ast::Literal::as_string)
                .and_then(|name| identifier_hint(name));
            match terminal.as_deref() {
                Some("Value") => meaningful_path_terminal(index.left.as_ref()),
                Some(name) => Some(lower_camel_case(name.to_owned())),
                None => meaningful_path_terminal(index.left.as_ref()),
            }
        }
        ast::RValue::Global(global) => identifier_hint(&global.0).map(lower_camel_case),
        ast::RValue::Local(local) => local.0.0.lock().0.clone(),
        ast::RValue::MethodCall(call) | ast::RValue::Select(ast::Select::MethodCall(call)) => {
            method_call_name_hint(call).or_else(|| meaningful_path_terminal(call.value.as_ref()))
        }
        ast::RValue::Call(call) | ast::RValue::Select(ast::Select::Call(call)) => {
            call_name_hint(call)
        }
        _ => None,
    }
}

fn identifier_hint(bytes: &[u8]) -> Option<String> {
    let name = std::str::from_utf8(bytes).ok()?;
    let mut chars = name.chars();
    let first = chars.next()?;
    if !(first == '_' || first.is_ascii_alphabetic())
        || !chars.all(|character| character == '_' || character.is_ascii_alphanumeric())
        || matches!(
            name,
            "and"
                | "break"
                | "continue"
                | "do"
                | "else"
                | "elseif"
                | "end"
                | "false"
                | "for"
                | "function"
                | "if"
                | "in"
                | "local"
                | "nil"
                | "not"
                | "or"
                | "repeat"
                | "return"
                | "then"
                | "true"
                | "until"
                | "while"
        )
    {
        return None;
    }
    Some(name.to_owned())
}

fn lower_camel_case(name: String) -> String {
    let mut bytes = name.into_bytes();
    let uppercase_prefix = bytes
        .iter()
        .take_while(|byte| byte.is_ascii_uppercase())
        .count();
    let lowercase_count = if uppercase_prefix > 1 && uppercase_prefix < bytes.len() {
        uppercase_prefix - 1
    } else {
        uppercase_prefix.max(1).min(bytes.len())
    };
    for byte in &mut bytes[..lowercase_count] {
        byte.make_ascii_lowercase();
    }
    String::from_utf8(bytes).expect("identifier hints are ASCII")
}

fn unique_name(hint: String, used_names: &mut rustc_hash::FxHashSet<String>) -> String {
    if used_names.insert(hint.clone()) {
        return hint;
    }
    for suffix in 2.. {
        let candidate = format!("{hint}{suffix}");
        if used_names.insert(candidate.clone()) {
            return candidate;
        }
    }
    unreachable!()
}

#[cfg(test)]
mod tests {
    use super::{
        declare_function_locals, fold_overwritten_temp_setups, hoist_cross_branch_label_tails,
        inline_residual_gotos, inline_short_goto_targets, inline_terminal_gotos,
        lower_state_machine_initializers, make_local_names_scope_safe,
        normalize_empty_guard_branches, seed_semantic_local_names, spill_excess_locals,
        structure_forward_label_regions,
    };
    use ast::{LValue, Local, LocalRw, RcLocal, Statement};
    use parking_lot::Mutex;
    use rustc_hash::FxHashSet;
    use triomphe::Arc;

    #[test]
    fn spills_excess_temporaries_into_a_scoped_register_bank() {
        let mut block = ast::Block::default();
        for index in 0..140 {
            let local = RcLocal::new(Local::new(None));
            block.push(
                ast::Assign::new(
                    vec![local.into()],
                    vec![ast::Literal::Number(index as f64).into()],
                )
                .into(),
            );
        }

        let bank = spill_excess_locals(&mut block, &FxHashSet::default())
            .expect("more than the local budget must spill");
        assert_eq!(bank.0.0.lock().0.as_deref(), Some("registers"));
        assert!(matches!(
            block.first(),
            Some(Statement::Assign(assign)) if assign.prefix
        ));
        let indexed_writes = block
            .iter()
            .flat_map(Statement::values_written)
            .filter(|local| **local == bank)
            .count();
        assert_eq!(indexed_writes, 1);
        assert!(
            block
                .iter()
                .filter_map(Statement::as_assign)
                .flat_map(|assign| &assign.left)
                .any(|left| matches!(left, LValue::Index(_)))
        );
    }

    #[test]
    fn hoists_state_machine_locals_outside_the_dispatch_loop() {
        let state = RcLocal::new(Local::new(Some("pc".to_owned())));
        let accumulator = RcLocal::new(Local::new(Some("accumulator".to_owned())));
        let mut dispatch = ast::Block::default();
        dispatch.push(
            ast::Assign::new(
                vec![accumulator.clone().into()],
                vec![ast::Literal::Number(0.0).into()],
            )
            .into(),
        );
        dispatch.push(
            ast::Assign::new(
                vec![accumulator.clone().into()],
                vec![
                    ast::Binary::new(
                        accumulator.clone().into(),
                        ast::Literal::Number(1.0).into(),
                        ast::BinaryOperation::Add,
                    )
                    .into(),
                ],
            )
            .into(),
        );
        dispatch.push(
            ast::Assign::new(
                vec![state.clone().into()],
                vec![ast::Literal::Number(1.0).into()],
            )
            .into(),
        );

        let block = Arc::new(Mutex::new(ast::Block(vec![
            ast::Assign::new(vec![state.into()], vec![ast::Literal::Number(0.0).into()]).into(),
            ast::While::new(ast::Literal::Boolean(true).into(), dispatch).into(),
        ])));
        declare_function_locals(Arc::clone(&block), &[], &[], true);

        let block = block.lock();
        assert!(matches!(
            block.first(),
            Some(Statement::Assign(assign)) if assign.prefix && assign.right.is_empty()
        ));
        let dispatch = block
            .iter()
            .find_map(Statement::as_while)
            .expect("state machine must retain its dispatch loop")
            .block
            .lock();
        assert!(
            dispatch.iter().all(|statement| {
                !matches!(statement, Statement::Assign(assign) if assign.prefix)
            })
        );
    }

    #[test]
    fn replaces_cross_region_goto_to_terminal_return() {
        let label = ast::Label("done".to_owned());
        let terminal = ast::Return::new(vec![ast::Literal::Number(7.0).into()]);
        let then_block = ast::Block(vec![label.clone().into(), terminal.clone().into()]);
        let mut block = ast::Block(vec![
            ast::If::new(
                ast::Literal::Boolean(true).into(),
                then_block,
                ast::Block::default(),
            )
            .into(),
            ast::Goto::new(label).into(),
        ]);

        assert_eq!(inline_terminal_gotos(&mut block), 1);
        assert_eq!(super::count_unstructured_jumps(&block), 0);
        assert!(matches!(block.last(), Some(Statement::Return(value)) if value == &terminal));
    }

    #[test]
    fn inlines_short_shared_tail_chains() {
        let shared = ast::Label("shared".to_owned());
        let done = ast::Label("done".to_owned());
        let value = RcLocal::new(Local::new(Some("value".to_owned())));
        let terminal = ast::Return::new(vec![value.clone().into()]);
        let mut block = ast::Block(vec![
            ast::Goto::new(shared.clone()).into(),
            shared.into(),
            ast::Assign::new(
                vec![value.clone().into()],
                vec![ast::Literal::Number(7.0).into()],
            )
            .into(),
            ast::Goto::new(done.clone()).into(),
            done.into(),
            terminal.clone().into(),
        ]);

        assert!(inline_residual_gotos(&mut block) >= 2);
        assert_eq!(super::count_unstructured_jumps(&block), 0);
        assert!(matches!(block.first(), Some(Statement::Assign(_))));
        assert!(matches!(block.get(1), Some(Statement::Return(value)) if value == &terminal));
    }

    #[test]
    fn refuses_to_inline_recursive_structured_tail() {
        let shared = ast::Label("shared".to_owned());
        let recursive_branch = ast::If::new(
            ast::Global::from("retry").into(),
            ast::Block(vec![ast::Goto::new(shared.clone()).into()]),
            ast::Block::default(),
        );
        let mut block = ast::Block(vec![
            ast::Goto::new(shared.clone()).into(),
            shared.into(),
            recursive_branch.into(),
            ast::Return::default().into(),
        ]);

        assert_eq!(inline_short_goto_targets(&mut block), 0);
        assert!(matches!(block.first(), Some(Statement::Goto(_))));
        let branch = block.get(2).and_then(Statement::as_if).unwrap();
        assert!(matches!(
            branch.then_block.lock().first(),
            Some(Statement::Goto(_))
        ));
    }

    #[test]
    fn structures_forward_goto_as_one_shot_region() {
        let done = ast::Label("done".to_owned());
        let mut block = ast::Block(vec![
            ast::If::new(
                ast::Global::from("skip").into(),
                ast::Block(vec![ast::Goto::new(done.clone()).into()]),
                ast::Block::default(),
            )
            .into(),
            ast::Call::new(ast::Global::from("work").into(), Vec::new()).into(),
            done.into(),
            ast::Call::new(ast::Global::from("finish").into(), Vec::new()).into(),
        ]);

        assert_eq!(structure_forward_label_regions(&mut block), 1);
        assert_eq!(super::count_unstructured_jumps(&block), 0);
        let output = block.to_string();
        assert!(output.contains("repeat"), "{output}");
        assert!(output.contains("break"), "{output}");
        assert!(output.contains("work()"), "{output}");
        assert!(output.contains("finish()"), "{output}");
    }

    #[test]
    fn rejects_forward_region_with_escaped_loop_control() {
        let done = ast::Label("done".to_owned());
        let mut block = ast::Block(vec![
            ast::Goto::new(done.clone()).into(),
            ast::Break {}.into(),
            done.into(),
            ast::Return::default().into(),
        ]);

        assert_eq!(structure_forward_label_regions(&mut block), 0);
        assert_eq!(super::count_unstructured_jumps(&block), 2);
        assert_eq!(super::count_invalid_loop_controls(&block, 0), 1);
    }

    #[test]
    fn hoists_sibling_branch_continuation_after_wait_loop() {
        let continuation = ast::Label("continue_after_wait".to_owned());
        let condition = RcLocal::new(Local::new(Some("condition".to_owned())));
        let completed = RcLocal::new(Local::new(Some("completed".to_owned())));
        let wait_loop = ast::While::new(
            ast::Literal::Boolean(true).into(),
            ast::Block(vec![ast::Goto::new(continuation.clone()).into()]),
        );
        let then_block = ast::Block(vec![
            continuation.into(),
            ast::Assign::new(
                vec![completed.clone().into()],
                vec![ast::Literal::Boolean(true).into()],
            )
            .into(),
        ]);
        let mut block = ast::Block(vec![
            ast::If::new(
                condition.into(),
                then_block,
                ast::Block(vec![wait_loop.into()]),
            )
            .into(),
        ]);

        assert_eq!(hoist_cross_branch_label_tails(&mut block), 1);
        assert_eq!(super::count_unstructured_jumps(&block), 0);
        assert!(matches!(block.last(), Some(Statement::Assign(_))));
        let branch = block.first().and_then(Statement::as_if).unwrap();
        let then_block = branch.then_block.lock();
        let loop_statement = then_block.first().and_then(Statement::as_while).unwrap();
        let loop_body = loop_statement.block.lock();
        assert!(matches!(loop_body.first(), Some(Statement::Break(_))));
    }

    #[test]
    fn folds_adjacent_setup_overwritten_by_its_only_use() {
        let source = RcLocal::new(Local::new(Some("source".to_owned())));
        let temp = RcLocal::new(Local::new(Some("temp".to_owned())));
        let argument = RcLocal::new(Local::new(Some("argument".to_owned())));
        let mut block = ast::Block(vec![
            ast::Assign::new(vec![temp.clone().into()], vec![source.clone().into()]).into(),
            ast::Assign::new(
                vec![temp.clone().into()],
                vec![ast::Call::new(temp.clone().into(), vec![argument.into()]).into()],
            )
            .into(),
        ]);

        assert_eq!(fold_overwritten_temp_setups(&mut block), 1);
        assert_eq!(block.len(), 1);
        assert_eq!(block.to_string(), "temp = source(argument)");
    }

    #[test]
    fn preserves_setup_when_target_has_multiple_uses() {
        let source = RcLocal::new(Local::new(Some("source".to_owned())));
        let temp = RcLocal::new(Local::new(Some("temp".to_owned())));
        let mut block = ast::Block(vec![
            ast::Assign::new(vec![temp.clone().into()], vec![source.into()]).into(),
            ast::Assign::new(
                vec![temp.clone().into()],
                vec![
                    ast::Binary::new(temp.clone().into(), temp.into(), ast::BinaryOperation::Add)
                        .into(),
                ],
            )
            .into(),
        ]);

        assert_eq!(fold_overwritten_temp_setups(&mut block), 0);
        assert_eq!(block.len(), 2);
    }

    #[test]
    fn names_runtime_prologue_roles_without_debug_metadata() {
        let lua_type = RcLocal::new(Local::new(None));
        let type_of = RcLocal::new(Local::new(None));
        let native_vector3 = RcLocal::new(Local::new(None));
        let success = RcLocal::new(Local::new(None));
        let result = RcLocal::new(Local::new(None));
        let vector3 = RcLocal::new(Local::new(None));
        let mut block = ast::Block(vec![
            ast::Assign::new(
                vec![lua_type.clone().into()],
                vec![ast::Global::from("type").into()],
            )
            .into(),
            ast::Assign::new(
                vec![type_of.clone().into()],
                vec![ast::Global::from("typeof").into()],
            )
            .into(),
            ast::Assign::new(
                vec![native_vector3.clone().into()],
                vec![ast::Global::from("Vector3").into()],
            )
            .into(),
            ast::Assign::new(
                vec![success.clone().into(), result.clone().into()],
                vec![ast::RValue::Select(ast::Select::Call(ast::Call::new(
                    ast::Global::from("pcall").into(),
                    vec![
                        ast::Global::from("require").into(),
                        ast::Literal::from("@lune/roblox").into(),
                    ],
                )))],
            )
            .into(),
            ast::Assign::new(
                vec![vector3.clone().into()],
                vec![native_vector3.clone().into()],
            )
            .into(),
        ]);

        seed_semantic_local_names(&mut block);

        assert_eq!(lua_type.0.0.lock().0.as_deref(), Some("luaType"));
        assert_eq!(type_of.0.0.lock().0.as_deref(), Some("typeOf"));
        assert_eq!(
            native_vector3.0.0.lock().0.as_deref(),
            Some("nativeVector3")
        );
        assert_eq!(success.0.0.lock().0.as_deref(), Some("success"));
        assert_eq!(result.0.0.lock().0.as_deref(), Some("result"));
        assert_eq!(vector3.0.0.lock().0.as_deref(), Some("vector3"));
    }

    #[test]
    fn keeps_captured_and_snapshot_locals_textually_distinct() {
        let outer_gun = RcLocal::new(Local::new(Some("gun".to_owned())));
        let snapshot = RcLocal::new(Local::new(Some("gun".to_owned())));
        let closure = ast::Closure {
            function: by_address::ByAddress(Arc::new(Mutex::new(ast::Function {
                body: ast::Block(vec![
                    {
                        let mut declaration = ast::Assign::new(
                            vec![snapshot.clone().into()],
                            vec![outer_gun.clone().into()],
                        );
                        declaration.prefix = true;
                        declaration.into()
                    },
                    ast::If::new(
                        ast::Binary::new(
                            snapshot.clone().into(),
                            outer_gun.clone().into(),
                            ast::BinaryOperation::Equal,
                        )
                        .into(),
                        ast::Block::default(),
                        ast::Block::default(),
                    )
                    .into(),
                ]),
                ..Default::default()
            }))),
            upvalues: vec![ast::Upvalue::Ref(outer_gun.clone())],
        };
        let closure_local = RcLocal::new(Local::new(Some("callback".to_owned())));
        let mut outer_declaration = ast::Assign::new(
            vec![outer_gun.clone().into()],
            vec![ast::Literal::Nil.into()],
        );
        outer_declaration.prefix = true;
        let mut closure_declaration =
            ast::Assign::new(vec![closure_local.into()], vec![closure.into()]);
        closure_declaration.prefix = true;
        let mut block = ast::Block(vec![outer_declaration.into(), closure_declaration.into()]);

        make_local_names_scope_safe(&mut block);

        assert_eq!(outer_gun.0.0.lock().0.as_deref(), Some("gun"));
        assert_eq!(snapshot.0.0.lock().0.as_deref(), Some("gun2"));
        let output = block.to_string();
        assert!(output.contains("local gun2 = gun"), "{output}");
        assert!(output.contains("gun2 == gun"), "{output}");
    }

    #[test]
    fn permits_matching_local_names_in_sibling_branches() {
        let condition = RcLocal::new(Local::new(Some("condition".to_owned())));
        let then_target = RcLocal::new(Local::new(Some("targetCFrame".to_owned())));
        let else_target = RcLocal::new(Local::new(Some("targetCFrame".to_owned())));
        let mut then_declaration = ast::Assign::new(
            vec![then_target.clone().into()],
            vec![ast::Literal::Nil.into()],
        );
        then_declaration.prefix = true;
        let mut else_declaration = ast::Assign::new(
            vec![else_target.clone().into()],
            vec![ast::Literal::Nil.into()],
        );
        else_declaration.prefix = true;
        let mut block = ast::Block(vec![ast::If::new(
            condition.into(),
            ast::Block(vec![then_declaration.into()]),
            ast::Block(vec![else_declaration.into()]),
        )
        .into()]);

        make_local_names_scope_safe(&mut block);

        assert_eq!(then_target.0.0.lock().0.as_deref(), Some("targetCFrame"));
        assert_eq!(else_target.0.0.lock().0.as_deref(), Some("targetCFrame"));
    }

    #[test]
    fn normalizes_empty_then_branch_into_a_guard() {
        let condition = RcLocal::new(Local::new(Some("ready".to_owned())));
        let mut block = ast::Block(vec![ast::If::new(
            condition.into(),
            ast::Block::default(),
            ast::Block(vec![ast::Return::new(Vec::new()).into()]),
        )
        .into()]);

        normalize_empty_guard_branches(&mut block);

        let Statement::If(value) = &block[0] else {
            panic!("expected an if statement");
        };
        assert!(value.else_block.lock().is_empty());
        assert_eq!(value.then_block.lock().len(), 1);
        assert!(matches!(value.condition, ast::RValue::Unary(_)));
        assert!(block.to_string().starts_with("if not ready then"));
    }

    #[test]
    fn prefers_argument_roles_over_generic_property_names() {
        fn index(left: ast::RValue, key: &str) -> ast::RValue {
            ast::Index::new(left, ast::Literal::from(key).into()).into()
        }

        let attachment = RcLocal::new(Local::new(Some("attachment".to_owned())));
        let weapon_service = RcLocal::new(Local::new(Some("weaponService".to_owned())));
        let tool = RcLocal::new(Local::new(Some("tool".to_owned())));
        let shot_origin = RcLocal::new(Local::new(None));
        let target_position = RcLocal::new(Local::new(None));
        let mut block = ast::Block(vec![
            ast::Assign::new(
                vec![shot_origin.clone().into()],
                vec![index(attachment.into(), "WorldCFrame")],
            )
            .into(),
            ast::Assign::new(
                vec![target_position.clone().into()],
                vec![ast::MethodCall::new(
                    weapon_service.into(),
                    "GetTargetPosition".to_owned(),
                    vec![ast::Literal::Number(1.0).into(), ast::Literal::Number(2.0).into()],
                )
                .into()],
            )
            .into(),
            ast::MethodCall::new(
                index(tool.into(), "Shoot"),
                "FireServer".to_owned(),
                vec![shot_origin.clone().into(), target_position.clone().into()],
            )
            .into(),
        ]);

        seed_semantic_local_names(&mut block);

        assert_eq!(
            shot_origin.0.0.lock().0.as_deref(),
            Some("shotOriginCFrame")
        );
        assert_eq!(
            target_position.0.0.lock().0.as_deref(),
            Some("targetPosition")
        );
    }

    #[test]
    fn avoids_shadowing_referenced_globals() {
        let timestamp = RcLocal::new(Local::new(Some("tick".to_owned())));
        let mut declaration = ast::Assign::new(
            vec![timestamp.clone().into()],
            vec![ast::Call::new(ast::Global::from("tick").into(), Vec::new()).into()],
        );
        declaration.prefix = true;
        let mut block = ast::Block(vec![declaration.into()]);

        make_local_names_scope_safe(&mut block);

        assert_eq!(timestamp.0.0.lock().0.as_deref(), Some("tick2"));
        assert_eq!(block.to_string(), "local tick2 = tick()");
    }

    #[test]
    fn names_services_requires_and_instance_paths() {
        fn index(left: ast::RValue, key: &str) -> ast::RValue {
            ast::Index::new(left, ast::Literal::from(key).into()).into()
        }

        let run_service = RcLocal::new(Local::new(None));
        let replicated_storage = RcLocal::new(Local::new(None));
        let tween_helper = RcLocal::new(Local::new(None));
        let ui_animate = RcLocal::new(Local::new(None));
        let intro_sequence = RcLocal::new(Local::new(None));
        let current_room = RcLocal::new(Local::new(None));
        let game_status = RcLocal::new(Local::new(None));
        let raycast_params = RcLocal::new(Local::new(None));
        let mut block = ast::Block(vec![
            ast::Assign::new(
                vec![run_service.clone().into()],
                vec![
                    ast::MethodCall::new(
                        ast::Global::from("game").into(),
                        "GetService".to_owned(),
                        vec![ast::Literal::from("RunService").into()],
                    )
                    .into(),
                ],
            )
            .into(),
            ast::Assign::new(
                vec![ui_animate.clone().into()],
                vec![
                    ast::Call::new(
                        ast::Global::from("require").into(),
                        vec![index(
                            index(replicated_storage.clone().into(), "Packages"),
                            "UIAnimate",
                        )],
                    )
                    .into(),
                ],
            )
            .into(),
            ast::Assign::new(
                vec![replicated_storage.clone().into()],
                vec![
                    ast::MethodCall::new(
                        ast::Global::from("game").into(),
                        "GetService".to_owned(),
                        vec![ast::Literal::from("ReplicatedStorage").into()],
                    )
                    .into(),
                ],
            )
            .into(),
            ast::Assign::new(
                vec![tween_helper.clone().into()],
                vec![
                    ast::Call::new(
                        ast::Global::from("require").into(),
                        vec![index(
                            index(replicated_storage.clone().into(), "Scripts"),
                            "TweenHelper",
                        )],
                    )
                    .into(),
                ],
            )
            .into(),
            ast::Assign::new(
                vec![intro_sequence.clone().into()],
                vec![
                    ast::Call::new(
                        ast::Global::from("require").into(),
                        vec![index(ast::Global::from("script").into(), "IntroSequence")],
                    )
                    .into(),
                ],
            )
            .into(),
            ast::Assign::new(
                vec![current_room.clone().into()],
                vec![index(
                    index(
                        index(ast::Global::from("workspace").into(), "Values"),
                        "CurrentRoom",
                    ),
                    "Value",
                )],
            )
            .into(),
            ast::Assign::new(
                vec![game_status.clone().into()],
                vec![
                    ast::MethodCall::new(
                        current_room.clone().into(),
                        "WaitForChild".to_owned(),
                        vec![ast::Literal::from("GameStatus").into()],
                    )
                    .into(),
                ],
            )
            .into(),
            ast::Assign::new(
                vec![raycast_params.clone().into()],
                vec![
                    ast::Call::new(
                        index(ast::Global::from("RaycastParams").into(), "new"),
                        Vec::new(),
                    )
                    .into(),
                ],
            )
            .into(),
        ]);

        seed_semantic_local_names(&mut block);

        assert_eq!(run_service.0.0.lock().0.as_deref(), Some("runService"));
        assert_eq!(
            replicated_storage.0.0.lock().0.as_deref(),
            Some("replicatedStorage")
        );
        assert_eq!(tween_helper.0.0.lock().0.as_deref(), Some("tweenHelper"));
        assert_eq!(ui_animate.0.0.lock().0.as_deref(), Some("uiAnimate"));
        assert_eq!(
            intro_sequence.0.0.lock().0.as_deref(),
            Some("introSequence")
        );
        assert_eq!(current_room.0.0.lock().0.as_deref(), Some("currentRoom"));
        assert_eq!(game_status.0.0.lock().0.as_deref(), Some("gameStatus"));
        assert_eq!(
            raycast_params.0.0.lock().0.as_deref(),
            Some("raycastParams")
        );
    }

    #[test]
    fn names_returned_module_tables_and_explicit_function_aliases() {
        let module = RcLocal::new(Local::new(None));
        let cleanup = RcLocal::new(Local::new(None));
        let closure = ast::Closure {
            function: by_address::ByAddress(Arc::new(Mutex::new(ast::Function::default()))),
            upvalues: Vec::new(),
        };
        let mut block = ast::Block(vec![
            ast::Assign::new(
                vec![module.clone().into()],
                vec![ast::Table::default().into()],
            )
            .into(),
            ast::Assign::new(vec![cleanup.clone().into()], vec![closure.into()]).into(),
            ast::Assign::new(
                vec![LValue::Index(ast::Index::new(
                    module.clone().into(),
                    ast::Literal::from("Cleanup").into(),
                ))],
                vec![cleanup.clone().into()],
            )
            .into(),
            ast::Assign::new(
                vec![LValue::Index(ast::Index::new(
                    module.clone().into(),
                    ast::Literal::from("Version").into(),
                ))],
                vec![ast::Literal::Number(2.0).into()],
            )
            .into(),
            ast::Return::new(vec![module.clone().into()]).into(),
        ]);

        seed_semantic_local_names(&mut block);

        assert_eq!(module.0.0.lock().0.as_deref(), Some("module"));
        assert_eq!(cleanup.0.0.lock().0.as_deref(), Some("cleanup"));
    }

    #[test]
    fn names_tagged_table_constructors_and_parameters() {
        let side = RcLocal::new(Local::new(Some("left".to_owned())));
        let text = RcLocal::new(Local::new(None));
        let time = RcLocal::new(Local::new(None));
        let cipher = RcLocal::new(Local::new(None));
        let constructor = RcLocal::new(Local::new(None));
        let function = ast::Function {
            parameters: vec![text.clone(), time.clone(), cipher.clone()],
            body: ast::Block(vec![
                ast::Return::new(vec![
                    ast::Table(vec![
                        (Some(ast::Literal::from("Type").into()), side.clone().into()),
                        (Some(ast::Literal::from("Text").into()), text.clone().into()),
                        (Some(ast::Literal::from("Time").into()), time.clone().into()),
                        (
                            Some(ast::Literal::from("Cipher").into()),
                            cipher.clone().into(),
                        ),
                    ])
                    .into(),
                ])
                .into(),
            ]),
            ..Default::default()
        };
        let closure = ast::Closure {
            function: by_address::ByAddress(Arc::new(Mutex::new(function))),
            upvalues: vec![ast::Upvalue::Copy(side)],
        };
        let mut block = ast::Block(vec![
            ast::Assign::new(vec![constructor.clone().into()], vec![closure.into()]).into(),
        ]);

        seed_semantic_local_names(&mut block);

        assert_eq!(constructor.0.0.lock().0.as_deref(), Some("makeLeft"));
        assert_eq!(text.0.0.lock().0.as_deref(), Some("text"));
        assert_eq!(time.0.0.lock().0.as_deref(), Some("time"));
        assert_eq!(cipher.0.0.lock().0.as_deref(), Some("cipher"));
    }

    #[test]
    fn propagates_table_roles_from_fields_and_call_parameters() {
        let type_table = RcLocal::new(Local::new(None));
        let left = RcLocal::new(Local::new(None));
        let cipher_table = RcLocal::new(Local::new(None));
        let text = RcLocal::new(Local::new(None));
        let time = RcLocal::new(Local::new(None));
        let cipher = RcLocal::new(Local::new(None));
        let constructor = RcLocal::new(Local::new(None));
        let result = RcLocal::new(Local::new(None));
        let function = ast::Function {
            parameters: vec![text.clone(), time.clone(), cipher.clone()],
            body: ast::Block(vec![
                ast::Return::new(vec![
                    ast::Table(vec![
                        (Some(ast::Literal::from("Type").into()), left.clone().into()),
                        (Some(ast::Literal::from("Text").into()), text.clone().into()),
                        (Some(ast::Literal::from("Time").into()), time.clone().into()),
                        (
                            Some(ast::Literal::from("Cipher").into()),
                            cipher.clone().into(),
                        ),
                    ])
                    .into(),
                ])
                .into(),
            ]),
            ..Default::default()
        };
        let closure = ast::Closure {
            function: by_address::ByAddress(Arc::new(Mutex::new(function))),
            upvalues: vec![ast::Upvalue::Copy(left.clone())],
        };
        let mut block = ast::Block(vec![
            ast::Assign::new(
                vec![type_table.clone().into()],
                vec![
                    ast::Table(vec![(
                        Some(ast::Literal::from("Left").into()),
                        ast::Literal::Number(0.0).into(),
                    )])
                    .into(),
                ],
            )
            .into(),
            ast::Assign::new(
                vec![left.clone().into()],
                vec![
                    ast::Index::new(type_table.clone().into(), ast::Literal::from("Left").into())
                        .into(),
                ],
            )
            .into(),
            ast::Assign::new(
                vec![cipher_table.clone().into()],
                vec![
                    ast::Table(vec![(
                        Some(ast::Literal::from("Person").into()),
                        ast::Literal::Number(1.0).into(),
                    )])
                    .into(),
                ],
            )
            .into(),
            ast::Assign::new(vec![constructor.clone().into()], vec![closure.into()]).into(),
            ast::Assign::new(
                vec![result.into()],
                vec![
                    ast::Call::new(
                        constructor.into(),
                        vec![
                            ast::Literal::from("hello").into(),
                            ast::Literal::Nil.into(),
                            ast::Index::new(
                                cipher_table.clone().into(),
                                ast::Literal::from("Person").into(),
                            )
                            .into(),
                        ],
                    )
                    .into(),
                ],
            )
            .into(),
        ]);

        seed_semantic_local_names(&mut block);

        assert_eq!(type_table.0.0.lock().0.as_deref(), Some("type"));
        assert_eq!(cipher_table.0.0.lock().0.as_deref(), Some("cipher"));
    }

    #[test]
    fn lowers_internal_loop_initializers_before_formatting() {
        let generator = RcLocal::new(Local::new(None));
        let state = RcLocal::new(Local::new(None));
        let control = RcLocal::new(Local::new(None));
        let block = ast::Block(vec![
            ast::GenericForInit::new(generator, state, control).into(),
        ]);
        let lowered = lower_state_machine_initializers(block);
        assert!(matches!(lowered.first(), Some(Statement::Assign(_))));
        assert!(!lowered.to_string().contains("[internal control]"));
    }

    #[test]
    fn preserves_numeric_for_first_iteration_in_state_machine_lowering() {
        let counter = RcLocal::new(Local::new(Some("counter".to_owned())));
        let limit = RcLocal::new(Local::new(Some("limit".to_owned())));
        let step = RcLocal::new(Local::new(Some("step".to_owned())));
        let block = ast::Block(vec![ast::NumForInit::new(counter, limit, step).into()]);

        let lowered = lower_state_machine_initializers(block);
        assert!(lowered.to_string().contains("counter - step"));
    }

    #[test]
    fn preserves_generalized_iteration_in_state_machine_lowering() {
        let generator = RcLocal::new(Local::new(Some("generator".to_owned())));
        let state = RcLocal::new(Local::new(Some("state".to_owned())));
        let control = RcLocal::new(Local::new(Some("control".to_owned())));
        let block = ast::Block(vec![
            ast::GenericForInit::new_generalized(generator, state, control).into(),
        ]);

        let lowered = lower_state_machine_initializers(block);
        let output = lowered.to_string();
        assert!(output.contains("getmetatable(generator)"));
        assert!(output.contains("__iter"));
        assert!(output.contains("next"));
        assert!(!output.contains("GenericForInit"));
    }
}
