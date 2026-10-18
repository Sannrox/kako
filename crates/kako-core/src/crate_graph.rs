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
        let toml = crate_toml("kako-core");
        assert!(
            !toml.contains("sekai-client") && !toml.contains("kako-source-sekai"),
            "kako-core must not depend on the Sekai adapter:\n{toml}"
        );
        assert!(
            !toml.contains("sekai-keys"),
            "kako-core must not depend on sekai-keys:\n{toml}"
        );
    }

    #[test]
    fn sekai_adapter_stays_out_of_core_and_off_the_default_runner() {
        let kako = crate_toml("kako");
        assert!(
            kako.contains("optional = true") && kako.contains("kako-source-sekai"),
            "kako must take kako-source-sekai as an optional feature:\n{kako}"
        );
        let adapter = crate_toml("kako-source-sekai");
        for dep in ["duckdb", "mizuchi", "arrow", "kekkai"] {
            assert!(
                !adapter.contains(dep),
                "kako-source-sekai must not depend on {dep}:\n{adapter}"
            );
        }
        assert!(
            adapter.contains("optional = true") && adapter.contains("sekai-client"),
            "sekai-client must stay optional on kako-source-sekai:\n{adapter}"
        );
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
        for file in ["lib.rs", "run.rs", "local.rs", "pool.rs"] {
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
