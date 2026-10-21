use crate::console::Env;
use crate::CmdValue;
use async_trait::async_trait;
use elecrust::alloc::boxed::Box;
use elecrust::alloc::string::String;
use elecrust::alloc::vec::Vec;
use elecrust::error::Result;

#[async_trait(?Send)]
pub trait CmdParser {
    fn parser_id(&self) -> &'static str;
    fn help(&self) -> &'static [(&'static str, &'static str)];
    /// 解析并执行一条命令。
    /// `cmd` 是命令名；`args` 只含参数（`args[0]` 为第一个参数，不含命令名本身）。
    /// `input` 是管道上游传入的数据：源命令可忽略它，过滤类命令用 `input.take()` 取走。
    /// 返回 `None` 表示本解析器不认识该命令（交给下一个解析器）。
    async fn parse(
        &self,
        env: &Env,
        cmd: &str,
        args: &Vec<String>,
        input: &mut Option<CmdValue>,
    ) -> Option<Result<CmdValue>>;
}
