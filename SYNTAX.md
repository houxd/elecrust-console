# Elecrust Console 语法说明

面向单片机的交互式控制台，语法是 nushell 的一个子集（结构化管道、代码块、`for`/`while`/`if`），
字符串变量替换则采用 u-boot / bash 风格的 `${name}`。

在 PC 上可以直接用模拟器试：`cargo run -p elecrust-emulate`。

```nu
let dir = "/spi"
for i in 1..3 { write "${dir}/f${i}.txt" "content ${i}" }
ls $dir | where size > 0 | sort-by size -r | each { |f| print "${f.name} (${f.size} bytes)" }
```

---

## 1. 行、语句与注释

一行可以写多条语句，用 `;` 或换行分隔。

```nu
let a = 1; let b = 2; print "${a}${b}"
```

`#` 开头到行尾是注释（只有出现在词首才算注释，`a#b` 是一个普通的词）。

```nu
echo hi   # 这里是注释
```

**续行**。以下三种情况控制台不会执行，而是把提示符换成 `... ` 继续读下一行：

| 情况 | 例子 |
| --- | --- |
| 花括号 / 圆括号 / 方括号没配平 | `for i in 1..3 {` |
| 引号没闭合 | `let s = "abc` |
| 行尾是竖线 | `ls \|` |

也可以用行尾反斜杠强制续行。多行输入累计上限 4096 字符，单行上限 512 字符。
按 `Ctrl+C` 放弃当前没写完的输入。

```nu
for i in 1..3 {
    print "item ${i}"
}
```

---

## 2. 值与类型

| 类型 | 写法 / 来源 | 说明 |
| --- | --- | --- |
| `nothing` | 命令无返回 | 不会打印任何东西 |
| `int` | `42`、`-7` | i64 |
| `float` | `3.14` | f64 |
| `bool` | `true`、`false` | |
| `string` | `"abc"`、`'abc'` | |
| `binary` | `read <path>` | 打印为字节数组 |
| `list` | `[1 2 3]` | |
| `record` | 遍历表格的一行 | 按 `字段: 值` 逐行打印 |
| `table` | `ls`、`ps` 等命令 | 自动对齐成表格打印 |
| `range` | `1..10` | 惰性，不会在堆上展开 |

表达式语句的结果会自动打印，`let` 语句不打印。

---

## 3. 命令与参数

一段管道如果以「不像值」的裸词开头，就按命令调用解析；命令的参数默认是**字面字符串**，
所以 `ls -l`、`cd ..`、`cat a.txt`、`sleep 0.5` 都按你写的原样传进去。

参数在这几种情况下会被求值：

| 写法 | 结果 |
| --- | --- |
| `$name` | 变量的值，保留原本的类型 |
| `${name}` | 变量的值转成字符串后拼进这个词 |
| `(...)` | 子管道的结果 |
| `[...]` | 列表 |
| `{ ... }` | 代码块（只有 `each` 这类命令接受） |

```nu
echo 1 + 2          # 三个字面参数，输出 "1 + 2"
echo (1 + 2)        # 3
write /tmp/a.txt $x # 把变量 x 的值写进去
```

> 命令最终收到的参数一律是字符串（`CmdParser` 接口如此），结构化的值只在管道、
> 表达式和变量之间流动。

---

## 4. 字符串与变量替换

| 写法 | 转义 | `${}` 替换 |
| --- | --- | --- |
| `"..."` | 支持 `\n \t \r \\ \" \0`，其它 `\x` 得到 `x` | 是 |
| `'...'` | 不处理，原样保留 | 否 |
| 裸词 | `\` 转义下一个字符 | 是 |

```nu
let i = 2
print "/spi/f${i}.txt"     # /spi/f2.txt
print /spi/f${i}.txt       # 一样，裸词也会替换
print 'literal ${i}'       # literal ${i}  单引号不替换
print "escaped \${i}"      # escaped ${i}
print "plain $i stays"     # plain $i stays  不带花括号就是普通文本
```

花括号里可以写字段路径：`"${p.0.task}"`。

`${name}` 永远产出字符串，`$name` 保留原本的类型，这个区别是有意的：

```nu
let a = 42
$a + 1         # 43
"${a}" + 1     # "421"
```

字符串里不支持表达式插值，复杂情况用 `+` 拼接：

```nu
print ("total " + (ps | length) + " tasks")
```

---

## 5. 变量

```nu
let x = 5              # 定义（mut 是等价的别名）
$x = $x + 1            # 赋值
$x                     # 取值
```

变量是会话级的，重复 `let` 会覆盖。循环变量（`for` 的 `$i`、`each` 的 `$it`）只在循环期间有效，
循环结束后会恢复成原来的值。

**取字段**用 `.`，可以连写：

| 目标 | 写法 | 结果 |
| --- | --- | --- |
| record 的字段 | `$f.name` | 字段值 |
| list 的下标 | `$list.0` | 第 0 个元素 |
| table 的行 | `$t.0` | 该行的 record |
| table 的列 | `$t.name` | 该列的 list |
| binary 的字节 | `$b.3` | int |

后缀取字段对任意表达式都有效：

```nu
(ps).0.task        # console
[10 20 30].1       # 20
```

---

## 6. 表达式与运算符

运算符**两侧必须有空格**（否则会被当成词的一部分，这正是 `cd ..`、`ls -l` 不受影响的原因）。

优先级从低到高：

| 优先级 | 运算符 |
| --- | --- |
| 1 | `or` |
| 2 | `and` |
| 3 | `==` `!=` `<` `<=` `>` `>=` |
| 4 | `+` `-` |
| 5 | `*` `/` `mod`（`%` 同义） |
| 6 | `not`（前缀） |
| 7 | 字面量、`$var`、`(...)`、`[...]`、后缀 `.field` |

语义要点：

- `+` 对数字求和，对字符串拼接，对列表连接；一侧是字符串时另一侧自动转字符串。
- `/` 整数能整除时结果仍是整数，否则退化为浮点；除零报错。
- 比较可以跨 `int`/`float`；字符串按字典序；类型不可比时报错。
- `==` / `!=` 更宽松，类型不同直接判为不等而不报错。
- `and` / `or` 短路求值，结果是 `bool`。

**真假判定**（`if`、`while`、`and`、`or` 用）：`false`、`0`、`0.0`、空字符串、空列表、
空表格、空区间、`nothing` 为假，其余为真。

---

## 7. 区间与列表

```nu
1..5        # 1 2 3 4 5，含上界
1..<5       # 1 2 3 4，不含上界
10..7       # 10 9 8 7，起点大于终点时倒序
1..$n       # 两端可以是整数或变量
[1 2 3]     # 列表，元素之间空格分隔
[1, 2, 3]   # 逗号可写可不写
[a b "c d"] # 裸词元素是字符串
```

区间是惰性的，`for i in 1..100000 { ... }` 不会占用额外内存。

---

## 8. 管道

`|` 把上一段的结果交给下一段。

```nu
ls /spi | length
ps | each { |p| print $p.task }
ls /spi | print
```

只有需要输入的命令才会取走它（如 `length`、`each`、`where`、`sort-by`、不带参数的 `print`）。

---

## 9. 过滤与排序

### where

`where` 保留条件成立的项。条件里的**裸标识符表示当前项的字段**，所以 `size > 0`
的意思是「这一行的 size 大于 0」：

```nu
ls | where size > 0
ls | where type == "file"
ls | where type == "file" and size > 100
ps | where task == "console"
```

因为裸词已经被用来表示字段，条件里的字符串必须加引号：`where type == file` 会被理解成
「和当前项的 `file` 字段比较」，字段不存在就报错。字段写错列名同样会报错，不会静默过滤掉全部数据。

输入不是表格时（或者想写得更清楚时）用 `$it` 指代当前项：

```nu
[3 1 2] | where $it > 1
ls | where $it.size > 0
```

`(...)` 里面是一条普通管道（裸词又变回命令名），字段简写在括号内不生效，
要在括号里取字段就写 `$it`：

```nu
ls | where not size              # 可以，not 直接作用在字段上
ls | where not ($it.size > 0)    # 括号里要写 $it，不能写 not (size > 0)
```

条件比较复杂就换成代码块，参数名自己定，块的值为真即保留：

```nu
ls | where { |f| $f.size > 0 and $f.name != "log.txt" }
```

表格进、表格出（保留列对齐），其它输入产出列表。

### sort-by

```nu
ls | sort-by size          # 按 size 升序
ls | sort-by size -r       # 降序，也可以写 --reverse
ls | sort-by type name     # 先按 type，相同再按 name
ps | sort-by               # 表格不给列名时按第一列排
[3 1 2] | sort-by          # 列表不给列名时按元素本身排
```

排序是稳定的，相等的项保持原有顺序。两个值类型不同、没法比大小时按文本比较，
所以混合数据也不会报错。表格里指定的列名不存在会报错。
区间本身就是有序的，`sort-by` 只在 `-r` 时把两端换过来，不会展开成列表。

---

## 10. 代码块

```nu
{ print "hi" }            # 无参数
{ |it| print $it }        # 声明一个参数
```

代码块目前用于 `for` / `while` / `if` 的循环体，以及 `each` / `where` 的参数。
块的值是最后一条语句的值。

---

## 11. 控制流

```nu
for i in 1..3 { print $i }
for f in (ls /spi) { print $f.name }
for x in [a b c] { print $x }

while $n > 0 { $n = $n - 1 }

if $n == 0 {
    print "zero"
} else if $n > 0 {
    print "positive"
} else {
    print "negative"
}

for i in 1..10 {
    if $i mod 2 == 0 { continue }
    if $i > 7 { break }
    print $i
}
```

`for` 能遍历的东西：

| 输入 | 每次拿到 |
| --- | --- |
| range | int |
| list | 元素 |
| table | 一行（record） |
| record | `{key, value}` 形式的 record |
| 标量（int/string/...） | 它自己，只循环一次 |
| nothing | 不循环 |

循环每轮都会让出 CPU，所以 `while true { }` 这类死循环也能用 `Ctrl+C` 终止。

---

## 12. 内置命令

语言级内置（由控制台直接处理）：

| 命令 | 说明 |
| --- | --- |
| `help`、`?` | 显示语法和全部命令 |
| `echo <args...>` | 把参数作为值返回（不打印）；无参数时透传管道输入；多个参数返回列表 |
| `print [-n] <args...>` | 打印参数，`-n` 表示不换行；无参数时打印管道输入 |
| `each { \|it\| ... }` | 对输入的每一项执行代码块，收集非空结果为列表；参数名默认 `it` |
| `where <cond>` | 保留条件成立的项，见第 9 节；也接受 `where { \|it\| ... }` |

`echo` 和 `print` 的区别：`echo` 产出值（由语句层决定是否打印，可以存进变量或接着管道），
`print` 直接输出、返回 `nothing`。

内置命令集（`BuiltinCmds`）：

| 命令 | 说明 |
| --- | --- |
| `clear` | 清屏 |
| `sleep <seconds>` | 休眠，支持小数 |
| `<pipeline> \| length` | 元素个数（字符串是字符数） |
| `<pipeline> \| sort-by [column...] [-r]` | 按列排序，见第 9 节 |
| `ps` | 任务列表 |
| `kill <task_id>` | 结束任务 |
| `free` | 可用内存 |
| `pref` | 任务轮询频率 |
| `lsdev` | 设备列表 |
| `panic` | 主动 panic（调试用） |
| `date` | 读取 RTC 时间 |
| `df` | 文件系统用量 |
| `mount [mount_point]` | 挂载；不带参数时列出挂载表 |
| `umount <mount_point>` | 卸载 |
| `format <mount_point>` | 格式化（会自动先卸载，原本挂着的话再挂回去） |
| `sync` | 刷写缓存 |
| `ls [path]` | 列目录 |
| `cd <path>` | 切换目录 |
| `pwd` | 当前目录 |
| `mkdir <path>` | 建目录 |
| `rm <path>` | 删文件 |
| `mv <old> <new>` | 改名 / 移动 |
| `touch <path>` | 不存在则创建空文件 |
| `cat <path>` | 按文本读取 |
| `read <path>` | 按二进制读取 |
| `write <path> <content...>` | 覆盖写入文本 |

路径支持相对写法，会按当前工作目录展开。

---

## 13. 任务与 Ctrl+C

每条顶层语句作为一个前台任务运行（整个 `for` 循环算一条语句），执行期间按 `Ctrl+C`
会终止它。`ps` 可以看到正在跑的任务，`kill <id>` 结束后台任务。

在提示符空闲时按 `Ctrl+C` 会清空当前输入（包括没写完的多行输入）。
上下方向键翻历史（保留最近 10 条），左右方向键移动光标。

---

## 14. 容易踩的坑

- `( ) [ ] { } | ;` 是分隔符，要作为普通字符出现就加引号：`write a.txt "{json}"`。
- 运算符两侧要空格，`1+2` 是一个词而不是加法。
- 命令参数是字面字符串，`echo 1 + 2` 输出 `1 + 2`，要算数写 `echo (1 + 2)`。
- 字符串里替换变量必须用 `${}`，裸的 `$name` 在双引号里不展开。
- `${}` 的结果永远是字符串，需要保留类型就用 `$name`。
- 字符串里不能写表达式，用 `+` 拼接代替。
- `where` 的条件里裸词是字段名，字符串要加引号：`where type == "file"`。
- `10..1` 是倒序区间，`1..<1` 是空区间。
- 行编辑器只接受 ASCII 可见字符，非 ASCII（比如中文）敲不进去，会被直接忽略；
  命令输出没有这个限制。

---

## 15. 扩展命令

自定义命令实现 `CmdParser` 后用 `env.add_parser(...)` 注册即可，`help` 会自动列出：

```rust
#[async_trait(?Send)]
impl CmdParser for MyCmds {
    fn parser_id(&self) -> &'static str { "mine" }
    fn help(&self) -> &'static [(&'static str, &'static str)] {
        &[("blink <times>", "Blink the LED")]
    }
    async fn parse(
        &self,
        env: &Env,
        cmd: &str,
        args: &Vec<String>,
        input: &mut Option<CmdValue>,
    ) -> Option<Result<CmdValue>> {
        match cmd {
            "blink" => Some(self.cmd_blink(env, args)),
            _ => None,
        }
    }
}
```

返回 `None` 表示不认识这个命令，交给下一个解析器；返回 `CmdValue::table(...)`
就能直接得到对齐的表格输出，并且可以被 `each`、`length`、`$it.field` 消费。

---

## 16. 命令别名

把常用的长命令固化成短名字，用 `env.add_aliases(...)` 注册一张静态表：

```rust
const ALIASES: &[(&str, &str)] = &[
    ("ll", "ls"),
    ("dirs", "ls | where type == \"dir\""),
    ("biggest", "ls | where size > 0 | sort-by size -r"),
    ("watch", "while true { print ''; ls; sleep 1 }"),
];

env.add_aliases(ALIASES);
```

表是 `&'static` 的，直接编进二进制待在 flash 里，控制台只保存一个指针，
不像交互式的 `alias` 命令那样常驻 RAM。可以多次调用注册多张表，后注册的优先，
`help` 里会单独列出 `Aliases` 一节。

用法上和普通命令一样，命令行上多写的参数会接到展开后的**最后一段**命令上：

```nu
ll /spi            # 等价于 ls /spi
biggest | length   # 别名可以出现在管道的任意一段
for f in (dirs) { print $f.name }
```

几条约定：

- 别名的内容在每次调用时才解析，所以 flash 里存的就是一行文本，不占 RAM。
- 别名里可以再用别名；展开期间同名的别名会被屏蔽，所以 `("ls", "ls /spi")`
  这种自引用是安全的（里面的 `ls` 指真正的命令），互相引用的别名也只会报「命令未找到」而不会死循环。
- 别名可以是一条管道，也可以是 `while` / `for` / `if` / `let` 或分号分隔的多条语句。
  只有单条管道才会把命令行上多写的参数接到最后一段命令上；脚本类别名不能再带参数。
- `each` 本身是管道命令，写在别名里和写在命令行里一样，例如 `ls | each { |it| print $it.name }`。
- 别名不能覆盖 `each`、`where`、`echo`、`print`、`help` 这些语言内置命令。
