//! Read-only inventory of skills owned by the standard Hermes user install.
//!
//! This bridge never accepts a profile path. It reads only `~/.hermes`, keeps
//! Hermes as the lifecycle owner, and exposes enough state for MCP, CLI, and
//! dashboard consumers to inspect skills and pending approvals.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::config_error;
use super::skill_frontmatter::{SkillFrontmatterValue, parse_skill_frontmatter};
use crate::errors::Result;

const MAX_SKILL_BODY_CHARS: usize = 100_000;
const MAX_SKILL_DEPTH: usize = 4;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HermesSkillBridgeOptions {
    pub include_skill_bodies: bool,
    pub include_pending_payloads: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HermesSkillBridgeSnapshot {
    pub agent_home: PathBuf,
    pub skills_dir: PathBuf,
    pub skill_count: usize,
    pub pending_skill_count: usize,
    pub pending_skill_corrupt_count: usize,
    pub usage_record_count: usize,
    pub archive_count: usize,
    pub skills: Vec<HermesSkillSummary>,
    pub pending_skills: Vec<HermesPendingSkillWrite>,
    pub usage_records: BTreeMap<String, Value>,
    pub contracts: HermesSkillBridgeContracts,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HermesSkillSummary {
    pub name: String,
    pub path: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body_markdown: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Value>,
    pub pending_write_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HermesPendingSkillWrite {
    pub id: String,
    pub source_path: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HermesSkillBridgeContracts {
    pub lifecycle_owner: String,
    pub mutation_policy: String,
    pub discovery_policy: String,
}

impl Default for HermesSkillBridgeContracts {
    fn default() -> Self {
        Self {
            lifecycle_owner: "hermes".to_string(),
            mutation_policy: "read_only; use Hermes to mutate Hermes-owned skills".to_string(),
            discovery_policy: "standard_user_install_only".to_string(),
        }
    }
}

/// Loads Hermes-owned skill state from the one supported user install.
pub fn load_standard_hermes_skill_bridge(
    options: HermesSkillBridgeOptions,
) -> Result<HermesSkillBridgeSnapshot> {
    let user_home = crate::agents::home_dir().ok_or_else(|| {
        config_error("could not determine the user home for Hermes skill inventory")
    })?;
    load_standard_hermes_skill_bridge_from_user_home(&user_home, options)
}

fn load_standard_hermes_skill_bridge_from_user_home(
    user_home: &Path,
    options: HermesSkillBridgeOptions,
) -> Result<HermesSkillBridgeSnapshot> {
    let agent_home = user_home.join(".hermes");
    let skills_dir = agent_home.join("skills");
    let usage_records = load_usage_records(&skills_dir)?;
    let (pending_skills, pending_skill_corrupt_count) =
        load_pending_skill_writes(&agent_home, options.include_pending_payloads)?;
    let mut pending_by_name: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for pending in &pending_skills {
        if let Some(name) = &pending.name {
            pending_by_name
                .entry(name.clone())
                .or_default()
                .push(pending.id.clone());
        }
    }

    let mut skill_dirs = Vec::new();
    collect_skill_dirs(&skills_dir, &skills_dir, 0, &mut skill_dirs)?;
    let mut skills = Vec::with_capacity(skill_dirs.len());
    for (skill_dir, skill_md) in skill_dirs {
        let contents = fs::read_to_string(&skill_md).map_err(|error| {
            config_error(format!(
                "failed to read Hermes skill '{}': {error}",
                skill_md.display()
            ))
        })?;
        let frontmatter = parse_scalar_frontmatter(&contents);
        let name = frontmatter
            .get("name")
            .or_else(|| frontmatter.get("id"))
            .cloned()
            .unwrap_or_else(|| {
                skill_dir
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            });
        let relative_parent = skill_dir
            .parent()
            .and_then(|parent| parent.strip_prefix(&skills_dir).ok());
        let category = relative_parent
            .filter(|relative| !relative.as_os_str().is_empty())
            .map(|relative| relative.display().to_string());
        skills.push(HermesSkillSummary {
            path: skill_dir,
            category,
            description: frontmatter
                .get("description")
                .or_else(|| frontmatter.get("summary"))
                .cloned(),
            body_markdown: options
                .include_skill_bodies
                .then(|| contents.chars().take(MAX_SKILL_BODY_CHARS).collect()),
            usage: usage_records.get(&name).cloned(),
            pending_write_ids: pending_by_name.remove(&name).unwrap_or_default(),
            name,
        });
    }
    skills.sort_by(|left, right| left.name.cmp(&right.name));

    let archive_count = count_visible_entries(&skills_dir.join(".archive"))?;
    Ok(HermesSkillBridgeSnapshot {
        agent_home,
        skills_dir,
        skill_count: skills.len(),
        pending_skill_count: pending_skills.len(),
        pending_skill_corrupt_count,
        usage_record_count: usage_records.len(),
        archive_count,
        skills,
        pending_skills,
        usage_records,
        contracts: HermesSkillBridgeContracts::default(),
    })
}

fn collect_skill_dirs(
    root: &Path,
    directory: &Path,
    depth: usize,
    skills: &mut Vec<(PathBuf, PathBuf)>,
) -> Result<()> {
    if depth > MAX_SKILL_DEPTH {
        return Ok(());
    }
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(config_error(format!(
                "failed to read Hermes skills directory '{}': {error}",
                directory.display()
            )));
        }
    };
    for entry in entries {
        let entry = entry.map_err(|error| {
            config_error(format!(
                "failed to read Hermes skill entry '{}': {error}",
                directory.display()
            ))
        })?;
        let file_type = entry.file_type().map_err(|error| {
            config_error(format!(
                "failed to inspect Hermes skill entry '{}': {error}",
                entry.path().display()
            ))
        })?;
        if !file_type.is_dir() || file_type.is_symlink() {
            continue;
        }
        let path = entry.path();
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let skill_md = path.join("SKILL.md");
        if skill_md.is_file() {
            skills.push((path, skill_md));
        } else if path.starts_with(root) {
            collect_skill_dirs(root, &path, depth + 1, skills)?;
        }
    }
    Ok(())
}

fn load_usage_records(skills_dir: &Path) -> Result<BTreeMap<String, Value>> {
    let path = skills_dir.join(".usage.json");
    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => {
            return Err(config_error(format!(
                "failed to read Hermes skill usage '{}': {error}",
                path.display()
            )));
        }
    };
    Ok(serde_json::from_str(&contents).unwrap_or_default())
}

fn load_pending_skill_writes(
    agent_home: &Path,
    include_payloads: bool,
) -> Result<(Vec<HermesPendingSkillWrite>, usize)> {
    let pending_dir = agent_home.join("pending/skills");
    let entries = match fs::read_dir(&pending_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok((Vec::new(), 0)),
        Err(error) => {
            return Err(config_error(format!(
                "failed to read Hermes pending skills '{}': {error}",
                pending_dir.display()
            )));
        }
    };
    let mut pending = Vec::new();
    let mut corrupt_count = 0;
    for entry in entries {
        let entry = entry.map_err(|error| {
            config_error(format!(
                "failed to read Hermes pending skill entry '{}': {error}",
                pending_dir.display()
            ))
        })?;
        let path = entry.path();
        if entry
            .file_type()
            .map(|kind| !kind.is_file())
            .unwrap_or(true)
            || path.extension().and_then(|extension| extension.to_str()) != Some("json")
        {
            continue;
        }
        let contents = fs::read_to_string(&path).map_err(|error| {
            config_error(format!(
                "failed to read Hermes pending skill '{}': {error}",
                path.display()
            ))
        })?;
        let Ok(value) = serde_json::from_str::<Value>(&contents) else {
            corrupt_count += 1;
            continue;
        };
        let payload = value.get("payload").cloned();
        let string = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_string);
        let name = payload
            .as_ref()
            .and_then(|payload| payload.get("name"))
            .and_then(Value::as_str)
            .map(str::to_string);
        pending.push(HermesPendingSkillWrite {
            id: string("id").unwrap_or_else(|| {
                path.file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            }),
            source_path: path,
            action: string("action"),
            name,
            summary: string("summary"),
            origin: string("origin"),
            created_at: value.get("created_at").cloned(),
            payload: include_payloads.then_some(payload).flatten(),
        });
    }
    pending.sort_by(|left, right| left.id.cmp(&right.id));
    Ok((pending, corrupt_count))
}

fn count_visible_entries(directory: &Path) -> Result<usize> {
    match fs::read_dir(directory) {
        Ok(entries) => Ok(entries
            .filter_map(std::result::Result::ok)
            .filter(|entry| !entry.file_name().to_string_lossy().starts_with('.'))
            .count()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(config_error(format!(
            "failed to read Hermes skill archive '{}': {error}",
            directory.display()
        ))),
    }
}

fn parse_scalar_frontmatter(contents: &str) -> BTreeMap<String, String> {
    parse_skill_frontmatter(contents)
        .map(|fields| {
            fields
                .into_iter()
                .filter_map(|(key, value)| match value {
                    SkillFrontmatterValue::Scalar(value) => Some((key.to_ascii_lowercase(), value)),
                    SkillFrontmatterValue::Block(_) => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_inventory_reads_skills_pending_and_usage_without_a_profile_selector() {
        let temp = tempfile::tempdir().unwrap();
        let skill = temp.path().join(".hermes/skills/workflow");
        fs::create_dir_all(&skill).unwrap();
        fs::write(
            skill.join("SKILL.md"),
            "---\nname: workflow\ndescription: Reusable workflow\n---\n\nDo the work.\n",
        )
        .unwrap();
        fs::write(
            temp.path().join(".hermes/skills/.usage.json"),
            r#"{"workflow":{"uses":3}}"#,
        )
        .unwrap();
        let pending = temp.path().join(".hermes/pending/skills");
        fs::create_dir_all(&pending).unwrap();
        fs::write(
            pending.join("pending-1.json"),
            r#"{"id":"pending-1","payload":{"name":"workflow","body":"draft"}}"#,
        )
        .unwrap();

        let snapshot = load_standard_hermes_skill_bridge_from_user_home(
            temp.path(),
            HermesSkillBridgeOptions {
                include_skill_bodies: true,
                include_pending_payloads: false,
            },
        )
        .unwrap();

        assert_eq!(snapshot.agent_home, temp.path().join(".hermes"));
        assert_eq!(snapshot.skill_count, 1);
        assert_eq!(snapshot.pending_skill_count, 1);
        assert_eq!(snapshot.usage_record_count, 1);
        assert_eq!(snapshot.skills[0].pending_write_ids, ["pending-1"]);
        assert!(snapshot.skills[0].body_markdown.is_some());
        assert!(snapshot.pending_skills[0].payload.is_none());
        assert_eq!(
            snapshot.contracts.discovery_policy,
            "standard_user_install_only"
        );
    }

    #[test]
    fn inventory_skips_symlinked_skill_directories() {
        let temp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::create_dir_all(outside.path().join("escaped")).unwrap();
        fs::write(
            outside.path().join("escaped/SKILL.md"),
            "---\nname: escaped\n---\n",
        )
        .unwrap();
        let skills = temp.path().join(".hermes/skills");
        fs::create_dir_all(&skills).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.path().join("escaped"), skills.join("escaped")).unwrap();

        let snapshot = load_standard_hermes_skill_bridge_from_user_home(
            temp.path(),
            HermesSkillBridgeOptions::default(),
        )
        .unwrap();
        assert!(snapshot.skills.is_empty());
    }
}
