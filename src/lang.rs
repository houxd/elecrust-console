//! 控制台命令行的词法与语法解析。
//!
//! 支持（受嵌入式环境约束的 nushell 风格子集）：
//! - 引号与转义：`'...'`（原样）、`"..."`（支持 `\n \t \r \\ \" \0`）、行外 `\` 转义
//! - `;`/换行分句、`|` 结构化管道、`#` 行注释
//! - 变量：`let name = <pipeline>` 定义、`$name` 引用、`$name.field` 取字段
//! - 字面量：整数、浮点、`true`/`false`、引号字符串、列表 `[1 2 3]`、区间 `1..10` / `1..<10`
//! - 变量替换：裸词和双引号串里的 `${name}` / `${name.field}` 会替换为变量的值
//!   （u-boot / bash 风格，边界明确）；单引号串原样保留，`\$` 表示字面的 `$`
//! - 表达式：`+ - * / mod` 与 `== != < <= > >= and or not`（运算符两侧需空格）
//! - 控制流：`for x in <expr> { ... }`、`while <cond> { ... }`、`if/else`、`break`、`continue`
//! - 代码块：`{ |it| ... }`，可作为 `each` 等命令的参数
//! - 过滤：`where <条件>`，条件按行求值，其中的裸标识符表示当前行的字段
//!
//! 本模块不依赖具体的执行逻辑（不引用 CmdValue），只产出 AST，便于单独测试与复用。

use core::iter::Peekable;
use core::str::Chars;
use elecrust::alloc::boxed::Box;
use elecrust::alloc::format;
use elecrust::alloc::string::{String, ToString};
use elecrust::alloc::vec::Vec;

/// 词法单元。除结构符号外的文本都归入 `Word`。
enum Token {
    /// 一个词。`quoted` 表示其中出现过引号（引号词不会被当作 `$变量`、关键字或数字字面量）。
    Word { text: String, quoted: bool },
    /// 含 `${...}` 的词，已经拆成字面量与变量引用交替的若干段。
    InterpStr(Vec<StrPart>),
    Pipe,
    Semi,
    Newline,
    LParen,
    RParen,
    LBracket,
    RBracket,
    LBrace,
    RBrace,
}

impl Token {
    /// 用于错误信息的可读文本。
    fn desc(&self) -> &str {
        match self {
            Token::Word { text, .. } => text,
            Token::InterpStr(_) => "\"...${}...\"",
            Token::Pipe => "|",
            Token::Semi => ";",
            Token::Newline => "<newline>",
            Token::LParen => "(",
            Token::RParen => ")",
            Token::LBracket => "[",
            Token::RBracket => "]",
            Token::LBrace => "{",
            Token::RBrace => "}",
        }
    }

    fn is_word(&self, s: &str) -> bool {
        matches!(self, Token::Word { text, quoted: false } if text == s)
    }
}

/// 二元运算符。
#[derive(Clone, Copy)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
}

/// 含变量替换的词里的一段：原样文本，或一个待替换的变量路径。
#[derive(Clone)]
pub enum StrPart {
    Lit(String),
    Var(String),
}

/// 表达式：管道中不涉及命令调用的那部分语法。
#[derive(Clone)]
pub enum Expr {
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(String),
    /// 含 `${...}` 的词，求值时把各段拼成一个字符串。
    Interp(Vec<StrPart>),
    /// `$name` 或 `$name.field.0`，保存的是 `$` 之后的完整路径。
    Var(String),
    List(Vec<Expr>),
    Range {
        start: Box<Expr>,
        end: Box<Expr>,
        /// `1..5` 为 true，`1..<5` 为 false。
        inclusive: bool,
    },
    Not(Box<Expr>),
    /// 后缀取字段 `(...).name.0`，`path` 是 `.` 之后的完整路径。
    Field {
        base: Box<Expr>,
        path: String,
    },
    Binary {
        op: BinOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
    /// 括号子管道 `( ... )`。
    Sub(Pipeline),
}

/// 命令参数：表达式或代码块（如 `each { |it| ... }`）。
#[derive(Clone)]
pub enum Arg {
    Expr(Expr),
    Block(Block),
}

/// 管道中的一段。
#[derive(Clone)]
pub enum Stage {
    /// 表达式作为数据源，例如 `5`、`$x`、`1..10`、`$a + 1`。
    Expr(Expr),
    /// 命令调用：命令名 + 参数。
    Call { name: String, args: Vec<Arg> },
}

pub type Pipeline = Vec<Stage>;

/// 花括号代码块，`params` 来自 `{ |it| ... }` 里的参数声明。
#[derive(Clone)]
pub struct Block {
    pub params: Vec<String>,
    pub stmts: Vec<Statement>,
}

/// 一条语句。
#[derive(Clone)]
pub enum Statement {
    /// `let name = <pipeline>`，也用于 `$name = <pipeline>` 赋值。
    Let { name: String, value: Pipeline },
    /// 直接求值并打印结果的管道。
    Expr(Pipeline),
    For {
        var: String,
        iter: Pipeline,
        body: Block,
    },
    While {
        cond: Pipeline,
        body: Block,
    },
    If {
        cond: Pipeline,
        then_body: Block,
        else_body: Option<Block>,
    },
    Break,
    Continue,
}

/// 解析一整行（或多行）输入为语句列表。
pub fn parse_line(input: &str) -> Result<Block, String> {
    let tokens = tokenize(input).map_err(LexError::into_message)?;
    let mut parser = Parser {
        tokens,
        pos: 0,
        allow_block_arg: true,
        row_condition: false,
    };
    let stmts = parser.parse_stmts(false)?;
    Ok(Block {
        params: Vec::new(),
        stmts,
    })
}

/// 判断输入是否还没写完（引号未闭合、括号未配对、行尾是 `|` 等），
/// 供控制台决定是否显示续行提示符继续读取下一行。
pub fn is_incomplete(input: &str) -> bool {
    let tokens = match tokenize(input) {
        Ok(tokens) => tokens,
        // 引号没闭合、行尾反斜杠是「还没写完」，其余词法错误应当直接报错
        Err(LexError::Incomplete(_)) => return true,
        Err(LexError::Invalid(_)) => return false,
    };
    let mut depth = 0i32;
    let mut last: Option<&Token> = None;
    for tok in tokens.iter() {
        match tok {
            Token::LParen | Token::LBracket | Token::LBrace => depth += 1,
            Token::RParen | Token::RBracket | Token::RBrace => depth -= 1,
            _ => {}
        }
        if !matches!(tok, Token::Newline) {
            last = Some(tok);
        }
    }
    depth > 0 || matches!(last, Some(Token::Pipe))
}

/// 词法错误分两类：`Incomplete` 表示这一行还没写完（引号没闭合、行尾反斜杠），
/// 控制台应该继续读下一行；`Invalid` 是确定的写错了，应该直接报错。
enum LexError {
    Incomplete(&'static str),
    Invalid(String),
}

impl LexError {
    fn into_message(self) -> String {
        match self {
            LexError::Incomplete(msg) => msg.to_string(),
            LexError::Invalid(msg) => msg,
        }
    }
}

fn tokenize(input: &str) -> Result<Vec<Token>, LexError> {
    let mut tokens = Vec::new();
    let mut chars = input.chars().peekable();
    while let Some(&c) = chars.peek() {
        match c {
            '\n' => {
                chars.next();
                tokens.push(Token::Newline);
            }
            '\r' => {
                chars.next();
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                tokens.push(Token::Newline);
            }
            c if c.is_whitespace() => {
                chars.next();
            }
            '#' => {
                while let Some(&c) = chars.peek() {
                    if c == '\n' {
                        break;
                    }
                    chars.next();
                }
            }
            '|' => {
                chars.next();
                tokens.push(Token::Pipe);
            }
            ';' => {
                chars.next();
                tokens.push(Token::Semi);
            }
            '(' => {
                chars.next();
                tokens.push(Token::LParen);
            }
            ')' => {
                chars.next();
                tokens.push(Token::RParen);
            }
            '[' => {
                chars.next();
                tokens.push(Token::LBracket);
            }
            ']' => {
                chars.next();
                tokens.push(Token::RBracket);
            }
            '{' => {
                chars.next();
                tokens.push(Token::LBrace);
            }
            '}' => {
                chars.next();
                tokens.push(Token::RBrace);
            }
            _ => tokens.push(read_word(&mut chars)?),
        }
    }
    Ok(tokens)
}

fn is_delimiter(c: char) -> bool {
    matches!(c, '|' | ';' | '(' | ')' | '[' | ']' | '{' | '}')
}

/// 读取一个词。词可以由裸文本、`'...'`、`"..."` 若干段拼接而成。
/// 裸文本段和双引号段里的 `${name}` 会被拆成变量引用（u-boot / bash 风格），
/// 单引号段原样保留；用 `\$` 也可以得到字面的 `$`。
/// 只要出现过 `${...}`，产出的就是 `InterpStr`（求值时拼成字符串），否则是普通 `Word`。
fn read_word(chars: &mut Peekable<Chars>) -> Result<Token, LexError> {
    let mut buf = String::new();
    let mut parts: Vec<StrPart> = Vec::new();
    let mut quoted_any = false;
    let mut interpolated = false;

    while let Some(&c) = chars.peek() {
        match c {
            c if c.is_whitespace() => break,
            c if is_delimiter(c) => break,
            '\'' => {
                chars.next();
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(ch) => buf.push(ch),
                        None => return Err(LexError::Incomplete("unterminated single quote")),
                    }
                }
                quoted_any = true;
            }
            '"' => {
                chars.next();
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some('n') => buf.push('\n'),
                            Some('t') => buf.push('\t'),
                            Some('r') => buf.push('\r'),
                            Some('\\') => buf.push('\\'),
                            Some('"') => buf.push('"'),
                            Some('0') => buf.push('\0'),
                            Some(other) => buf.push(other),
                            None => {
                                return Err(LexError::Incomplete("unterminated escape in string"))
                            }
                        },
                        Some('$') => interpolated |= read_var_ref(chars, &mut buf, &mut parts)?,
                        Some(ch) => buf.push(ch),
                        None => return Err(LexError::Incomplete("unterminated double quote")),
                    }
                }
                quoted_any = true;
            }
            '\\' => {
                chars.next();
                match chars.next() {
                    // 行尾反斜杠是续行，吃掉换行继续拼接本词
                    Some('\n') => {}
                    Some('\r') => {
                        if chars.peek() == Some(&'\n') {
                            chars.next();
                        }
                    }
                    Some(ch) => buf.push(ch),
                    None => return Err(LexError::Incomplete("trailing backslash")),
                }
            }
            '$' => {
                chars.next();
                interpolated |= read_var_ref(chars, &mut buf, &mut parts)?;
            }
            _ => {
                buf.push(c);
                chars.next();
            }
        }
    }

    if interpolated {
        if !buf.is_empty() {
            parts.push(StrPart::Lit(buf));
        }
        Ok(Token::InterpStr(parts))
    } else {
        Ok(Token::Word {
            text: buf,
            quoted: quoted_any,
        })
    }
}

/// `$` 已经被消费。后面跟着 `{name}` 就记下一次变量引用并返回 true，
/// 否则把 `$` 当普通字符（`$name` 这种裸引用由语法层处理，不在字符串里展开）。
fn read_var_ref(
    chars: &mut Peekable<Chars>,
    buf: &mut String,
    parts: &mut Vec<StrPart>,
) -> Result<bool, LexError> {
    if chars.peek() != Some(&'{') {
        buf.push('$');
        return Ok(false);
    }
    chars.next(); // {

    let mut name = String::new();
    loop {
        match chars.next() {
            Some('}') => break,
            Some(c) if c.is_alphanumeric() || c == '_' || c == '.' => name.push(c),
            Some(c) => {
                return Err(LexError::Invalid(format!(
                    "unexpected '{}' in ${{{}...}}",
                    c, name
                )))
            }
            None => return Err(LexError::Invalid("unterminated ${...}".to_string())),
        }
    }
    if name.is_empty() {
        return Err(LexError::Invalid("empty variable name in ${}".to_string()));
    }

    if !buf.is_empty() {
        parts.push(StrPart::Lit(core::mem::take(buf)));
    }
    parts.push(StrPart::Var(name));
    Ok(true)
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
    /// 为 false 时 `{` 不会被当作命令参数，用于 `for`/`while`/`if` 的头部。
    allow_block_arg: bool,
    /// 为 true 时裸标识符表示当前行的字段，用于 `where` 的条件表达式。
    row_condition: bool,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn at_keyword(&self, kw: &str) -> bool {
        matches!(self.peek(), Some(tok) if tok.is_word(kw))
    }

    /// 当前 token 是否为给定运算符之一，是则返回它。
    fn peek_op(&self, ops: &[(&str, BinOp)]) -> Option<BinOp> {
        if let Some(Token::Word {
            text,
            quoted: false,
        }) = self.peek()
        {
            for (name, op) in ops.iter() {
                if text == name {
                    return Some(*op);
                }
            }
        }
        None
    }

    fn skip_newlines(&mut self) {
        while matches!(self.peek(), Some(Token::Newline)) {
            self.pos += 1;
        }
    }

    fn skip_separators(&mut self) {
        while matches!(self.peek(), Some(Token::Newline) | Some(Token::Semi)) {
            self.pos += 1;
        }
    }

    /// 取出一个非引号词（用于变量名、关键字后的标识符）。
    fn take_name(&mut self, what: &str) -> Result<String, String> {
        match self.peek() {
            Some(Token::Word {
                text,
                quoted: false,
            }) => {
                let name = text.trim_start_matches('$').to_string();
                if name.is_empty() {
                    return Err(format!("invalid {}", what));
                }
                self.pos += 1;
                Ok(name)
            }
            Some(tok) => Err(format!("expected {}, found '{}'", what, tok.desc())),
            None => Err(format!("expected {}", what)),
        }
    }

    fn expect(&mut self, want: fn(&Token) -> bool, what: &str) -> Result<(), String> {
        match self.peek() {
            Some(tok) if want(tok) => {
                self.pos += 1;
                Ok(())
            }
            Some(tok) => Err(format!("expected {}, found '{}'", what, tok.desc())),
            None => Err(format!("expected {}", what)),
        }
    }

    /// 解析语句序列。`in_braces` 为 true 时读到 `}` 结束并消费它。
    fn parse_stmts(&mut self, in_braces: bool) -> Result<Vec<Statement>, String> {
        let mut stmts = Vec::new();
        loop {
            self.skip_separators();
            match self.peek() {
                None => {
                    if in_braces {
                        return Err("unterminated block: expected '}'".to_string());
                    }
                    break;
                }
                Some(Token::RBrace) => {
                    if !in_braces {
                        return Err("unexpected '}'".to_string());
                    }
                    self.pos += 1;
                    break;
                }
                _ => {}
            }

            stmts.push(self.parse_statement()?);

            match self.peek() {
                None | Some(Token::Semi) | Some(Token::Newline) | Some(Token::RBrace) => {}
                Some(tok) => {
                    return Err(format!("unexpected '{}' after statement", tok.desc()));
                }
            }
        }
        Ok(stmts)
    }

    fn parse_statement(&mut self) -> Result<Statement, String> {
        if self.at_keyword("let") || self.at_keyword("mut") {
            self.pos += 1;
            let name = self.take_name("variable name")?;
            self.expect(|t| t.is_word("="), "'=' after variable name")?;
            self.skip_newlines();
            let value = self.parse_pipeline()?;
            return Ok(Statement::Let { name, value });
        }
        if self.at_keyword("for") {
            self.pos += 1;
            let var = self.take_name("loop variable")?;
            self.expect(|t| t.is_word("in"), "'in' after loop variable")?;
            self.skip_newlines();
            let iter = self.parse_header_pipeline()?;
            let body = self.parse_brace_block()?;
            return Ok(Statement::For { var, iter, body });
        }
        if self.at_keyword("while") {
            self.pos += 1;
            let cond = self.parse_header_pipeline()?;
            let body = self.parse_brace_block()?;
            return Ok(Statement::While { cond, body });
        }
        if self.at_keyword("if") {
            return self.parse_if();
        }
        if self.at_keyword("break") {
            self.pos += 1;
            return Ok(Statement::Break);
        }
        if self.at_keyword("continue") {
            self.pos += 1;
            return Ok(Statement::Continue);
        }
        // `$name = <pipeline>` 形式的赋值
        if let Some(Token::Word {
            text,
            quoted: false,
        }) = self.peek()
        {
            if text.starts_with('$') && text.len() > 1 && !text.contains('.') {
                if matches!(self.tokens.get(self.pos + 1), Some(tok) if tok.is_word("=")) {
                    let name = text[1..].to_string();
                    self.pos += 2;
                    self.skip_newlines();
                    let value = self.parse_pipeline()?;
                    return Ok(Statement::Let { name, value });
                }
            }
        }

        Ok(Statement::Expr(self.parse_pipeline()?))
    }

    fn parse_if(&mut self) -> Result<Statement, String> {
        self.pos += 1; // if
        let cond = self.parse_header_pipeline()?;
        let then_body = self.parse_brace_block()?;

        // `else` 允许出现在换行之后，若不是 else 则回退，避免吃掉下一条语句
        let saved = self.pos;
        self.skip_newlines();
        if !self.at_keyword("else") {
            self.pos = saved;
            return Ok(Statement::If {
                cond,
                then_body,
                else_body: None,
            });
        }
        self.pos += 1; // else
        self.skip_newlines();
        let else_body = if self.at_keyword("if") {
            Block {
                params: Vec::new(),
                stmts: {
                    let mut stmts = Vec::new();
                    stmts.push(self.parse_if()?);
                    stmts
                },
            }
        } else {
            self.parse_brace_block()?
        };
        Ok(Statement::If {
            cond,
            then_body,
            else_body: Some(else_body),
        })
    }

    /// 解析 `for`/`while`/`if` 头部的管道：其中的 `{` 属于循环体，不作为命令参数。
    fn parse_header_pipeline(&mut self) -> Result<Pipeline, String> {
        let saved = self.allow_block_arg;
        self.allow_block_arg = false;
        let result = self.parse_pipeline();
        self.allow_block_arg = saved;
        result
    }

    fn parse_brace_block(&mut self) -> Result<Block, String> {
        self.skip_newlines();
        self.expect(|t| matches!(t, Token::LBrace), "'{'")?;
        self.skip_newlines();

        let mut params = Vec::new();
        if matches!(self.peek(), Some(Token::Pipe)) {
            self.pos += 1;
            loop {
                match self.peek() {
                    Some(Token::Pipe) => {
                        self.pos += 1;
                        break;
                    }
                    Some(Token::Word { .. }) => {
                        params.push(self.take_name("block parameter")?);
                    }
                    Some(tok) => {
                        return Err(format!("unexpected '{}' in block parameters", tok.desc()));
                    }
                    None => return Err("unterminated block parameters".to_string()),
                }
            }
        }

        // 块内允许使用 `{}` 作为命令参数
        let saved = (self.allow_block_arg, self.row_condition);
        self.allow_block_arg = true;
        self.row_condition = false;
        let stmts = self.parse_stmts(true);
        (self.allow_block_arg, self.row_condition) = saved;
        Ok(Block {
            params,
            stmts: stmts?,
        })
    }

    fn parse_pipeline(&mut self) -> Result<Pipeline, String> {
        let mut stages = Vec::new();
        loop {
            stages.push(self.parse_stage()?);
            if matches!(self.peek(), Some(Token::Pipe)) {
                self.pos += 1;
                self.skip_newlines();
            } else {
                break;
            }
        }
        Ok(stages)
    }

    fn parse_stage(&mut self) -> Result<Stage, String> {
        let is_call = match self.peek() {
            Some(Token::Word {
                text,
                quoted: false,
            }) => !word_is_value(text),
            Some(Token::Word { .. }) => false,
            Some(Token::InterpStr(_)) | Some(Token::LParen) | Some(Token::LBracket) => false,
            Some(tok) => return Err(format!("unexpected '{}'", tok.desc())),
            None => return Err("unexpected end of input".to_string()),
        };

        if is_call {
            return self.parse_call();
        }
        let expr = self.parse_expr()?;
        self.expect_stage_end()?;
        Ok(Stage::Expr(expr))
    }

    fn expect_stage_end(&mut self) -> Result<(), String> {
        match self.peek() {
            None
            | Some(Token::Semi)
            | Some(Token::Newline)
            | Some(Token::Pipe)
            | Some(Token::LBrace)
            | Some(Token::RBrace)
            | Some(Token::RParen)
            | Some(Token::RBracket) => Ok(()),
            Some(tok) => Err(format!("unexpected '{}' in expression", tok.desc())),
        }
    }

    fn parse_call(&mut self) -> Result<Stage, String> {
        let name = match self.peek() {
            Some(Token::Word { text, .. }) => text.clone(),
            _ => return Err("invalid command name".to_string()),
        };
        self.pos += 1;

        if name == "where" {
            let args = self.parse_where_arg()?;
            return Ok(Stage::Call { name, args });
        }

        let mut args = Vec::new();
        loop {
            match self.peek() {
                None
                | Some(Token::Semi)
                | Some(Token::Newline)
                | Some(Token::Pipe)
                | Some(Token::RParen)
                | Some(Token::RBracket)
                | Some(Token::RBrace) => break,
                Some(Token::LBrace) => {
                    if !self.allow_block_arg {
                        break;
                    }
                    args.push(Arg::Block(self.parse_brace_block()?));
                }
                Some(Token::InterpStr(_)) | Some(Token::LParen) | Some(Token::LBracket) => {
                    args.push(Arg::Expr(self.parse_primary()?));
                }
                Some(Token::Word { text, quoted }) => {
                    // 命令参数默认按字面字符串传递，只有 `$var` 会取变量值，
                    // 这样 `ls -l`、`cd ..`、`cat a.txt` 这类写法不受表达式语法影响
                    let arg = if !*quoted && text.starts_with('$') && text.len() > 1 {
                        Expr::Var(text[1..].to_string())
                    } else {
                        Expr::Str(text.clone())
                    };
                    self.pos += 1;
                    args.push(Arg::Expr(arg));
                }
            }
        }
        Ok(Stage::Call { name, args })
    }

    /// `where` 的参数：一个按行求值的条件表达式（裸标识符即字段），或一个代码块。
    fn parse_where_arg(&mut self) -> Result<Vec<Arg>, String> {
        let mut args = Vec::new();
        match self.peek() {
            Some(Token::LBrace) if self.allow_block_arg => {
                args.push(Arg::Block(self.parse_brace_block()?));
            }
            None
            | Some(Token::Semi)
            | Some(Token::Newline)
            | Some(Token::Pipe)
            | Some(Token::LBrace)
            | Some(Token::RParen)
            | Some(Token::RBracket)
            | Some(Token::RBrace) => {
                return Err("expected a condition after 'where'".to_string());
            }
            _ => {
                let saved = self.row_condition;
                self.row_condition = true;
                let cond = self.parse_expr();
                self.row_condition = saved;
                args.push(Arg::Expr(cond?));
                self.expect_stage_end()?;
            }
        }
        Ok(args)
    }

    fn parse_expr(&mut self) -> Result<Expr, String> {
        self.parse_binary(0)
    }

    /// 按优先级从低到高解析二元运算符（同级左结合）。
    fn parse_binary(&mut self, level: usize) -> Result<Expr, String> {
        const LEVELS: &[&[(&str, BinOp)]] = &[
            &[("or", BinOp::Or)],
            &[("and", BinOp::And)],
            &[
                ("==", BinOp::Eq),
                ("!=", BinOp::Ne),
                ("<=", BinOp::Le),
                (">=", BinOp::Ge),
                ("<", BinOp::Lt),
                (">", BinOp::Gt),
            ],
            &[("+", BinOp::Add), ("-", BinOp::Sub)],
            &[
                ("*", BinOp::Mul),
                ("/", BinOp::Div),
                ("mod", BinOp::Mod),
                ("%", BinOp::Mod),
            ],
        ];

        if level >= LEVELS.len() {
            return self.parse_unary();
        }
        let mut lhs = self.parse_binary(level + 1)?;
        while let Some(op) = self.peek_op(LEVELS[level]) {
            self.pos += 1;
            self.skip_newlines();
            let rhs = self.parse_binary(level + 1)?;
            lhs = Expr::Binary {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            };
        }
        Ok(lhs)
    }

    fn parse_unary(&mut self) -> Result<Expr, String> {
        if self.at_keyword("not") {
            self.pos += 1;
            self.skip_newlines();
            return Ok(Expr::Not(Box::new(self.parse_unary()?)));
        }
        self.parse_primary()
    }

    fn parse_primary(&mut self) -> Result<Expr, String> {
        let expr = self.parse_atom()?;
        Ok(self.parse_field_suffix(expr))
    }

    /// `(ps).0.task`、`[1 2 3].0` 这类后缀取字段。
    /// 词法上 `.0.task` 是一个独立的词，这里直接接在前一个表达式后面。
    fn parse_field_suffix(&mut self, mut expr: Expr) -> Expr {
        while let Some(Token::Word {
            text,
            quoted: false,
        }) = self.peek()
        {
            // 至少要有一个字段名，且不能是 `..`（区间写法）
            if !text.starts_with('.') || text.len() < 2 || text[1..].starts_with('.') {
                break;
            }
            let path = text[1..].to_string();
            self.pos += 1;
            expr = Expr::Field {
                base: Box::new(expr),
                path,
            };
        }
        expr
    }

    fn parse_atom(&mut self) -> Result<Expr, String> {
        match self.peek() {
            Some(Token::LParen) => {
                self.pos += 1;
                self.skip_newlines();
                // 括号里是一条普通管道，不再套用 where 条件的字段规则
                let saved = (self.allow_block_arg, self.row_condition);
                self.allow_block_arg = true;
                self.row_condition = false;
                let pipeline = self.parse_pipeline();
                (self.allow_block_arg, self.row_condition) = saved;
                let pipeline = pipeline?;
                self.skip_newlines();
                self.expect(|t| matches!(t, Token::RParen), "')'")?;
                Ok(Expr::Sub(pipeline))
            }
            Some(Token::LBracket) => self.parse_list(),
            Some(Token::InterpStr(parts)) => {
                let expr = Expr::Interp(parts.clone());
                self.pos += 1;
                Ok(expr)
            }
            Some(Token::Word { text, quoted }) => {
                let expr = if *quoted {
                    Expr::Str(text.clone())
                } else if self.row_condition && is_field_word(text) {
                    Expr::Field {
                        base: Box::new(Expr::Var("it".to_string())),
                        path: text.clone(),
                    }
                } else {
                    word_to_expr(text)
                };
                self.pos += 1;
                Ok(expr)
            }
            Some(tok) => Err(format!("unexpected '{}' in expression", tok.desc())),
            None => Err("unexpected end of expression".to_string()),
        }
    }

    fn parse_list(&mut self) -> Result<Expr, String> {
        self.pos += 1; // [
        let mut items = Vec::new();
        loop {
            self.skip_newlines();
            match self.peek() {
                Some(Token::RBracket) => {
                    self.pos += 1;
                    break;
                }
                None => return Err("unterminated list: expected ']'".to_string()),
                Some(Token::Word {
                    text,
                    quoted: false,
                }) if text.contains(',') => {
                    // 兼容 `[1, 2, 3]` / `[1,2,3]`：逗号在词法上属于词的一部分，这里再拆开
                    let text = text.clone();
                    self.pos += 1;
                    for piece in text.split(',') {
                        if !piece.is_empty() {
                            items.push(word_to_expr(piece));
                        }
                    }
                }
                _ => items.push(self.parse_expr()?),
            }
        }
        Ok(Expr::List(items))
    }
}

/// 一个非引号词是否为「值」（而不是命令名）。用于判断管道段是表达式还是命令调用。
fn word_is_value(text: &str) -> bool {
    (text.starts_with('$') && text.len() > 1)
        || text == "true"
        || text == "false"
        || text == "not"
        || text.parse::<i64>().is_ok()
        || text.parse::<f64>().is_ok()
        || parse_range_word(text).is_some()
}

/// `where` 条件里的裸词是否表示字段名。数字、`true`、`$var`、`-r` 这类词不算，
/// 其余标识符（含 `a.b` 这样的路径）都当作当前行的字段。
fn is_field_word(text: &str) -> bool {
    !word_is_value(text)
        && !text.is_empty()
        && !text.starts_with('.')
        && !text.starts_with('-')
        && text
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '-' || c == '.')
}

fn word_to_expr(text: &str) -> Expr {
    if text.starts_with('$') && text.len() > 1 {
        return Expr::Var(text[1..].to_string());
    }
    if text == "true" {
        return Expr::Bool(true);
    }
    if text == "false" {
        return Expr::Bool(false);
    }
    if let Ok(i) = text.parse::<i64>() {
        return Expr::Int(i);
    }
    if let Ok(f) = text.parse::<f64>() {
        return Expr::Float(f);
    }
    if let Some((start, end, inclusive)) = parse_range_word(text) {
        return Expr::Range {
            start: Box::new(start),
            end: Box::new(end),
            inclusive,
        };
    }
    Expr::Str(text.to_string())
}

/// 解析 `1..10`、`1..<10`、`$a..$b` 这类区间词；两端必须是整数或变量，
/// 否则返回 None（这样 `..`、`../dir` 仍然是普通的词）。
fn parse_range_word(text: &str) -> Option<(Expr, Expr, bool)> {
    let idx = text.find("..")?;
    let lhs = &text[..idx];
    let mut rhs = &text[idx + 2..];
    let inclusive = if let Some(stripped) = rhs.strip_prefix('<') {
        rhs = stripped;
        false
    } else {
        true
    };
    Some((range_bound(lhs)?, range_bound(rhs)?, inclusive))
}

fn range_bound(text: &str) -> Option<Expr> {
    if text.starts_with('$') && text.len() > 1 {
        return Some(Expr::Var(text[1..].to_string()));
    }
    text.parse::<i64>().ok().map(Expr::Int)
}
