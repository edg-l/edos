//! Script execution: file-based shell script parsing and control flow.

use crate::command;
use crate::run_chain;

// ---------------------------------------------------------------------------
// Block AST
// ---------------------------------------------------------------------------

/// A parsed shell script block.
enum Block {
    /// A single command line.
    Simple(String),
    /// An if/elif/else/end construct.
    If {
        condition: String,
        body: Vec<Block>,
        elifs: Vec<(String, Vec<Block>)>,
        else_body: Option<Vec<Block>>,
    },
    /// A while loop.
    While { condition: String, body: Vec<Block> },
    /// A for loop.
    For {
        var: String,
        values: Vec<String>,
        body: Vec<Block>,
    },
}

// ---------------------------------------------------------------------------
// Flow control signals
// ---------------------------------------------------------------------------

enum FlowControl {
    /// Normal execution; carries the last exit code.
    Normal(i32),
    /// `break` was encountered inside a loop.
    Break,
    /// `continue` was encountered inside a loop.
    Continue,
    /// `exit` was called or set -e triggered; carries the exit code.
    Exit(i32),
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

/// Parse a slice of (already comment-stripped, trimmed) non-empty lines into
/// a list of Blocks.
fn parse_blocks(lines: &[&str]) -> Result<Vec<Block>, String> {
    let mut idx = 0usize;
    parse_block_list(lines, &mut idx, false)
}

/// Parse lines starting at `*idx` into a Vec<Block>, stopping when it
/// encounters `end`, `elif`, `else`, or end-of-input.
///
/// `top_level` controls whether `end`/`elif`/`else` are valid terminators
/// (they are not at the top level).
fn parse_block_list(lines: &[&str], idx: &mut usize, nested: bool) -> Result<Vec<Block>, String> {
    let mut blocks = Vec::new();

    while *idx < lines.len() {
        let line = lines[*idx];
        let first_word = line.split_whitespace().next().unwrap_or("");

        // Terminators that end nested blocks
        if nested && matches!(first_word, "end" | "elif" | "else") {
            // Don't consume; let the caller handle it.
            break;
        }

        match first_word {
            "if" => {
                let condition = line["if".len()..].trim().to_string();
                if condition.is_empty() {
                    return Err(format!("if: missing condition on line: {}", line));
                }
                *idx += 1;
                let body = parse_block_list(lines, idx, true)?;

                let mut elifs = Vec::new();
                let mut else_body = None;

                // Consume elif/else/end
                loop {
                    if *idx >= lines.len() {
                        return Err("if: missing 'end'".to_string());
                    }
                    let kw = lines[*idx].split_whitespace().next().unwrap_or("");
                    match kw {
                        "elif" => {
                            let elif_cond = lines[*idx]["elif".len()..].trim().to_string();
                            if elif_cond.is_empty() {
                                return Err(format!(
                                    "elif: missing condition on line: {}",
                                    lines[*idx]
                                ));
                            }
                            *idx += 1;
                            let elif_body = parse_block_list(lines, idx, true)?;
                            elifs.push((elif_cond, elif_body));
                        }
                        "else" => {
                            *idx += 1;
                            let eb = parse_block_list(lines, idx, true)?;
                            else_body = Some(eb);
                            // After else we expect end
                            if *idx >= lines.len()
                                || lines[*idx].split_whitespace().next() != Some("end")
                            {
                                return Err("if/else: missing 'end'".to_string());
                            }
                            *idx += 1; // consume end
                            break;
                        }
                        "end" => {
                            *idx += 1;
                            break;
                        }
                        _ => {
                            return Err(format!("if: unexpected keyword '{}' inside if block", kw));
                        }
                    }
                }

                blocks.push(Block::If {
                    condition,
                    body,
                    elifs,
                    else_body,
                });
            }

            "while" => {
                let condition = line["while".len()..].trim().to_string();
                if condition.is_empty() {
                    return Err(format!("while: missing condition on line: {}", line));
                }
                *idx += 1;
                let body = parse_block_list(lines, idx, true)?;
                if *idx >= lines.len() || lines[*idx].split_whitespace().next() != Some("end") {
                    return Err("while: missing 'end'".to_string());
                }
                *idx += 1; // consume end
                blocks.push(Block::While { condition, body });
            }

            "for" => {
                // Syntax: for <var> in <value1> [value2 ...]
                let rest = line["for".len()..].trim();
                let (var, values) = parse_for_header(rest)?;
                *idx += 1;
                let body = parse_block_list(lines, idx, true)?;
                if *idx >= lines.len() || lines[*idx].split_whitespace().next() != Some("end") {
                    return Err("for: missing 'end'".to_string());
                }
                *idx += 1; // consume end
                blocks.push(Block::For { var, values, body });
            }

            "end" => {
                if nested {
                    // Caller will handle this
                    break;
                } else {
                    return Err("unexpected 'end' at top level".to_string());
                }
            }

            "elif" | "else" => {
                if nested {
                    break;
                } else {
                    return Err(format!("unexpected '{}' at top level", first_word));
                }
            }

            _ => {
                blocks.push(Block::Simple(line.to_string()));
                *idx += 1;
            }
        }
    }

    Ok(blocks)
}

/// Parse the header of a for loop: `<var> in <value...>`.
/// Values are space-separated and go through `expand_variables`/`expand_tilde`.
fn parse_for_header(rest: &str) -> Result<(String, Vec<String>), String> {
    let mut parts = rest.splitn(2, "in");
    let var_part = parts
        .next()
        .ok_or_else(|| "for: missing variable name".to_string())?
        .trim();
    let values_part = parts
        .next()
        .ok_or_else(|| "for: missing 'in' keyword".to_string())?
        .trim();

    if var_part.is_empty() {
        return Err("for: missing variable name".to_string());
    }
    // Variable name must not contain spaces
    let var = var_part.to_string();

    // Parse values respecting quotes (reuse parse_command logic)
    let expanded = command::expand_variables(values_part);
    let values: Vec<String> = command::parse_command(&expanded)
        .into_iter()
        .map(|v| command::expand_tilde(&v))
        .collect();

    Ok((var, values))
}

// ---------------------------------------------------------------------------
// Executor
// ---------------------------------------------------------------------------

fn execute_blocks(blocks: &[Block]) -> FlowControl {
    let mut last_code = 0i32;
    for block in blocks {
        match execute_block(block) {
            FlowControl::Normal(code) => {
                last_code = code;
            }
            other => return other,
        }
    }
    FlowControl::Normal(last_code)
}

fn execute_block(block: &Block) -> FlowControl {
    match block {
        Block::Simple(line) => {
            // Check for bare break/continue keywords before executing
            let trimmed = line.trim();
            let first_word = trimmed.split_whitespace().next().unwrap_or("");

            if first_word == "break" {
                return FlowControl::Break;
            }
            if first_word == "continue" {
                return FlowControl::Continue;
            }

            let code = run_chain(line);
            if code == -1 {
                return FlowControl::Exit(0);
            }
            command::set_last_exit_code(code);

            if code != 0 && command::exit_on_error() {
                return FlowControl::Exit(code);
            }

            FlowControl::Normal(code)
        }

        Block::If {
            condition,
            body,
            elifs,
            else_body,
        } => {
            let cond_code = run_chain(condition);
            if cond_code == -1 {
                return FlowControl::Exit(0);
            }

            if cond_code == 0 {
                // Condition true: execute body
                execute_blocks(body)
            } else {
                // Try elif branches
                for (elif_cond, elif_body) in elifs {
                    let ec = run_chain(elif_cond);
                    if ec == -1 {
                        return FlowControl::Exit(0);
                    }
                    if ec == 0 {
                        return execute_blocks(elif_body);
                    }
                }
                // Execute else body if present
                if let Some(eb) = else_body {
                    execute_blocks(eb)
                } else {
                    FlowControl::Normal(0)
                }
            }
        }

        Block::While { condition, body } => {
            let mut last_code = 0i32;
            loop {
                let cond_code = run_chain(condition);
                if cond_code == -1 {
                    return FlowControl::Exit(0);
                }
                if cond_code != 0 {
                    // Condition false: exit loop
                    break;
                }

                match execute_blocks(body) {
                    FlowControl::Normal(code) => {
                        last_code = code;
                    }
                    FlowControl::Break => break,
                    FlowControl::Continue => continue,
                    other => return other,
                }
            }
            FlowControl::Normal(last_code)
        }

        Block::For { var, values, body } => {
            let mut last_code = 0i32;
            for value in values {
                // Set loop variable in environment
                unsafe { std::env::set_var(var, value) };

                match execute_blocks(body) {
                    FlowControl::Normal(code) => {
                        last_code = code;
                    }
                    FlowControl::Break => break,
                    FlowControl::Continue => continue,
                    other => return other,
                }
            }
            FlowControl::Normal(last_code)
        }
    }
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Execute a shell script file.
///
/// `args[0]` should be the script path; subsequent elements are positional
/// parameters ($1, $2, ...).
///
/// Returns the exit code (0 = success).
pub fn run_script(path: &str, args: &[String]) -> i32 {
    command::set_script_args(args);

    let source = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("sh: {}: {}", path, e);
            return 127;
        }
    };

    // Collect lines, stripping shebang and comments, skipping empty lines
    let raw_lines: Vec<&str> = source.lines().collect();
    let start = if raw_lines
        .first()
        .map(|l| l.starts_with("#!"))
        .unwrap_or(false)
    {
        1 // skip shebang
    } else {
        0
    };

    let lines: Vec<&str> = raw_lines[start..]
        .iter()
        .map(|l| command::strip_comment(l).trim())
        .filter(|l| !l.is_empty())
        .collect();

    let blocks = match parse_blocks(&lines) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("sh: {}: parse error: {}", path, e);
            return 1;
        }
    };

    match execute_blocks(&blocks) {
        FlowControl::Normal(code) => code,
        FlowControl::Exit(code) => code,
        FlowControl::Break => {
            eprintln!("sh: break outside of loop");
            1
        }
        FlowControl::Continue => {
            eprintln!("sh: continue outside of loop");
            1
        }
    }
}
