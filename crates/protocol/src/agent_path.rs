use std::{fmt, ops::Deref, str::FromStr};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AgentPathError {
    #[error("agent path must not be empty")]
    EmptyPath,
    #[error("absolute agent paths must start with `/root`")]
    InvalidRoot,
    #[error("absolute agent path must not end with `/`")]
    AbsoluteTrailingSlash,
    #[error("relative agent path must not end with `/`")]
    RelativeTrailingSlash,
    #[error("agent_name must not be empty")]
    EmptyAgentName,
    #[error("agent_name `{name}` is reserved")]
    ReservedAgentName { name: String },
    #[error("agent_name must not contain `/`")]
    AgentNameContainsSlash,
    #[error("agent_name must use only lowercase letters, digits, and underscores")]
    InvalidAgentNameCharacters,
}

#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(try_from = "String", into = "String")]
#[schemars(with = "String")]
pub struct AgentPath(String);

impl AgentPath {
    pub const ROOT: &str = "/root";
    const ROOT_SEGMENT: &str = "root";

    pub fn root() -> Self {
        Self(Self::ROOT.to_string())
    }

    pub fn from_string(path: String) -> Result<Self, AgentPathError> {
        validate_absolute_path(path.as_str())?;
        Ok(Self(path))
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    pub fn is_root(&self) -> bool {
        self.as_str() == Self::ROOT
    }

    pub fn name(&self) -> &str {
        if self.is_root() {
            return Self::ROOT_SEGMENT;
        }

        self.as_str()
            .rsplit('/')
            .next()
            .filter(|segment| !segment.is_empty())
            .unwrap_or(Self::ROOT_SEGMENT)
    }

    pub fn join(&self, agent_name: &str) -> Result<Self, AgentPathError> {
        validate_agent_name(agent_name)?;
        Self::from_string(format!("{self}/{agent_name}"))
    }

    pub fn resolve(&self, reference: &str) -> Result<Self, AgentPathError> {
        if reference.is_empty() {
            return Err(AgentPathError::EmptyPath);
        }
        if reference == Self::ROOT {
            return Ok(Self::root());
        }
        if reference.starts_with('/') {
            return Self::try_from(reference);
        }

        validate_relative_reference(reference)?;
        Self::from_string(format!("{self}/{reference}"))
    }
}

impl TryFrom<String> for AgentPath {
    type Error = AgentPathError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::from_string(value)
    }
}

impl TryFrom<&str> for AgentPath {
    type Error = AgentPathError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::from_string(value.to_string())
    }
}

impl From<AgentPath> for String {
    fn from(value: AgentPath) -> Self {
        value.0
    }
}

impl FromStr for AgentPath {
    type Err = AgentPathError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::try_from(s)
    }
}

impl AsRef<str> for AgentPath {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl Deref for AgentPath {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.as_str()
    }
}

impl fmt::Display for AgentPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

fn validate_agent_name(agent_name: &str) -> Result<(), AgentPathError> {
    if agent_name.is_empty() {
        return Err(AgentPathError::EmptyAgentName);
    }
    if agent_name == AgentPath::ROOT_SEGMENT {
        return Err(AgentPathError::ReservedAgentName {
            name: agent_name.to_string(),
        });
    }
    if agent_name == "." || agent_name == ".." {
        return Err(AgentPathError::ReservedAgentName {
            name: agent_name.to_string(),
        });
    }
    if agent_name.contains('/') {
        return Err(AgentPathError::AgentNameContainsSlash);
    }
    if !agent_name
        .chars()
        .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_')
    {
        return Err(AgentPathError::InvalidAgentNameCharacters);
    }
    Ok(())
}

fn validate_absolute_path(path: &str) -> Result<(), AgentPathError> {
    let Some(stripped) = path.strip_prefix('/') else {
        return Err(AgentPathError::InvalidRoot);
    };
    let mut segments = stripped.split('/');
    let Some(root) = segments.next() else {
        return Err(AgentPathError::EmptyPath);
    };
    if root != AgentPath::ROOT_SEGMENT {
        return Err(AgentPathError::InvalidRoot);
    }
    if stripped.ends_with('/') {
        return Err(AgentPathError::AbsoluteTrailingSlash);
    }
    for segment in segments {
        validate_agent_name(segment)?;
    }
    Ok(())
}

fn validate_relative_reference(reference: &str) -> Result<(), AgentPathError> {
    if reference.ends_with('/') {
        return Err(AgentPathError::RelativeTrailingSlash);
    }
    for segment in reference.split('/') {
        validate_agent_name(segment)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{AgentPath, AgentPathError};

    #[test]
    fn root_has_expected_name() {
        let root = AgentPath::root();
        assert_eq!(root.as_str(), AgentPath::ROOT);
        assert_eq!(root.name(), "root");
        assert!(root.is_root());
    }

    #[test]
    fn join_builds_child_paths() -> Result<(), AgentPathError> {
        let root = AgentPath::root();
        let child = root.join("researcher")?;
        assert_eq!(child.as_str(), "/root/researcher");
        assert_eq!(child.name(), "researcher");
        Ok(())
    }

    #[test]
    fn resolve_supports_relative_and_absolute_references() -> Result<(), AgentPathError> {
        let current = AgentPath::try_from("/root/researcher")?;
        assert_eq!(
            current.resolve("worker")?,
            AgentPath::try_from("/root/researcher/worker")?
        );
        assert_eq!(
            current.resolve("/root/other")?,
            AgentPath::try_from("/root/other")?
        );
        Ok(())
    }

    #[test]
    fn invalid_names_and_paths_are_rejected() {
        assert_eq!(
            AgentPath::root().resolve(""),
            Err(AgentPathError::EmptyPath)
        );
        assert_eq!(
            AgentPath::try_from("/root/"),
            Err(AgentPathError::AbsoluteTrailingSlash)
        );
        assert_eq!(
            AgentPath::root().resolve("worker/"),
            Err(AgentPathError::RelativeTrailingSlash)
        );
        assert_eq!(
            AgentPath::root().join(""),
            Err(AgentPathError::EmptyAgentName)
        );
        assert_eq!(
            AgentPath::root().join("root"),
            Err(AgentPathError::ReservedAgentName {
                name: "root".to_string(),
            })
        );
        assert_eq!(
            AgentPath::root().join("worker/name"),
            Err(AgentPathError::AgentNameContainsSlash)
        );
        assert_eq!(
            AgentPath::root().join("BadName"),
            Err(AgentPathError::InvalidAgentNameCharacters)
        );
        assert_eq!(
            AgentPath::try_from("/not-root"),
            Err(AgentPathError::InvalidRoot)
        );
        assert_eq!(
            AgentPath::root().resolve("../sibling"),
            Err(AgentPathError::ReservedAgentName {
                name: "..".to_string(),
            })
        );
    }

    #[test]
    fn agent_path_serde_uses_validated_string_shape() -> Result<(), Box<dyn std::error::Error>> {
        let path = AgentPath::try_from("/root/researcher")?;
        let value = serde_json::to_value(&path)?;
        assert_eq!(value, serde_json::json!("/root/researcher"));
        let decoded: AgentPath = serde_json::from_value(value)?;
        assert_eq!(decoded, path);

        let invalid = serde_json::from_value::<AgentPath>(serde_json::json!("/root/BadName"));
        assert!(invalid.is_err());
        Ok(())
    }
}
