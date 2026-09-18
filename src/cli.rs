//! Validate arguments before loading credentials, opening files, or starting a server.
#[derive(Debug, Default)]
pub(crate) struct Options {
    pub help: bool,
    #[cfg_attr(not(feature = "proxy"), allow(dead_code))]
    pub server: ServerOptions,
}
#[derive(Debug, Default)]
pub(crate) struct ServerOptions {
    pub host: Option<String>,
    pub port: Option<u16>,
}

fn value<'a>(args: &'a [String], at: &mut usize, flag: &str) -> Result<&'a str, String> {
    *at += 1;
    args.get(*at)
        .filter(|s| !s.is_empty() && !s.starts_with('-'))
        .map(String::as_str)
        .ok_or_else(|| format!("{flag} requires a value"))
}
fn port(raw: &str) -> Result<u16, String> {
    raw.parse()
        .map_err(|_| "--port must be an integer from 0 to 65535".into())
}
fn parse_host(raw: &str) -> Result<std::net::IpAddr, std::net::AddrParseError> {
    raw.strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(raw)
        .parse()
}
fn is_help(arg: &str) -> bool {
    matches!(arg, "--help" | "-h")
}
fn flags(args: &[String], allowed: &[&str]) -> Result<bool, String> {
    let mut help = false;
    for arg in args {
        if is_help(arg) {
            help = true;
        } else if !allowed.contains(&arg.as_str()) {
            return Err(format!("unknown argument: {arg}"));
        }
    }
    Ok(help)
}

pub(crate) fn parse(args: &[String]) -> Result<Options, String> {
    let mut options = Options::default();
    let command = args.get(1).map(String::as_str).unwrap_or("help");
    let tail = args.get(2..).unwrap_or_default();
    if tail.len() == 1 && is_help(&tail[0]) {
        options.help = true;
    }
    match command {
        "proxy" | "gateway" => {
            let mut at = 0;
            while at < tail.len() {
                let arg = tail[at].as_str();
                match arg {
                    "--help" | "-h" => options.help = true,
                    "--port" | "-p" => {
                        options.server.port = Some(port(value(tail, &mut at, arg)?)?)
                    }
                    "--host" => options.server.host = Some(value(tail, &mut at, arg)?.into()),
                    _ if arg.starts_with("--port=") => options.server.port = Some(port(&arg[7..])?),
                    _ if arg.starts_with("--host=") => options.server.host = Some(arg[7..].into()),
                    _ => return Err(format!("unknown argument for {command}: {arg}")),
                }
                at += 1;
            }
            if options
                .server
                .host
                .as_ref()
                .is_some_and(|host| parse_host(host).is_err())
            {
                return Err("--host must be an IPv4 or IPv6 address".into());
            }
        }
        "chat" => {
            let mut at = 0;
            while at < tail.len() {
                match tail[at].as_str() {
                    "--help" | "-h" => options.help = true,
                    "--log" => {
                        value(tail, &mut at, "--log")?;
                    }
                    arg => return Err(format!("unknown argument for chat: {arg}")),
                }
                at += 1;
            }
        }
        "models" => options.help = flags(tail, &["--all", "--json", "--refresh"])?,
        "configure" | "list" | "ls" | "path" | "help" | "--help" | "-h" => {
            options.help |= flags(tail, &[])?
        }
        "get" | "set" => {
            if !options.help
                && (tail.len() != if command == "set" { 2 } else { 1 } || tail[0].starts_with('-'))
            {
                return Err(usage(command).unwrap().into());
            }
        }
        "login" | "logout" => {
            if tail.len() == 2 && tail[0] == "chatgpt" && is_help(&tail[1]) {
                options.help = true;
            }
            if !options.help
                && (tail.first().map(String::as_str) != Some("chatgpt")
                    || (tail.len() != 1
                        && !(command == "login" && tail.len() == 2 && tail[1] == "--status")))
            {
                return Err(usage(command).unwrap().into());
            }
        }
        "docker" => {
            let sub = tail.first().map(String::as_str).unwrap_or("help");
            match sub {
                "help" | "--help" | "-h" => {
                    options.help = true;
                    flags(tail.get(1..).unwrap_or_default(), &[])?;
                }
                "start" => {
                    let mut at = 1;
                    while at < tail.len() {
                        match tail[at].as_str() {
                            "--help" | "-h" => options.help = true,
                            "--port" | "-p" => {
                                port(value(tail, &mut at, "--port")?)?;
                            }
                            arg => return Err(format!("unknown argument for docker start: {arg}")),
                        }
                        at += 1;
                    }
                }
                "logs" => options.help = flags(&tail[1..], &["--follow", "-f"])?,
                "stop" | "status" | "build" => options.help = flags(&tail[1..], &[])?,
                _ => return Err(format!("unknown docker command: {sub}")),
            }
        }
        _ => return Err(format!("unknown command: {command}")),
    }
    Ok(options)
}

pub(crate) fn usage(command: &str) -> Option<&'static str> {
    Some(match command {
        "proxy"=>"Usage: llmshim proxy [--host <IP>] [--port <PORT>]\n\nCLI options override LLMSHIM_HOST/LLMSHIM_PORT and saved configuration.\n  -h, --help  Print help without starting the server",
        "gateway"=>"Usage: llmshim gateway [--host <IP>] [--port <PORT>]\n\nCLI options override LLMSHIM_HOST/LLMSHIM_PORT and saved configuration.\n  -h, --help  Print help without starting the server",
        "chat"=>"Usage: llmshim chat [--log <PATH>]",
        "models"=>"Usage: llmshim models [--all] [--json] [--refresh]",
        "get"=>"Usage: llmshim get <key>",
        "set"=>"Usage: llmshim set <key> <value>",
        "login"=>"Usage: llmshim login chatgpt [--status]",
        "logout"=>"Usage: llmshim logout chatgpt",
        "docker"=>"Usage: llmshim docker <start [--port <PORT>]|stop|status|logs [--follow]|build>",
        "configure"=>"Usage: llmshim configure",
        "list"|"ls"=>"Usage: llmshim list",
        "path"=>"Usage: llmshim path",
        _=>return None,
    })
}

#[cfg(feature = "proxy")]
impl ServerOptions {
    pub fn address(
        &self,
        default_host: &str,
        default_port: u16,
    ) -> Result<std::net::SocketAddr, String> {
        let host = self.host.as_deref().unwrap_or(default_host);
        let ip = parse_host(host)
            .map_err(|_| format!("invalid listen address: {host}; expected an IP address"))?;
        Ok(std::net::SocketAddr::new(
            ip,
            self.port.unwrap_or(default_port),
        ))
    }
}
#[cfg(feature = "proxy")]
pub(crate) async fn bind(addr: std::net::SocketAddr) -> Result<tokio::net::TcpListener, String> {
    tokio::net::TcpListener::bind(addr).await.map_err(|error| {
        if error.kind() == std::io::ErrorKind::AddrInUse {
            format!("cannot listen on {addr}: port is already in use")
        } else {
            format!("cannot listen on {addr}: {error}")
        }
    })
}

#[cfg(all(test, feature = "proxy"))]
mod tests {
    use super::*;
    #[test]
    fn server_flags_override_defaults_and_support_ipv6() {
        let args = ["llmshim", "proxy", "--host", "::1", "--port=38471"].map(str::to_owned);
        let options = parse(&args).unwrap();
        assert_eq!(
            options.server.address("0.0.0.0", 3000).unwrap().to_string(),
            "[::1]:38471"
        );
        let options = ServerOptions::default();
        assert_eq!(
            options.address("[::1]", 3000).unwrap().to_string(),
            "[::1]:3000"
        );
        assert_eq!(
            options.address("127.0.0.1", 4000).unwrap().to_string(),
            "127.0.0.1:4000"
        );
    }
    #[tokio::test]
    async fn occupied_port_is_a_readable_error() {
        let listener = bind("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let error = bind(addr).await.unwrap_err();
        assert_eq!(
            error,
            format!("cannot listen on {addr}: port is already in use")
        );
    }
}
