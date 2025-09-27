use alloc::{string::ToString, vec::Vec};
use elibc::graphics::{Color, GraphicsError};

use super::state::{MARGIN, TerminalState};

pub(super) fn render(state: &mut TerminalState) -> Result<(), GraphicsError> {
    if !state.is_dirty() {
        return Ok(());
    }

    state.screen.fill(Color::BLACK)?;

    for (line_idx, line) in state.buffer.iter().enumerate() {
        if line_idx >= state.max_output_lines {
            break;
        }

        if line.is_empty() {
            continue;
        }

        let y_pos = (line_idx as u64) * state.line_height + MARGIN;
        state
            .screen
            .draw_text(MARGIN, y_pos, line, &state.text_style)?;
    }

    let input_y = state.screen.height() as u64 - state.line_height - MARGIN;
    state
        .screen
        .draw_text(MARGIN, input_y, &state.prompt_text, &state.prompt_style)?;

    if !state.input_line.is_empty() {
        let prompt_width = (state.prompt_text.len() as u64) * state.char_width;
        state.screen.draw_text(
            prompt_width + MARGIN,
            input_y,
            &state.input_line,
            &state.text_style,
        )?;
    }

    draw_profiling_overlay(state, input_y)?;

    let prompt_width = (state.prompt_text.len() as u64) * state.char_width;
    let cursor_x = prompt_width + (state.cursor_x as u64) * state.char_width + MARGIN;
    let cursor_y = input_y;

    state
        .screen
        .draw_rect(cursor_x, cursor_y, 2, state.char_height, Color::WHITE)?;

    state.screen.render()?;
    state.clear_dirty();
    Ok(())
}

fn draw_profiling_overlay(state: &mut TerminalState, input_y: u64) -> Result<(), GraphicsError> {
    let overlay_text = state.profiling_overlay().to_string();
    if overlay_text.is_empty() {
        return Ok(());
    }

    let lines: Vec<&str> = overlay_text
        .lines()
        .filter(|line| !line.is_empty())
        .collect();

    if lines.is_empty() {
        return Ok(());
    }

    let screen_width = state.screen.width() as u64;
    let total_lines = lines.len();

    for (idx, line) in lines.into_iter().enumerate() {
        let chars = line.chars().count() as u64;
        let text_width = chars.saturating_mul(state.char_width);
        let x_offset = text_width.saturating_add(MARGIN);
        let x = if x_offset >= screen_width {
            MARGIN
        } else {
            screen_width - x_offset
        };

        let line_height = state.line_height;
        let y = input_y
            .saturating_sub((total_lines - idx) as u64 * line_height)
            .saturating_sub(MARGIN);

        state.screen.draw_text(x, y, line, &state.prompt_style)?;
    }

    Ok(())
}
