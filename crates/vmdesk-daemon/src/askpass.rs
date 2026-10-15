//! `vmdesk askpass`: the `SSH_ASKPASS` helper behind "Open terminal" for password VMs.
//!
//! `ssh` runs it for every prompt once `SSH_ASKPASS_REQUIRE=force` is set. It answers the
//! first password prompt of each `ssh` process from the keychain and passes everything
//! else to the user on the terminal: host-key questions, key passphrases, and the
//! password again if the stored one was wrong.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::ExitCode;

/// What `ssh` asks for, judged from the prompt text.
#[derive(Debug, PartialEq, Eq)]
enum Prompt {
    Password,
    /// A yes/no question, e.g. accepting a new host key; the answer is shown as typed.
    Confirm,
    /// Anything else secret (key passphrase, OTP); read without echo.
    Secret,
}

fn classify(prompt: &str) -> Prompt {
    let p = prompt.to_lowercase();
    if p.contains("yes/no") {
        Prompt::Confirm
    } else if p.contains("password") && !p.contains("passphrase") && !p.contains("new password") {
        Prompt::Password
    } else {
        Prompt::Secret
    }
}

/// Marks that this `ssh` process already got the stored password, so a retry goes to
/// the user instead of repeating a wrong password.
fn used_marker() -> PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR").map_or_else(std::env::temp_dir, PathBuf::from);
    dir.join(format!(
        "vmdesk-askpass-{}",
        std::os::unix::process::parent_id()
    ))
}

pub async fn askpass(entry: &str, prompt: &str) -> ExitCode {
    let kind = classify(prompt);
    let marker = used_marker();
    if kind == Prompt::Password
        && !marker.exists()
        && let Ok(password) = vmdesk_transport::keychain::get_password(entry).await
    {
        let _ = std::fs::write(&marker, b"");
        println!("{password}");
        return ExitCode::SUCCESS;
    }
    match ask_user(prompt, kind == Prompt::Confirm) {
        Ok(answer) => {
            println!("{answer}");
            ExitCode::SUCCESS
        }
        // ssh treats a failing helper as "cancelled".
        Err(_) => ExitCode::FAILURE,
    }
}

fn ask_user(prompt: &str, visible: bool) -> std::io::Result<String> {
    if !visible {
        return rpassword::prompt_password(prompt);
    }
    let mut tty = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")?;
    tty.write_all(prompt.as_bytes())?;
    tty.flush()?;
    let mut line = String::new();
    BufReader::new(tty).read_line(&mut line)?;
    Ok(line.trim_end_matches(['\r', '\n']).to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_ssh_prompts() {
        assert_eq!(classify("root@172.16.9.89's password: "), Prompt::Password);
        assert_eq!(classify("(root@172.16.9.89) Password: "), Prompt::Password);
        assert_eq!(
            classify("Are you sure you want to continue connecting (yes/no/[fingerprint])? "),
            Prompt::Confirm
        );
        assert_eq!(
            classify("Enter passphrase for key '/root/.ssh/id_ed25519': "),
            Prompt::Secret
        );
        assert_eq!(classify("New password: "), Prompt::Secret);
        assert_eq!(classify("Verification code: "), Prompt::Secret);
    }
}
