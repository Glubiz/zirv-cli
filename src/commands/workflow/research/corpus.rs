//! The task corpus a manifest's `[corpus] file` points at (issue #801):
//! every task's family, class, and dev/validation/holdout split. This
//! module only reads and indexes it -- authoring the real
//! `docs/benchmarks/wrapped-vs-vanilla/corpus.toml` is issue #801's own
//! work, out of this lane's scope.

use std::collections::BTreeSet;
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::manifest::Split;
use crate::commands::ctx::CtxResult;

pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Task {
    pub id: String,
    pub family: String,
    pub class: String,
    pub split: Split,
    #[serde(default)]
    pub kind: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Corpus {
    pub schema: u32,
    #[serde(default)]
    pub version: String,
    #[serde(rename = "task", default)]
    pub tasks: Vec<Task>,
}

impl Corpus {
    pub fn parse(text: &str) -> CtxResult<Self> {
        let corpus: Corpus = toml::from_str(text)?;
        if corpus.schema != SCHEMA_VERSION {
            return Err(format!(
                "corpus schema {} is unsupported (expected {SCHEMA_VERSION})",
                corpus.schema
            )
            .into());
        }
        let mut seen = BTreeSet::new();
        for task in &corpus.tasks {
            if !seen.insert(task.id.as_str()) {
                return Err(format!("duplicate corpus task id '{}'", task.id).into());
            }
        }
        Ok(corpus)
    }

    pub fn load(path: &Path) -> CtxResult<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|err| format!("could not read corpus '{}': {err}", path.display()))?;
        Self::parse(&text)
    }

    pub fn tasks_for_split(&self, split: Split) -> Vec<&Task> {
        self.tasks
            .iter()
            .filter(|task| task.split == split)
            .collect()
    }

    pub fn families(&self) -> BTreeSet<&str> {
        self.tasks.iter().map(|task| task.family.as_str()).collect()
    }

    pub fn is_single_family(&self) -> bool {
        self.families().len() <= 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> &'static str {
        r#"
schema = 1
version = "1"

[[task]]
id = "t1"
family = "ledgerlite"
class = "bounded"
split = "dev"

[[task]]
id = "t2"
family = "ledgerlite"
class = "bug"
split = "validation"

[[task]]
id = "t3"
family = "ledgerlite"
class = "bug"
split = "holdout"
"#
    }

    #[test]
    fn splits_index_correctly() {
        let corpus = Corpus::parse(sample()).unwrap();
        assert_eq!(corpus.tasks_for_split(Split::Dev).len(), 1);
        assert_eq!(corpus.tasks_for_split(Split::Validation).len(), 1);
        assert_eq!(corpus.tasks_for_split(Split::Holdout).len(), 1);
        assert!(corpus.is_single_family());
    }

    #[test]
    fn duplicate_task_ids_are_refused() {
        let text = format!(
            "{}\n[[task]]\nid = \"t1\"\nfamily = \"x\"\nclass = \"y\"\nsplit = \"dev\"\n",
            sample()
        );
        assert!(Corpus::parse(&text).is_err());
    }
}
