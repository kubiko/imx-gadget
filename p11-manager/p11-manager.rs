use std::env;
use std::fs::{self, File};
use std::path::Path;
use std::process::{Command, ExitCode, Stdio};
use std::thread;
use std::time::Duration;

const RUN_P11_KIT_DIR: &str = "/run/p11-kit";
const P11_KIT_SERVER_ENV_BASE: &str = "/tmp/pkcs11";
const MAX_TRIES: u32 = 5;
const RETRY_DELAY: Duration = Duration::from_secs(5);

// Syslog logger helper; falls back to stderr (journal) if /dev/log is unavailable
fn logger(msg: &str) {
    let sent = std::os::unix::net::UnixDatagram::unbound()
        .and_then(|stream| {
            stream.connect("/dev/log")?;
            stream.send(format!("<13>p11-manager: {}", msg).as_bytes())
        })
        .is_ok();
    if !sent {
        eprintln!("p11-manager: {}", msg);
    }
}

fn get_arch_triplet() -> Option<&'static str> {
    match env::var("SNAP_ARCH").as_deref() {
        Ok("arm64") => Some("aarch64-linux-gnu"),
        Ok("armhf") => Some("arm-linux-gnueabihf"),
        Ok("amd64") => Some("x86_64-linux-gnu"),
        _ => None,
    }
}

// Extract VAR=value from a line of `p11-kit server` sh-style output,
// which looks like: P11_KIT_SERVER_PID=1234; export P11_KIT_SERVER_PID;
fn env_value<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let value = line.strip_prefix(key)?;
    Some(value.split(';').next().unwrap_or(value).trim_matches('"'))
}

fn stop_p11_kit_servers() {
    let entries = match fs::read_dir("/tmp") {
        Ok(entries) => entries,
        Err(err) => {
            logger(&format!("Failed to read /tmp: {}", err));
            return;
        }
    };

    let mut found = false;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        if !name_str.starts_with("pkcs11-") || !name_str.ends_with(".env") {
            continue;
        }
        found = true;
        let path = entry.path();

        match fs::read_to_string(&path) {
            Ok(content) => {
                let mut pid = "";
                let mut addr = "";
                for line in content.lines() {
                    if let Some(value) = env_value(line, "P11_KIT_SERVER_PID=") {
                        pid = value;
                    } else if let Some(value) = env_value(line, "P11_KIT_SERVER_ADDRESS=") {
                        addr = value;
                    }
                }

                if pid.is_empty() || !pid.bytes().all(|b| b.is_ascii_digit()) {
                    logger(&format!("Ignoring env file without valid PID: {}", path.display()));
                } else {
                    logger(&format!("Stopping p11-kit server: PID={}, {}", pid, addr));

                    let stopped = Command::new("/usr/bin/p11-kit")
                        .args(["server", "-k"])
                        .env("P11_KIT_SERVER_PID", pid)
                        .env("P11_KIT_SERVER_ADDRESS", addr)
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .status()
                        .map(|status| status.success())
                        .unwrap_or(false);
                    if !stopped {
                        logger(&format!("Failed to stop p11-kit server: PID={}", pid));
                    }
                }
            }
            Err(err) => logger(&format!("Failed to read {}: {}", path.display(), err)),
        }
        let _ = fs::remove_file(&path);
    }

    if !found {
        logger("No running p11-kit servers found");
    }
}

fn start_p11_kit_servers_for_provider(provider_name: &str, provider_path: &str) -> bool {
    logger(&format!("start_p11_kit_servers_for_provider: {}", provider_name));

    for attempt in 1..=MAX_TRIES {
        let retry = |reason: &str| {
            logger(&format!("{}, attempt {}/{}", reason, attempt, MAX_TRIES));
            if attempt < MAX_TRIES {
                thread::sleep(RETRY_DELAY);
            }
        };

        let out = match Command::new("/usr/bin/p11tool")
            .args(["--provider", provider_path, "--list-token-urls"])
            .output()
        {
            Ok(out) => out,
            Err(err) => {
                retry(&format!("Failed to run p11tool: {}", err));
                continue;
            }
        };

        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);

        if stdout.contains("PKCS #11 error") || stderr.contains("PKCS #11 error") {
            retry("p11tool reported a PKCS #11 error");
            continue;
        }

        let token_urls = stdout
            .split_whitespace()
            .chain(stderr.split_whitespace())
            .filter(|word| word.starts_with("pkcs11:"));

        let mut all_started = true;
        for (token_num, token_url) in token_urls.enumerate() {
            // Address the token by its slot URL, without the token attributes
            let token_url = token_url.split(";token=").next().unwrap_or(token_url);

            let socket_name = format!("{}-slot-{}", provider_name, token_num);
            let socket_path = format!("{}/{}", RUN_P11_KIT_DIR, socket_name);
            let env_file_path = format!("{}-{}.env", P11_KIT_SERVER_ENV_BASE, socket_name);

            logger(&format!(
                "Starting p11-kit server --provider {} --name {}",
                provider_path, socket_path
            ));

            let env_file = match File::create(&env_file_path) {
                Ok(file) => file,
                Err(err) => {
                    logger(&format!("Failed to create env file {}: {}", env_file_path, err));
                    all_started = false;
                    continue;
                }
            };

            let started = Command::new("/usr/bin/p11-kit")
                .args(["server", "--provider", provider_path, "--name", &socket_path, token_url])
                .stdout(Stdio::from(env_file))
                .stderr(Stdio::null())
                .status()
                .map(|status| status.success())
                .unwrap_or(false);
            if !started {
                logger(&format!("Failed to start p11-kit server for {}", socket_path));
                let _ = fs::remove_file(&env_file_path);
                all_started = false;
            }
        }
        return all_started;
    }

    logger(&format!(
        "Giving up on provider {} after {} tries",
        provider_name, MAX_TRIES
    ));
    false
}

fn start_p11_kit_servers(providers: &[&str]) -> bool {
    if let Err(err) = fs::create_dir_all(RUN_P11_KIT_DIR) {
        logger(&format!("Failed to create {}: {}", RUN_P11_KIT_DIR, err));
        return false;
    }
    stop_p11_kit_servers();

    let Some(arch_triplet) = get_arch_triplet() else {
        logger("Unsupported or unset SNAP_ARCH");
        return false;
    };

    let snap = env::var("SNAP").unwrap_or_default();
    let lib_dir = format!("{}/usr/lib/{}", snap, arch_triplet);

    let mut all_ok = true;
    for provider_pair in providers {
        let Some((name, lib_name)) = provider_pair.split_once(':') else {
            logger(&format!("Invalid provider format skipped: {}", provider_pair));
            continue;
        };
        let (name, lib_name) = (name.trim(), lib_name.trim());

        let path = if lib_name.starts_with('/') {
            lib_name.to_string()
        } else {
            format!("{}/{}", lib_dir, lib_name)
        };

        logger(&format!("Handling provider: {}", name));
        if Path::new(&path).exists() {
            if !start_p11_kit_servers_for_provider(name, &path) {
                all_ok = false;
            }
        } else {
            logger(&format!("Provider library not found: {}", path));
        }
    }
    all_ok
}

fn print_help(bin_name: &str) {
    println!(
        "p11-manager - A lightweight tool to manage p11-kit server instances.\n\n\
         Usage: {0} <COMMAND> [ARGS]\n\n\
         Commands:\n\
         \x20 start              Start p11-kit servers. Requires at least one --provider flag.\n\
         \x20                    Format: --provider \"name:lib.so\"\n\
         \x20                    Example: {0} start --provider optee:libckteec.so --provider \"sc: opensc-pkcs11.so\"\n\
         \x20 stop               Stop all running p11-kit servers.\n\
         \x20 help, -h, --help   Print this help message.",
        bin_name
    );
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    let bin_name = args.first().map(String::as_str).unwrap_or("p11-manager");

    if args.len() < 2 {
        print_help(bin_name);
        logger("Missing arguments.");
        return ExitCode::FAILURE;
    }

    match args[1].as_str() {
        "start" => {
            let mut providers: Vec<&str> = Vec::new();
            let mut iter = args.iter().skip(2); // Skip binary name and "start"

            while let Some(arg) = iter.next() {
                if arg == "--provider" {
                    match iter.next() {
                        Some(val) => providers.push(val),
                        None => {
                            eprintln!("Error: Missing value for --provider\n");
                            print_help(bin_name);
                            logger("Missing value for --provider.");
                            return ExitCode::FAILURE;
                        }
                    }
                } else {
                    eprintln!("Error: Unknown argument '{}' for start command\n", arg);
                    print_help(bin_name);
                    logger(&format!("Unknown argument for start: {}", arg));
                    return ExitCode::FAILURE;
                }
            }

            if providers.is_empty() {
                eprintln!("Error: At least one --provider must be specified.\n");
                print_help(bin_name);
                logger("Missing providers string.");
                return ExitCode::FAILURE;
            }

            if start_p11_kit_servers(&providers) {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        "stop" => {
            stop_p11_kit_servers();
            ExitCode::SUCCESS
        }
        "help" | "-h" | "--help" => {
            print_help(bin_name);
            ExitCode::SUCCESS
        }
        other => {
            eprintln!("Error: Unknown command '{}'\n", other);
            print_help(bin_name);
            logger(&format!("Unknown option {}", other));
            ExitCode::FAILURE
        }
    }
}
