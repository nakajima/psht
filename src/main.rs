mod commands;
mod container;
mod detect;
mod git;

use std::env;
use std::process;

#[derive(Debug, PartialEq)]
enum Command {
    GitReceivePack(String),
    GitUploadPack(String),
    Deploy(String),
    Ps,
    Logs(String),
    Stop(String),
}

fn parse_app_name(raw: &str) -> String {
    let name = raw.trim_matches('\'').trim_matches('"');
    let name = name.strip_suffix(".git").unwrap_or(name);
    name.to_string()
}

fn parse_ssh_command(cmd: &str) -> Result<Command, String> {
    let parts: Vec<&str> = cmd.splitn(2, ' ').collect();
    match parts.as_slice() {
        ["git-receive-pack", arg] => Ok(Command::GitReceivePack(parse_app_name(arg))),
        ["git-upload-pack", arg] => Ok(Command::GitUploadPack(parse_app_name(arg))),
        ["deploy", app] => Ok(Command::Deploy(app.to_string())),
        ["ps"] | ["ps", ..] => Ok(Command::Ps),
        ["logs", app] => Ok(Command::Logs(app.to_string())),
        ["stop", app] => Ok(Command::Stop(app.to_string())),
        _ => Err(format!("unknown command: {cmd}")),
    }
}

fn parse_args(args: &[String]) -> Result<Command, String> {
    match args.iter().map(|s| s.as_str()).collect::<Vec<_>>().as_slice() {
        ["deploy", app] => Ok(Command::Deploy(app.to_string())),
        ["ps"] => Ok(Command::Ps),
        ["logs", app] => Ok(Command::Logs(app.to_string())),
        ["stop", app] => Ok(Command::Stop(app.to_string())),
        _ => Err(format!("unknown arguments: {}", args.join(" "))),
    }
}

fn run() -> Result<(), String> {
    let args: Vec<String> = env::args().skip(1).collect();

    let command = if !args.is_empty() {
        parse_args(&args)?
    } else if let Ok(ssh_cmd) = env::var("SSH_ORIGINAL_COMMAND") {
        parse_ssh_command(&ssh_cmd)?
    } else {
        return Err("no command provided. usage: ssh psht@host <command>".to_string());
    };

    match command {
        Command::GitReceivePack(app) => git::handle_receive_pack(&app),
        Command::GitUploadPack(app) => git::handle_upload_pack(&app),
        Command::Deploy(app) => commands::deploy(&app),
        Command::Ps => commands::ps(),
        Command::Logs(app) => commands::logs(&app),
        Command::Stop(app) => commands::stop(&app),
    }
}

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e}");
        process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_git_receive_pack() {
        let cmd = parse_ssh_command("git-receive-pack 'myapp'").unwrap();
        assert_eq!(cmd, Command::GitReceivePack("myapp".to_string()));
    }

    #[test]
    fn parse_git_receive_pack_with_git_suffix() {
        let cmd = parse_ssh_command("git-receive-pack 'myapp.git'").unwrap();
        assert_eq!(cmd, Command::GitReceivePack("myapp".to_string()));
    }

    #[test]
    fn parse_git_upload_pack() {
        let cmd = parse_ssh_command("git-upload-pack 'myapp'").unwrap();
        assert_eq!(cmd, Command::GitUploadPack("myapp".to_string()));
    }

    #[test]
    fn parse_ssh_deploy() {
        let cmd = parse_ssh_command("deploy myapp").unwrap();
        assert_eq!(cmd, Command::Deploy("myapp".to_string()));
    }

    #[test]
    fn parse_ssh_ps() {
        let cmd = parse_ssh_command("ps").unwrap();
        assert_eq!(cmd, Command::Ps);
    }

    #[test]
    fn parse_ssh_logs() {
        let cmd = parse_ssh_command("logs myapp").unwrap();
        assert_eq!(cmd, Command::Logs("myapp".to_string()));
    }

    #[test]
    fn parse_ssh_stop() {
        let cmd = parse_ssh_command("stop myapp").unwrap();
        assert_eq!(cmd, Command::Stop("myapp".to_string()));
    }

    #[test]
    fn parse_ssh_unknown_command() {
        let result = parse_ssh_command("wat");
        assert!(result.is_err());
    }

    #[test]
    fn parse_argv_deploy() {
        let args = vec!["deploy".to_string(), "myapp".to_string()];
        let cmd = parse_args(&args).unwrap();
        assert_eq!(cmd, Command::Deploy("myapp".to_string()));
    }

    #[test]
    fn parse_argv_ps() {
        let args = vec!["ps".to_string()];
        let cmd = parse_args(&args).unwrap();
        assert_eq!(cmd, Command::Ps);
    }

    #[test]
    fn parse_argv_logs() {
        let args = vec!["logs".to_string(), "myapp".to_string()];
        let cmd = parse_args(&args).unwrap();
        assert_eq!(cmd, Command::Logs("myapp".to_string()));
    }

    #[test]
    fn parse_argv_stop() {
        let args = vec!["stop".to_string(), "myapp".to_string()];
        let cmd = parse_args(&args).unwrap();
        assert_eq!(cmd, Command::Stop("myapp".to_string()));
    }

    #[test]
    fn parse_argv_unknown() {
        let args = vec!["nope".to_string()];
        let result = parse_args(&args);
        assert!(result.is_err());
    }

    #[test]
    fn parse_app_name_strips_quotes_and_git() {
        assert_eq!(parse_app_name("'myapp.git'"), "myapp");
        assert_eq!(parse_app_name("\"myapp.git\""), "myapp");
        assert_eq!(parse_app_name("myapp"), "myapp");
        assert_eq!(parse_app_name("'myapp'"), "myapp");
    }
}
