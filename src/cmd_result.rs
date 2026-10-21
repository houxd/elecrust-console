use core::cmp::Ordering;
use elecrust::alloc::format;
use elecrust::alloc::string::{String, ToString};
use elecrust::alloc::vec::Vec;
use elecrust::err;
use elecrust::error::Result;
use elecrust::{print, println};

/// 命令产出的结构化数据单元（类似 nushell 的 Value）。
/// 既是命令的返回值，也是管道里各段之间传递的数据，还能存入变量。
#[derive(Clone)]
pub enum CmdValue {
    None,
    String(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    Binary(Vec<u8>),
    List(Vec<CmdValue>),
    Record(Vec<(String, CmdValue)>),
    Table(Vec<String>, Vec<Vec<CmdValue>>),
    /// 整数区间，惰性表示，避免 `1..100000` 这类写法在堆上展开。
    Range {
        start: i64,
        end: i64,
        /// `1..5` 为 true（含 5），`1..<5` 为 false（不含 5）。
        inclusive: bool,
    },
}

/// 计算区间的实际迭代参数：`(第一个值, 最后一个值, 步进)`，空区间返回 None。
/// 起点大于终点时自动倒序，与 nushell 的 `10..1` 行为一致。
pub fn range_parts(start: i64, end: i64, inclusive: bool) -> Option<(i64, i64, i64)> {
    if start <= end {
        let last = if inclusive { end } else { end - 1 };
        if last < start { None } else { Some((start, last, 1)) }
    } else {
        let last = if inclusive { end } else { end + 1 };
        if last > start { None } else { Some((start, last, -1)) }
    }
}

impl CmdValue {
    /// 用 &str 切片快速构造 Table 的表头。
    pub fn table(headers: &[&str], rows: Vec<Vec<CmdValue>>) -> Self {
        CmdValue::Table(headers.iter().map(|h| h.to_string()).collect(), rows)
    }

    /// 用 &str 键快速构造 Record。
    pub fn record(fields: Vec<(&str, CmdValue)>) -> Self {
        CmdValue::Record(
            fields
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
        )
    }

    /// 把表格的一行按表头组装成 Record（`for`/`each` 遍历表格时用）。
    pub fn table_row(headers: &[String], row: &[CmdValue]) -> Self {
        CmdValue::Record(
            headers
                .iter()
                .enumerate()
                .map(|(i, h)| (h.clone(), row.get(i).cloned().unwrap_or(CmdValue::None)))
                .collect(),
        )
    }

    /// 是否为数值类型（用于表格右对齐、排序、比较等）。
    pub fn is_number(&self) -> bool {
        matches!(self, CmdValue::Int(_) | CmdValue::Float(_))
    }

    /// 类型名，用于错误信息。
    pub fn type_name(&self) -> &'static str {
        match self {
            CmdValue::None => "nothing",
            CmdValue::String(_) => "string",
            CmdValue::Int(_) => "int",
            CmdValue::Float(_) => "float",
            CmdValue::Bool(_) => "bool",
            CmdValue::Binary(_) => "binary",
            CmdValue::List(_) => "list",
            CmdValue::Record(_) => "record",
            CmdValue::Table(_, _) => "table",
            CmdValue::Range { .. } => "range",
        }
    }

    pub fn to_string(&self) -> String {
        match self {
            CmdValue::None => String::new(),
            CmdValue::String(value) => value.clone(),
            CmdValue::Int(value) => value.to_string(),
            CmdValue::Float(value) => format!("{}", value),
            CmdValue::Bool(value) => value.to_string(),
            CmdValue::Binary(binary) => format!("{{BINARY[{}]}}", binary.len()),
            CmdValue::List(list) => format!("{{LIST[{}]}}", list.len()),
            CmdValue::Record(record) => format!("{{RECORD[{}]}}", record.len()),
            CmdValue::Table(headers, rows) => {
                format!("{{TABLE[{}x{}]}}", headers.len(), rows.len())
            }
            CmdValue::Range {
                start,
                end,
                inclusive,
            } => {
                if *inclusive {
                    format!("{}..{}", start, end)
                } else {
                    format!("{}..<{}", start, end)
                }
            }
        }
    }
    pub fn print(&self) {
        match self {
            CmdValue::None => {}
            CmdValue::String(value) => println!("{}", value),
            CmdValue::Int(value) => println!("{}", value),
            CmdValue::Float(value) => println!("{}", value),
            CmdValue::Bool(value) => println!("{}", value),
            CmdValue::Binary(binary) => println!("{:?}", binary),
            CmdValue::List(list) => {
                for item in list.iter() {
                    item.print();
                }
            }
            CmdValue::Range {
                start,
                end,
                inclusive,
            } => {
                if let Some((first, last, step)) = range_parts(*start, *end, *inclusive) {
                    let mut i = first;
                    loop {
                        println!("{}", i);
                        if i == last {
                            break;
                        }
                        i += step;
                    }
                }
            }
            CmdValue::Record(record) => {
                for (key, value) in record.iter() {
                    println!("{}: {}", key, value.to_string());
                }
            }
            CmdValue::Table(headers, table) => {
                // 列之间的间隔
                const GAP: &str = "  ";
                let cols = headers.len();

                // 统计每列的最大宽度（同时兼容行列数不一致的情况）
                let mut max_widths = Vec::with_capacity(cols);
                for header in headers.iter() {
                    max_widths.push(header.len());
                }
                for row in table.iter() {
                    for (i, cell) in row.iter().enumerate() {
                        if i < cols {
                            max_widths[i] = max_widths[i].max(cell.to_string().len());
                        }
                    }
                }

                // 打印表头（最后一列不补空格，避免尾随空白）
                for (i, header) in headers.iter().enumerate() {
                    if i + 1 < cols {
                        print!("{:<width$}{}", header, GAP, width = max_widths[i]);
                    } else {
                        print!("{}", header);
                    }
                }
                println!();

                // 表头下的分割线，长度覆盖整个表宽
                let total: usize =
                    max_widths.iter().sum::<usize>() + GAP.len() * cols.saturating_sub(1);
                println!("{}", "-".repeat(total));

                // 打印内容
                for row in table.iter() {
                    for i in 0..cols {
                        let cell = row.get(i);
                        let text = cell.map(|c| c.to_string()).unwrap_or_default();
                        // 数字右对齐，字符串及其它类型（含转成字符串的）左对齐
                        let right_align = matches!(cell, Some(c) if c.is_number());
                        if i + 1 < cols {
                            if right_align {
                                print!("{:>width$}{}", text, GAP, width = max_widths[i]);
                            } else {
                                print!("{:<width$}{}", text, GAP, width = max_widths[i]);
                            }
                        } else {
                            // 最后一列：右对齐用前导空格补齐；左对齐不补，避免尾随空白
                            if right_align {
                                print!("{:>width$}", text, width = max_widths[i]);
                            } else {
                                print!("{}", text);
                            }
                        }
                    }
                    println!();
                }
            }
        }
    }
}

/// 表达式求值需要的取值、判断与运算。
impl CmdValue {
    /// 条件真假判定（`if` / `while` / `and` / `or` 用）。
    /// 空字符串、0、空容器、None 为假，其余为真。
    pub fn truthy(&self) -> bool {
        match self {
            CmdValue::None => false,
            CmdValue::Bool(b) => *b,
            CmdValue::Int(i) => *i != 0,
            CmdValue::Float(f) => *f != 0.0,
            CmdValue::String(s) => !s.is_empty(),
            CmdValue::Binary(b) => !b.is_empty(),
            CmdValue::List(l) => !l.is_empty(),
            CmdValue::Record(r) => !r.is_empty(),
            CmdValue::Table(_, rows) => !rows.is_empty(),
            CmdValue::Range {
                start,
                end,
                inclusive,
            } => range_parts(*start, *end, *inclusive).is_some(),
        }
    }

    /// 元素个数：容器返回长度，字符串返回字符数，标量返回 None。
    pub fn len(&self) -> Option<usize> {
        match self {
            CmdValue::String(s) => Some(s.chars().count()),
            CmdValue::Binary(b) => Some(b.len()),
            CmdValue::List(l) => Some(l.len()),
            CmdValue::Record(r) => Some(r.len()),
            CmdValue::Table(_, rows) => Some(rows.len()),
            CmdValue::Range {
                start,
                end,
                inclusive,
            } => Some(match range_parts(*start, *end, *inclusive) {
                Some((first, last, _)) => ((last - first).unsigned_abs() as usize) + 1,
                None => 0,
            }),
            _ => None,
        }
    }

    /// 取字段或下标，供 `$var.field` / `$var.0` 使用。
    /// 表格用列名取一整列，用数字取某一行。
    pub fn get(&self, key: &str) -> Option<CmdValue> {
        match self {
            CmdValue::Record(fields) => fields
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value.clone()),
            CmdValue::List(items) => key.parse::<usize>().ok().and_then(|i| items.get(i).cloned()),
            CmdValue::Binary(bytes) => key
                .parse::<usize>()
                .ok()
                .and_then(|i| bytes.get(i).map(|b| CmdValue::Int(*b as i64))),
            CmdValue::Table(headers, rows) => {
                if let Ok(index) = key.parse::<usize>() {
                    rows.get(index)
                        .map(|row| CmdValue::table_row(headers, row))
                } else {
                    let col = headers.iter().position(|h| h == key)?;
                    Some(CmdValue::List(
                        rows.iter()
                            .map(|row| row.get(col).cloned().unwrap_or(CmdValue::None))
                            .collect(),
                    ))
                }
            }
            _ => None,
        }
    }

    fn as_f64(&self) -> Option<f64> {
        match self {
            CmdValue::Int(i) => Some(*i as f64),
            CmdValue::Float(f) => Some(*f),
            _ => None,
        }
    }

    fn int_pair(&self, rhs: &CmdValue) -> Option<(i64, i64)> {
        match (self, rhs) {
            (CmdValue::Int(a), CmdValue::Int(b)) => Some((*a, *b)),
            _ => None,
        }
    }

    fn num_pair(&self, rhs: &CmdValue) -> Option<(f64, f64)> {
        Some((self.as_f64()?, rhs.as_f64()?))
    }

    /// 相加：数值求和，字符串拼接，列表连接。
    pub fn add(&self, rhs: &CmdValue) -> Result<CmdValue> {
        if let Some((a, b)) = self.int_pair(rhs) {
            return Ok(CmdValue::Int(a.wrapping_add(b)));
        }
        if let Some((a, b)) = self.num_pair(rhs) {
            return Ok(CmdValue::Float(a + b));
        }
        match (self, rhs) {
            (CmdValue::String(a), b) => Ok(CmdValue::String(format!("{}{}", a, b.to_string()))),
            (a, CmdValue::String(b)) => Ok(CmdValue::String(format!("{}{}", a.to_string(), b))),
            (CmdValue::List(a), CmdValue::List(b)) => {
                let mut items = a.clone();
                items.extend(b.iter().cloned());
                Ok(CmdValue::List(items))
            }
            (a, b) => Err(err!("cannot add {} and {}", a.type_name(), b.type_name())),
        }
    }

    pub fn sub(&self, rhs: &CmdValue) -> Result<CmdValue> {
        if let Some((a, b)) = self.int_pair(rhs) {
            return Ok(CmdValue::Int(a.wrapping_sub(b)));
        }
        match self.num_pair(rhs) {
            Some((a, b)) => Ok(CmdValue::Float(a - b)),
            None => Err(err!(
                "cannot subtract {} from {}",
                rhs.type_name(),
                self.type_name()
            )),
        }
    }

    pub fn mul(&self, rhs: &CmdValue) -> Result<CmdValue> {
        if let Some((a, b)) = self.int_pair(rhs) {
            return Ok(CmdValue::Int(a.wrapping_mul(b)));
        }
        match self.num_pair(rhs) {
            Some((a, b)) => Ok(CmdValue::Float(a * b)),
            None => Err(err!(
                "cannot multiply {} and {}",
                self.type_name(),
                rhs.type_name()
            )),
        }
    }

    /// 相除：整数能整除时保持整数，否则退化为浮点。
    pub fn div(&self, rhs: &CmdValue) -> Result<CmdValue> {
        if let Some((a, b)) = self.int_pair(rhs) {
            if b == 0 {
                return Err(err!("division by zero"));
            }
            if a % b == 0 {
                return Ok(CmdValue::Int(a / b));
            }
            return Ok(CmdValue::Float(a as f64 / b as f64));
        }
        match self.num_pair(rhs) {
            Some((_, b)) if b == 0.0 => Err(err!("division by zero")),
            Some((a, b)) => Ok(CmdValue::Float(a / b)),
            None => Err(err!(
                "cannot divide {} by {}",
                self.type_name(),
                rhs.type_name()
            )),
        }
    }

    pub fn rem(&self, rhs: &CmdValue) -> Result<CmdValue> {
        if let Some((a, b)) = self.int_pair(rhs) {
            if b == 0 {
                return Err(err!("division by zero"));
            }
            return Ok(CmdValue::Int(a % b));
        }
        match self.num_pair(rhs) {
            Some((_, b)) if b == 0.0 => Err(err!("division by zero")),
            Some((a, b)) => Ok(CmdValue::Float(a % b)),
            None => Err(err!(
                "cannot take {} mod {}",
                self.type_name(),
                rhs.type_name()
            )),
        }
    }

    /// 大小比较，类型不可比时返回 None。
    pub fn compare(&self, rhs: &CmdValue) -> Option<Ordering> {
        match (self, rhs) {
            (CmdValue::String(a), CmdValue::String(b)) => Some(a.as_str().cmp(b.as_str())),
            (CmdValue::Bool(a), CmdValue::Bool(b)) => Some(a.cmp(b)),
            _ => {
                let (a, b) = self.num_pair(rhs)?;
                a.partial_cmp(&b)
            }
        }
    }

    /// 相等判断，比 `compare` 宽松：不同类型直接判为不等而不是报错。
    pub fn equals(&self, rhs: &CmdValue) -> bool {
        match (self, rhs) {
            (CmdValue::None, CmdValue::None) => true,
            (CmdValue::String(a), CmdValue::String(b)) => a == b,
            (CmdValue::Bool(a), CmdValue::Bool(b)) => a == b,
            (CmdValue::Binary(a), CmdValue::Binary(b)) => a == b,
            (CmdValue::List(a), CmdValue::List(b)) => {
                a.len() == b.len() && a.iter().zip(b.iter()).all(|(x, y)| x.equals(y))
            }
            (CmdValue::Record(a), CmdValue::Record(b)) => {
                a.len() == b.len()
                    && a.iter()
                        .zip(b.iter())
                        .all(|((ka, va), (kb, vb))| ka == kb && va.equals(vb))
            }
            _ => matches!(self.compare(rhs), Some(Ordering::Equal)),
        }
    }
}
