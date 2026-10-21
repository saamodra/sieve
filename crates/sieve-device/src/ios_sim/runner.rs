use std::process::Command;

use anyhow::{Context, Result, bail};

/// Runs an external program and returns its stdout.
pub trait CommandRunner {
    fn run(&self, program: &str, args: &[&str]) -> Result<String>;
}

/// Spawns real processes. A non-zero exit becomes an error carrying stderr.
pub struct SystemRunner;

impl CommandRunner for SystemRunner {
    fn run(&self, program: &str, args: &[&str]) -> Result<String> {
        let out = Command::new(program)
            .args(args)
            .output()
            .with_context(|| format!("running `{program}`"))?;
        if !out.status.success() {
            bail!(
                "`{program} {}` failed ({}): {}",
                args.join(" "),
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

#[cfg(test)]
pub(crate) mod mock {
    use std::{cell::RefCell, collections::HashMap};

    use super::*;

    /// Records every call as `"program arg arg"` and answers from canned
    /// replies keyed by the full command line. Unknown commands return "".
    #[derive(Default)]
    pub struct MockRunner {
        pub calls: RefCell<Vec<String>>,
        replies: HashMap<String, String>,
        fail_on: Option<String>,
    }

    impl MockRunner {
        pub fn reply(mut self, cmd: &str, out: &str) -> Self {
            self.replies.insert(cmd.to_string(), out.to_string());
            self
        }

        /// Commands starting with `prefix` return an error.
        pub fn fail(mut self, prefix: &str) -> Self {
            self.fail_on = Some(prefix.to_string());
            self
        }

        pub fn calls(&self) -> Vec<String> {
            self.calls.borrow().clone()
        }

        /// Only the commands that change state.
        pub fn mutations(&self) -> Vec<String> {
            self.calls()
                .into_iter()
                .filter(|c| c.contains(" -set") || c.contains(" keychain "))
                .collect()
        }
    }

    impl CommandRunner for MockRunner {
        fn run(&self, program: &str, args: &[&str]) -> Result<String> {
            let line = format!("{program} {}", args.join(" "));
            self.calls.borrow_mut().push(line.clone());
            if self.fail_on.as_ref().is_some_and(|p| line.starts_with(p)) {
                bail!("mock failure: {line}");
            }
            Ok(self.replies.get(&line).cloned().unwrap_or_default())
        }
    }
}
