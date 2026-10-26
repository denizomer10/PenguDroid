//! Interpret platform compat metadata for the APK's own targetSdk. This is the
//! default policy; package-specific user/developer overrides remain separate.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Rule { disabled: bool, since: Option<u32> }

fn parse(text: &str) -> Result<BTreeMap<i64, Rule>, String> {
    let doc = roxmltree::Document::parse(text).map_err(|e| e.to_string())?;
    let mut rules = BTreeMap::new();
    for node in doc.descendants().filter(|n| n.has_tag_name("compat-change")) {
        let id = node.attribute("id").ok_or("compat change without id")?
            .parse::<i64>().map_err(|e| e.to_string())?;
        if id <= 0 { return Err("invalid compat change id".into()); }
        let since = node.attribute("enableSinceTargetSdk").map(str::parse::<u32>)
            .transpose().map_err(|e| e.to_string())?;
        let after = node.attribute("enableAfterTargetSdk").map(str::parse::<u32>)
            .transpose().map_err(|e| e.to_string())?
            .map(|v| v.checked_add(1).ok_or("targetSdk threshold overflow"))
            .transpose()?;
        if since.is_some() && after.is_some() && since != after {
            return Err(format!("conflicting targetSdk thresholds for {id}"));
        }
        let disabled = match node.attribute("disabled") {
            None | Some("false") => false, Some("true") => true,
            Some(_) => return Err(format!("invalid disabled flag for {id}")),
        };
        let rule = Rule { disabled, since: since.or(after) };
        if rules.insert(id, rule).is_some_and(|old| old != rule) {
            return Err(format!("conflicting rules for {id}"));
        }
    }
    Ok(rules)
}

fn config_files(directory: &Path, output: &mut Vec<PathBuf>) -> Result<(), String> {
    if !directory.is_dir() { return Ok(()); }
    for entry in std::fs::read_dir(directory).map_err(|e| e.to_string())? {
        let path = entry.map_err(|e| e.to_string())?.path();
        if path.is_file() && path.extension().is_some_and(|e| e == "xml") { output.push(path); }
    }
    Ok(())
}

pub fn disabled_changes(root: &Path, target_sdk: u32) -> Result<Vec<i64>, String> {
    let mut files = Vec::new();
    for partition in ["system", "product", "system_ext", "system/product", "system/system_ext"] {
        config_files(&root.join(partition).join("etc/compatconfig"), &mut files)?;
    }
    let apex = root.join("system/apex");
    if apex.is_dir() {
        for entry in std::fs::read_dir(apex).map_err(|e| e.to_string())? {
            config_files(&entry.map_err(|e| e.to_string())?.path().join("etc/compatconfig"), &mut files)?;
        }
    }
    files.sort(); files.dedup();
    let mut rules = BTreeMap::new();
    for file in files {
        let text = std::fs::read_to_string(&file).map_err(|e| format!("{}: {e}", file.display()))?;
        for (id, rule) in parse(&text).map_err(|e| format!("{}: {e}", file.display()))? {
            if rules.insert(id, rule).is_some_and(|old| old != rule) {
                return Err(format!("conflicting platform compat change {id}"));
            }
        }
    }
    Ok(rules.into_iter().filter_map(|(id, rule)|
        (rule.disabled || rule.since.is_some_and(|since| target_sdk < since)).then_some(id)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sdk_thresholds_and_explicit_disabled_changes_preserve_old_app_behavior() {
        let rules = parse(r#"<config><compat-change id="1" disabled="true" />
            <compat-change id="2" enableAfterTargetSdk="33" />
            <compat-change id="3" enableSinceTargetSdk="37" />
            <compat-change id="4" /></config>"#).unwrap();
        let disabled = |sdk| rules.iter().filter_map(|(&id, rule)|
            (rule.disabled || rule.since.is_some_and(|since| sdk < since)).then_some(id)).collect::<Vec<_>>();
        assert_eq!(disabled(33), vec![1, 2, 3]);
        assert_eq!(disabled(36), vec![1, 3]);
        assert_eq!(disabled(37), vec![1]);
        assert!(parse(r#"<config><compat-change id="1" enableSinceTargetSdk="37" enableAfterTargetSdk="33" /></config>"#).is_err());
    }
}
