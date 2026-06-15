use std::{env, path::Path, process::Command};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct EnvironmentContext {
    pub(crate) working_directory: String,
    pub(crate) is_git_repo: String,
    pub(crate) platform: String,
    pub(crate) os_version: String,
    pub(crate) shell: String,
    pub(crate) rg_available: String,
    pub(crate) fd_available: String,
    pub(crate) eza_available: String,
}

impl EnvironmentContext {
    pub(crate) fn gather(cwd: &Path) -> Self {
        Self {
            working_directory: cwd.display().to_string(),
            is_git_repo: git_repo_status(cwd),
            platform: env::consts::OS.to_string(),
            os_version: os_version(),
            shell: env::var("SHELL").unwrap_or_else(|_| "unknown".to_string()),
            rg_available: command_available("rg"),
            fd_available: command_available("fd"),
            eza_available: command_available("eza"),
        }
    }

    pub(crate) fn apply(&self, prompt: &str) -> String {
        prompt
            .replace("${working_directory}", &self.working_directory)
            .replace("${is_git_repo}", &self.is_git_repo)
            .replace("${platform}", &self.platform)
            .replace("${os_version}", &self.os_version)
            .replace("${shell}", &self.shell)
            .replace("${rg_available}", &self.rg_available)
            .replace("${fd_available}", &self.fd_available)
            .replace("${eza_available}", &self.eza_available)
    }
}

fn git_repo_status(cwd: &Path) -> String {
    let is_repo = Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(cwd)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .is_some_and(|stdout| stdout.trim() == "true");

    if is_repo { "yes" } else { "no" }.to_string()
}

fn os_version() -> String {
    Command::new("uname")
        .arg("-r")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|stdout| stdout.trim().to_string())
        .filter(|version| !version.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

fn command_available(command: &str) -> String {
    let available = Command::new(command)
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success());

    if available { "yes" } else { "no" }.to_string()
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::EnvironmentContext;

    fn context() -> EnvironmentContext {
        EnvironmentContext {
            working_directory: "/workspace/cazean".to_string(),
            is_git_repo: "yes".to_string(),
            platform: "macos".to_string(),
            os_version: "25.0.0".to_string(),
            shell: "/bin/zsh".to_string(),
            rg_available: "yes".to_string(),
            fd_available: "no".to_string(),
            eza_available: "yes".to_string(),
        }
    }

    #[test]
    fn applies_all_supported_placeholders() {
        let prompt = concat!(
            "Working directory: ${working_directory}\n",
            "Git repository: ${is_git_repo}\n",
            "Platform: ${platform}\n",
            "OS version: ${os_version}\n",
            "Shell: ${shell}\n",
            "rg available: ${rg_available}\n",
            "fd available: ${fd_available}\n",
            "eza available: ${eza_available}\n"
        );

        assert_eq!(
            context().apply(prompt),
            concat!(
                "Working directory: /workspace/cazean\n",
                "Git repository: yes\n",
                "Platform: macos\n",
                "OS version: 25.0.0\n",
                "Shell: /bin/zsh\n",
                "rg available: yes\n",
                "fd available: no\n",
                "eza available: yes\n"
            )
        );
    }

    #[test]
    fn leaves_prompts_without_placeholders_unchanged() {
        let prompt = "Static prompt with no environment context.";

        assert_eq!(context().apply(prompt), prompt);
    }

    #[test]
    fn gather_detects_git_repo() -> Result<(), Box<dyn std::error::Error>> {
        let workspace = tempfile::TempDir::new()?;
        let status = Command::new("git")
            .arg("init")
            .arg(workspace.path())
            .status()?;
        assert!(status.success(), "git init should succeed");

        let context = EnvironmentContext::gather(workspace.path());

        assert_eq!(context.is_git_repo, "yes");
        Ok(())
    }

    #[test]
    fn gather_detects_non_repo() -> Result<(), Box<dyn std::error::Error>> {
        let workspace = tempfile::TempDir::new()?;

        let context = EnvironmentContext::gather(workspace.path());

        assert_eq!(context.is_git_repo, "no");
        Ok(())
    }
}
