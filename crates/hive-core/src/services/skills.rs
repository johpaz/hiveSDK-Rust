//! Bounded native SKILL.md loading. Importing never executes workflow steps.
use crate::{Error, Result};
use serde_json::Value;
use std::{fs::File, io::Read, path::Path};

const MAX_BYTES: usize = 262_144;

pub struct LoadedSkill {
    pub name: String,
    pub data: Value,
}

pub fn load(path: &Path) -> Result<LoadedSkill> {
    let path = if path.is_dir() {
        path.join("SKILL.md")
    } else {
        path.to_owned()
    };
    let file =
        File::open(path).map_err(|e| Error::Input(format!("cannot open skill: {}", e.kind())))?;
    if !file
        .metadata()
        .map_err(|_| Error::Input("cannot inspect skill".into()))?
        .is_file()
    {
        return Err(Error::Input("skill must be a regular file".into()));
    }
    let mut bytes = Vec::new();
    file.take(MAX_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| Error::Input("cannot read skill".into()))?;
    if bytes.len() > MAX_BYTES {
        return Err(Error::Input("skill exceeds 256 KiB".into()));
    }
    let content =
        String::from_utf8(bytes).map_err(|_| Error::Input("skill is not UTF-8".into()))?;
    parse(&content)
}

pub fn parse(content: &str) -> Result<LoadedSkill> {
    if content.len() > MAX_BYTES {
        return Err(Error::Input("skill exceeds 256 KiB".into()));
    }
    let normalized = content.replace("\r\n", "\n");
    let rest = normalized
        .strip_prefix("---\n")
        .ok_or_else(|| Error::Input("skill requires YAML frontmatter".into()))?;
    let (frontmatter, body) = rest
        .split_once("\n---\n")
        .ok_or_else(|| Error::Input("skill frontmatter is not terminated".into()))?;
    let yaml: serde_yaml::Value =
        serde_yaml::from_str(frontmatter).map_err(|_| Error::Input("invalid skill YAML".into()))?;
    let mut data = serde_json::to_value(yaml)
        .map_err(|_| Error::Input("skill metadata must be JSON-compatible".into()))?;
    let object = data
        .as_object_mut()
        .ok_or_else(|| Error::Input("skill frontmatter must be an object".into()))?;
    if object.contains_key("content") {
        return Err(Error::Input(
            "skill content belongs after frontmatter".into(),
        ));
    }
    object.insert("content".into(), Value::String(body.to_owned()));
    object.entry("enabled").or_insert(Value::Bool(true));
    let schema = super::schema(super::EntityKind::Skills);
    if !crate::validation::compile(&schema)?.is_valid(&data) {
        return Err(Error::Input("skill metadata does not match schema".into()));
    }
    let name = data["name"].as_str().unwrap().to_owned();
    super::identifier(&name)?;
    Ok(LoadedSkill { name, data })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_unicode_crlf_and_workflow_metadata_without_executing_it() {
        let skill = parse("---\r\nname: correo\r\ndescription: Redactar correo\r\ntools: [send_mail]\r\nsteps:\r\n  - step: 1\r\n    action: send_mail\r\n    instruction: Escribir\r\n---\r\n# Correo\r\nTexto áéí\r\n").unwrap();
        assert_eq!(skill.name, "correo");
        assert_eq!(skill.data["steps"][0]["action"], "send_mail");
        assert!(skill.data["content"].as_str().unwrap().contains("áéí"));
    }

    #[test]
    fn rejects_invalid_metadata_yaml_and_oversized_files() {
        for invalid in [
            "body",
            "---\nname: x\n",
            "---\nname: [\n---\nbody",
            "---\nname: x\ndescription: x\nproject_id: p\n---\nbody",
            "---\nname: x\nname: y\ndescription: x\n---\nbody",
        ] {
            assert!(parse(invalid).is_err(), "{invalid}");
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("SKILL.md");
        std::fs::write(&path, "a".repeat(MAX_BYTES + 1)).unwrap();
        assert!(load(dir.path()).is_err());
    }
}
