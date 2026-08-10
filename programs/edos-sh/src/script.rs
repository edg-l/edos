//! Script execution: file-based shell script parsing and control flow.

use crate::command;
use crate::run_chain;

// ---------------------------------------------------------------------------
// Block AST
// ---------------------------------------------------------------------------

/// A parsed shell script block.
#[derive(Clone)]
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
    /// A function definition.
    FunctionDef { name: String, body: Vec<Block> },
    /// A command with a heredoc body: the command line, the heredoc content,
    /// and whether variable expansion is suppressed (raw=true for `<<'MARKER'`).
    Heredoc {
        line: String,
        content: String,
        raw: bool,
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
    /// `return` was called inside a function; carries the return code.
    Return(i32),
}

// ---------------------------------------------------------------------------
// Function registry
// ---------------------------------------------------------------------------

/// Registered shell functions: (name, body).
/// Vec of tuples is used because HashMap::new() is not const on this target.
static FUNCTIONS: std::sync::Mutex<Vec<(String, Vec<Block>)>> = std::sync::Mutex::new(Vec::new());

/// Register a function, replacing any existing function with the same name.
fn register_function(name: String, body: Vec<Block>) {
    let mut fns = FUNCTIONS.lock().unwrap();
    if let Some(entry) = fns.iter_mut().find(|(n, _)| n == &name) {
        entry.1 = body;
    } else {
        fns.push((name, body));
    }
}

/// Look up a function by name and return a clone of its body.
fn lookup_function(name: &str) -> Option<Vec<Block>> {
    FUNCTIONS
        .lock()
        .unwrap()
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, body)| body.clone())
}

/// Check whether a function with the given name exists.
pub fn is_function(name: &str) -> bool {
    FUNCTIONS.lock().unwrap().iter().any(|(n, _)| n == name)
}

/// Return the names of all registered functions (for tab completion).
pub fn function_names() -> Vec<String> {
    FUNCTIONS
        .lock()
        .unwrap()
        .iter()
        .map(|(n, _)| n.clone())
        .collect()
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
        // Skip blank lines and comment-only lines.
        // We do this here (rather than pre-filtering in run_script) so that
        // raw heredoc body lines are preserved between the marker and its delimiter.
        let stripped = command::strip_comment(lines[*idx]).trim();
        if stripped.is_empty() {
            *idx += 1;
            continue;
        }
        let line = stripped;
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

            "function" => {
                let name = line["function".len()..].trim().to_string();
                if name.is_empty() {
                    return Err("function: missing name".to_string());
                }
                *idx += 1;
                let body = parse_block_list(lines, idx, true)?;
                if *idx >= lines.len() || lines[*idx].split_whitespace().next() != Some("end") {
                    return Err(format!("function {}: missing 'end'", name));
                }
                *idx += 1; // consume end
                blocks.push(Block::FunctionDef { name, body });
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
                if let Some((cleaned_line, marker, raw)) = command::parse_heredoc_marker(line) {
                    // Advance past the command line itself
                    *idx += 1;
                    // Consume raw lines until one matches the marker exactly.
                    // We use the raw (un-stripped) lines so blank lines inside
                    // the heredoc body are preserved.
                    let mut content_lines: Vec<String> = Vec::new();
                    while *idx < lines.len() {
                        let raw_line = lines[*idx];
                        *idx += 1;
                        if raw_line.trim() == marker {
                            break;
                        }
                        content_lines.push(raw_line.to_string());
                    }
                    let content = content_lines.join("\n");
                    blocks.push(Block::Heredoc {
                        line: cleaned_line,
                        content,
                        raw,
                    });
                } else {
                    blocks.push(Block::Simple(line.to_string()));
                    *idx += 1;
                }
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

    // The word list is a command line's worth of words: quotes, tilde and
    // pathname expansion all apply, so `for f in *.txt` iterates the matches.
    let expanded = command::expand_variables(values_part);
    let values = crate::glob::expand_words(&command::parse_command(&expanded));

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
            if first_word == "return" {
                let rest = trimmed["return".len()..].trim();
                let code: i32 = if rest.is_empty() {
                    0
                } else {
                    rest.parse().unwrap_or(0)
                };
                return FlowControl::Return(code);
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

        Block::FunctionDef { name, body } => {
            register_function(name.clone(), body.clone());
            FlowControl::Normal(0)
        }

        Block::Heredoc { line, content, raw } => {
            // Expand variables in the heredoc content unless the marker was quoted.
            let expanded_content = if *raw {
                content.clone()
            } else {
                command::expand_variables(content)
            };

            let (read_fd, write_fd) = match edos_lib::process::pipe() {
                Some(fds) => fds,
                None => {
                    eprintln!("sh: pipe failed for heredoc");
                    return FlowControl::Normal(1);
                }
            };

            // Write content followed by a trailing newline into the write end.
            if !expanded_content.is_empty() {
                let bytes = format!("{}\n", expanded_content).into_bytes();
                edos_lib::process::write(write_fd, &bytes);
            }
            edos_lib::process::close(write_fd);

            // Run the command with the read end as its stdin.
            let code = crate::run_segment_with_stdin(line, read_fd);
            edos_lib::process::close(read_fd);

            command::set_last_exit_code(code);
            if code != 0 && command::exit_on_error() {
                return FlowControl::Exit(code);
            }
            FlowControl::Normal(code)
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

    // Collect all raw lines (blank lines and heredoc content must be preserved).
    // Blank/comment filtering is done inside parse_block_list so heredoc body
    // lines are not accidentally discarded.
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

    let lines: Vec<&str> = raw_lines[start..].iter().copied().collect();

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
        FlowControl::Return(_) => {
            eprintln!("sh: return outside of function");
            1
        }
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

/// Call a registered shell function with the given argument list.
///
/// Saves and restores positional parameters around the call.
pub fn call_function(name: &str, args: &[String]) -> i32 {
    let body = match lookup_function(name) {
        Some(b) => b,
        None => {
            eprintln!("sh: {}: not a function", name);
            return 127;
        }
    };
    // Save positional params
    let saved = command::get_all_script_args();
    // Set function params: $0 = function name, $1.. = args
    let mut func_args = vec![name.to_string()];
    func_args.extend_from_slice(args);
    command::set_script_args(&func_args);
    // Execute body
    let result = execute_blocks(&body);
    // Restore positional params
    command::set_script_args(&saved);
    match result {
        FlowControl::Normal(code) | FlowControl::Return(code) => code,
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

/// Parse body lines and register the function under `name`.
pub fn parse_and_register_function(name: &str, body_lines: &[String]) -> Result<(), String> {
    let lines: Vec<&str> = body_lines.iter().map(|s| s.as_str()).collect();
    let blocks = parse_blocks(&lines)?;
    register_function(name.to_string(), blocks);
    Ok(())
}
