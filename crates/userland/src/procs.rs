//! Programs on processes: ps and kill, as procps' are, over the process calls. A PID is a long
//! word; the first eight of it name it, as ps shows it, and fewer do as long as one matches.
use crate::files::parsed;
use crate::io::Io;
use crate::sys::{Sys, call};
use abi::{Call, Errno};
use serde_json::Value;
use terminal_worker_core::cli::{archives as args, parse};

/// What a process runs, as ps shows it: its program and what it works on.
fn command(process: &Value) -> String {
    let command = &process["command"];
    let on = command["archive"]
        .as_str()
        .or(command["file"].as_str())
        .unwrap_or_default();
    format!("{} {on}", process["kind"].as_str().unwrap_or_default())
        .trim_end()
        .to_string()
}

fn short(pid: &str) -> &str {
    pid.get(..8).unwrap_or(pid)
}

pub async fn ps(sys: &impl Sys, io: &mut Io<'_>) -> i32 {
    let _ = parsed!(io, args::Ps);
    let processes: Vec<Value> = match call(sys, Call::Ps).await {
        Ok(processes) => processes,
        Err(error) => {
            io.fail(format!("ps: {error}"));
            return 1;
        }
    };
    io.say(format!("{:<8}  {:<10}  COMMAND", "PID", "STAT"));
    for process in &processes {
        io.say(format!(
            "{:<8}  {:<10}  {}",
            short(process["pid"].as_str().unwrap_or_default()),
            process["state"].as_str().unwrap_or_default(),
            command(process)
        ));
    }
    0
}

pub async fn kill(sys: &impl Sys, io: &mut Io<'_>) -> i32 {
    let kill = parsed!(io, args::Kill);
    let processes: Vec<Value> = call(sys, Call::Ps).await.unwrap_or_default();
    let mut status = 0;
    for given in &kill.pids {
        let matching: Vec<&str> = processes
            .iter()
            .filter_map(|process| process["pid"].as_str())
            .filter(|pid| pid.starts_with(given.as_str()))
            .collect();
        let pid = match matching.as_slice() {
            [pid] => pid.to_string(),
            // Not one of the account's: the kernel says whose, if anyone's.
            [] => given.clone(),
            _ => {
                io.fail(format!("kill: ({given}) - ambiguous: name more of it"));
                status = 1;
                continue;
            }
        };
        if let Err(error) = call::<Value>(sys, Call::Kill { pid }).await {
            let error = if error.errno == Errno::ESRCH {
                "No such process".to_string()
            } else {
                error.to_string()
            };
            io.fail(format!("kill: ({given}) - {error}"));
            status = 1;
        }
    }
    status
}
