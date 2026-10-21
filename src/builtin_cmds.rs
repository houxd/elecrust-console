use crate::CmdParser;
use crate::CmdValue;
use crate::Env;
use async_trait::async_trait;
use core::cell::RefCell;
use core::cmp::Ordering;
use elecrust::alloc;
use elecrust::alloc::boxed::Box;
use elecrust::alloc::format;
use elecrust::alloc::rc::Rc;
use elecrust::alloc::string::String;
use elecrust::alloc::string::ToString;
use elecrust::alloc::vec;
use elecrust::alloc::vec::Vec;
use elecrust::err;
use elecrust::error::Error;
use elecrust::error::Result;
use elecrust::print;
use elecrust::sys::Device;
use elecrust::time::{Date, Month, OffsetDateTime, Time, UtcDateTime, UtcOffset};
use elecrust::{println, sys};

const DATE_USAGE: &str = "usage: date [-u] [-s <date_time> | -S] [-f <rtc>]";

/// 读取 TZ（整型，单位秒）。未设置时按 UTC 处理。
fn env_tz_offset(env: &Env) -> Result<Option<UtcOffset>> {
    match env.get_var_int("TZ") {
        Some(seconds) => UtcOffset::from_whole_seconds(seconds as i32)
            .map(Some)
            .map_err(|e| Error::new_owned(format!("invalid timezone offset: {e}"))),
        None => Ok(None),
    }
}

/// 把墙上时钟转成 RTC 使用的 UTC。TZ 存在时按本地时区换算，否则当作 UTC。
fn wall_clock_to_utc(date: Date, time: Time, tz: Option<UtcOffset>) -> Result<UtcDateTime> {
    match tz {
        Some(offset) => OffsetDateTime::new_in_offset(date, time, offset)
            .checked_to_utc()
            .ok_or_else(|| Error::new("datetime out of range")),
        None => Ok(UtcDateTime::new(date, time)),
    }
}

fn format_datetime(utc: UtcDateTime, tz: Option<UtcOffset>) -> Result<CmdValue> {
    match tz {
        Some(offset) => {
            let local = utc
                .checked_to_offset(offset)
                .ok_or_else(|| Error::new("datetime out of range"))?;
            Ok(CmdValue::String(format!("{local}")))
        }
        None => Ok(CmdValue::String(format!("{utc}"))),
    }
}

/// 解析 `date` 输出的时间文本，例如 `2000-01-01 0:29:55.0 +00`。
/// 时区后缀只作格式校验，换算用 TZ / `-u`。
fn parse_date_time_text(input: &str) -> Result<(Date, Time)> {
    let input = input.trim();
    let mut parts = input.split_whitespace();
    let date_part = parts.next().ok_or_else(|| Error::new(DATE_USAGE))?;
    let time_part = parts
        .next()
        .ok_or_else(|| Error::new("invalid datetime, expected 'YYYY-MM-DD H:MM:SS.0 +00'"))?;
    if let Some(offset_part) = parts.next() {
        parse_offset_text(offset_part)?;
    }
    if parts.next().is_some() {
        return Err(err!("invalid datetime: {input}"));
    }
    Ok((parse_date_part(date_part)?, parse_time_part(time_part)?))
}

fn parse_date_part(s: &str) -> Result<Date> {
    let mut segs = s.split('-');
    let year = parse_component::<i32>(segs.next(), "year")?;
    let month = parse_component::<u8>(segs.next(), "month")?;
    let day = parse_component::<u8>(segs.next(), "day")?;
    if segs.next().is_some() {
        return Err(err!("invalid date: {s}"));
    }
    let month = Month::try_from(month).map_err(|e| err!("invalid month: {e}"))?;
    Date::from_calendar_date(year, month, day).map_err(|e| err!("invalid date: {e}"))
}

fn parse_time_part(s: &str) -> Result<Time> {
    let (hms, frac) = match s.split_once('.') {
        Some((hms, frac)) => (hms, Some(frac)),
        None => (s, None),
    };
    let mut segs = hms.split(':');
    let hour = parse_component::<u8>(segs.next(), "hour")?;
    let minute = parse_component::<u8>(segs.next(), "minute")?;
    let second = parse_component::<u8>(segs.next(), "second")?;
    if segs.next().is_some() {
        return Err(err!("invalid time: {s}"));
    }
    let nano = match frac {
        Some(frac) => parse_subsecond(frac)?,
        None => 0,
    };
    Time::from_hms_nano(hour, minute, second, nano).map_err(|e| err!("invalid time: {e}"))
}

fn parse_subsecond(frac: &str) -> Result<u32> {
    if frac.is_empty() || frac.len() > 9 || !frac.bytes().all(|b| b.is_ascii_digit()) {
        return Err(err!("invalid subsecond: .{frac}"));
    }
    let mut nano = 0u32;
    for (i, digit) in frac.bytes().enumerate() {
        nano += (digit - b'0') as u32 * 10u32.pow(8 - i as u32);
    }
    Ok(nano)
}

/// 接受 `+00`、`+08`、`+08:00`、`+08:00:00` 这类 time crate 输出的时区后缀。
fn parse_offset_text(s: &str) -> Result<UtcOffset> {
    let (sign, rest) = if let Some(rest) = s.strip_prefix('+') {
        (1i8, rest)
    } else if let Some(rest) = s.strip_prefix('-') {
        (-1i8, rest)
    } else {
        return Err(err!("invalid timezone offset: {s}"));
    };
    let mut segs = rest.split(':');
    let hours = parse_component::<i8>(segs.next(), "offset hour")?;
    let minutes = match segs.next() {
        Some(m) => parse_component::<i8>(Some(m), "offset minute")?,
        None => 0,
    };
    let seconds = match segs.next() {
        Some(sec) => parse_component::<i8>(Some(sec), "offset second")?,
        None => 0,
    };
    if segs.next().is_some() {
        return Err(err!("invalid timezone offset: {s}"));
    }
    UtcOffset::from_hms(sign * hours, sign * minutes, sign * seconds)
        .map_err(|e| err!("invalid timezone offset: {e}"))
}

fn parse_component<T: core::str::FromStr>(raw: Option<&str>, name: &str) -> Result<T> {
    let raw = raw.ok_or_else(|| err!("missing {name}"))?;
    raw.parse::<T>().map_err(|_| err!("invalid {name}: {raw}"))
}

/// 排序用的比较。类型不同、无法比大小时退化成按文本比较，
/// 保证 sort-by 对混合数据也有稳定可预期的结果。
fn compare_cells(a: Option<&CmdValue>, b: Option<&CmdValue>) -> Ordering {
    match (a, b) {
        (Some(a), Some(b)) => a
            .compare(b)
            .unwrap_or_else(|| a.to_string().cmp(&b.to_string())),
        (None, None) => Ordering::Equal,
        (None, _) => Ordering::Less,
        (_, None) => Ordering::Greater,
    }
}

#[allow(unused)]
pub struct BuiltinCmds;

#[allow(unused)]
impl BuiltinCmds {
    pub fn new() -> Self {
        BuiltinCmds
    }

    pub fn cmd_null(&self, _env: &Env, _args: &Vec<String>) -> Result<CmdValue> {
        Ok(CmdValue::None)
    }

    pub fn cmd_clear(&self, _env: &Env, _args: &Vec<String>) -> Result<CmdValue> {
        print!("\x1B[2J\x1B[1;1H");
        Ok(CmdValue::None)
    }

    /// 统计管道输入的元素个数：列表/表格返回条数，字符串返回字符数。
    pub fn cmd_length(&self, _env: &Env, input: Option<CmdValue>) -> Result<CmdValue> {
        match input {
            Some(value) => match value.len() {
                Some(len) => Ok(CmdValue::Int(len as i64)),
                None => Err(err!("{} has no length", value.type_name())),
            },
            None => Err(Error::new("usage: <pipeline> | length")),
        }
    }

    /// 按列排序管道输入。表格排序后仍是表格；不给列名时表格按第一列排序，
    /// 列表按元素本身排序。`-r` / `--reverse` 反转结果。
    pub fn cmd_sort_by(
        &self,
        _env: &Env,
        args: &Vec<String>,
        input: Option<CmdValue>,
    ) -> Result<CmdValue> {
        let mut columns: Vec<&str> = Vec::new();
        let mut reverse = false;
        for arg in args.iter() {
            match arg.as_str() {
                "-r" | "--reverse" => reverse = true,
                flag if flag.starts_with('-') => return Err(err!("unknown flag: {flag}")),
                column => columns.push(column),
            }
        }

        let input =
            input.ok_or_else(|| Error::new("usage: <pipeline> | sort-by [column...] [-r]"))?;
        match input {
            CmdValue::Table(headers, mut rows) => {
                // 列名在这里就换算成下标，避免每次比较都去查表头
                let mut indexes = Vec::with_capacity(columns.len());
                for column in columns.iter() {
                    match headers.iter().position(|header| header.as_str() == *column) {
                        Some(index) => indexes.push(index),
                        None => return Err(err!("column '{column}' not found")),
                    }
                }
                if indexes.is_empty() {
                    indexes.push(0);
                }
                rows.sort_by(|a, b| {
                    indexes
                        .iter()
                        .map(|&i| compare_cells(a.get(i), b.get(i)))
                        .find(|ord| *ord != Ordering::Equal)
                        .unwrap_or(Ordering::Equal)
                });
                if reverse {
                    rows.reverse();
                }
                Ok(CmdValue::Table(headers, rows))
            }
            CmdValue::List(mut items) => {
                items.sort_by(|a, b| {
                    if columns.is_empty() {
                        return compare_cells(Some(a), Some(b));
                    }
                    columns
                        .iter()
                        .map(|column| {
                            let (a, b) = (a.get(column), b.get(column));
                            compare_cells(a.as_ref(), b.as_ref())
                        })
                        .find(|ord| *ord != Ordering::Equal)
                        .unwrap_or(Ordering::Equal)
                });
                if reverse {
                    items.reverse();
                }
                Ok(CmdValue::List(items))
            }
            CmdValue::Range {
                start,
                end,
                inclusive,
            } if columns.is_empty() => {
                // 区间本身就是有序的，逆序时把两端换过来即可，不必展开成列表
                match crate::cmd_result::range_parts(start, end, inclusive) {
                    Some((first, last, _)) if reverse => Ok(CmdValue::Range {
                        start: last,
                        end: first,
                        inclusive: true,
                    }),
                    _ => Ok(CmdValue::Range {
                        start,
                        end,
                        inclusive,
                    }),
                }
            }
            other => Err(err!("cannot sort {}", other.type_name())),
        }
    }

    // pub fn cmd_reset(&self, _args: &Vec<String>) -> TaskResult {
    //     sys::get_device().cpu().cpu_reset();
    //     Ok(())
    // }

    pub async fn cmd_sleep(&self, _env: &Env, args: &Vec<String>) -> Result<CmdValue> {
        if let Some(sec_str) = args.get(0) {
            if let Ok(sec) = sec_str.parse::<f32>() {
                sys::sleep_ms((sec * 1000.0) as u32).await;
                Ok(CmdValue::None)
            } else {
                // println!("Invalid sleep duration: {}", sec_str);
                Err(err!("invalid sleep duration: {sec_str}"))
            }
        } else {
            // println!("Usage: sleep <seconds>");
            Err(Error::new("usage: sleep <seconds>"))
        }
    }

    pub fn cmd_ps(&self, env: &Env, _args: &Vec<String>) -> Result<CmdValue> {
        let task_list = env.spawner().task_list();
        let mut table = Vec::new();
        for (task_id, name) in task_list.iter() {
            table.push(vec![
                CmdValue::Int(*task_id as i64),
                CmdValue::String(name.clone()),
            ]);
        }
        Ok(CmdValue::table(&["id", "task"], table))
    }

    pub fn cmd_kill(&self, env: &Env, args: &Vec<String>) -> Result<CmdValue> {
        if let Some(id_str) = args.get(0) {
            if let Ok(id) = id_str.parse::<u16>() {
                env.spawner().kill(id);
                println!("Killed task with ID {}", id);
                Ok(CmdValue::None)
            } else {
                // println!("Invalid task ID: {}", id_str);
                Err(err!("invalid task ID: {id_str}"))
            }
        } else {
            // println!("Usage: kill <task_id>");
            Err(Error::new("usage: kill <task_id>"))
        }
    }

    pub fn cmd_killall(&self, env: &Env, args: &Vec<String>) -> Result<CmdValue> {
        if let Some(target_name) = args.get(0) {
            env.spawner().task_list().iter().for_each(|(task_id, name)| {
                if name == target_name {
                    println!("Killed task with ID {}, name {}", task_id, name);
                    env.spawner().kill(*task_id);
                }
            });
            Ok(CmdValue::None)
        } else {
            // println!("Usage: killall <task_name>");
            Err(Error::new("usage: killall <task_name>"))
        }
    }

    pub fn cmd_free(&self, _env: &Env, _args: &Vec<String>) -> Result<CmdValue> {
        const MAX_BLOCK_INDEX: usize = 24; // 32;
        let mut total = 0u32;
        let mut block = 1u32 << MAX_BLOCK_INDEX; // 从4KB开始尝试分配
        let mut ptrs: [(*mut u8, usize); MAX_BLOCK_INDEX] =
            [(core::ptr::null_mut(), 0); MAX_BLOCK_INDEX];
        let mut ptrs_count = 0usize;

        while block > 0 && ptrs_count < MAX_BLOCK_INDEX {
            if let Ok(layout) = core::alloc::Layout::from_size_align(block as usize, 4) {
                let ptr = unsafe { alloc::alloc::alloc(layout) };
                if !ptr.is_null() {
                    total += block;
                    ptrs[ptrs_count] = (ptr, block as usize);
                    ptrs_count += 1;
                }
            }
            block /= 2; // 减小块大小
        }

        // 释放所有分配的内存，使用正确的大小
        for i in 0..ptrs_count {
            let (ptr, size) = ptrs[i];
            if !ptr.is_null() {
                if let Ok(layout) = core::alloc::Layout::from_size_align(size, 4) {
                    unsafe { alloc::alloc::dealloc(ptr, layout) };
                }
            }
        }

        // println!("Free memory: {} bytes", total);
        Ok(CmdValue::record(vec![(
            "free memory",
            CmdValue::Int(total as i64),
        )]))
    }

    pub fn cmd_panic(&self, _env: &Env, _args: &Vec<String>) -> Result<CmdValue> {
        panic!("MANUAL PANIC");
        Ok(CmdValue::None)
    }

    pub async fn cmd_pref(&self, _env: &Env, _args: &Vec<String>) -> Result<CmdValue> {
        let c = Rc::new(RefCell::new(0u32));
        let f1 = async {
            loop {
                if let Ok(mut cc) = c.try_borrow_mut() {
                    *cc += 1;
                }
                sys::yield_now().await;
            }
        };
        let f2 = async {
            loop {
                sys::sleep_ms(1000).await;
                if let Ok(mut cc) = c.try_borrow_mut() {
                    println!("poll freq: {} times/sec", *cc);
                    *cc = 0;
                }
            }
        };
        sys::join(f1, f2).await;
        Ok(CmdValue::None)
    }

    pub fn cmd_lsdev(&self, env: &Env, _args: &Vec<String>) -> Result<CmdValue> {
        let mut table = Vec::new();
        for device in env.board().devices().iter() {
            let driver = device.driver();
            let device_type = match device {
                Device::SysTick(_) => "systick",
                Device::Tty(_) => "tty",
                Device::Mtd(_) => "mtd",
                Device::FsDriver(_) => "fs",
                Device::Generic(driver) => "generic",
                Device::Gpio(gpio_driver) => "gpio",
                Device::Uart(uart_driver) => "uart",
                Device::Spi(spi_driver) => "spi",
                Device::Rtc(rtc_driver) => "rtc",
            };
            table.push(vec![
                CmdValue::String(driver.device_name().to_string()),
                CmdValue::String(device_type.to_string()),
                CmdValue::String(driver.driver_name().to_string()),
            ]);
        }
        Ok(CmdValue::table(
            &["device_name", "device_type", "driver_name"],
            table,
        ))
    }

    fn cmd_df(&self, env: &Env, _args: &Vec<String>) -> Result<CmdValue> {
        let fs = env.board().fs();
        let mut table = Vec::new();
        for entry in fs.fstab().iter() {
            let info = entry.fs_driver().info();
            if let Ok(info) = info {
                table.push(vec![
                    CmdValue::String(entry.mount_point().to_string()),
                    CmdValue::Int(info.total_bytes as i64),
                    CmdValue::Int(info.used_bytes as i64),
                    CmdValue::Int((info.total_bytes - info.used_bytes) as i64),
                ]);
            } else {
                table.push(vec![
                    CmdValue::String(entry.mount_point().to_string()),
                    CmdValue::String("unknown".to_string()),
                    CmdValue::String("unknown".to_string()),
                    CmdValue::String("unknown".to_string()),
                ]);
            }
        }
        Ok(CmdValue::table(
            &["mount_point", "total", "used", "free"],
            table,
        ))
    }

    fn cmd_mount(&self, env: &Env, args: &Vec<String>) -> Result<CmdValue> {
        if let Some(mount_point) = args.get(0) {
            let fs = env.board().fs();
            match fs.mount(env.resolve(mount_point)) {
                Ok(_) => Ok(CmdValue::None),
                Err(e) => Err(err!("failed to mount filesystem {mount_point}: {e}")),
            }
        } else {
            // 输出当前挂载的文件系统
            let fs = env.board().fs();
            let mut table = Vec::new();
            for entry in fs.fstab().iter() {
                let mount_point = entry.mount_point();
                let fs_driver = entry.fs_driver();
                let dev_name = if let Some(dev) = env.board().get_dev_by_driver(fs_driver.as_ref())
                {
                    dev.name().to_string()
                } else {
                    "unknown".to_string()
                };
                let mounted = entry.mounted();
                table.push(vec![
                    CmdValue::String(mount_point.to_string()),
                    CmdValue::String(dev_name.to_string()),
                    CmdValue::String(fs_driver.driver_name().to_string()),
                    CmdValue::String(mounted.to_string()),
                ]);
            }
            Ok(CmdValue::table(
                &["mount_point", "dev_name", "fs_driver", "mounted"],
                table,
            ))
        }
    }
    fn cmd_umount(&self, env: &Env, args: &Vec<String>) -> Result<CmdValue> {
        if let Some(mount_point) = args.get(0) {
            let fs = env.board().fs();
            match fs.unmount(env.resolve(mount_point)) {
                Ok(_) => Ok(CmdValue::None),
                Err(e) => Err(err!("failed to unmount filesystem {mount_point}: {e}")),
            }
        } else {
            Err(Error::new("usage: unmount <mount_point>"))
        }
    }
    fn cmd_format(&self, env: &Env, args: &Vec<String>) -> Result<CmdValue> {
        if let Some(mount_point) = args.get(0) {
            let fs = env.board().fs();
            match fs.format(env.resolve(mount_point)) {
                Ok(_) => Ok(CmdValue::None),
                Err(e) => Err(err!("failed to format filesystem {mount_point}: {e}")),
            }
        } else {
            Err(Error::new("usage: format <mount_point>"))
        }
    }
    fn cmd_mkdir(&self, env: &Env, args: &Vec<String>) -> Result<CmdValue> {
        if let Some(path) = args.get(0) {
            let fs = env.board().fs();
            match fs.mkdir(env.resolve(path)) {
                Ok(_) => Ok(CmdValue::None),
                Err(e) => Err(err!("failed to create directory {path}: {e}")),
            }
        } else {
            Err(Error::new("usage: mkdir <path>"))
        }
    }
    fn cmd_rm(&self, env: &Env, args: &Vec<String>) -> Result<CmdValue> {
        if let Some(path) = args.get(0) {
            let fs = env.board().fs();
            match fs.unlink(env.resolve(path)) {
                Ok(_) => Ok(CmdValue::None),
                Err(e) => Err(err!("failed to delete file {path}: {e}")),
            }
        } else {
            Err(Error::new("usage: rm <path>"))
        }
    }
    fn cmd_mv(&self, env: &Env, args: &Vec<String>) -> Result<CmdValue> {
        if let (Some(old_path), Some(new_path)) = (args.get(0), args.get(1)) {
            let fs = env.board().fs();
            match fs.rename(env.resolve(old_path), env.resolve(new_path)) {
                Ok(_) => Ok(CmdValue::None),
                Err(e) => Err(err!("failed to rename {old_path} to {new_path}: {e}")),
            }
        } else {
            Err(Error::new("usage: mv <old_path> <new_path>"))
        }
    }
    fn cmd_sync(&self, env: &Env, _args: &Vec<String>) -> Result<CmdValue> {
        let fs = env.board().fs();
        match fs.sync() {
            Ok(_) => Ok(CmdValue::None),
            Err(e) => Err(err!("failed to sync filesystem: {e}")),
        }
    }
    fn cmd_ls(&self, env: &Env, args: &Vec<String>) -> Result<CmdValue> {
        let fs = env.board().fs();
        let path = if let Some(p) = args.get(0) {
            env.resolve(p)
        } else {
            env.cwd()
        };
        match fs.readdir(&path) {
            Ok(entries) => {
                let mut table = Vec::new();
                for entry in entries {
                    let type_str = if entry.is_file() { "file" } else { "dir" };
                    table.push(vec![
                        CmdValue::String(entry.name.clone()),
                        CmdValue::String(type_str.to_string()),
                        CmdValue::Int(entry.size as i64),
                    ]);
                }
                Ok(CmdValue::table(&["name", "type", "size"], table))
            }
            Err(e) => Err(err!("failed to read directory {path}: {e}")),
        }
    }
    fn cmd_cd(&self, env: &Env, args: &Vec<String>) -> Result<CmdValue> {
        if let Some(path) = args.get(0) {
            match env.set_cwd(path) {
                Ok(_) => Ok(CmdValue::None),
                Err(e) => Err(err!("failed to change directory to {path}: {e}")),
            }
        } else {
            Err(Error::new("usage: cd <path>"))
        }
    }
    fn cmd_pwd(&self, env: &Env, _args: &Vec<String>) -> Result<CmdValue> {
        Ok(CmdValue::String(env.cwd()))
    }
    fn cmd_touch(&self, env: &Env, args: &Vec<String>) -> Result<CmdValue> {
        if let Some(path) = args.get(0) {
            let fs = env.board().fs();
            // 以追加模式打开：文件不存在则创建，已存在则保留内容不变
            let mut file = fs.open(env.resolve(path), "a")?;
            file.close()?;
            Ok(CmdValue::None)
        } else {
            Err(Error::new("usage: touch <path>"))
        }
    }

    fn cmd_cat(&self, env: &Env, args: &Vec<String>) -> Result<CmdValue> {
        if let Some(path) = args.get(0) {
            let content = self.read_file(env, path)?;
            // 以文本方式返回，非法 UTF-8 字节用替换字符呈现
            let text = String::from_utf8_lossy(&content).into_owned();
            Ok(CmdValue::String(text))
        } else {
            Err(Error::new("usage: cat <path>"))
        }
    }

    fn cmd_read(&self, env: &Env, args: &Vec<String>) -> Result<CmdValue> {
        if let Some(path) = args.get(0) {
            let content = self.read_file(env, path)?;
            Ok(CmdValue::Binary(content))
        } else {
            Err(Error::new("usage: read <path>"))
        }
    }

    fn cmd_write(&self, env: &Env, args: &Vec<String>) -> Result<CmdValue> {
        if args.len() < 2 {
            return Err(Error::new("usage: write <path> <content...>"));
        }
        let path = &args[0];
        // 内容可能包含空格，合并 path 之后的所有参数
        let content = args[1..].join(" ");

        let fs = env.board().fs();
        let mut file = fs.open(env.resolve(path), "w")?;
        let bytes = content.as_bytes();
        let mut written = 0usize;
        while written < bytes.len() {
            let n = file.write(&bytes[written..])?;
            if n == 0 {
                break;
            }
            written += n;
        }
        file.flush()?;
        file.close()?;
        Ok(CmdValue::None)
    }

    // 读取整个文件为字节，供 cat / read 复用
    fn read_file(&self, env: &Env, path: &str) -> Result<Vec<u8>> {
        let fs = env.board().fs();
        let mut file = fs.open(env.resolve(path), "r")?;
        let mut content = Vec::new();
        let mut buf = [0u8; 256];
        loop {
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            content.extend_from_slice(&buf[..n]);
        }
        file.close()?;
        Ok(content)
    }

    /// date [-f <rtc_device>]          显示时间（有 TZ 则本地时间，否则 UTC）
    /// date -u [-f <rtc_device>]       显示 UTC 时间
    /// date -s <dt> [-f <rtc_device>]  设置时间，格式与输出相同，例如 2000-01-01 0:29:55.0 +00
    /// <pipeline> | date -S [-f <rtc_device>]  从管道输入设置时间
    fn cmd_date(
        &self,
        env: &Env,
        args: &Vec<String>,
        input: &mut Option<CmdValue>,
    ) -> Result<CmdValue> {
        let mut utc_only = false;
        let mut set_text = None;
        let mut set_from_pipe = false;
        let mut rtc_device = "rtc0";
        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "-u" | "--utc" => utc_only = true,
                "-S" => set_from_pipe = true,
                "-s" | "--set" => {
                    i += 1;
                    set_text = Some(args.get(i).ok_or_else(|| Error::new(DATE_USAGE))?.clone());
                }
                "-f" | "--rtc" => {
                    i += 1;
                    rtc_device = args.get(i).ok_or_else(|| Error::new(DATE_USAGE))?;
                }
                flag if flag.starts_with('-') => return Err(err!("unknown flag: {flag}")),
                other => return Err(err!("unexpected argument: {other}")),
            }
            i += 1;
        }
        if set_text.is_some() && set_from_pipe {
            return Err(Error::new("date: -s and -S cannot be used together"));
        }

        let Some(Device::Rtc(rtc)) = env.board().get_dev_by_name(rtc_device) else {
            return Err(err!("rtc device {rtc_device} not found"));
        };

        let tz = if utc_only { None } else { env_tz_offset(env)? };

        let set_text = if set_from_pipe {
            match input.take() {
                Some(CmdValue::String(s)) => Some(s),
                Some(other) => {
                    return Err(err!("date -S expected a string, got {}", other.type_name()));
                }
                None => return Err(Error::new("usage: <pipeline> | date [-u] -S")),
            }
        } else {
            set_text
        };

        if let Some(text) = set_text {
            let (date, time) = parse_date_time_text(&text)?;
            rtc.rtc_write(&wall_clock_to_utc(date, time, tz)?)?;
        }

        format_datetime(rtc.rtc_read()?, tz)
    }

    fn cmd_env(&self, env: &Env, _args: &Vec<String>) -> Result<CmdValue> {
        let vars = env.get_vars();
        let mut table = Vec::new();
        for (name, value) in vars {
            table.push(vec![CmdValue::String(name), value]);
        }
        Ok(CmdValue::table(&["name", "value"], table))
    }
}

#[async_trait(?Send)]
impl CmdParser for BuiltinCmds {
    fn parser_id(&self) -> &'static str {
        "Console Builtin"
    }

    #[rustfmt::skip]
    fn help(&self) -> &'static [(&'static str, &'static str)] {
        &[
            ("null",            "Do nothing"),
            ("clear",           "Clear the screen"),
            ("reset",           "Perform a system reset"),
            ("sleep <seconds>", "Sleep for a specified number of seconds"),
            ("<pipeline> | length", "Count the items of the piped value"),
            ("<pipeline> | sort-by [column...] [-r]", "Sort the piped value by column"),
            ("ps",              "Show running tasks"),
            ("kill <task_id>",  "Terminate a task"),
            ("killall <task_name>", "Terminate all tasks with the given name"),
            ("free",            "Show free memory"),
            ("pref",            "Show task polling frequency"),
            ("lsdev",           "List available devices"),
            ("panic",           "Trigger a panic"),
            ("df",              "Show filesystem usage"),
            ("mount [mount_point]", "Mount a filesystem"),
            ("umount <mount_point>", "Unmount a filesystem"),
            ("mkdir <path>", "Create a directory"),
            ("rm <path>", "Delete a file"),
            ("ls <path>", "List a directory"),
            ("cd <path>", "Change directory"),
            ("pwd", "Show current directory"),
            ("format <mount_point>", "Format a filesystem"),
            ("mv <old> <new>", "Rename/move a file"),
            ("sync", "Flush filesystem caches to storage"),
            ("touch <path>", "Create an empty file if absent"),
            ("cat <path>", "Print file contents as text"),
            ("read <path>", "Read file contents as binary"),
            ("write <path> <content>", "Write text to a file (overwrite)"),
            ("date [-u] [-s <date_time> | -S] [-f <rtc>]", "Show or set the system date and time"),
            ("<pipeline> | date -S", "Set the system date and time from pipeline"),
            ("env", "Show environment variables"),
        ]
    }

    async fn parse(
        &self,
        env: &Env,
        cmd: &str,
        args: &Vec<String>,
        input: &mut Option<CmdValue>,
    ) -> Option<Result<CmdValue>> {
        match cmd {
            // "reset" => self.cmd_reset(&args),
            "null" => Some(self.cmd_null(env, &args)),
            "clear" => Some(self.cmd_clear(env, &args)),
            "length" => Some(self.cmd_length(env, input.take())),
            "sort-by" => Some(self.cmd_sort_by(env, &args, input.take())),
            "sleep" => Some(self.cmd_sleep(env, &args).await),
            "ps" => Some(self.cmd_ps(env, &args)),
            "kill" => Some(self.cmd_kill(env, &args)),
            "killall" => Some(self.cmd_killall(env, &args)),
            "free" => Some(self.cmd_free(env, &args)),
            "pref" => Some(self.cmd_pref(env, &args).await),
            "lsdev" => Some(self.cmd_lsdev(env, &args)),
            "panic" => Some(self.cmd_panic(env, &args)),
            "df" => Some(self.cmd_df(env, &args)),
            "mount" => Some(self.cmd_mount(env, &args)),
            "umount" => Some(self.cmd_umount(env, &args)),
            "mkdir" => Some(self.cmd_mkdir(env, &args)),
            "rm" => Some(self.cmd_rm(env, &args)),
            "ls" => Some(self.cmd_ls(env, &args)),
            "cd" => Some(self.cmd_cd(env, &args)),
            "pwd" => Some(self.cmd_pwd(env, &args)),
            "format" => Some(self.cmd_format(env, &args)),
            "mv" => Some(self.cmd_mv(env, &args)),
            "sync" => Some(self.cmd_sync(env, &args)),
            "touch" => Some(self.cmd_touch(env, &args)),
            "cat" => Some(self.cmd_cat(env, &args)),
            "read" => Some(self.cmd_read(env, &args)),
            "write" => Some(self.cmd_write(env, &args)),
            "date" => Some(self.cmd_date(env, &args, input)),
            "env" => Some(self.cmd_env(env, &args)),
            _ => return None, // Command not found
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_utc_display_format() {
        let (date, time) = parse_date_time_text("2000-01-01 0:29:55.0 +00").unwrap();
        assert_eq!(date.year(), 2000);
        assert_eq!(date.month(), Month::January);
        assert_eq!(date.day(), 1);
        assert_eq!(time.hour(), 0);
        assert_eq!(time.minute(), 29);
        assert_eq!(time.second(), 55);
    }

    #[test]
    fn parse_offset_display_format() {
        let (date, time) = parse_date_time_text("2000-01-01 8:29:55.0 +08:00:00").unwrap();
        assert_eq!(date.day(), 1);
        assert_eq!(time.hour(), 8);
        assert_eq!(time.minute(), 29);
    }

    #[test]
    fn set_without_tz_keeps_utc() {
        let (date, time) = parse_date_time_text("2000-01-01 8:00:00.0 +00").unwrap();
        let utc = wall_clock_to_utc(date, time, None).unwrap();
        assert_eq!(utc.hour(), 8);
    }

    #[test]
    fn set_with_tz_converts_local_to_utc() {
        let (date, time) = parse_date_time_text("2000-01-01 8:00:00.0 +08:00:00").unwrap();
        let tz = UtcOffset::from_whole_seconds(8 * 3600).unwrap();
        let utc = wall_clock_to_utc(date, time, Some(tz)).unwrap();
        assert_eq!(utc.year(), 2000);
        assert_eq!(utc.day(), 1);
        assert_eq!(utc.hour(), 0);
    }
}
