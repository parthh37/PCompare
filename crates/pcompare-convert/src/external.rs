//! External programs: conversion commands of file format rules, and
//! third-party comparison tools. Command lines are split into a program and
//! arguments here and run directly (never through a shell), so paths with
//! spaces or shell metacharacters are passed through intact.

use pcompare_core::{CoreError, Result};
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// A conversion command gets this long before it is killed.
const CONVERT_TIMEOUT: Duration = Duration::from_secs(120);

/// Splits a command line into words: whitespace separates them, double
/// quotes group (a doubled `""` inside quotes is a literal quote), and
/// backslashes are ordinary characters (Windows paths).
pub fn split_command_line(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                chars.next();
                cur.push('"');
            }
            '"' => {
                quoted = !quoted;
                in_word = true;
            }
            c if c.is_whitespace() && !quoted => {
                if in_word {
                    words.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            c => {
                cur.push(c);
                in_word = true;
            }
        }
    }
    if in_word {
        words.push(cur);
    }
    words
}

/// A program and its arguments, ready to spawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCommand {
    pub program: String,
    pub args: Vec<String>,
}

impl ToolCommand {
    fn command(&self) -> Command {
        let mut cmd = Command::new(&self.program);
        cmd.args(&self.args);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // No console window flashing up for console-mode converters.
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        cmd
    }

    /// Starts the program without waiting for it (external comparison tools).
    pub fn spawn_detached(&self) -> Result<()> {
        let mut cmd = Command::new(&self.program);
        cmd.args(&self.args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        cmd.spawn()
            .map(drop)
            .map_err(|e| CoreError::Other(format!("cannot start {}: {e}", self.program)))
    }
}

/// Splits `line` and substitutes `%name` placeholders inside each word
/// (`/left:%1` works). Returns the command and which placeholders occurred.
fn expand(line: &str, subs: &[(&str, &str)]) -> Result<(ToolCommand, Vec<bool>)> {
    let mut used = vec![false; subs.len()];
    let words: Vec<String> = split_command_line(line)
        .into_iter()
        .map(|w| {
            let mut w = w;
            for (i, (key, value)) in subs.iter().enumerate() {
                if w.contains(key) {
                    used[i] = true;
                    w = w.replace(key, value);
                }
            }
            w
        })
        .collect();
    let mut words = words.into_iter();
    let program = words
        .next()
        .filter(|p| !p.is_empty())
        .ok_or_else(|| CoreError::Other("the command line is empty".into()))?;
    Ok((
        ToolCommand {
            program,
            args: words.collect(),
        },
        used,
    ))
}

/// An external comparison tool's command line with `%1` / `%2` replaced by
/// the left / right paths; without either placeholder both are appended.
pub fn expand_tool_command(line: &str, left: &str, right: &str) -> Result<ToolCommand> {
    let (mut cmd, used) = expand(line, &[("%1", left), ("%2", right)])?;
    if !used.iter().any(|&u| u) {
        cmd.args
            .extend([left, right].into_iter().filter(|p| !p.is_empty()).map(String::from));
    }
    Ok(cmd)
}

/// Runs a file format rule's conversion command on `input` and returns the
/// text it produced: the `%t` file it wrote, or else its standard output.
/// `%s` is the input path (appended when missing).
pub fn run_conversion_command(line: &str, input: &Path, output: &Path) -> Result<String> {
    let input_s = input.to_string_lossy();
    let output_s = output.to_string_lossy();
    let (mut cmd, used) = expand(line, &[("%s", &input_s), ("%t", &output_s)])?;
    if !used[0] {
        cmd.args.push(input_s.into_owned());
    }
    let to_file = used[1];
    let _ = std::fs::remove_file(output);
    let mut child = cmd
        .command()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| CoreError::Other(format!("cannot run {}: {e}", cmd.program)))?;
    // Drain the pipes on threads so a chatty converter can't block on a full pipe.
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut p) = pipe {
                let _ = p.read_to_end(&mut buf);
            }
            buf
        })
    };
    let stdout = drain(child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
    let stderr = drain(child.stderr.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() > CONVERT_TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(CoreError::Other(format!(
                    "{} did not finish within {} seconds",
                    cmd.program,
                    CONVERT_TIMEOUT.as_secs()
                )));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => return Err(CoreError::Other(format!("{}: {e}", cmd.program))),
        }
    };
    let out = stdout.join().unwrap_or_default();
    let err = stderr.join().unwrap_or_default();
    if !status.success() {
        let msg = String::from_utf8_lossy(&err);
        let msg = msg.trim();
        return Err(CoreError::Other(format!(
            "{} failed ({status}){}",
            cmd.program,
            if msg.is_empty() {
                String::new()
            } else {
                format!(": {}", msg.chars().take(400).collect::<String>())
            }
        )));
    }
    if to_file {
        let text = pcompare_core::textfile::load_text(output).map(|t| t.text);
        let _ = std::fs::remove_file(output);
        text.map_err(|e| CoreError::Other(format!("{} wrote no output file ({e})", cmd.program)))
    } else {
        Ok(crate::decode_text(&out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_quoted_words() {
        assert_eq!(
            split_command_line(r#""C:\Program Files\Diff.exe" /x "a ""b""" %1  %2 """#),
            vec![r"C:\Program Files\Diff.exe", "/x", r#"a "b""#, "%1", "%2", ""]
        );
        assert!(split_command_line("   ").is_empty());
    }

    #[test]
    fn expands_tool_placeholders() {
        let c = expand_tool_command(r#""C:\Tools\Diff.exe" /left:%1 %2"#, r"C:\a b.psd", "/r.psd").unwrap();
        assert_eq!(c.program, r"C:\Tools\Diff.exe");
        assert_eq!(c.args, vec![r"/left:C:\a b.psd", "/r.psd"]);
        // Without placeholders the paths are appended.
        let c = expand_tool_command("difftool", "l", "r").unwrap();
        assert_eq!(c.args, vec!["l", "r"]);
        assert!(expand_tool_command("  ", "l", "r").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn runs_conversion_commands() {
        let dir = std::env::temp_dir().join(format!("pcompare-convert-ext-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let input = dir.join("in file.txt");
        let output = dir.join("out.txt");
        std::fs::write(&input, "b\na\n").unwrap();
        // Output via %t.
        let text = run_conversion_command("sort -o %t %s", &input, &output).unwrap();
        assert_eq!(text, "a\nb\n");
        assert!(!output.exists(), "the temporary output is removed");
        // Output via stdout, input appended.
        assert_eq!(run_conversion_command("cat", &input, &output).unwrap(), "b\na\n");
        // Failures carry stderr.
        let err = run_conversion_command("cat --definitely-not-an-option", &input, &output).unwrap_err();
        assert!(err.to_string().contains("failed"), "{err}");
        assert!(run_conversion_command("pcompare-no-such-program %s", &input, &output).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
