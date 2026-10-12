//! Compile-graph invariants for the workspace split (ADR 0010).

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    fn workspace_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
    }

    fn crate_toml(name: &str) -> String {
        fs::read_to_string(
            workspace_root()
                .join("crates")
                .join(name)
                .join("Cargo.toml"),
        )
        .unwrap()
    }

    fn assert_no_weight(name: &str) {
        let toml = crate_toml(name);
        for dep in ["duckdb", "mizuchi", "arrow", "kekkai"] {
            assert!(
                !toml.contains(dep),
                "{name} must not depend on {dep}:\n{toml}"
            );
        }
    }

    #[test]
    fn core_carries_no_engine_or_sandbox_weight() {
        assert_no_weight("kako-core");
    }

    #[test]
    fn sandbox_does_not_pull_sql_engines() {
        let toml = crate_toml("kako-sandbox");
        for dep in ["duckdb", "mizuchi", "arrow"] {
            assert!(
                !toml.contains(dep),
                "kako-sandbox must not depend on {dep}:\n{toml}"
            );
        }
        assert!(toml.contains("kekkai"), "{toml}");
    }

    #[test]
    fn run_loop_sources_do_not_name_sql_engines() {
        let kako_src = workspace_root().join("crates/kako/src");
        for file in ["lib.rs", "run.rs", "local.rs"] {
            let text = fs::read_to_string(kako_src.join(file)).unwrap();
            for needle in ["duckdb::", "mizuchi::", "arrow::", "kekkai::"] {
                assert!(
                    !text.contains(needle),
                    "{file} must not name {needle}:\n{text}"
                );
            }
        }
    }
}
