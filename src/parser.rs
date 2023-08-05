use crate::common::{CompileError, Location};
use alloc::{boxed::Box, format, string::String, vec::Vec};

// TODO: add parser tests

#[derive(Debug, PartialEq, Clone)]
pub enum AtomKind {
    Symbol,
    String,
}

#[derive(Debug, PartialEq, Clone)]
pub enum SExpr {
    Atom {
        value: String,
        kind: AtomKind,
        loc: Location,
    },
    List {
        value: Vec<SExpr>,
        loc: Location,
    },
}

impl SExpr {
    pub fn loc(&self) -> &Location {
        match self {
            Self::Atom { loc, .. } => loc,
            Self::List { loc, .. } => loc,
        }
    }
}

pub type ParseResult = Result<SExpr, CompileError>;

#[derive(Clone)]
struct Parser {
    file_name: Box<str>,
    chars: Vec<char>,
    index: usize,
    line: usize,
    col: usize,
}

pub fn parse(file_name: &str, script: &str) -> Result<Vec<SExpr>, CompileError> {
    Parser::new(file_name, script).parse_all()
}

impl Parser {
    fn new(file_name: &str, script: &str) -> Self {
        Self {
            file_name: file_name.into(),
            chars: script.chars().collect::<Vec<_>>(),
            index: 0,
            line: 1,
            col: 1,
        }
    }

    fn parse_all(&mut self) -> Result<Vec<SExpr>, CompileError> {
        self.skip_space();

        let mut items = Vec::new();

        while self.index < self.chars.len() {
            if !is_list_start(self.current_char()?) {
                return Err(self.err_unexpected_char());
            }

            let res = self.parse_list()?;
            self.skip_space();
            items.push(res);
        }

        Ok(items)
    }

    fn parse_expr(&mut self) -> ParseResult {
        if is_list_start(self.current_char()?) {
            self.parse_list()
        } else {
            self.parse_atom()
        }
    }

    fn parse_atom(&mut self) -> ParseResult {
        match self.current_char()? {
            '"' => self.parse_string(),
            _ => self.parse_symbol(),
        }
    }

    fn parse_string(&mut self) -> ParseResult {
        let mut loc = self.loc();

        self.next_char(); // skip start quote

        let mut value = String::new();

        loop {
            let c = self.current_char()?;
            match c {
                '"' => break,
                '\\' => {
                    self.next_char();
                    match self.current_char()? {
                        'n' => value.push('\n'),
                        '\\' | '"' => value.push(c),
                        _ => {
                            return Err(self.err_unexpected_char());
                        }
                    }
                }
                _ => {
                    value.push(c);
                }
            };
            self.next_char();
        }

        self.next_char(); // skip end quote

        loc.length = self.index - loc.offset;

        Ok(SExpr::Atom {
            value,
            kind: AtomKind::String,
            loc,
        })
    }

    fn parse_symbol(&mut self) -> ParseResult {
        let mut loc = self.loc();

        loop {
            let c = self.current_char()?;
            if is_space(c) || is_list_end(c) || c == ';' {
                break;
            }
            self.next_char();
        }

        loc.length = self.index - loc.offset;

        Ok(SExpr::Atom {
            value: self.chars[loc.offset..self.index].iter().collect(),
            kind: AtomKind::Symbol,
            loc,
        })
    }

    fn parse_list(&mut self) -> ParseResult {
        let mut loc = self.loc();

        let list_start_char = self.current_char()?;
        self.next_char(); // eat list start

        self.skip_space();

        let mut items = Vec::new();

        while !is_list_end(self.current_char()?) {
            let res = self.parse_expr()?;
            self.skip_space();
            items.push(res);
        }

        let list_end_char = self.current_char()?;
        self.next_char(); // eat list end

        if !is_valid_list_chars(list_start_char, list_end_char) {
            return Err(self.err_unexpected_char());
        }

        loc.length = self.index - loc.offset;

        if list_start_char == '{' && items.len() >= 2 {
            return m_expr_to_s_expr(items, loc);
        }

        Ok(SExpr::List { value: items, loc })
    }

    fn skip_space(&mut self) {
        while self.current_char().map(is_space).unwrap_or(false) {
            self.next_char();
        }

        if let Ok(';') = self.current_char() {
            self.next_char();

            while self.current_char().map(|c| c != '\n').unwrap_or(false) {
                self.next_char();
            }

            if let Ok('\n') = self.current_char() {
                self.next_char();
            }

            self.skip_space();
        }
    }

    fn next_char(&mut self) {
        self.index += 1;

        if let Ok(char) = self.current_char() {
            if char == '\n' {
                self.col = 0;
                self.line += 1;
                return;
            }

            self.col += 1;
        }
    }

    fn current_char(&mut self) -> Result<char, CompileError> {
        self.chars
            .get(self.index)
            .copied()
            .ok_or_else(|| self.err_unexpected_eof())
    }

    fn err_unexpected_char(&self) -> CompileError {
        CompileError {
            message: format!("ParseError: Unexpected character"),
            loc: self.loc(),
        }
    }

    fn err_unexpected_eof(&self) -> CompileError {
        CompileError {
            message: format!("ParseError: Unexpected EOF"),
            loc: self.loc(),
        }
    }

    fn loc(&self) -> Location {
        Location {
            file_name: self.file_name.clone(),
            offset: self.index,
            length: 1,
            line: self.line,
            col: self.col,
        }
    }
}

// ❓ {1 + 2 - 3 * 4}
// 🚫 (+ 1 (- 2 (* 3 4)))
// ✅ (* (- (+ 1 2) 3) 4)
fn m_expr_to_s_expr(mut items: Vec<SExpr>, loc: Location) -> ParseResult {
    if items.len() % 2 != 1 {
        return Err(CompileError {
            message: format!("Invalid m-expr: even length"),
            loc,
        });
    }

    // TODO: implement chained ops
    items.swap(0, 1);

    Ok(SExpr::List { value: items, loc })
}

fn is_list_start(c: char) -> bool {
    match c {
        '(' | '{' | '[' => true,
        _ => false,
    }
}

fn is_list_end(c: char) -> bool {
    match c {
        ')' | '}' | ']' => true,
        _ => false,
    }
}

fn is_valid_list_chars(start: char, end: char) -> bool {
    match (start, end) {
        ('(', ')') => true,
        ('{', '}') => true,
        ('[', ']') => true,
        _ => false,
    }
}

fn is_space(c: char) -> bool {
    match c {
        ' ' | '\n' | '\t' => true,
        _ => false,
    }
}
