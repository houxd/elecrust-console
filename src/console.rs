use crate::cmd_result::range_parts;
use crate::lang::{self, Arg, BinOp, Block, Expr, Pipeline, Stage, Statement, StrPart};
use crate::CmdParser;
use crate::CmdValue;
use core::cell::RefCell;
use core::cmp::Ordering;
use core::future::Future;
use core::pin::Pin;
use elecrust::alloc::boxed::Box;
use elecrust::alloc::collections::vec_deque::VecDeque;
use elecrust::alloc::format;
use elecrust::alloc::rc::Rc;
use elecrust::alloc::string::String;
use elecrust::alloc::string::ToString;
use elecrust::alloc::vec;
use elecrust::alloc::vec::Vec;
use elecrust::driver::tty::TtyDriver;
use elecrust::err;
use elecrust::executor::{Spawner, TaskId};
use elecrust::fs::FileType;
use elecrust::sys::{Board, get_tty_dev};
use elecrust::{error::Result, print, println, sys};

const HISTORY_SIZE: usize = 10; // 历史记录最大条数
const LINE_BUFFER_SIZE: usize = 512; // 每行最大字符数
const CONTINUATION_PROMPT: &str = "... "; // 多行输入（如 for 循环）未写完时的提示符
const PENDING_BUFFER_SIZE: usize = 4096; // 多行输入累计的最大字符数

#[derive(Debug, Clone, Copy)]
enum EscapeState {
    Normal,
    Escape,
    Bracket,
}

// #[allow(unused)]
// pub enum Key {
//     Char(u8),
//     CtrlC,
//     Enter,
//     Backspace,
//     Up,
//     Down,
//     Left,
//     Right,
//     Unknown(u8),
// }

pub struct SharedState {
    spawner: Spawner,
    board: Rc<dyn Board>,
    tty: Rc<dyn TtyDriver>,
    prompt: String,
    // 当前工作目录（shell 的状态）。相对路径由此解析为绝对路径后再交给 Vfs。
    cwd: String,
    // 历史记录
    history: VecDeque<Vec<u8>>,
    history_index: Option<usize>,
    // 当前编辑状态
    current_line: Vec<u8>,
    cursor_pos: usize,
    // ANSI转义序列状态
    escape_state: EscapeState,
    cmds_parser_list: VecDeque<Rc<dyn CmdParser>>,
    // 命令别名表。只保存静态表的引用，表本身在 flash 里，不占 RAM
    aliases: Vec<&'static [(&'static str, &'static str)]>,
    // 正在展开的别名，用于防止别名自己套自己
    alias_stack: Vec<&'static str>,
    // 会话级变量表（name -> value）
    vars: Vec<(String, CmdValue)>,
    // 尚未闭合的多行输入（括号/引号没写完时暂存在这里）
    pending: String,
}

impl SharedState {
    pub fn new(spawner: Spawner, device: Rc<dyn Board>) -> Self {
        SharedState {
            spawner,
            board: device.clone(),
            tty: get_tty_dev(), //device.default_tty(),
            prompt: String::from("> "),
            cwd: String::from("/"),
            history: VecDeque::new(),
            history_index: None,
            current_line: Vec::new(),
            cursor_pos: 0,
            escape_state: EscapeState::Normal,
            cmds_parser_list: VecDeque::new(),
            aliases: Vec::new(),
            alias_stack: Vec::new(),
            vars: Vec::new(),
            pending: String::new(),
        }
    }

    // 显示提示符：还有未写完的多行输入时显示续行提示符
    fn show_prompt(&self) {
        if self.pending.is_empty() {
            self.tty.tty_write(self.prompt.as_bytes());
        } else {
            self.tty.tty_write(CONTINUATION_PROMPT.as_bytes());
        }
        self.tty.tty_flush();
    }

    // 添加命令到历史记录
    fn add_to_history(&mut self, line: &[u8]) {
        if line.is_empty() {
            return;
        }

        // 如果和最后一条记录相同，不添加
        if let Some(last) = self.history.back() {
            // 改为 back()
            if last.as_slice() == line {
                return;
            }
        }

        let history_line = line.to_vec();

        if self.history.len() >= HISTORY_SIZE {
            // 移除最旧的记录
            self.history.pop_front();
        }

        self.history.push_back(history_line);
    }

    // 从历史记录加载
    fn load_from_history(&mut self, index: usize) {
        if index < self.history.len() {
            self.clear_current_line();
            self.current_line.clear();

            if let Some(history_line) = self.history.iter().nth(index) {
                // 使用 iter().nth(index)
                self.current_line = history_line.clone(); // 直接克隆
            }

            self.cursor_pos = self.current_line.len();
            self.redraw_line();
        }
    }

    // 清除当前行显示
    fn clear_current_line(&self) {
        // 移动到行首
        self.tty.tty_putc(b'\r');
        // 清除整行
        self.tty.tty_write(b"\x1b[K");
        self.show_prompt();
    }

    // 重绘当前行
    fn redraw_line(&self) {
        self.tty.tty_write(self.current_line.as_slice());
        // 移动光标到正确位置
        if self.cursor_pos < self.current_line.len() {
            let moves_back = self.current_line.len() - self.cursor_pos;
            for _ in 0..moves_back {
                self.tty.tty_putc(b'\x08'); // 退格
            }
        }
        self.tty.tty_flush();
    }

    // 向左移动光标
    fn move_cursor_left(&mut self) {
        if self.cursor_pos > 0 {
            self.cursor_pos -= 1;
            self.tty.tty_putc(b'\x08'); // 退格
            self.tty.tty_flush();
        }
    }

    // 向右移动光标
    fn move_cursor_right(&mut self) {
        if self.cursor_pos < self.current_line.len() {
            self.tty.tty_putc(self.current_line[self.cursor_pos]);
            self.cursor_pos += 1;
            self.tty.tty_flush();
        }
    }

    // 在光标位置插入字符
    fn insert_char(&mut self, c: u8) {
        if self.current_line.len() >= LINE_BUFFER_SIZE {
            return;
        }

        // 如果光标在末尾，直接添加
        if self.cursor_pos == self.current_line.len() {
            self.current_line.push(c); // Vec::push 不会失败（除非内存不足）
            self.tty.tty_putc(c);
            self.cursor_pos += 1;
            self.tty.tty_flush();
        } else {
            // 使用 Vec 的 insert 方法
            self.current_line.insert(self.cursor_pos, c); // 直接使用 insert
            self.cursor_pos += 1;

            // 重绘整行
            self.clear_current_line();
            self.redraw_line();
        }
    }

    // 删除光标前的字符
    fn backspace(&mut self) {
        if self.cursor_pos > 0 {
            // 移动后面的字符向前
            for i in self.cursor_pos..self.current_line.len() {
                if i > 0 {
                    let next_char = self.current_line[i];
                    self.current_line[i - 1] = next_char;
                }
            }

            if !self.current_line.is_empty() {
                self.current_line.pop();
            }
            self.cursor_pos -= 1;

            // 重绘整行
            self.clear_current_line();
            self.redraw_line();
        }
    }

    // 放弃当前输入（含未写完的多行输入），回到新的提示符
    fn cancel_input(&mut self) {
        self.tty.tty_putc(b'^');
        self.tty.tty_putc(b'C');
        self.tty.tty_putc(b'\r');
        self.tty.tty_putc(b'\n');
        self.tty.tty_flush();
        self.current_line.clear();
        self.cursor_pos = 0;
        self.history_index = None;
        self.pending.clear();
        self.show_prompt();
    }

    // 复位控制台状态
    fn reset(&mut self) {
        self.tty.tty_flush();
        self.tty.tty_clear_rx();
        self.current_line.clear();
        self.cursor_pos = 0;
        self.history_index = None;
        self.show_prompt();
    }

    // 轮训 TTY 输入字符并处理, 当收到回车或换行时，返回当前行内容, 否则返回 None
    fn poll_tty_input(&mut self) -> Option<Vec<u8>> {
        let c = self.tty.tty_getc();
        if let Some(b) = c {
            // let escape_state = self.state.borrow().escape_state;
            match self.escape_state {
                EscapeState::Normal => {
                    match b {
                        // 回车或换行，处理命令
                        b'\r' | b'\n' => {
                            self.tty.tty_putc(b'\r');
                            self.tty.tty_putc(b'\n');
                            self.tty.tty_flush();
                            return Some(self.current_line.clone());
                        }
                        // Ctrl+C (ASCII 3) - 终止当前输入
                        3 => {
                            self.cancel_input();
                        }
                        // 退格键
                        8 | 127 => {
                            self.backspace();
                        }
                        // ESC序列开始
                        27 => {
                            self.escape_state = EscapeState::Escape;
                        }
                        // 可打印字符和空格
                        b if b.is_ascii_graphic() || b == b' ' => {
                            self.insert_char(b);
                        }
                        // 忽略其他控制字符
                        _ => {}
                    }
                }
                EscapeState::Escape => match b {
                    b'[' => {
                        self.escape_state = EscapeState::Bracket;
                    }
                    _ => {
                        self.escape_state = EscapeState::Normal;
                    }
                },
                EscapeState::Bracket => {
                    match b {
                        // 上箭头
                        b'A' => {
                            if !self.history.is_empty() {
                                let new_index = match self.history_index {
                                    None => self.history.len() - 1,
                                    Some(idx) if idx > 0 => idx - 1,
                                    Some(_) => 0,
                                };
                                self.history_index = Some(new_index);
                                self.load_from_history(new_index);
                            }
                        }
                        // 下箭头
                        b'B' => {
                            let history_index = self.history_index;
                            let history_len = self.history.len();
                            match history_index {
                                Some(idx) if idx < history_len - 1 => {
                                    self.history_index = Some(idx + 1);
                                    self.load_from_history(idx + 1);
                                }
                                Some(_) => {
                                    // 到达历史记录末尾，清空当前行
                                    self.history_index = None;
                                    self.clear_current_line();
                                    self.current_line.clear();
                                    self.cursor_pos = 0;
                                }
                                None => {}
                            }
                        }
                        // 右箭头
                        b'C' => {
                            self.move_cursor_right();
                        }
                        // 左箭头
                        b'D' => {
                            self.move_cursor_left();
                        }
                        _ => {}
                    }
                    self.escape_state = EscapeState::Normal;
                }
            }
        }

        None
    }
}

#[derive(Clone)]
pub struct Env {
    state: Rc<RefCell<SharedState>>,
}

impl Env {
    pub fn spawn(
        &self,
        name: impl Into<String>,
        future: Pin<Box<dyn Future<Output = Result<()>>>>,
    ) -> TaskId {
        self.state.borrow().spawner.spawn(name, future)
    }

    pub fn board(&self) -> Rc<dyn Board> {
        self.state.borrow().board.clone()
    }

    /// 返回当前工作目录（绝对路径）。
    pub fn cwd(&self) -> String {
        self.state.borrow().cwd.clone()
    }

    /// 把用户输入的路径解析为归一化后的绝对路径：
    /// 以 `/` 开头视为绝对路径，否则拼接到当前 cwd 之后，再消除 `.`、`..` 和多余的 `/`。
    pub fn resolve(&self, path: impl AsRef<str>) -> String {
        let path = path.as_ref();
        let combined = if path.starts_with('/') {
            path.to_string()
        } else {
            format!("{}/{}", self.cwd(), path)
        };

        let mut parts: Vec<&str> = Vec::new();
        for part in combined.split('/') {
            if part == "." || part.is_empty() {
                continue;
            } else if part == ".." {
                parts.pop();
            } else {
                parts.push(part);
            }
        }
        format!("/{}", parts.join("/"))
    }

    /// 切换当前工作目录。会校验目标存在且为目录。
    pub fn set_cwd(&self, path: impl AsRef<str>) -> Result<()> {
        let abs = self.resolve(path);
        let fs = self.board().fs();
        let stat = fs.stat(&abs)?;
        match stat.file_type {
            FileType::Directory => {
                self.state.borrow_mut().cwd = abs;
                Ok(())
            }
            FileType::NotExist => Err(err!("{abs} not found")),
            _ => Err(err!("{abs} is not a directory")),
        }
    }

    pub fn spawner(&self) -> Spawner {
        self.state.borrow().spawner.clone()
    }

    pub fn tty(&self) -> Rc<dyn TtyDriver> {
        self.state.borrow().tty.clone()
    }

    pub async fn getc(&self) -> u8 {
        // let console = Console::get_mut();
        loop {
            if let Some(b) = self.state.borrow().tty.tty_getc() {
                return b;
            }
            sys::yield_now().await;
        }
    }

    pub async fn readline(&self, buffer: &mut [u8]) -> usize {
        //let console = Console::get_mut();
        let mut index = 0usize;
        loop {
            if let Some(b) = self.state.borrow().tty.tty_getc() {
                if b == b'\r' || b == b'\n' {
                    break;
                }
                if index < buffer.len() {
                    buffer[index] = b;
                    index += 1;
                }
            }
            sys::yield_now().await;
        }
        index
    }

    pub fn add_parser(&self, parser: impl CmdParser + 'static) {
        self.state
            .borrow_mut()
            .cmds_parser_list
            .push_back(Rc::new(parser));
    }

    /// 注册一张命令别名表：`&[("ll", "ls -l"), ("bigfiles", "ls | where size > 0")]`。
    /// 表是 `'static` 的，直接编进二进制，运行时只多一个指针；可以多次调用注册多张表。
    ///
    /// 使用别名时，命令行上多写的参数会接到别名展开后的最后一段命令上，
    /// 例如 `ll /spi` 相当于 `ls -l /spi`。别名不能覆盖 `each`、`where` 这类语言内置命令。
    pub fn add_aliases(&self, aliases: &'static [(&'static str, &'static str)]) {
        self.state.borrow_mut().aliases.push(aliases);
    }

    /// 查别名。正在展开的别名当作不存在，这样 `("ls", "ls -l")` 里的 `ls`
    /// 指向真正的命令，不会无限递归。
    fn alias_body(&self, name: &str) -> Option<&'static str> {
        let state = self.state.borrow();
        if state.alias_stack.contains(&name) {
            return None;
        }
        state
            .aliases
            .iter()
            .rev()
            .find_map(|table| table.iter().find(|(alias, _)| *alias == name))
            .map(|(_, body)| *body)
    }

    /// 已注册的别名，后注册的表优先（与查找顺序一致）。
    fn alias_list(&self) -> Vec<(&'static str, &'static str)> {
        let state = self.state.borrow();
        let mut list: Vec<(&'static str, &'static str)> = Vec::new();
        for table in state.aliases.iter().rev() {
            for (name, body) in table.iter() {
                if !list.iter().any(|(seen, _)| seen == name) {
                    list.push((name, body));
                }
            }
        }
        list
    }

    fn enter_alias(&self, name: &str) -> bool {
        let mut state = self.state.borrow_mut();
        // 存静态表里的那份 name，避免为了记录展开状态再分配字符串
        let name = state
            .aliases
            .iter()
            .find_map(|table| table.iter().find(|(alias, _)| *alias == name))
            .map(|(alias, _)| *alias);
        match name {
            Some(name) => {
                state.alias_stack.push(name);
                true
            }
            None => false,
        }
    }

    fn leave_alias(&self) {
        self.state.borrow_mut().alias_stack.pop();
    }

    pub fn get_vars(&self) -> Vec<(String, CmdValue)> {
        self.state.borrow().vars.clone()
    }

    /// 读取会话变量（返回克隆）。
    pub fn get_var(&self, name: &str) -> Option<CmdValue> {
        self.state
            .borrow()
            .vars
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.clone())
    }

    pub fn get_var_str(&self, name: &str) -> Option<String> {
        if let Some(CmdValue::String(s)) = self.get_var(name) {
            Some(s)
        } else {
            None
        }
    }
    pub fn get_var_int(&self, name: &str) -> Option<i64> {
        if let Some(CmdValue::Int(i)) = self.get_var(name) {
            Some(i)
        } else {
            None
        }
    }
    pub fn get_var_float(&self, name: &str) -> Option<f64> {
        if let Some(CmdValue::Float(f)) = self.get_var(name) {
            Some(f)
        } else {
            None
        }
    }
    pub fn get_var_bool(&self, name: &str) -> Option<bool> {
        if let Some(CmdValue::Bool(b)) = self.get_var(name) {
            Some(b)
        } else {
            None
        }
    }

    /// 设置会话变量（已存在则覆盖）。
    pub fn set_var(&self, name: String, value: CmdValue) {
        let mut state = self.state.borrow_mut();
        if let Some(slot) = state.vars.iter_mut().find(|(n, _)| *n == name) {
            slot.1 = value;
        } else {
            state.vars.push((name, value));
        }
    }

    /// 删除会话变量（循环结束后恢复循环变量时使用）。
    pub fn remove_var(&self, name: &str) {
        let mut state = self.state.borrow_mut();
        if let Some(pos) = state.vars.iter().position(|(n, _)| n == name) {
            state.vars.remove(pos);
        }
    }

    // pub async fn join(id: u16) {
    //     loop {
    //         if !Executor::is_running(id) {
    //             break;
    //         }
    //         sys::yield_now().await;
    //     }
    // }

    // pub async fn read_key() -> Option<Key> {
    //     let console = Console::get_mut();
    //     let io = SimpleOs::console();
    //     let mut escape_state = EscapeState::Normal;
    //     loop {
    //         if let Some(b) = io.console_getc() {
    //             match escape_state {
    //                 EscapeState::Normal => match b {
    //                     b'\r' | b'\n' => return Some(Key::Enter),
    //                     3 => return Some(Key::CtrlC),
    //                     8 | 127 => return Some(Key::Backspace),
    //                     27 => {
    //                         escape_state = EscapeState::Escape;
    //                     }
    //                     b if b.is_ascii_graphic() || b == b' ' => {
    //                         return Some(Key::Char(b));
    //                     }
    //                     _ => {
    //                         return Some(Key::Unknown(b));
    //                     }
    //                 },
    //                 EscapeState::Escape => match b {
    //                     b'[' => {
    //                         escape_state = EscapeState::Bracket;
    //                     }
    //                     _ => {
    //                         escape_state = EscapeState::Normal;
    //                     }
    //                 },
    //                 EscapeState::Bracket => {
    //                     escape_state = EscapeState::Normal;
    //                     match b {
    //                         b'A' => return Some(Key::Up),
    //                         b'B' => return Some(Key::Down),
    //                         b'C' => return Some(Key::Right),
    //                         b'D' => return Some(Key::Left),
    //                         _ => return Some(Key::Unknown(b)),
    //                     }
    //                 }
    //             }
    //         }
    //         sys::yield_now();
    //     }
    // }
}

pub struct Console {
    state: Rc<RefCell<SharedState>>,
}

impl Console {
    pub fn new(spawner: Spawner, device: Rc<dyn Board>) -> (Self, Env) {
        let state = Rc::new(RefCell::new(SharedState::new(spawner, device)));
        (
            Console {
                state: state.clone(),
            },
            Env {
                state: state.clone(),
            },
        )
    }

    /// 把整段输入解析为语句并逐条执行。
    async fn run_line(&self, line: &str) {
        let block = match lang::parse_line(line) {
            Ok(block) => block,
            Err(e) => {
                println!("parse error: {}", e);
                return;
            }
        };
        for stmt in block.stmts {
            self.exec_statement(stmt).await;
        }
    }

    /// 执行一条语句：整条语句（包括循环体）作为一个前台任务运行，支持 Ctrl+C 整体终止。
    /// `let` 语句把结果存入变量（不打印），其余语句打印各自管道的结果。
    async fn exec_statement(&self, stmt: Statement) {
        self.state.borrow().tty.tty_flush();
        self.state.borrow().tty.tty_clear_rx();

        let parser_list = self.state.borrow().cmds_parser_list.clone();
        let env = Env {
            state: self.state.clone(),
        };
        let spawner = self.state.borrow().spawner.clone();
        let task_name = statement_task_name(&stmt);

        let pid = spawner.spawn(
            task_name,
            Box::pin(async move {
                if let Err(e) = eval_statement(&env, &parser_list, &stmt, true).await {
                    println!("Error: {}", e);
                }
                Ok(())
            }),
        );

        // 等待前台任务结束, 监听 Ctrl+C 终止
        loop {
            sys::yield_now().await;

            if !spawner.is_running(pid) {
                break;
            }

            // 监听 Ctrl+C 以终止前台任务
            if self.state.borrow().tty.tty_get_break() {
                spawner.kill(pid);
            }
        }
    }

    async fn parse_cmdline(&self, line_bytes: Vec<u8>) {
        let line_str = core::str::from_utf8(line_bytes.as_slice())
            .unwrap_or("")
            .trim_end();
        if line_str.trim().is_empty() && self.state.borrow().pending.is_empty() {
            return;
        }

        // 添加到历史记录
        self.state
            .borrow_mut()
            .add_to_history(line_str.trim().as_bytes());

        // 与之前未写完的行拼接；复制成拥有所有权的字符串，避免在 await 期间持有借用
        let line = {
            let mut state = self.state.borrow_mut();
            if !state.pending.is_empty() {
                state.pending.push('\n');
            }
            state.pending.push_str(line_str);
            state.pending.clone()
        };

        // 引号未闭合、花括号没配平或行尾是 `|`，说明还要接着输入
        if lang::is_incomplete(&line) {
            if line.len() > PENDING_BUFFER_SIZE {
                self.state.borrow_mut().pending.clear();
                println!("input too long, discarded");
            }
            return;
        }
        self.state.borrow_mut().pending.clear();
        self.run_line(&line).await;
    }

    pub async fn run(&self) {
        println!("Console started. Type 'help' or ? for commands.");
        self.state.borrow_mut().reset();
        loop {
            // 有的 tty 驱动把 Ctrl+C 作为 break 信号而不是普通字符上报，
            // 这里同样要能放弃未写完的多行输入
            if self.state.borrow().tty.tty_get_break() {
                self.state.borrow_mut().cancel_input();
            }
            let line = self.state.borrow_mut().poll_tty_input();
            if let Some(line) = line {
                self.parse_cmdline(line).await;
                self.state.borrow_mut().reset();
            }
            sys::yield_now().await;
        }
    }
}

/// 给任务取个可读的名字，方便 `ps` / `kill`。
fn statement_task_name(stmt: &Statement) -> String {
    match stmt {
        Statement::Let { name, .. } => format!("let {}", name),
        Statement::Expr(pipeline) => pipeline_task_name(pipeline),
        Statement::For { .. } => "for".to_string(),
        Statement::While { .. } => "while".to_string(),
        Statement::If { .. } => "if".to_string(),
        Statement::Break => "break".to_string(),
        Statement::Continue => "continue".to_string(),
    }
}

fn pipeline_task_name(pipeline: &Pipeline) -> String {
    match pipeline.first() {
        Some(Stage::Call { name, .. }) => name.clone(),
        Some(Stage::Expr(_)) => "expr".to_string(),
        None => "empty".to_string(),
    }
}

#[rustfmt::skip]
const LANGUAGE_HELP: &[(&str, &str)] = &[
    ("let <name> = <expr>",             "Define a variable ($name to use it)"),
    ("\"text ${name}\"",                  "Substitute a variable into a word or string"),
    ("$name = <expr>",                  "Assign a new value to a variable"),
    ("for <var> in <expr> { ... }",     "Loop over a range, list or table"),
    ("while <cond> { ... }",            "Loop while the condition holds"),
    ("if <cond> { ... } else { ... }",  "Run a block conditionally"),
    ("break | continue",                "Leave or restart the current loop"),
    ("<expr> | each { |it| ... }",      "Run a block for each item, collect results"),
    ("<expr> | where <cond>",           "Keep the items matching the condition (e.g. size > 0)"),
    ("echo <args...>",                  "Return the arguments as a value"),
    ("print [-n] <args...>",            "Print the arguments (-n: no newline)"),
    ("help|?",                          "Show this help message"),
];

fn print_help(env: &Env, parsers: &VecDeque<Rc<dyn CmdParser>>) {
    println!("Language:");
    for (syntax, desc) in LANGUAGE_HELP.iter() {
        println!("  {:<40} - {}", syntax, desc);
    }
    // println!("Available commands:");
    for parser in parsers.iter() {
        println!("{} commands:", parser.parser_id());
        for (cmd, desc) in parser.help().iter() {
            println!("  {:<40} - {}", cmd, desc);
        }
    }
    let aliases = env.alias_list();
    if !aliases.is_empty() {
        println!("Aliases:");
        for (name, body) in aliases.iter() {
            println!("  {:<40} - {}", name, body);
        }
    }
}

/// 语句执行的结果：正常结束并带一个值，或者需要中断/继续外层循环。
enum Flow {
    Value(CmdValue),
    Break,
    Continue,
}

/// 求值函数之间存在相互递归（块里可以再嵌套循环），
/// 在 no_std 下用装箱的 future 打断类型递归。
type EvalFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;

type Parsers = VecDeque<Rc<dyn CmdParser>>;

/// 执行一条语句。`print` 为 true 时，表达式语句会把结果直接打印出来。
fn eval_statement<'a>(
    env: &'a Env,
    parsers: &'a Parsers,
    stmt: &'a Statement,
    print: bool,
) -> EvalFuture<'a, Result<Flow>> {
    Box::pin(async move {
        match stmt {
            Statement::Let { name, value } => {
                let value = eval_pipeline(env, parsers, value).await?;
                env.set_var(name.clone(), value);
                Ok(Flow::Value(CmdValue::None))
            }
            Statement::Expr(pipeline) => {
                let value = eval_pipeline(env, parsers, pipeline).await?;
                if print {
                    value.print();
                }
                Ok(Flow::Value(value))
            }
            Statement::Break => Ok(Flow::Break),
            Statement::Continue => Ok(Flow::Continue),
            Statement::If {
                cond,
                then_body,
                else_body,
            } => {
                if eval_pipeline(env, parsers, cond).await?.truthy() {
                    eval_block(env, parsers, then_body, print).await
                } else if let Some(else_body) = else_body {
                    eval_block(env, parsers, else_body, print).await
                } else {
                    Ok(Flow::Value(CmdValue::None))
                }
            }
            Statement::While { cond, body } => {
                loop {
                    // 每轮让出一次 CPU，保证死循环也能被 Ctrl+C 打断
                    sys::yield_now().await;
                    if !eval_pipeline(env, parsers, cond).await?.truthy() {
                        break;
                    }
                    if let Flow::Break = eval_block(env, parsers, body, print).await? {
                        break;
                    }
                }
                Ok(Flow::Value(CmdValue::None))
            }
            Statement::For { var, iter, body } => {
                let source = eval_pipeline(env, parsers, iter).await?;
                let saved = env.get_var(var);
                let mut failure = None;
                for item in value_items(source) {
                    sys::yield_now().await;
                    env.set_var(var.clone(), item);
                    match eval_block(env, parsers, body, print).await {
                        Ok(Flow::Break) => break,
                        Ok(_) => {}
                        Err(e) => {
                            failure = Some(e);
                            break;
                        }
                    }
                }
                restore_var(env, var, saved);
                match failure {
                    Some(e) => Err(e),
                    None => Ok(Flow::Value(CmdValue::None)),
                }
            }
        }
    })
}

/// 顺序执行块内语句，`break`/`continue` 直接向外传递。
fn eval_block<'a>(
    env: &'a Env,
    parsers: &'a Parsers,
    block: &'a Block,
    print: bool,
) -> EvalFuture<'a, Result<Flow>> {
    Box::pin(async move {
        let mut last = CmdValue::None;
        for stmt in block.stmts.iter() {
            match eval_statement(env, parsers, stmt, print).await? {
                Flow::Value(value) => last = value,
                flow => return Ok(flow),
            }
        }
        Ok(Flow::Value(last))
    })
}

/// 依次执行管道的每一段，把上一段的输出作为下一段的输入，返回最终结果。
fn eval_pipeline<'a>(
    env: &'a Env,
    parsers: &'a Parsers,
    pipeline: &'a Pipeline,
) -> EvalFuture<'a, Result<CmdValue>> {
    eval_pipeline_with(env, parsers, pipeline, None)
}

/// 同上，但第一段可以带一个上游传来的输入（别名展开时用）。
fn eval_pipeline_with<'a>(
    env: &'a Env,
    parsers: &'a Parsers,
    pipeline: &'a Pipeline,
    input: Option<CmdValue>,
) -> EvalFuture<'a, Result<CmdValue>> {
    Box::pin(async move {
        let mut input = input;
        for stage in pipeline.iter() {
            let output = match stage {
                Stage::Expr(expr) => eval_expr(env, parsers, expr).await?,
                Stage::Call { name, args } => {
                    eval_call(env, parsers, name, args, input.take()).await?
                }
            };
            input = Some(output);
        }
        Ok(input.unwrap_or(CmdValue::None))
    })
}

/// 执行一次命令调用。需要结构化数据或代码块的内置命令在这里直接处理，
/// 其余命令把参数转成字符串后交给已注册的解析器。
fn eval_call<'a>(
    env: &'a Env,
    parsers: &'a Parsers,
    name: &'a str,
    args: &'a [Arg],
    input: Option<CmdValue>,
) -> EvalFuture<'a, Result<CmdValue>> {
    Box::pin(async move {
        match name {
            "help" | "?" => {
                print_help(env, parsers);
                Ok(CmdValue::None)
            }
            "each" => eval_each(env, parsers, args, input).await,
            "where" => eval_where(env, parsers, args, input).await,
            "echo" | "print" => {
                let mut values = Vec::with_capacity(args.len());
                let mut newline = true;
                for arg in args.iter() {
                    match arg {
                        Arg::Expr(Expr::Str(flag)) if name == "print" && flag == "-n" => {
                            newline = false;
                        }
                        Arg::Expr(expr) => values.push(eval_expr(env, parsers, expr).await?),
                        Arg::Block(_) => return Err(err!("{} does not accept a block", name)),
                    }
                }
                if name == "echo" {
                    Ok(match values.len() {
                        0 => input.unwrap_or(CmdValue::None),
                        1 => values.remove(0),
                        _ => CmdValue::List(values),
                    })
                } else {
                    print_values(&values, input, newline);
                    Ok(CmdValue::None)
                }
            }
            _ => {
                if let Some(body) = env.alias_body(name) {
                    if env.enter_alias(name) {
                        let result = eval_alias(env, parsers, name, body, args, input).await;
                        env.leave_alias();
                        return result;
                    }
                }
                // args 只含参数（不含命令名）；命令名由独立的 name/cmd 参数传递
                let mut resolved_args = Vec::with_capacity(args.len());
                for arg in args.iter() {
                    match arg {
                        Arg::Expr(expr) => {
                            resolved_args.push(eval_expr(env, parsers, expr).await?.to_string())
                        }
                        Arg::Block(_) => {
                            return Err(err!("command '{}' does not accept a block", name))
                        }
                    }
                }
                dispatch(env, parsers, name, &resolved_args, input).await
            }
        }
    })
}

/// 展开一条别名：把别名的文本解析后执行。每次调用都重新解析，别名表因此可以一直待在 flash 里。
///
/// - 单条管道：命令行上多写的参数接到最后一段命令上，上游管道输入交给第一段。
/// - `while` / `for` / `if` / `let` 或分号分隔的多条语句：按脚本执行（循环体里的表达式会打印）。
async fn eval_alias(
    env: &Env,
    parsers: &Parsers,
    name: &str,
    body: &'static str,
    args: &[Arg],
    input: Option<CmdValue>,
) -> Result<CmdValue> {
    let block = lang::parse_line(body).map_err(|e| err!("alias '{}': {}", name, e))?;

    if matches!(block.stmts.as_slice(), [Statement::Expr(_)]) {
        let mut pipeline = match block.stmts.into_iter().next() {
            Some(Statement::Expr(pipeline)) => pipeline,
            _ => unreachable!(),
        };
        if !args.is_empty() {
            match pipeline.last_mut() {
                Some(Stage::Call { args: tail, .. }) => tail.extend(args.iter().cloned()),
                _ => return Err(err!("alias '{}' takes no arguments", name)),
            }
        }
        return eval_pipeline_with(env, parsers, &pipeline, input).await;
    }

    if !args.is_empty() {
        return Err(err!("alias '{}' takes no arguments", name));
    }
    match eval_block(env, parsers, &block, true).await? {
        Flow::Value(value) => Ok(value),
        Flow::Break => Err(err!("break outside of loop")),
        Flow::Continue => Err(err!("continue outside of loop")),
    }
}

/// `each { |it| ... }`：对输入的每一项执行代码块，收集块的结果。
async fn eval_each(
    env: &Env,
    parsers: &Parsers,
    args: &[Arg],
    input: Option<CmdValue>,
) -> Result<CmdValue> {
    let block = args
        .iter()
        .find_map(|arg| match arg {
            Arg::Block(block) => Some(block),
            _ => None,
        })
        .ok_or_else(|| err!("usage: <pipeline> | each {{ |it| ... }}"))?;

    let param = block
        .params
        .first()
        .cloned()
        .unwrap_or_else(|| "it".to_string());
    let saved = env.get_var(&param);

    let mut collected = Vec::new();
    let mut failure = None;
    for item in value_items(input.unwrap_or(CmdValue::None)) {
        sys::yield_now().await;
        env.set_var(param.clone(), item);
        match eval_block(env, parsers, block, false).await {
            Ok(Flow::Value(value)) => {
                if !matches!(value, CmdValue::None) {
                    collected.push(value);
                }
            }
            Ok(Flow::Continue) => {}
            Ok(Flow::Break) => break,
            Err(e) => {
                failure = Some(e);
                break;
            }
        }
    }
    restore_var(env, &param, saved);
    match failure {
        Some(e) => Err(e),
        None => Ok(CmdValue::List(collected)),
    }
}

/// `where <cond>` / `where { |it| ... }`：保留条件成立的项。
/// 表格进表格出，其余输入产生列表。
async fn eval_where(
    env: &Env,
    parsers: &Parsers,
    args: &[Arg],
    input: Option<CmdValue>,
) -> Result<CmdValue> {
    let cond = args
        .first()
        .ok_or_else(|| err!("usage: <pipeline> | where <condition>"))?;
    // 表达式形式固定用 $it 指代当前项，块形式用它自己声明的参数
    let param = match cond {
        Arg::Block(block) => block
            .params
            .first()
            .cloned()
            .unwrap_or_else(|| "it".to_string()),
        Arg::Expr(_) => "it".to_string(),
    };
    let saved = env.get_var(&param);

    let mut failure = None;
    let kept = match input.unwrap_or(CmdValue::None) {
        CmdValue::Table(headers, rows) => {
            let mut kept = Vec::new();
            for row in rows.into_iter() {
                sys::yield_now().await;
                env.set_var(param.clone(), CmdValue::table_row(&headers, &row));
                match eval_condition(env, parsers, cond).await {
                    Ok(true) => kept.push(row),
                    Ok(false) => {}
                    Err(e) => {
                        failure = Some(e);
                        break;
                    }
                }
            }
            CmdValue::Table(headers, kept)
        }
        other => {
            let mut kept = Vec::new();
            for item in value_items(other) {
                sys::yield_now().await;
                env.set_var(param.clone(), item.clone());
                match eval_condition(env, parsers, cond).await {
                    Ok(true) => kept.push(item),
                    Ok(false) => {}
                    Err(e) => {
                        failure = Some(e);
                        break;
                    }
                }
            }
            CmdValue::List(kept)
        }
    };
    restore_var(env, &param, saved);
    match failure {
        Some(e) => Err(e),
        None => Ok(kept),
    }
}

async fn eval_condition(env: &Env, parsers: &Parsers, cond: &Arg) -> Result<bool> {
    match cond {
        Arg::Expr(expr) => Ok(eval_expr(env, parsers, expr).await?.truthy()),
        // 块里的 break/continue 对过滤没有意义，一律按不匹配处理
        Arg::Block(block) => match eval_block(env, parsers, block, false).await? {
            Flow::Value(value) => Ok(value.truthy()),
            _ => Ok(false),
        },
    }
}

fn eval_expr<'a>(
    env: &'a Env,
    parsers: &'a Parsers,
    expr: &'a Expr,
) -> EvalFuture<'a, Result<CmdValue>> {
    Box::pin(async move {
        match expr {
            Expr::Int(value) => Ok(CmdValue::Int(*value)),
            Expr::Float(value) => Ok(CmdValue::Float(*value)),
            Expr::Bool(value) => Ok(CmdValue::Bool(*value)),
            Expr::Str(value) => Ok(CmdValue::String(value.clone())),
            Expr::Interp(parts) => {
                let mut text = String::new();
                for part in parts.iter() {
                    match part {
                        StrPart::Lit(literal) => text.push_str(literal),
                        StrPart::Var(path) => text.push_str(&lookup_var(env, path)?.to_string()),
                    }
                }
                Ok(CmdValue::String(text))
            }
            Expr::Var(path) => lookup_var(env, path),
            Expr::List(items) => {
                let mut values = Vec::with_capacity(items.len());
                for item in items.iter() {
                    values.push(eval_expr(env, parsers, item).await?);
                }
                Ok(CmdValue::List(values))
            }
            Expr::Range {
                start,
                end,
                inclusive,
            } => {
                let start = expect_int(&eval_expr(env, parsers, start).await?)?;
                let end = expect_int(&eval_expr(env, parsers, end).await?)?;
                Ok(CmdValue::Range {
                    start,
                    end,
                    inclusive: *inclusive,
                })
            }
            Expr::Not(inner) => Ok(CmdValue::Bool(
                !eval_expr(env, parsers, inner).await?.truthy(),
            )),
            Expr::Field { base, path } => {
                let mut value = eval_expr(env, parsers, base).await?;
                for key in path.split('.') {
                    if key.is_empty() {
                        continue;
                    }
                    let type_name = value.type_name();
                    value = value
                        .get(key)
                        .ok_or_else(|| err!("{} has no field '{}'", type_name, key))?;
                }
                Ok(value)
            }
            Expr::Binary { op, lhs, rhs } => {
                // and / or 短路求值
                if let BinOp::And | BinOp::Or = op {
                    let left = eval_expr(env, parsers, lhs).await?.truthy();
                    let shortcut = matches!(op, BinOp::Or);
                    if left == shortcut {
                        return Ok(CmdValue::Bool(shortcut));
                    }
                    return Ok(CmdValue::Bool(eval_expr(env, parsers, rhs).await?.truthy()));
                }
                let lhs = eval_expr(env, parsers, lhs).await?;
                let rhs = eval_expr(env, parsers, rhs).await?;
                apply_binop(*op, &lhs, &rhs)
            }
            Expr::Sub(pipeline) => eval_pipeline(env, parsers, pipeline).await,
        }
    })
}

fn apply_binop(op: BinOp, lhs: &CmdValue, rhs: &CmdValue) -> Result<CmdValue> {
    match op {
        BinOp::Add => lhs.add(rhs),
        BinOp::Sub => lhs.sub(rhs),
        BinOp::Mul => lhs.mul(rhs),
        BinOp::Div => lhs.div(rhs),
        BinOp::Mod => lhs.rem(rhs),
        BinOp::Eq => Ok(CmdValue::Bool(lhs.equals(rhs))),
        BinOp::Ne => Ok(CmdValue::Bool(!lhs.equals(rhs))),
        BinOp::And => Ok(CmdValue::Bool(lhs.truthy() && rhs.truthy())),
        BinOp::Or => Ok(CmdValue::Bool(lhs.truthy() || rhs.truthy())),
        BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
            let ord = lhs.compare(rhs).ok_or_else(|| {
                err!(
                    "cannot compare {} with {}",
                    lhs.type_name(),
                    rhs.type_name()
                )
            })?;
            let result = match op {
                BinOp::Lt => ord == Ordering::Less,
                BinOp::Le => ord != Ordering::Greater,
                BinOp::Gt => ord == Ordering::Greater,
                _ => ord != Ordering::Less,
            };
            Ok(CmdValue::Bool(result))
        }
    }
}

/// 读取 `$name` 或 `$name.field.0` 这样的变量路径。
fn lookup_var(env: &Env, path: &str) -> Result<CmdValue> {
    let mut parts = path.split('.');
    let name = parts.next().unwrap_or_default();
    let mut value = env
        .get_var(name)
        .ok_or_else(|| err!("variable ${} not found", name))?;
    for key in parts {
        value = value
            .get(key)
            .ok_or_else(|| err!("${} has no field '{}'", name, key))?;
    }
    Ok(value)
}

fn expect_int(value: &CmdValue) -> Result<i64> {
    match value {
        CmdValue::Int(i) => Ok(*i),
        CmdValue::Float(f) => Ok(*f as i64),
        other => Err(err!(
            "range bounds must be integers, found {}",
            other.type_name()
        )),
    }
}

fn restore_var(env: &Env, name: &str, saved: Option<CmdValue>) {
    match saved {
        Some(value) => env.set_var(name.to_string(), value),
        None => env.remove_var(name),
    }
}

fn print_values(values: &[CmdValue], input: Option<CmdValue>, newline: bool) {
    // 没有参数时打印管道输入，单个参数按结构化方式打印（表格仍是表格）
    if values.is_empty() {
        match input {
            Some(value) if newline => value.print(),
            Some(value) => print!("{}", value.to_string()),
            None if newline => println!(),
            None => {}
        }
        return;
    }
    if values.len() == 1 && newline {
        values[0].print();
        return;
    }
    let text = values
        .iter()
        .map(|value| value.to_string())
        .collect::<Vec<String>>()
        .join(" ");
    if newline {
        println!("{}", text);
    } else {
        print!("{}", text);
    }
}

/// `for` / `each` 遍历一个值时产生的元素序列。
/// 区间是惰性的，`for i in 1..100000` 不会在堆上展开成列表。
enum ValueIter {
    Empty,
    Once(Option<CmdValue>),
    Range {
        next: i64,
        last: i64,
        step: i64,
        done: bool,
    },
    Items(vec::IntoIter<CmdValue>),
}

impl Iterator for ValueIter {
    type Item = CmdValue;

    fn next(&mut self) -> Option<CmdValue> {
        match self {
            ValueIter::Empty => None,
            ValueIter::Once(value) => value.take(),
            ValueIter::Range {
                next,
                last,
                step,
                done,
            } => {
                if *done {
                    return None;
                }
                let current = *next;
                if current == *last {
                    *done = true;
                } else {
                    *next += *step;
                }
                Some(CmdValue::Int(current))
            }
            ValueIter::Items(items) => items.next(),
        }
    }
}

fn value_items(value: CmdValue) -> ValueIter {
    match value {
        CmdValue::None => ValueIter::Empty,
        CmdValue::List(items) => ValueIter::Items(items.into_iter()),
        CmdValue::Table(headers, rows) => {
            let rows: Vec<CmdValue> = rows
                .iter()
                .map(|row| CmdValue::table_row(&headers, row))
                .collect();
            ValueIter::Items(rows.into_iter())
        }
        CmdValue::Record(fields) => {
            let entries: Vec<CmdValue> = fields
                .into_iter()
                .map(|(key, value)| {
                    CmdValue::record(vec![("key", CmdValue::String(key)), ("value", value)])
                })
                .collect();
            ValueIter::Items(entries.into_iter())
        }
        CmdValue::Range {
            start,
            end,
            inclusive,
        } => match range_parts(start, end, inclusive) {
            Some((first, last, step)) => ValueIter::Range {
                next: first,
                last,
                step,
                done: false,
            },
            None => ValueIter::Empty,
        },
        other => ValueIter::Once(Some(other)),
    }
}

/// 把命令分发给能识别它的解析器。`input` 只会被匹配到的解析器取走。
async fn dispatch(
    env: &Env,
    parsers: &VecDeque<Rc<dyn CmdParser>>,
    name: &str,
    args: &Vec<String>,
    input: Option<CmdValue>,
) -> Result<CmdValue> {
    let mut input = input;
    for parser in parsers.iter() {
        if let Some(result) = parser.parse(env, name, args, &mut input).await {
            return result;
        }
    }
    Err(err!("Unknown command: {}", name))
}
