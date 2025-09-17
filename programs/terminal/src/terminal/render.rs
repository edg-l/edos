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
