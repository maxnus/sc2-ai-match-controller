use std::fs::{self, File, OpenOptions};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use tokio::net::lookup_host;
use tracing::{info, warn};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

#[tokio::main]
async fn main() {
    let _guards = init_controller_logs();

    // Run the bot in a spawned task to prevent panics from terminating the program
    let bot_task = tokio::spawn(async {
        run_bot().await;
    });

    // Wait for the bot task to complete (whether it panics or returns normally)
    let exit_code = if bot_task.await.is_err() { "2" } else { "0" };
    let _ = std::fs::write("/logs/signal.exit", exit_code);

    // Notice: The controller will keep running even after the bot process exits.
    // When it's in Kubenetes environment, this allows the Kubernetes Job to complete without restarts.
    // The `signal.exit` file signals to the match_controller that the bot process exited,
    // so that it can create the match result accordingly.
    // The responsibility for this signal will be removed from the bot controller in the next iteration,
    // so that base bot Docker images have simpler interface and responsibilities.
    wait_for_sigterm().await;

    info!("Bot controller exits");
}

async fn run_bot() {
    let game_host = std::env::var("GAME_HOST").unwrap_or_else(|_| "127.0.0.1".into());
    let game_port = std::env::var("GAME_PORT").expect("Missing GAME_PORT environment variable");
    let game_pass = std::env::var("GAME_PASS").unwrap_or_else(|_| game_port.clone());

    let bot_name = std::env::var("BOT_NAME").expect("Missing BOT_NAME environment variable");
    let opponent_id = std::env::var("OPPONENT_ID").expect("Missing OPPONENT_ID environment variable");

    let game_address = format!("{game_host}:{game_port}");
    let server_address = match lookup_host(game_address).await {
        Ok(mut addrs) => addrs.next().map(|x| x.ip().to_string()),
        Err(_) => None,
    }
    .unwrap_or(game_host);

    fs::create_dir_all("/bot/logs").expect("Could not create bot logs directory");

    let mut command = construct_bot_command("/bot", &bot_name);
    let command = command
        .stdout(create_log_file("/bot/logs/stdout.log"))
        .stderr(create_log_file("/bot/logs/stderr.log"))
        .arg("--GamePort")
        .arg(&game_port)
        .arg("--LadderServer")
        .arg(server_address)
        .arg("--StartPort")
        .arg(&game_pass)
        .arg("--OpponentId")
        .arg(opponent_id)
        .args(requested_bot_args())
        .current_dir("/bot");

    info!("Starting bot with command {:?}", &command);
    match command.status() {
        Ok(exit_status) => {
            info!("Bot process exited with status: {}", exit_status);
            let exit_code = exit_status.code().unwrap_or(2).to_string();
            let _ = std::fs::write("/logs/signal.exit", exit_code);
        }
        Err(e) => {
            panic!("Bot process failed with error: {}", e);
        }
    };
}

/// Extra arguments the requester of this match asked this bot to be started
/// with, as a JSON array of strings in BOT_ARGS. Ladder matches never carry
/// any, so an absent or empty value is the normal case.
///
/// A value we can't read costs the requester their arguments, not the match:
/// the bot still starts, and the match still produces a result.
fn requested_bot_args() -> Vec<String> {
    let raw = match std::env::var("BOT_ARGS") {
        Ok(raw) => raw,
        Err(_) => return Vec::new(),
    };
    parse_bot_args(&raw)
}

fn parse_bot_args(raw: &str) -> Vec<String> {
    if raw.trim().is_empty() {
        return Vec::new();
    }
    match serde_json::from_str::<Vec<String>>(raw) {
        Ok(args) => args,
        Err(e) => {
            warn!("Ignoring unreadable BOT_ARGS {:?}: {}", raw, e);
            Vec::new()
        }
    }
}

fn init_controller_logs() -> (tracing_appender::non_blocking::WorkerGuard, tracing_appender::non_blocking::WorkerGuard) {
    let controller_logs = create_log_file("/logs/controller.log");

    let (non_blocking_stdout, stdout_guard) = tracing_appender::non_blocking(std::io::stdout());
    let (non_blocking_controller_logs, controller_logs_guard) = tracing_appender::non_blocking(controller_logs);

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(non_blocking_controller_logs)
                .with_file(true)
                .with_ansi(false)
                .with_line_number(true)
                .with_target(false),
        )
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(non_blocking_stdout)
                .with_file(true)
                .with_line_number(true)
                .with_target(false),
        )
        .init();

    info!("Controller logs initialized.");
    (stdout_guard, controller_logs_guard)
}

fn create_log_file(file_name: &str) -> File {
    OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(file_name)
        .unwrap_or_else(|_| panic!("Could not create file {file_name}"))
}

fn construct_bot_command(bot_folder: &str, bot_name: &str) -> Command {
    info!("Constructing bot command...");

    let bot_path = Path::new(&bot_folder);

    if exists(bot_folder, "run.py") {
        command("python", &["run.py"])
    } else if exists(bot_folder, &format!("{bot_name}.dll")) {
        command("dotnet", &[&format!("{bot_name}.dll")])
    } else if exists(bot_folder, &format!("{bot_name}.jar")) {
        command("java", &["-jar", &format!("{bot_name}.jar")])
    } else if exists(bot_folder, &format!("{bot_name}.js")) {
        command("node", &[&format!("{bot_name}.js")])
    } else if exists(bot_folder, &format!("{bot_name}.exe")) {
        command("wine", &[&format!("{bot_name}.exe")])
    } else if exists(bot_folder, &format!("./{bot_name}")) {
        #[cfg(unix)]
        {
            let bot_binary = Path::new(&bot_folder).join(&bot_name);
            if let Ok(file) = std::fs::metadata(&bot_binary) {
                info!("Setting bot file permissions for: {}", bot_binary.display());
                let mut permissions = file.permissions();
                permissions.set_mode(0o777);
                let _ = std::fs::set_permissions(&bot_binary, permissions);
            }
        }

        Command::new(format!("./{bot_name}"))
    } else {
        // The executable was not found, list the contents of the bot folder for debugging
        info!("Listing contents of bot folder: {}", bot_folder);
        if let Ok(entries) = std::fs::read_dir(bot_path) {
            for entry in entries.flatten() {
                info!("{:?}", entry.path());
            }
        }

        panic!("Bot executable not found in folder: {}", bot_folder);
    }
}

fn command(program: &str, args: &[&str]) -> Command {
    let mut cmd = Command::new(program);
    cmd.args(args);
    cmd
}

fn exists(folder: &str, executable: &str) -> bool {
    Path::new(folder).join(executable).exists()
}

async fn wait_for_sigterm() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        if let Ok(mut sigterm) = signal(SignalKind::terminate()) {
            let _ = sigterm.recv().await;
            info!("Received SIGTERM, shutting down gracefully");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::parse_bot_args;

    #[test]
    fn no_args_is_the_normal_case() {
        assert!(parse_bot_args("").is_empty());
        assert!(parse_bot_args("   ").is_empty());
        assert!(parse_bot_args("[]").is_empty());
    }

    #[test]
    fn args_arrive_as_separate_arguments() {
        assert_eq!(parse_bot_args(r#"["--tournament=worldcup","--build=cheese"]"#), vec!["--tournament=worldcup", "--build=cheese"]);
    }

    #[test]
    fn an_argument_may_contain_spaces() {
        // The website splits the requester's string shell-style, so a quoted
        // space stays inside one argument. Going through JSON rather than a
        // space-separated list is what keeps it there.
        assert_eq!(parse_bot_args(r#"["--message=good luck"]"#), vec!["--message=good luck"]);
    }

    #[test]
    fn unreadable_args_are_dropped_rather_than_fatal() {
        assert!(parse_bot_args("not json").is_empty());
        assert!(parse_bot_args(r#"{"a": 1}"#).is_empty());
        assert!(parse_bot_args("[1, 2]").is_empty());
    }
}
