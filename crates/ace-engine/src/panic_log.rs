//! Process-wide panic logging.
//!
//! A panic inside a tokio task is contained: the task ends and the daemon keeps running. A
//! panic on the main thread exits the process with code 101. Either way the only trace used
//! to be the standard library's untimestamped `thread '...' panicked at ...` line, which is
//! easy to lose among operational logs. The hook installed here writes one timestamped
//! `[panic]` line first, then hands over to the previous hook (the standard one prints the
//! message and, with `RUST_BACKTRACE`, a backtrace), so both stay in the log.
//!
//! The hook runs only when something panics; it adds nothing to any normal code path.

use std::any::Any;
use std::panic::PanicHookInfo;

/// Install the `[panic]` logging hook in front of the current hook. Call it once, first
/// thing in `main`.
pub fn install() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        log(info);
        previous(info);
    }));
}

fn log(info: &PanicHookInfo<'_>) {
    let thread = std::thread::current();
    let location = info
        .location()
        .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()));
    let line = describe(
        thread.name(),
        location.as_deref(),
        payload_message(info.payload()),
    );
    // `alog!` panics when stderr cannot be written, and a panic inside a panic hook aborts the
    // process, so this path uses the non-panicking writer and ignores a failed write.
    let _ = ace_log::try_log(format_args!("{line}"));
}

fn payload_message(payload: &(dyn Any + Send)) -> &str {
    if let Some(message) = payload.downcast_ref::<&'static str>() {
        message
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message
    } else {
        "<non-string panic payload>"
    }
}

/// The `[panic]` line. On the main thread the process exits with code 101 after the hook, so
/// the line says so; elsewhere only the panicking task or thread ends.
fn describe(thread: Option<&str>, location: Option<&str>, message: &str) -> String {
    let thread = thread.unwrap_or("<unnamed>");
    let location = location.unwrap_or("<unknown location>");
    let effect = if thread == "main" {
        "the process exits with code 101"
    } else {
        "this thread or task ends"
    };
    format!("[panic] thread '{thread}' panicked at {location}: {message} ({effect})")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    const CHILD_ENV: &str = "OUTPACE_TEST_PANIC_LOG_CHILD";

    #[test]
    fn describes_thread_location_message_and_effect() {
        assert_eq!(
            describe(Some("main"), Some("src/x.rs:1:2"), "boom"),
            "[panic] thread 'main' panicked at src/x.rs:1:2: boom (the process exits with code 101)"
        );
        assert_eq!(
            describe(None, None, "boom"),
            "[panic] thread '<unnamed>' panicked at <unknown location>: boom (this thread or task ends)"
        );
    }

    #[test]
    fn payload_message_reads_str_and_string_payloads() {
        let s: Box<dyn Any + Send> = Box::new("static");
        let owned: Box<dyn Any + Send> = Box::new(String::from("owned"));
        let other: Box<dyn Any + Send> = Box::new(7_u8);
        assert_eq!(payload_message(s.as_ref()), "static");
        assert_eq!(payload_message(owned.as_ref()), "owned");
        assert_eq!(
            payload_message(other.as_ref()),
            "<non-string panic payload>"
        );
    }

    /// The child half of [`hook_logs_panics_from_any_thread_in_a_child_process`]. It is a
    /// no-op unless the parent sets the env var, so the hook never touches the shared test
    /// process.
    #[test]
    fn panic_log_child_entry() {
        if std::env::var_os(CHILD_ENV).is_none() {
            return;
        }
        install();
        let worker = std::thread::Builder::new()
            .name("panic-log-worker".into())
            .spawn(|| panic!("worker {}", "exploded"))
            .unwrap();
        assert!(worker.join().is_err());
        std::process::exit(0);
    }

    #[test]
    fn hook_logs_panics_from_any_thread_in_a_child_process() {
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "panic_log::tests::panic_log_child_entry",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD_ENV, "1")
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "child failed: {stderr}");
        let line = stderr
            .lines()
            .find(|line| line.contains("[panic]"))
            .unwrap_or_else(|| panic!("no [panic] line in child stderr: {stderr}"));
        assert!(
            line.contains("[panic] thread 'panic-log-worker' panicked at ")
                && line.contains("panic_log.rs:")
                && line.ends_with(": worker exploded (this thread or task ends)"),
            "{line}"
        );
        // The previous (standard) hook still runs, after ours. Newer toolchains print the
        // thread id after its name, so match only the start of that line.
        let lines: Vec<&str> = stderr.lines().collect();
        let ours = lines.iter().position(|l| l.contains("[panic]")).unwrap();
        let standard = lines
            .iter()
            .position(|l| l.starts_with("thread 'panic-log-worker'") && l.contains("panicked at"));
        assert!(
            standard.is_some_and(|at| at > ours),
            "standard panic output missing or first: {stderr}"
        );
    }
}
