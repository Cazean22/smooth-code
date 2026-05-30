use std::{env, path::Path, process::Command};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct EnvironmentContext {
    pub(crate) working_directory: String,
    pub(crate) is_git_repo: String,
    pub(crate) platform: String,
    pub(crate) os_version: String,
    pub(crate) shell: String,
}

impl EnvironmentContext {
    pub(crate) fn gather(cwd: &Path) -> Self {
        Self {
            working_directory: cwd.display().to_string(),
            is_git_repo: git_repo_status(cwd),
            platform: env::consts::OS.to_string(),
            os_version: os_version(),
            shell: env::var("SHELL").unwrap_or_else(|_| "unknown".to_string()),
        }
    }

    pub(crate) fn apply(&self, prompt: &str) -> String {
        prompt
            .replace("${working_directory}", &self.working_directory)
            .replace("${is_git_repo}", &self.is_git_repo)
            .replace("${platform}", &self.platform)
            .replace("${os_version}", &self.os_version)
            .replace("${shell}", &self.shell)
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

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::EnvironmentContext;

    fn context() -> EnvironmentContext {
        EnvironmentContext {
            working_directory: "/workspace/smooth-code".to_string(),
            is_git_repo: "yes".to_string(),
            platform: "macos".to_string(),
            os_version: "25.0.0".to_string(),
            shell: "/bin/zsh".to_string(),
        }
    }

    #[test]
    fn applies_all_supported_placeholders() {
        let prompt = concat!(
            "Working directory: ${working_directory}\n",
            "Git repository: ${is_git_repo}\n",
            "Platform: ${platform}\n",
            "OS version: ${os_version}\n",
            "Shell: ${shell}\n"
        );

        assert_eq!(
            context().apply(prompt),
            concat!(
                "Working directory: /workspace/smooth-code\n",
                "Git repository: yes\n",
                "Platform: macos\n",
                "OS version: 25.0.0\n",
                "Shell: /bin/zsh\n"
            )
        );
    }

    #[test]
    fn leaves_prompts_without_placeholders_unchanged() {
        let prompt = "Static prompt with no environment context.";

        assert_eq!(context().apply(prompt), prompt);
    }

    #[test]
    fn gather_detects_git_repo() {
        let workspace = tempfile::TempDir::new().expect("tempdir");
        let status = Command::new("git")
            .arg("init")
            .arg(workspace.path())
            .status()
            .expect("git init should run");
        assert!(status.success(), "git init should succeed");

        let context = EnvironmentContext::gather(workspace.path());

        assert_eq!(context.is_git_repo, "yes");
    }

    #[test]
    fn gather_detects_non_repo() {
        let workspace = tempfile::TempDir::new().expect("tempdir");

        let context = EnvironmentContext::gather(workspace.path());

        assert_eq!(context.is_git_repo, "no");
    }
}
