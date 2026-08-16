use std::process::{Command, Stdio};

#[test]
fn kill_terminates_child() {
    let mut child = Command::new("cmd.exe")
        .args(["/c", "ping", "-n", "30", "127.0.0.1"])
        .stdout(Stdio::null())
        .spawn()
        .expect("spawn child");
    tm_collector::kill_process(child.id()).expect("kill_process");
    let status = child.wait().expect("wait");
    assert!(!status.success(), "terminated child should not exit cleanly");
}

#[test]
fn kill_nonexistent_pid_errors() {
    // Pids are multiples of 4; 3 can never be a valid process id.
    assert!(tm_collector::kill_process(3).is_err());
}
