use std::{
    env,
    error::Error,
    ffi::{OsStr, OsString},
    fmt,
    io::{self, Write},
    process::Command as ProcessCommand,
    time::Duration,
};

use serde_json::Value;
use tokio::time::{Instant, sleep};

use crate::{
    activation::{ActivationError, ActivationService, NameAvailability},
    config::CertServerConfig,
};

const DEACTIVATION_WAIT_TIMEOUT: Duration = Duration::from_secs(5 * 60 + 15);
const DEACTIVATION_POLL_INTERVAL: Duration = Duration::from_secs(1);

// deb 包把特权配置程序固定安装在该路径，公共 CLI 不接受可替换的 helper。
//
// The deb installs the privileged setup program at this fixed path; the public
// CLI does not accept a replaceable helper.
const SETUP_HELPER: &str = "/usr/libexec/kundy/setup";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Serve,
    Activate,
    Deactivate,
    Setup { runtime_user: OsString },
    ApplyRuntimeConfig,
    ReloadPishoo,
    SetupHelp,
    Help,
    Version,
}

#[derive(Debug)]
pub struct CliError {
    message: String,
    source: Option<Box<dyn Error + Send + Sync>>,
}

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for CliError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source
            .as_deref()
            .map(|source| source as &(dyn Error + 'static))
    }
}

pub fn parse() -> Result<Command, CliError> {
    parse_from(env::args_os().skip(1))
}

fn parse_from(args: impl IntoIterator<Item = OsString>) -> Result<Command, CliError> {
    let args = args.into_iter().collect::<Vec<_>>();
    match args.as_slice() {
        [] => Ok(Command::Serve),
        [command] if command == OsStr::new("serve") => Ok(Command::Serve),
        [command] if command == OsStr::new("activate") => Ok(Command::Activate),
        [command] if command == OsStr::new("deactivate") => Ok(Command::Deactivate),
        [command, option, runtime_user]
            if command == OsStr::new("setup") && option == OsStr::new("--user") =>
        {
            Ok(Command::Setup {
                runtime_user: runtime_user.clone(),
            })
        }
        [host, command]
            if host == OsStr::new("host") && command == OsStr::new("runtime-config-apply") =>
        {
            Ok(Command::ApplyRuntimeConfig)
        }
        [host, command] if host == OsStr::new("host") && command == OsStr::new("pishoo-reload") => {
            Ok(Command::ReloadPishoo)
        }
        [flag]
            if flag == OsStr::new("-h")
                || flag == OsStr::new("--help")
                || flag == OsStr::new("help") =>
        {
            Ok(Command::Help)
        }
        [flag] if flag == OsStr::new("-V") || flag == OsStr::new("--version") => {
            Ok(Command::Version)
        }
        [command, flag]
            if command == OsStr::new("activate")
                && (flag == OsStr::new("-h") || flag == OsStr::new("--help")) =>
        {
            Ok(Command::Help)
        }
        [command, flag]
            if command == OsStr::new("deactivate")
                && (flag == OsStr::new("-h") || flag == OsStr::new("--help")) =>
        {
            Ok(Command::Help)
        }
        [command, flag]
            if command == OsStr::new("setup")
                && (flag == OsStr::new("-h") || flag == OsStr::new("--help")) =>
        {
            Ok(Command::SetupHelp)
        }
        _ => Err(CliError::message(
            "无法识别命令。请运行 `kundy --help` 查看用法。",
        )),
    }
}

pub fn print_help() {
    println!(
        "Kundy 一体机初始化服务\n\n用法:\n  kundy                         启动本地配置服务\n  kundy serve                   启动本地配置服务\n  kundy setup --user <用户>     配置宿主机服务（需要 root）\n  kundy activate                通过交互式终端激活本机\n  kundy deactivate              取消激活并清理本机身份\n  kundy --help                  显示帮助\n  kundy --version               显示版本"
    );
}

pub fn print_setup_help() {
    println!(
        "配置 Kundy 宿主机服务\n\n用法:\n  sudo kundy setup --user <用户>\n\n所选用户必须是已经存在的非 root 用户。"
    );
}

pub fn setup(runtime_user: &OsStr) -> Result<(), CliError> {
    let status = ProcessCommand::new(SETUP_HELPER)
        .arg("--user")
        .arg(runtime_user)
        .status()
        .map_err(|source| CliError::io("无法启动 Kundy 宿主机配置程序", source))?;
    if !status.success() {
        return Err(CliError::message(format!(
            "Kundy 宿主机配置失败（{status}）"
        )));
    }
    Ok(())
}

pub async fn deactivate(certserver: CertServerConfig) -> Result<(), CliError> {
    let service = ActivationService::new(&certserver).map_err(CliError::activation)?;
    let status = service.status().await.map_err(CliError::activation)?;
    if status.state == "factory_ready" {
        println!("本机当前没有已激活身份，无需取消激活。");
        return Ok(());
    }

    let display_name = status.display_name.as_deref().unwrap_or("本机身份");
    println!("即将取消激活：{display_name}");
    println!("本机证书、DHTTP 身份和宿主机运行配置将被移除。CertServer 中的绑定记录不会删除。\n");
    if !confirm("确认取消激活？输入 y 继续 [y/N]：")? {
        println!("已取消，未修改本机激活状态。");
        return Ok(());
    }
    if prompt("请输入 DEACTIVATE 以确认取消激活：")? != "DEACTIVATE" {
        println!("确认文本不匹配，已取消操作。");
        return Ok(());
    }

    println!("正在取消激活并清理本机身份...");
    let result = service.deactivate().await.map_err(CliError::activation)?;
    if result.state == "factory_ready" {
        println!("取消激活完成。");
        return Ok(());
    }

    println!("宿主机运行态仍在同步，请稍候...");
    let deadline = Instant::now() + DEACTIVATION_WAIT_TIMEOUT;
    let mut runtime_state = result.runtime_state;
    while Instant::now() < deadline {
        sleep(DEACTIVATION_POLL_INTERVAL).await;
        let result = service.deactivate().await.map_err(CliError::activation)?;
        runtime_state = result.runtime_state;
        if result.state == "factory_ready" {
            println!("取消激活完成。");
            return Ok(());
        }
        if result.runtime_state == "error" {
            break;
        }
    }

    println!(
        "宿主机运行态仍在同步（{runtime_state}），停用意图已经保存；稍后再次运行 kundy deactivate 将继续同一次操作。"
    );
    Ok(())
}

pub fn print_version() {
    println!("kundy {}", env!("CARGO_PKG_VERSION"));
}

pub async fn activate(certserver: CertServerConfig) -> Result<(), CliError> {
    let service = ActivationService::new(&certserver).map_err(CliError::activation)?;
    let status = service.status().await.map_err(CliError::activation)?;
    if status.state == "active" {
        let name = status.display_name.as_deref().unwrap_or("已配置名字");
        println!("本机已经激活：{name}");
        print_runtime_status(&service).await;
        return Ok(());
    }

    println!("Kundy 交互式激活");
    println!("激活成功后，永久名字不可更换。\n");

    let requested_subname = prompt_nonempty("子名字：")?;
    let activation_code = prompt_nonempty("激活码：")?;
    println!("正在验证激活信息...");
    let inspection = service
        .inspect_activation_code(&activation_code)
        .await
        .map_err(CliError::activation)?;
    println!("所属域：{}", inspection.parent_domain);

    let availability = if let Some(bound_subname) = inspection.bound_subname.as_deref() {
        if !bound_subname.eq_ignore_ascii_case(&requested_subname) {
            return Err(CliError::message(format!(
                "该激活码已经绑定子名字 {bound_subname}，不能用于 {requested_subname}。"
            )));
        }
        println!("该激活码已经绑定该子名字，将恢复原有身份。");
        service
            .check_name(&activation_code, bound_subname)
            .await
            .map_err(CliError::activation)?
    } else {
        prompt_available_name(&service, &activation_code, requested_subname).await?
    };

    println!("\n永久名字：{}", availability.display_name);
    println!("远程身份：{}", availability.domain_name);
    if !confirm("确认激活？输入 y 继续 [y/N]：")? {
        println!("已取消，未修改本机激活状态。");
        return Ok(());
    }

    println!("正在申请证书并配置本机身份...");
    let result = service
        .claim(&activation_code, &availability.subname)
        .await
        .map_err(CliError::activation)?;
    println!("激活完成：{}", result.display_name);
    print_runtime_status(&service).await;
    Ok(())
}

async fn prompt_available_name(
    service: &ActivationService,
    activation_code: &str,
    mut subname: String,
) -> Result<NameAvailability, CliError> {
    loop {
        println!("正在检测子名字...");
        let availability = service
            .check_name(activation_code, &subname)
            .await
            .map_err(CliError::activation)?;
        if availability.available {
            return Ok(availability);
        }
        let reason = match availability.unavailable_reason.as_deref() {
            Some("domain_blocked") => "该子名字不可申请。",
            Some("activation_code_name_locked") => "该激活码已经绑定其他子名字。",
            _ => "该子名字已被占用，请重新输入。",
        };
        println!("{reason}");
        subname = prompt_nonempty("子名字：")?;
    }
}

async fn print_runtime_status(service: &ActivationService) {
    match service.runtime_status().await {
        Ok(status) => match status.state {
            "ready" => println!("运行态：已就绪"),
            "error" => println!(
                "运行态：配置失败{}",
                status
                    .code
                    .as_deref()
                    .map(|code| format!("（{code}）"))
                    .unwrap_or_default()
            ),
            _ => println!("运行态：等待宿主机集成完成"),
        },
        Err(error) => eprintln!("无法读取运行态：{}", activation_error_message(&error)),
    }
}

fn prompt_nonempty(label: &str) -> Result<String, CliError> {
    loop {
        let value = prompt(label)?;
        if !value.is_empty() {
            return Ok(value);
        }
        println!("输入不能为空。");
    }
}

fn confirm(label: &str) -> Result<bool, CliError> {
    let answer = prompt(label)?;
    Ok(answer.eq_ignore_ascii_case("y") || answer.eq_ignore_ascii_case("yes"))
}

fn prompt(label: &str) -> Result<String, CliError> {
    print!("{label}");
    io::stdout()
        .flush()
        .map_err(|source| CliError::io("无法输出交互式提示", source))?;
    let mut input = String::new();
    let count = io::stdin()
        .read_line(&mut input)
        .map_err(|source| CliError::io("无法读取终端输入", source))?;
    if count == 0 {
        return Err(CliError::message("输入已结束，激活已取消。"));
    }
    Ok(input.trim().to_string())
}

fn activation_error_message(error: &ActivationError) -> String {
    match error {
        ActivationError::Remote { status, error } => {
            let detail = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("远程激活服务拒绝了请求");
            format!("{detail}（HTTP {status}）")
        }
        ActivationError::CertServerUnavailable => "无法连接远程激活服务。".to_string(),
        ActivationError::NameUnavailable => "该子名字当前不可用。".to_string(),
        ActivationError::PendingNameConflict { domain_name } => {
            format!("本机已经开始认领 {domain_name}，只能继续该名字。")
        }
        ActivationError::PendingCodeConflict => {
            "本机已有未完成的激活，请使用最初提交的激活码。".to_string()
        }
        ActivationError::NotActive => "本机尚未完成激活。".to_string(),
        ActivationError::RecoveryCredentialUnavailable => "本机没有保存恢复凭据。".to_string(),
        ActivationError::DeactivationInProgress => "本机正在取消激活。".to_string(),
        ActivationError::DeactivationCredentialInvalid => {
            "激活码不匹配，未执行取消激活。".to_string()
        }
        ActivationError::Internal { .. } => format!("本机激活失败：{error}"),
    }
}

impl CliError {
    fn message(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            source: None,
        }
    }

    fn io(message: impl Into<String>, source: io::Error) -> Self {
        Self {
            message: message.into(),
            source: Some(Box::new(source)),
        }
    }

    fn activation(source: ActivationError) -> Self {
        Self {
            message: activation_error_message(&source),
            source: Some(Box::new(source)),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::{Command, parse_from};

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn no_arguments_still_starts_the_daemon() {
        assert_eq!(parse_from(args(&[])).unwrap(), Command::Serve);
    }

    #[test]
    fn activate_is_an_argument_free_interactive_command() {
        assert_eq!(parse_from(args(&["activate"])).unwrap(), Command::Activate);
        assert!(parse_from(args(&["activate", "name"])).is_err());
    }

    #[test]
    fn deactivate_is_an_argument_free_interactive_command() {
        assert_eq!(
            parse_from(args(&["deactivate"])).unwrap(),
            Command::Deactivate
        );
        assert!(parse_from(args(&["deactivate", "name"])).is_err());
    }

    #[test]
    fn setup_requires_an_explicit_runtime_user() {
        assert_eq!(
            parse_from(args(&["setup", "--user", "appliance"])).unwrap(),
            Command::Setup {
                runtime_user: OsString::from("appliance")
            }
        );
        assert!(parse_from(args(&["setup"])).is_err());
        assert!(parse_from(args(&["setup", "--user"])).is_err());
        assert!(parse_from(args(&["setup", "appliance"])).is_err());
    }

    #[test]
    fn setup_has_command_specific_help() {
        assert_eq!(
            parse_from(args(&["setup", "--help"])).unwrap(),
            Command::SetupHelp
        );
    }

    #[test]
    fn host_helpers_have_fixed_argument_free_commands() {
        assert_eq!(
            parse_from(args(&["host", "runtime-config-apply"])).unwrap(),
            Command::ApplyRuntimeConfig
        );
        assert_eq!(
            parse_from(args(&["host", "pishoo-reload"])).unwrap(),
            Command::ReloadPishoo
        );
        assert!(parse_from(args(&["host", "runtime-config-apply", "other"])).is_err());
    }
}
