use std::env;
use std::fs::{self, File};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, ExitCode, Stdio};
use std::thread;
use std::time::Duration;

const BIN_NAME: &str = "p11-kit-server";
const RUN_P11_KIT_DIR: &str = "/run/p11-kit";
const P11_KIT_SERVER_ENV_BASE: &str = "/tmp/pkcs11";
const P11_KIT: &str = "/usr/bin/p11-kit";
const MAX_TRIES: u32 = 5;
const RETRY_DELAY: Duration = Duration::from_secs(5);

// Libraries of the providers that may be given by name alone
const KNOWN_PROVIDERS: &[(&str, &str)] = &[("optee", "libckteec.so")];

// Syslog logger helper; falls back to stderr (journal) if /dev/log is unavailable
fn logger(msg: &str) {
    let sent = std::os::unix::net::UnixDatagram::unbound()
        .and_then(|stream| {
            stream.connect("/dev/log")?;
            stream.send(format!("<13>{}: {}", BIN_NAME, msg).as_bytes())
        })
        .is_ok();
    if !sent {
        eprintln!("{}: {}", BIN_NAME, msg);
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

struct Provider {
    name: String,
    path: String,
}

// Resolve a provider given as "name:lib.so", or as "name" of a known provider.
// A library given without an absolute path is looked up in the snap's lib dir.
fn resolve_provider(spec: &str) -> Option<Provider> {
    let (name, lib_name) = match spec.split_once(':') {
        Some((name, lib_name)) => (name.trim(), lib_name.trim()),
        None => {
            let name = spec.trim();
            let lib_name = KNOWN_PROVIDERS
                .iter()
                .find(|(known, _)| *known == name)
                .map(|(_, lib_name)| *lib_name);
            match lib_name {
                Some(lib_name) => (name, lib_name),
                None => {
                    logger(&format!("Unknown provider, give it as name:lib.so: {}", spec));
                    return None;
                }
            }
        }
    };
    if name.is_empty() || lib_name.is_empty() {
        logger(&format!("Invalid provider format: {}", spec));
        return None;
    }

    let path = if lib_name.starts_with('/') {
        lib_name.to_string()
    } else {
        let Some(arch_triplet) = get_arch_triplet() else {
            logger("Unsupported or unset SNAP_ARCH");
            return None;
        };
        let snap = env::var("SNAP").unwrap_or_default();
        format!("{}/usr/lib/{}/{}", snap, arch_triplet, lib_name)
    };
    Some(Provider {
        name: name.to_string(),
        path,
    })
}

fn socket_name(provider_name: &str, slot: usize) -> String {
    format!("{}-slot-{}", provider_name, slot)
}

fn env_file_path(provider_name: &str, slot: usize) -> String {
    format!("{}-{}.env", P11_KIT_SERVER_ENV_BASE, socket_name(provider_name, slot))
}

// Extract VAR=value from a line of `p11-kit server` sh-style output,
// which looks like: P11_KIT_SERVER_PID=1234; export P11_KIT_SERVER_PID;
fn env_value<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let value = line.strip_prefix(key)?;
    Some(value.split(';').next().unwrap_or(value).trim_matches('"'))
}

// Stop the daemonized server an env file describes, and remove the file
fn stop_env_file_server(path: &Path) {
    match fs::read_to_string(path) {
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

                let stopped = Command::new(P11_KIT)
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
    let _ = fs::remove_file(path);
}

// Stop the daemonized servers, all of them, those of a provider, or the one of
// a provider's slot. A server of a single slot runs in the foreground, as the
// main process of its service, which systemd stops itself.
fn stop_p11_kit_servers(provider_name: Option<&str>, slot: Option<usize>) {
    if let (Some(name), Some(slot)) = (provider_name, slot) {
        let path = env_file_path(name, slot);
        if Path::new(&path).exists() {
            stop_env_file_server(Path::new(&path));
        } else {
            logger(&format!(
                "No daemonized p11-kit server for {}, systemd stops a foreground one",
                socket_name(name, slot)
            ));
        }
        return;
    }

    let entries = match fs::read_dir("/tmp") {
        Ok(entries) => entries,
        Err(err) => {
            logger(&format!("Failed to read /tmp: {}", err));
            return;
        }
    };

    let prefix = match provider_name {
        Some(name) => format!("pkcs11-{}-slot-", name),
        None => "pkcs11-".to_string(),
    };
    let mut found = false;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        if !name_str.starts_with(&prefix) || !name_str.ends_with(".env") {
            continue;
        }
        found = true;
        stop_env_file_server(&entry.path());
    }

    if !found {
        logger("No running p11-kit servers found");
    }
}

// List the slot URLs of a provider's tokens, in slot order, retrying while
// the provider is not ready
fn list_slot_urls(provider: &Provider) -> Option<Vec<String>> {
    for attempt in 1..=MAX_TRIES {
        let retry = |reason: &str| {
            logger(&format!("{}, attempt {}/{}", reason, attempt, MAX_TRIES));
            if attempt < MAX_TRIES {
                thread::sleep(RETRY_DELAY);
            }
        };

        let out = match Command::new("/usr/bin/p11tool")
            .args(["--provider", &provider.path, "--list-token-urls"])
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

        return Some(
            stdout
                .split_whitespace()
                .chain(stderr.split_whitespace())
                .filter(|word| word.starts_with("pkcs11:"))
                // Address the token by its slot URL, without the token attributes
                .map(|url| url.split(";token=").next().unwrap_or(url).to_string())
                .collect(),
        );
    }

    logger(&format!(
        "Giving up on provider {} after {} tries",
        provider.name, MAX_TRIES
    ));
    None
}

fn start_p11_kit_servers_for_provider(provider: &Provider) -> bool {
    logger(&format!("start_p11_kit_servers_for_provider: {}", provider.name));

    let Some(slot_urls) = list_slot_urls(provider) else {
        return false;
    };

    let mut all_started = true;
    for (slot, slot_url) in slot_urls.iter().enumerate() {
        let socket_path = format!("{}/{}", RUN_P11_KIT_DIR, socket_name(&provider.name, slot));
        let env_file_path = env_file_path(&provider.name, slot);

        logger(&format!(
            "Starting p11-kit server --provider {} --name {}",
            provider.path, socket_path
        ));

        let env_file = match File::create(&env_file_path) {
            Ok(file) => file,
            Err(err) => {
                logger(&format!("Failed to create env file {}: {}", env_file_path, err));
                all_started = false;
                continue;
            }
        };

        let started = Command::new(P11_KIT)
            .args(["server", "--provider", &provider.path, "--name", &socket_path, slot_url])
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
    all_started
}

// Start daemonized servers for every slot of every provider
fn start_p11_kit_servers(providers: &[Provider]) -> bool {
    if let Err(err) = fs::create_dir_all(RUN_P11_KIT_DIR) {
        logger(&format!("Failed to create {}: {}", RUN_P11_KIT_DIR, err));
        return false;
    }
    stop_p11_kit_servers(None, None);

    let mut all_ok = true;
    for provider in providers {
        logger(&format!("Handling provider: {}", provider.name));
        if Path::new(&provider.path).exists() {
            if !start_p11_kit_servers_for_provider(provider) {
                all_ok = false;
            }
        } else {
            logger(&format!("Provider library not found: {}", provider.path));
        }
    }
    all_ok
}

// Run the server of one slot of a provider in the foreground, in place of
// this process, so it is the main process of its service. Started by its
// socket, the server takes the socket systemd passes it (LISTEN_FDS, for
// LISTEN_PID, which exec keeps); otherwise it creates the socket itself.
// Returns only on failure.
fn run_p11_kit_server_for_slot(provider: &Provider, slot: usize) -> bool {
    logger(&format!("start_p11_kit_server_for_slot: {} slot {}", provider.name, slot));

    if !Path::new(&provider.path).exists() {
        logger(&format!("Provider library not found: {}", provider.path));
        return false;
    }
    let socket_activated = env::var_os("LISTEN_FDS").is_some();
    if !socket_activated {
        if let Err(err) = fs::create_dir_all(RUN_P11_KIT_DIR) {
            logger(&format!("Failed to create {}: {}", RUN_P11_KIT_DIR, err));
            return false;
        }
    }
    // a daemonized server of this slot would hold its socket
    let env_file = env_file_path(&provider.name, slot);
    if Path::new(&env_file).exists() {
        stop_env_file_server(Path::new(&env_file));
    }

    let Some(slot_urls) = list_slot_urls(provider) else {
        return false;
    };
    let Some(slot_url) = slot_urls.get(slot) else {
        logger(&format!(
            "Provider {} has no slot {}, it has {}",
            provider.name,
            slot,
            slot_urls.len()
        ));
        return false;
    };

    let socket_path = format!("{}/{}", RUN_P11_KIT_DIR, socket_name(&provider.name, slot));
    logger(&format!(
        "Starting p11-kit server --provider {} --name {}{}",
        provider.path,
        socket_path,
        if socket_activated { ", socket activated" } else { "" }
    ));
    let err = Command::new(P11_KIT)
        .args(["server", "-f", "--provider", &provider.path, "--name", &socket_path, slot_url])
        .stderr(Stdio::null())
        .exec();
    logger(&format!("Failed to run p11-kit server for {}: {}", socket_path, err));
    false
}

fn print_help(bin_name: &str) {
    println!(
        "{1} - A lightweight tool to manage p11-kit server instances.\n\n\
         Usage: {0} <COMMAND> [ARGS]\n\n\
         Commands:\n\
         \x20 start              Start p11-kit servers. Requires at least one --provider flag.\n\
         \x20                    Format: --provider \"name:lib.so\", or --provider name of a\n\
         \x20                    known provider (optee).\n\
         \x20                    Without --slot, start a daemonized server for each slot of\n\
         \x20                    each provider.\n\
         \x20                    With --slot <number>, run the server of that slot of the one\n\
         \x20                    provider in the foreground, socket activated when started by\n\
         \x20                    its socket.\n\
         \x20                    Example: {0} start --provider optee:libckteec.so --provider \"sc: opensc-pkcs11.so\"\n\
         \x20                    Example: {0} start --provider optee --slot 0\n\
         \x20 stop               Stop the daemonized p11-kit servers: all of them, those of\n\
         \x20                    --provider name, or the one of its --slot <number>.\n\
         \x20 help, -h, --help   Print this help message.",
        bin_name, BIN_NAME
    );
}

struct Args<'a> {
    providers: Vec<&'a str>,
    slot: Option<usize>,
}

fn parse_args<'a>(bin_name: &str, command: &str, args: &'a [String]) -> Result<Args<'a>, ()> {
    let fail = |msg: String| {
        eprintln!("Error: {}\n", msg);
        print_help(bin_name);
        logger(&msg);
    };
    let mut parsed = Args {
        providers: Vec::new(),
        slot: None,
    };
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--provider" => match iter.next() {
                Some(val) => parsed.providers.push(val),
                None => {
                    fail("Missing value for --provider".to_string());
                    return Err(());
                }
            },
            "--slot" => match iter.next().map(|val| val.parse::<usize>()) {
                Some(Ok(slot)) if parsed.slot.is_none() => parsed.slot = Some(slot),
                Some(Ok(_)) => {
                    fail("--slot given more than once".to_string());
                    return Err(());
                }
                _ => {
                    fail("--slot needs a slot number".to_string());
                    return Err(());
                }
            },
            other => {
                fail(format!("Unknown argument '{}' for {} command", other, command));
                return Err(());
            }
        }
    }
    if parsed.slot.is_some() && parsed.providers.len() != 1 {
        fail("--slot needs exactly one --provider".to_string());
        return Err(());
    }
    Ok(parsed)
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    let bin_name = args.first().map(String::as_str).unwrap_or(BIN_NAME);

    if args.len() < 2 {
        print_help(bin_name);
        logger("Missing arguments.");
        return ExitCode::FAILURE;
    }

    match args[1].as_str() {
        "start" => {
            let Ok(parsed) = parse_args(bin_name, "start", &args[2..]) else {
                return ExitCode::FAILURE;
            };
            if parsed.providers.is_empty() {
                eprintln!("Error: At least one --provider must be specified.\n");
                print_help(bin_name);
                logger("Missing providers string.");
                return ExitCode::FAILURE;
            }

            let ok = match parsed.slot {
                Some(slot) => match resolve_provider(parsed.providers[0]) {
                    Some(provider) => run_p11_kit_server_for_slot(&provider, slot),
                    None => false,
                },
                None => {
                    if get_arch_triplet().is_none() {
                        logger("Unsupported or unset SNAP_ARCH");
                        return ExitCode::FAILURE;
                    }
                    // as before, a provider that does not resolve is skipped
                    let providers: Vec<Provider> = parsed
                        .providers
                        .iter()
                        .filter_map(|spec| resolve_provider(spec))
                        .collect();
                    start_p11_kit_servers(&providers)
                }
            };
            if ok {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        "stop" => {
            let Ok(parsed) = parse_args(bin_name, "stop", &args[2..]) else {
                return ExitCode::FAILURE;
            };
            if parsed.providers.len() > 1 {
                eprintln!("Error: stop takes at most one --provider.\n");
                print_help(bin_name);
                logger("stop takes at most one --provider.");
                return ExitCode::FAILURE;
            }
            // stop goes by the provider's name, whether or not it is known
            let name = parsed
                .providers
                .first()
                .map(|spec| spec.split_once(':').map_or(*spec, |(name, _)| name).trim());
            stop_p11_kit_servers(name, parsed.slot);
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
