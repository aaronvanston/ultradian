//! The command catalog, frozen from 0.2.1's `schema --json` (catalog.json).
//! It is the single description of every command: `schema` and `describe`
//! print it, and the command-line parser is built from it, so what the
//! catalog promises and what the parser accepts cannot drift apart.

use std::sync::OnceLock;

use serde::Deserialize;
use serde_json::Value;

const CATALOG_JSON: &str = include_str!("catalog.json");

#[derive(Debug, Deserialize)]
pub struct Catalog {
    pub commands: Vec<CatalogCommand>,
    #[serde(rename = "schemaVersion")]
    pub schema_version: i64,
}

#[derive(Debug, Deserialize)]
pub struct CatalogCommand {
    pub path: Vec<String>,
    pub aliases: Vec<String>,
    pub module: String,
    pub summary: String,
    pub description: Option<String>,
    pub examples: Vec<String>,
    pub kind: String,
    pub arguments: Vec<CatalogArgument>,
    pub options: Vec<CatalogOption>,
}

#[derive(Debug, Deserialize)]
pub struct CatalogArgument {
    pub name: String,
    pub required: bool,
    pub variadic: bool,
    pub description: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CatalogOption {
    pub flags: String,
    pub description: String,
    pub choices: Option<Vec<String>>,
    #[serde(rename = "defaultValue")]
    pub default_value: Option<Value>,
}

/// The catalog as JSON, keys in their original order, for printing.
pub fn json() -> &'static Value {
    static JSON: OnceLock<Value> = OnceLock::new();
    JSON.get_or_init(|| serde_json::from_str(CATALOG_JSON).expect("catalog.json is valid JSON"))
}

/// The catalog as types, for building the parser.
pub fn catalog() -> &'static Catalog {
    static CATALOG: OnceLock<Catalog> = OnceLock::new();
    CATALOG.get_or_init(|| {
        serde_json::from_str(CATALOG_JSON).expect("catalog.json matches the catalog shape")
    })
}

impl Catalog {
    pub fn find(&self, path: &[String]) -> Option<&CatalogCommand> {
        self.commands.iter().find(|command| command.path == path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_embedded_catalog_parses() {
        let catalog = catalog();
        assert_eq!(catalog.schema_version, crate::output::SCHEMA_VERSION);
        assert_eq!(
            catalog.commands.len(),
            json()["commands"].as_array().map_or(0, Vec::len)
        );
        assert!(catalog.find(&["daemon".into(), "install".into()]).is_some());
    }
}
