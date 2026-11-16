#![cfg(all(feature = "services", feature = "tools"))]
use hive_agent_core::{services::skills, tools::NativeTools};
use serde_json::json;
use std::collections::HashSet;

/// Words a step may use for something the model does rather than a tool call.
const ABSTRACT_ACTIONS: [&str; 6] = ["act", "verify", "compose", "inspect", "plan", "review"];

fn available_tools() -> HashSet<String> {
    let dir = tempfile::tempdir().unwrap();
    let tools = NativeTools::from_json(json!({"root":dir.path(),"office":{},"obscura":{"allow_evaluate":true,"allow_credentials":true}})).unwrap();
    tools
        .tool_definitions()
        .into_iter()
        .map(|tool| tool.name)
        .collect()
}

#[test]
fn every_bundled_skill_parses_and_only_names_tools_the_sdk_has() {
    let available = available_tools();
    for (name, source) in skills::BUNDLED {
        let skill = skills::parse(source).unwrap_or_else(|error| panic!("{name}: {error:?}"));
        assert_eq!(skill.name, name);
        assert!(
            skill.data["description"]
                .as_str()
                .is_some_and(|text| text.len() > 20)
        );
        let tools = skill.data["tools"]
            .as_array()
            .unwrap_or_else(|| panic!("{name} lists its tools"));
        for tool in tools {
            assert!(
                available.contains(tool.as_str().unwrap()),
                "{name} lists `{tool}`, which the SDK does not define"
            );
        }
        for step in skill.data["steps"].as_array().into_iter().flatten() {
            let action = step["action"].as_str().unwrap();
            assert!(
                available.contains(action) || ABSTRACT_ACTIONS.contains(&action),
                "{name} step action `{action}` is neither a tool nor an abstract action"
            );
        }
        assert!(
            !skill.data["triggers"].as_array().unwrap().is_empty()
                && !skill.data["rules"].as_array().unwrap().is_empty(),
            "{name}"
        );
    }
}

#[test]
fn skills_never_send_the_model_to_chromium_playwright_or_puppeteer() {
    for (name, source) in skills::BUNDLED {
        let skill = skills::parse(source).unwrap();
        let rules: Vec<String> = skill.data["rules"]
            .as_array()
            .unwrap()
            .iter()
            .map(|rule| rule.as_str().unwrap().to_lowercase())
            .collect();
        let forbids = rules.iter().any(|rule| {
            rule.contains("never")
                && rule.contains("chromium")
                && rule.contains("playwright")
                && rule.contains("puppeteer")
        });
        assert!(
            forbids,
            "{name} must carry the rule that Chromium, Chrome, Playwright and Puppeteer are never used"
        );
        // Outside the prohibition, no instruction may recommend those browsers.
        let instructions: String = skill.data["steps"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|step| step["instruction"].as_str())
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase();
        for banned in ["chromium", "playwright", "puppeteer", "chrome"] {
            assert!(
                !instructions.contains(banned),
                "{name} steps mention {banned}"
            );
        }
    }
    let browser = skills::parse(skills::BUNDLED[0].1).unwrap();
    let rules = browser.data["rules"].to_string().to_lowercase();
    assert!(
        rules.contains("obscura")
            && rules.contains("not installed")
            && rules.contains("docs/obscura.md")
    );
    // Obscura's PDF is a picture of the page; the skills must not promise more than that.
    assert!(
        rules.contains("obscura_pdf is only a picture") && rules.contains("no text layer"),
        "{rules}"
    );
    let pdf = skills::parse(skills::BUNDLED[2].1).unwrap();
    assert!(
        pdf.data["rules"]
            .to_string()
            .contains("Do not use obscura_pdf for a report")
    );
}

#[test]
fn the_pdf_skill_requires_styling_and_a_read_back() {
    let pdf = skills::parse(skills::BUNDLED[2].1).unwrap();
    let rules = pdf.data["rules"].to_string().to_lowercase();
    for needle in [
        "blocks",
        "theme",
        "header/footer",
        "page numbers",
        "replaced_characters",
        "office_read_pdf",
        "no ocr",
    ] {
        assert!(
            rules.contains(needle)
                || pdf.data["content"]
                    .as_str()
                    .unwrap()
                    .to_lowercase()
                    .contains(needle),
            "pdf_report_writer must cover `{needle}`"
        );
    }
    let steps: Vec<&str> = pdf.data["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|step| step["action"].as_str().unwrap())
        .collect();
    assert_eq!(
        steps,
        ["compose", "office_write_pdf", "office_read_pdf"],
        "write, then verify by reading back"
    );
    let office = skills::parse(skills::BUNDLED[1].1).unwrap();
    assert!(
        office.data["rules"]
            .to_string()
            .contains("pdf_report_writer"),
        "the office skill points to the PDF skill"
    );
}

#[test]
fn bundled_skills_import_into_a_catalog_without_enabling_anything_else() {
    for (name, source) in skills::BUNDLED {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("SKILL.md"), source).unwrap();
        assert_eq!(skills::load(dir.path()).unwrap().name, name);
    }
}
