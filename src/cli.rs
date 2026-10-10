//! Command-line parsing without dependencies (clap would add ~200 KB and a long
//! compile on the Pi). Precedence: flag > environment variable > default.
//! One option table drives parsing, help output and typo suggestions.

use std::ffi::OsString;

pub struct Config {
    pub listen: String,
    pub socket: String,
    pub user: String,
    pub password: Option<String>,
    pub max_connections: usize,
}

pub enum Cli {
    Run(Config),
    /// Help or version was printed; exit successfully.
    Done,
}

struct Opt {
    short: Option<char>,
    long: &'static str,
    value: Option<&'static str>,
    env: Option<&'static str>,
    default: Option<&'static str>,
    help: &'static str,
}

const OPTS: &[Opt] = &[
    Opt {
        short: Some('l'),
        long: "listen",
        value: Some("ADDR"),
        env: Some("CONTRAIRE_LISTEN"),
        default: Some("0.0.0.0:8083"),
        help: "Address to listen on (HOST:PORT, or just PORT for all interfaces)",
    },
    Opt {
        short: Some('s'),
        long: "socket",
        value: Some("PATH"),
        env: Some("DOCKER_HOST"),
        default: Some("/var/run/docker.sock"),
        help: "Docker daemon socket (a unix:// prefix is accepted)",
    },
    Opt {
        short: Some('u'),
        long: "user",
        value: Some("NAME"),
        env: Some("CONTRAIRE_USER"),
        default: Some("admin"),
        help: "User name for HTTP basic auth",
    },
    Opt {
        short: Some('p'),
        long: "password-file",
        value: Some("PATH"),
        env: Some("CONTRAIRE_PASSWORD_FILE"),
        default: None,
        help: "Enable basic auth with the password in this file (or set CONTRAIRE_PASSWORD)",
    },
    Opt {
        short: None,
        long: "max-connections",
        value: Some("N"),
        env: Some("CONTRAIRE_MAX_CONNECTIONS"),
        default: Some("64"),
        help: "Maximum concurrent client connections (one thread each)",
    },
    Opt { short: Some('h'), long: "help", value: None, env: None, default: None, help: "Print help" },
    Opt { short: Some('V'), long: "version", value: None, env: None, default: None, help: "Print version" },
];

fn help() -> String {
    let mut s = format!(
        "{} {}\n{}\n\nUsage: {} [OPTIONS]\n\nOptions:\n",
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION"),
        env!("CARGO_PKG_DESCRIPTION"),
        env!("CARGO_PKG_NAME"),
    );
    for o in OPTS {
        let short = o.short.map_or("    ".to_string(), |c| format!("-{c}, "));
        let head = match o.value {
            Some(v) => format!("{short}--{} <{v}>", o.long),
            None => format!("{short}--{}", o.long),
        };
        s.push_str(&format!("  {head:<32}{}\n", o.help));
        let mut extra = String::new();
        if let Some(e) = o.env {
            extra.push_str(&format!("[env: {e}] "));
        }
        if let Some(d) = o.default {
            extra.push_str(&format!("[default: {d}]"));
        }
        if !extra.is_empty() {
            s.push_str(&format!("  {:<32}{}\n", "", extra.trim_end()));
        }
    }
    s
}

fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut prev = row[0];
        row[0] = i + 1;
        for (j, &cb) in b.iter().enumerate() {
            let cur = row[j + 1];
            row[j + 1] = (prev + (ca != cb) as usize).min(row[j] + 1).min(cur + 1);
            prev = cur;
        }
    }
    row[b.len()]
}

fn unknown(arg: &str) -> String {
    let name = arg.trim_start_matches('-');
    let best = OPTS
        .iter()
        .map(|o| (edit_distance(name, o.long), o.long))
        .min()
        .filter(|(d, _)| *d <= 2 && name.len() > 1);
    match best {
        Some((_, long)) => format!("unknown option '{arg}' (did you mean '--{long}'?)"),
        None => format!("unknown option '{arg}'"),
    }
}

/// Parse `args` (including argv[0]). `env` is injected so tests don't touch the process environment.
pub fn parse(args: impl IntoIterator<Item = OsString>, env: impl Fn(&str) -> Option<String>) -> Result<Cli, String> {
    let mut args = args.into_iter().skip(1).map(|a| {
        a.into_string().map_err(|a| format!("argument is not valid UTF-8: {}", a.to_string_lossy()))
    });
    let mut given: Vec<(&'static str, String)> = Vec::new();

    while let Some(arg) = args.next() {
        let arg = arg?;
        if arg == "--" {
            if let Some(extra) = args.next() {
                return Err(format!("unexpected argument '{}'", extra?));
            }
            break;
        }
        // Resolve to (option, inline value) pairs; a short cluster like -hV yields several.
        let mut found: Vec<(&Opt, Option<String>)> = Vec::new();
        if let Some(long) = arg.strip_prefix("--") {
            let (name, inline) = match long.split_once('=') {
                Some((n, v)) => (n, Some(v.to_string())),
                None => (long, None),
            };
            let opt = OPTS.iter().find(|o| o.long == name).ok_or_else(|| unknown(&arg))?;
            found.push((opt, inline));
        } else if let Some(cluster) = arg.strip_prefix('-').filter(|c| !c.is_empty()) {
            for (i, c) in cluster.char_indices() {
                let opt = OPTS.iter().find(|o| o.short == Some(c)).ok_or_else(|| unknown(&format!("-{c}")))?;
                if opt.value.is_some() {
                    // -lVALUE or -l=VALUE; otherwise the value is the next argument.
                    let rest = &cluster[i + c.len_utf8()..];
                    let rest = rest.strip_prefix('=').unwrap_or(rest);
                    found.push((opt, (!rest.is_empty()).then(|| rest.to_string())));
                    break;
                }
                found.push((opt, None));
            }
        } else {
            return Err(format!("unexpected argument '{arg}'"));
        }

        for (opt, inline) in found {
            match (opt.long, opt.value) {
                ("help", _) => {
                    print!("{}", help());
                    return Ok(Cli::Done);
                }
                ("version", _) => {
                    println!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
                    return Ok(Cli::Done);
                }
                (_, Some(meta)) => {
                    let v = match inline {
                        Some(v) => v,
                        None => args
                            .next()
                            .transpose()?
                            .ok_or_else(|| format!("option '--{}' requires a value <{meta}>", opt.long))?,
                    };
                    given.retain(|(k, _)| *k != opt.long);
                    given.push((opt.long, v));
                }
                (_, None) => given.push((opt.long, String::new())),
            }
        }
    }

    let get = |long: &str| -> Option<String> {
        let opt = OPTS.iter().find(|o| o.long == long)?;
        given
            .iter()
            .find(|(k, _)| *k == long)
            .map(|(_, v)| v.clone())
            .or_else(|| opt.env.and_then(&env).filter(|v| !v.is_empty()))
            .or_else(|| opt.default.map(str::to_string))
    };

    let mut listen = get("listen").unwrap_or_default();
    if listen.parse::<u16>().is_ok() {
        listen = format!("0.0.0.0:{listen}");
    } else if !listen.contains(':') {
        return Err(format!("invalid listen address '{listen}': expected HOST:PORT or PORT"));
    }

    let socket = get("socket").unwrap_or_default();
    let socket = match socket.split_once("://") {
        None => socket,
        Some(("unix", path)) => path.to_string(),
        Some(_) => return Err(format!("unsupported docker socket '{socket}': only unix sockets are supported")),
    };

    let max_connections = get("max-connections")
        .unwrap_or_default()
        .parse::<usize>()
        .ok()
        .filter(|n| (1..=4096).contains(n))
        .ok_or("--max-connections must be a number between 1 and 4096")?;

    let password = match get("password-file") {
        Some(path) => Some(read_password(&path)?),
        None => env("CONTRAIRE_PASSWORD").filter(|p| !p.is_empty()),
    };

    Ok(Cli::Run(Config {
        listen,
        socket,
        user: get("user").unwrap_or_default(),
        password,
        max_connections,
    }))
}

fn read_password(path: &str) -> Result<String, String> {
    use std::os::unix::fs::PermissionsExt;
    let meta = std::fs::metadata(path).map_err(|e| format!("cannot read password file '{path}': {e}"))?;
    if meta.permissions().mode() & 0o004 != 0 {
        eprintln!("contraire: warning: password file '{path}' is world-readable (chmod 600 it)");
    }
    let pw = std::fs::read_to_string(path).map_err(|e| format!("cannot read password file '{path}': {e}"))?;
    let pw = pw.trim_end_matches(['\n', '\r']);
    if pw.is_empty() {
        return Err(format!("password file '{path}' is empty"));
    }
    Ok(pw.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(args: &[&str], env: &[(&str, &str)]) -> Result<Config, String> {
        let argv = std::iter::once("contraire").chain(args.iter().copied()).map(OsString::from);
        let env: Vec<(String, String)> = env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        match parse(argv, |k| env.iter().find(|(e, _)| e == k).map(|(_, v)| v.clone()))? {
            Cli::Run(c) => Ok(c),
            Cli::Done => Err("done".into()),
        }
    }

    #[test]
    fn defaults() {
        let c = run(&[], &[]).unwrap();
        assert_eq!(c.listen, "0.0.0.0:8083");
        assert_eq!(c.socket, "/var/run/docker.sock");
        assert_eq!(c.user, "admin");
        assert!(c.password.is_none());
        assert_eq!(c.max_connections, 64);
    }

    #[test]
    fn value_syntaxes() {
        assert_eq!(run(&["--listen", "1.2.3.4:5"], &[]).unwrap().listen, "1.2.3.4:5");
        assert_eq!(run(&["--listen=1.2.3.4:5"], &[]).unwrap().listen, "1.2.3.4:5");
        assert_eq!(run(&["-l", "9000"], &[]).unwrap().listen, "0.0.0.0:9000");
        assert_eq!(run(&["-l127.0.0.1:1"], &[]).unwrap().listen, "127.0.0.1:1");
        assert_eq!(run(&["-l", "[::1]:80"], &[]).unwrap().listen, "[::1]:80");
        assert_eq!(run(&["-l", "1", "-l", "2"], &[]).unwrap().listen, "0.0.0.0:2");
    }

    #[test]
    fn precedence_flag_env_default() {
        let env = [("CONTRAIRE_LISTEN", "7000"), ("DOCKER_HOST", "unix:///run/d.sock")];
        let c = run(&[], &env).unwrap();
        assert_eq!(c.listen, "0.0.0.0:7000");
        assert_eq!(c.socket, "/run/d.sock");
        assert_eq!(run(&["-l", "8000"], &env).unwrap().listen, "0.0.0.0:8000");
        assert_eq!(run(&[], &[("CONTRAIRE_PASSWORD", "pw")]).unwrap().password.as_deref(), Some("pw"));
    }

    #[test]
    fn errors() {
        assert!(run(&["--lisen", "1"], &[]).err().unwrap().contains("did you mean '--listen'"));
        assert!(run(&["--listen"], &[]).err().unwrap().contains("requires a value"));
        assert!(run(&["-x"], &[]).err().unwrap().contains("unknown option '-x'"));
        assert!(run(&["stray"], &[]).err().unwrap().contains("unexpected argument"));
        assert!(run(&["-l", "localhost"], &[]).err().unwrap().contains("HOST:PORT"));
        assert!(run(&["-s", "tcp://1.2.3.4:2375"], &[]).err().unwrap().contains("only unix"));
        assert!(run(&["--max-connections", "0"], &[]).is_err());
        assert!(run(&["-p", "/nonexistent/pw"], &[]).err().unwrap().contains("cannot read"));
    }
}
