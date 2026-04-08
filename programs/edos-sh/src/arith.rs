//! Arithmetic expression evaluator for `$(( expr ))` expansion.

struct Parser<'a> {
    input: &'a str,
    pos: usize,
}

impl<'a> Parser<'a> {
    fn new(input: &'a str) -> Self {
        Parser { input, pos: 0 }
    }

    fn skip_whitespace(&mut self) {
        while self.pos < self.input.len() && self.input.as_bytes()[self.pos].is_ascii_whitespace() {
            self.pos += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.input.as_bytes().get(self.pos).copied()
    }

    fn advance(&mut self) {
        self.pos += 1;
    }

    fn parse_expr(&mut self) -> Result<i64, String> {
        self.parse_assignment()
    }

    /// Handle `name = expr` assignment, otherwise fall through to comparison.
    fn parse_assignment(&mut self) -> Result<i64, String> {
        // We need look-ahead: if what follows is `ident =` (not `==`), treat as assignment.
        let saved_pos = self.pos;
        self.skip_whitespace();

        // Try to read an identifier
        let ident_start = self.pos;
        while self.pos < self.input.len() {
            let b = self.input.as_bytes()[self.pos];
            if b.is_ascii_alphanumeric() || b == b'_' {
                self.pos += 1;
            } else {
                break;
            }
        }
        let ident_end = self.pos;

        if ident_end > ident_start {
            self.skip_whitespace();
            // Check for `=` but not `==`
            if self.peek() == Some(b'=') {
                let next = self.input.as_bytes().get(self.pos + 1).copied();
                if next != Some(b'=') {
                    // This is an assignment
                    let name = &self.input[ident_start..ident_end];
                    self.advance(); // consume '='
                    let val = self.parse_assignment()?;
                    // SAFETY: single-threaded shell; no other threads exist during evaluation.
                    unsafe { std::env::set_var(name, val.to_string()) };
                    return Ok(val);
                }
            }
        }

        // Not an assignment: restore position and fall through
        self.pos = saved_pos;
        self.parse_comparison()
    }

    fn parse_comparison(&mut self) -> Result<i64, String> {
        let mut left = self.parse_additive()?;

        loop {
            self.skip_whitespace();
            let b0 = self.peek();
            let b1 = self.input.as_bytes().get(self.pos + 1).copied();

            let op = match (b0, b1) {
                (Some(b'='), Some(b'=')) => "==",
                (Some(b'!'), Some(b'=')) => "!=",
                (Some(b'<'), Some(b'=')) => "<=",
                (Some(b'>'), Some(b'=')) => ">=",
                (Some(b'<'), _) => "<",
                (Some(b'>'), _) => ">",
                _ => break,
            };

            self.pos += op.len();
            let right = self.parse_additive()?;
            left = match op {
                "==" => i64::from(left == right),
                "!=" => i64::from(left != right),
                "<" => i64::from(left < right),
                ">" => i64::from(left > right),
                "<=" => i64::from(left <= right),
                ">=" => i64::from(left >= right),
                _ => unreachable!(),
            };
        }

        Ok(left)
    }

    fn parse_additive(&mut self) -> Result<i64, String> {
        let mut left = self.parse_multiplicative()?;

        loop {
            self.skip_whitespace();
            match self.peek() {
                Some(b'+') => {
                    self.advance();
                    left = left.wrapping_add(self.parse_multiplicative()?);
                }
                Some(b'-') => {
                    self.advance();
                    left = left.wrapping_sub(self.parse_multiplicative()?);
                }
                _ => break,
            }
        }

        Ok(left)
    }

    fn parse_multiplicative(&mut self) -> Result<i64, String> {
        let mut left = self.parse_unary()?;

        loop {
            self.skip_whitespace();
            match self.peek() {
                Some(b'*') => {
                    self.advance();
                    left = left.wrapping_mul(self.parse_unary()?);
                }
                Some(b'/') => {
                    self.advance();
                    let right = self.parse_unary()?;
                    if right == 0 {
                        return Err("division by zero".to_string());
                    }
                    left /= right;
                }
                Some(b'%') => {
                    self.advance();
                    let right = self.parse_unary()?;
                    if right == 0 {
                        return Err("division by zero".to_string());
                    }
                    left %= right;
                }
                _ => break,
            }
        }

        Ok(left)
    }

    fn parse_unary(&mut self) -> Result<i64, String> {
        self.skip_whitespace();
        match self.peek() {
            Some(b'-') => {
                self.advance();
                Ok(self.parse_primary()?.wrapping_neg())
            }
            Some(b'+') => {
                self.advance();
                self.parse_primary()
            }
            _ => self.parse_primary(),
        }
    }

    fn parse_primary(&mut self) -> Result<i64, String> {
        self.skip_whitespace();

        match self.peek() {
            Some(b'(') => {
                self.advance(); // consume '('
                let val = self.parse_expr()?;
                self.skip_whitespace();
                if self.peek() == Some(b')') {
                    self.advance();
                    Ok(val)
                } else {
                    Err(format!("expected ')' at position {}", self.pos))
                }
            }
            Some(b'$') => {
                self.advance(); // consume '$'
                let name = self.parse_identifier()?;
                Ok(lookup_var(&name))
            }
            Some(b) if b.is_ascii_digit() => {
                let start = self.pos;
                while self.pos < self.input.len()
                    && self.input.as_bytes()[self.pos].is_ascii_digit()
                {
                    self.pos += 1;
                }
                let s = &self.input[start..self.pos];
                s.parse::<i64>()
                    .map_err(|_| format!("integer overflow parsing '{}'", s))
            }
            Some(b) if b.is_ascii_alphabetic() || b == b'_' => {
                let name = self.parse_identifier()?;
                // Check for assignment: `=` but not `==`
                self.skip_whitespace();
                if self.peek() == Some(b'=')
                    && self.input.as_bytes().get(self.pos + 1).copied() != Some(b'=')
                {
                    self.advance(); // consume '='
                    let val = self.parse_assignment()?;
                    // SAFETY: single-threaded shell; no other threads exist during evaluation.
                    unsafe { std::env::set_var(&name, val.to_string()) };
                    Ok(val)
                } else {
                    Ok(lookup_var(&name))
                }
            }
            Some(b) => Err(format!(
                "unexpected character '{}' at position {}",
                b as char, self.pos
            )),
            None => Err("unexpected end of expression".to_string()),
        }
    }

    fn parse_identifier(&mut self) -> Result<String, String> {
        self.skip_whitespace();
        let start = self.pos;
        while self.pos < self.input.len() {
            let b = self.input.as_bytes()[self.pos];
            if b.is_ascii_alphanumeric() || b == b'_' {
                self.pos += 1;
            } else {
                break;
            }
        }
        if self.pos == start {
            Err(format!("expected identifier at position {}", self.pos))
        } else {
            Ok(self.input[start..self.pos].to_string())
        }
    }
}

/// Look up a variable by name from the environment, parsing its value as i64.
/// Returns 0 if the variable is unset or its value is not a valid integer.
fn lookup_var(name: &str) -> i64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(0)
}

/// Evaluate an arithmetic expression and return the result.
pub fn eval_arithmetic(expr: &str) -> Result<i64, String> {
    let mut parser = Parser::new(expr.trim());
    let result = parser.parse_expr()?;
    parser.skip_whitespace();
    if parser.pos < parser.input.len() {
        return Err(format!("unexpected character at position {}", parser.pos));
    }
    Ok(result)
}
