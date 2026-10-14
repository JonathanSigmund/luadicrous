use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::{Duration, Instant};

use luadicrous_bytecode::{Opcode, ParsedChunk, decode_prototype};

pub(super) fn decompile(chunk: &ParsedChunk) -> Result<String, String> {
    let started = Instant::now();
    let worker_chunk = chunk.clone();
    let worker = std::thread::Builder::new()
        .name("luadicrous-structured".to_owned())
        .stack_size(64 * 1024 * 1024)
        .spawn(move || {
            catch_unwind(AssertUnwindSafe(|| {
                luau_lifter::decompile_parsed_chunk(&worker_chunk)
            }))
        })
        .map_err(|error| format!("failed to start structured worker: {error}"))?;
    let source = worker
        .join()
        .map_err(|payload| format!("structured worker panicked: {}", panic_message(payload)))?
        .map_err(|payload| format!("structured lifter panicked: {}", panic_message(payload)))?
        .map_err(|error| error.to_string())?;

    if source.trim().is_empty() {
        if !is_empty_main(&chunk) {
            return Err("structured lifter emitted no source".to_owned());
        }
        return Ok(format!(
            "-- Decompiled by Luadicrous V2\n-- Decompiled in {}\nreturn\n",
            format_duration(started.elapsed())
        ));
    }
    if source.contains("failed to decompile") {
        return Err("one or more functions failed structured recovery".to_owned());
    }
    if contains_lua_goto_syntax(&source) {
        return Err("structured recovery left non-Luau control-flow syntax".to_owned());
    }

    Ok(format!(
        "-- Decompiled by Luadicrous V2\n-- Decompiled in {}\n{source}",
        format_duration(started.elapsed())
    ))
}

fn is_empty_main(chunk: &ParsedChunk) -> bool {
    let Some(prototype) = chunk.prototypes.get(chunk.main_proto as usize) else {
        return false;
    };
    if chunk.prototypes.len() != 1 || !prototype.constants.is_empty() || !prototype.child_protos.is_empty() {
        return false;
    }
    let decoded = decode_prototype(&prototype.instructions, chunk.header.luau_version);
    if decoded.quality.unknown_opcodes != 0 || decoded.quality.truncated_aux != 0 {
        return false;
    }
    matches!(decoded.instructions.as_slice(),
        [instruction] if instruction.opcode == Some(Opcode::Return) && instruction.b() == 1
    ) || matches!(decoded.instructions.as_slice(),
        [prepare, instruction] if prepare.opcode == Some(Opcode::PrepVarargs)
            && instruction.opcode == Some(Opcode::Return) && instruction.b() == 1
    )
}

fn format_duration(duration: Duration) -> String {
    if duration.as_secs() == 0 {
        format!("{:.1} ms", duration.as_secs_f64() * 1_000.0)
    } else {
        format!("{:.3} s", duration.as_secs_f64())
    }
}

fn contains_lua_goto_syntax(source: &str) -> bool {
    source.lines().any(|line| {
        let line = line.trim();
        line.starts_with("goto ")
            || (line.len() > 4 && line.starts_with("::") && line.ends_with("::"))
    })
}

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_owned()
    } else {
        "unknown panic".to_owned()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{contains_lua_goto_syntax, format_duration};

    #[test]
    fn rejects_only_actual_lua_goto_syntax() {
        assert!(contains_lua_goto_syntax("goto retry"));
        assert!(contains_lua_goto_syntax("  ::retry::  "));
        assert!(!contains_lua_goto_syntax(
            "local typed = value :: string\nlocal text = \"::retry::\""
        ));
    }

    #[test]
    fn formats_decompile_duration_for_humans() {
        assert_eq!(format_duration(Duration::from_micros(12_345)), "12.3 ms");
        assert_eq!(format_duration(Duration::from_millis(1_250)), "1.250 s");
    }
}
